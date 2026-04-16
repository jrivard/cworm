// Copyright 2026 Jason D. Rivard. Licensed under the GNU LGPL v2.1.

//! cworm — NetWare MONITOR.NLM-style worm screensaver.
//!
//!   • One worm per online CPU
//!   • Worm length  ∝  CPU utilisation²  (per core, from /proc/stat)
//!   • Global speed ∝  system load average  (/proc/loadavg)
//!   • Busier / longer worms move faster
//!   • Max length scales with terminal area

use std::collections::VecDeque;
use std::io;
use std::time::{Duration, Instant};

use crossterm::{
    event::{self, Event},
    execute,
    terminal::{disable_raw_mode, enable_raw_mode, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Frame,
    backend::CrosstermBackend,
    style::{Color, Style},
    widgets::Block,
    Terminal,
};

// ── worm appearance ───────────────────────────────────────────────────────────
// CP437: 219=█  178=▓  177=▒  176=░  (exact worm_chars[] from original)
const WORM_CHARS: [&str; 4] = ["█", "▓", "▒", "░"];

// Each worm cell is 2 terminal columns wide, matching x[n] and x[n]+1 draws.
const CELL_W: i16 = 2;

const WORM_MIN_LEN: usize = 4;
const WORM_MAX_LEN: usize = 36;   // absolute ceiling
const MAX_WORMS:    usize = 64;   // matches original MAX_WORMS

// Screen-area constants from original (AREA_BASE_LEN, AREA_EXT_LEN logic).
const AREA_BASE_LEN: usize = WORM_MAX_LEN / 2;   // 18
const AREA_MINLINES: u32   = 19;
const AREA_MINCOLS:  u32   = 80;
const AREA_MAXLINES: u32   = 64;
const AREA_MAXCOLS:  u32   = 160;

fn worm_max_length(cols: u16, rows: u16) -> usize {
    let area     = (cols as u32 * rows as u32)
                   .max(AREA_MINLINES * AREA_MINCOLS) as usize;
    let area_min = (AREA_MINLINES * AREA_MINCOLS) as usize;
    let area_max = (AREA_MAXLINES * AREA_MAXCOLS) as usize;
    let divisor  = (area_max - area_min) / AREA_BASE_LEN;
    let ext      = ((area - area_min) / divisor.max(1)).min(AREA_BASE_LEN);
    (AREA_BASE_LEN + ext).min(WORM_MAX_LEN)
}

// ── timing constants (from original nanosecond values, converted to ms) ──────
const MAX_DELAY_MS: u64 = 100;   // MAX_NANOSEC / 1_000_000
const MIN_DELAY_MS: u64 = 10;    // MIN_NANOSEC / 1_000_000
const MAX_LOADAVG:  u64 = 100;

fn load_to_delay_ms(load: f32) -> u64 {
    // delay = 100ms − load × 90ms/100  (Merkey: MAX_NANOSEC - n * range/MAX_LOADAVG)
    // Avoid integer truncation by keeping one decimal place of precision.
    let load = load.clamp(0.0, MAX_LOADAVG as f32);
    let delay = MAX_DELAY_MS as f32 - load * (MAX_DELAY_MS - MIN_DELAY_MS) as f32 / MAX_LOADAVG as f32;
    (delay.round() as u64).clamp(MIN_DELAY_MS, MAX_DELAY_MS)
}

// ── 16-color CGA palette (exact worm_colors[] order from original) ────────────
const COLORS: [(u8, u8, u8); 16] = [
    (255,  85,  85),  //  0  LTRED
    (  0,   0, 170),  //  1  BLUE
    ( 85, 255,  85),  //  2  LTGREEN
    ( 85, 255, 255),  //  3  LTCYAN
    (255, 255,  85),  //  4  YELLOW
    (255, 255, 255),  //  5  BRITEWHITE
    (170,   0, 170),  //  6  MAGENTA
    (170,  85,   0),  //  7  BROWN
    (170,   0,   0),  //  8  RED
    ( 85,  85, 255),  //  9  LTBLUE
    (255,  85, 255),  // 10  LTMAGENTA
    (170, 170, 170),  // 11  GRAY
    (255,  85,  85),  // 12  LTRED (repeat)
    (170, 170, 170),  // 13  WHITE
    (  0, 170,   0),  // 14  GREEN
    (  0, 170, 170),  // 15  CYAN
];

// ── CPU sampling (/proc/stat) ─────────────────────────────────────────────────

#[derive(Default, Clone)]
struct CpuTimes {
    user: u64, nice: u64, sys: u64, idle: u64,
    io: u64, irq: u64, sirq: u64, steal: u64,
    guest: u64, guest_nice: u64,
}

struct CpuSampler {
    num_cpus: usize,
    prev:     Vec<CpuTimes>,
    curr:     Vec<CpuTimes>,
}

impl CpuSampler {
    fn new() -> Self {
        let num_cpus = Self::detect_cpus();
        let empty = vec![CpuTimes::default(); num_cpus];
        let mut s = CpuSampler { num_cpus, prev: empty.clone(), curr: empty };
        s.sample();          // prime prev with a first reading
        s.prev = s.curr.clone();
        s
    }

    fn detect_cpus() -> usize {
        std::fs::read_to_string("/proc/cpuinfo")
            .unwrap_or_default()
            .lines()
            .filter(|l| l.starts_with("processor"))
            .count()
            .max(1)
            .min(MAX_WORMS)
    }

    /// Read /proc/stat and update curr[].
    fn sample(&mut self) {
        let Ok(data) = std::fs::read_to_string("/proc/stat") else { return };
        for line in data.lines() {
            if !line.starts_with("cpu") { continue; }
            let tag = line.split_whitespace().next().unwrap_or("");
            // Skip the aggregate "cpu" line; only per-core "cpu0", "cpu1", …
            if tag == "cpu" { continue; }
            let idx: usize = match tag[3..].parse() {
                Ok(n) => n,
                Err(_) => continue,
            };
            if idx >= self.num_cpus { continue; }
            let mut vals = line.split_whitespace().skip(1);
            let mut t = CpuTimes::default();
            macro_rules! next { () => { vals.next().and_then(|v| v.parse().ok()).unwrap_or(0) } }
            t.user = next!(); t.nice = next!(); t.sys  = next!(); t.idle = next!();
            t.io   = next!(); t.irq  = next!(); t.sirq = next!(); t.steal= next!();
            t.guest= next!(); t.guest_nice = next!();
            self.curr[idx] = t;
        }
    }

    /// Advance: move curr → prev, then read fresh curr.
    fn advance(&mut self) {
        std::mem::swap(&mut self.prev, &mut self.curr);
        self.sample();
    }

    /// Returns 0-100 utilisation for the given CPU, matching Merkey's formula.
    fn util_percent(&self, cpu: usize) -> u64 {
        if cpu >= self.num_cpus { return 0; }
        let p = &self.prev[cpu];
        let c = &self.curr[cpu];

        // Merkey computes: load = totaltime_delta − idletime_delta
        // (guest time is already included in user time, so subtract to avoid double-count)
        let du    = (c.user.saturating_sub(c.guest))
                        .saturating_sub(p.user.saturating_sub(p.guest));
        let dn    = (c.nice.saturating_sub(c.guest_nice))
                        .saturating_sub(p.nice.saturating_sub(p.guest_nice));
        let ds    = (c.sys  + c.irq  + c.sirq)
                        .saturating_sub(p.sys + p.irq + p.sirq);
        let di    = (c.idle + c.io).saturating_sub(p.idle + p.io);
        let dst   = c.steal.saturating_sub(p.steal);
        let dvirt = (c.guest + c.guest_nice)
                        .saturating_sub(p.guest + p.guest_nice);

        let total = du + dn + ds + di + dst + dvirt;
        if total == 0 { return 0; }
        total.saturating_sub(di) * 100 / total
    }
}

/// Read the 1-minute load average from /proc/loadavg (returns 0.0 on failure).
fn system_load() -> f32 {
    std::fs::read_to_string("/proc/loadavg")
        .ok()
        .and_then(|s| s.split_whitespace().next()
                       .and_then(|n| n.parse().ok()))
        .unwrap_or(0.0)
}

// ── direction encoding (Merkey's convention, unchanged from previous) ─────────
// 0=down  1=SE  2=right  3=NE  4=up  5=NW  6=left  7=SW
fn dir_delta(dir: u8) -> (i16, i16) {
    match dir {
        0 => (0,       1),
        1 => (1,       1),
        2 => (CELL_W,  0),
        3 => (1,      -1),
        4 => (0,      -1),
        5 => (-1,     -1),
        6 => (-CELL_W, 0),
        7 => (-1,      1),
        _ => (0,       1),
    }
}

// ── character selection (Merkey's div/mod formula) ────────────────────────────
fn char_index(n: usize, length: usize) -> usize {
    let div  = length / 4;
    let mod_ = length % 4;
    if div == 0 { return 0; }
    let c = if n < (div + 1) * mod_ { n / (div + 1) } else { (n - mod_) / div };
    c % 4
}

// ── worm types ────────────────────────────────────────────────────────────────

struct Worm {
    segs:          VecDeque<(u16, u16)>,
    dir:           u8,
    length:        usize,   // current drawn length (grows/shrinks toward target)
    target_length: usize,   // desired length based on CPU load
    runlength:     u16,
    /// When this worm should next advance.  Period = base_delay × (limit+1).
    next_step:     Instant,
}

struct NetwareState {
    worms:          Vec<Worm>,
    rng:            u64,
    cpu:            CpuSampler,
    /// Base step delay derived from /proc/loadavg (shared by all worms).
    base_delay_ms:  u64,
    last_cpu_poll:  Instant,
    /// Cached screen dimensions for worm_max_length.
    term_w:         u16,
    term_h:         u16,
}

// How often to re-read /proc/stat and /proc/loadavg.
const CPU_POLL_MS: u64 = 500;

impl NetwareState {
    fn new(w: u16, h: u16) -> Self {
        let seed = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos() as u64)
            .unwrap_or(0xDEAD_BEEF_1337_CAFE);
        let mut rng  = seed;
        let cpu      = CpuSampler::new();
        let n_worms  = cpu.num_cpus;
        let worms    = (0..n_worms).map(|i| mk_worm(&mut rng, w, h, i)).collect();
        NetwareState {
            worms,
            rng,
            cpu,
            base_delay_ms: MAX_DELAY_MS,
            last_cpu_poll: Instant::now(),
            term_w: w,
            term_h: h,
        }
    }

    fn step(&mut self, w: u16, h: u16) {
        if w < 4 || h < 4 { return; }

        self.term_w = w;
        self.term_h = h;

        // ── poll CPU stats every CPU_POLL_MS ─────────────────────────────────
        if self.last_cpu_poll.elapsed().as_millis() >= CPU_POLL_MS as u128 {
            self.cpu.advance();
            self.base_delay_ms = load_to_delay_ms(system_load());
            let wmax = worm_max_length(w, h);

            for (i, worm) in self.worms.iter_mut().enumerate() {
                let util = self.cpu.util_percent(i);
                // Merkey: len = util² × worm_max_length / 10000
                let target = (util * util * wmax as u64 / 10000) as usize;
                worm.target_length = target.max(WORM_MIN_LEN).min(wmax);
            }
            self.last_cpu_poll = Instant::now();
        }

        // ── step each worm when its timer fires ───────────────────────────────
        // Period = base_delay × (limit+1):
        //   busy long worm  (limit=0) → base_delay   (fastest)
        //   idle short worm (limit=4) → base_delay×5 (slowest)
        let now  = Instant::now();
        let wmax = worm_max_length(w, h);
        for worm in self.worms.iter_mut() {
            if now < worm.next_step { continue; }

            // Grow or shrink one segment toward target.
            if worm.length < worm.target_length {
                worm.length += 1;
            } else if worm.length > worm.target_length {
                worm.length -= 1;
            }
            worm.length = worm.length.max(WORM_MIN_LEN);

            step_worm(worm, w, h, &mut self.rng);

            let limit  = limit_for(worm.length, wmax);
            let period = Duration::from_millis(self.base_delay_ms * (limit as u64 + 1));
            worm.next_step = now + period;
        }
    }
}

/// Merkey: limit = 4 − length / (wmax / 4).  Longer = faster (lower limit).
fn limit_for(length: usize, wmax: usize) -> u32 {
    let div = (wmax / 4).max(1);
    4u32.saturating_sub((length / div) as u32)
}

fn mk_worm(rng: &mut u64, w: u16, h: u16, _cpu: usize) -> Worm {
    let cols = w.max(4) as u64;
    let rows = h.max(4) as u64;
    let col  = (rng_next(rng) % (cols - 1)) as u16;
    let row  = (rng_next(rng) % rows) as u16;
    // Start on a cardinal direction (even), matching Merkey's init.
    let dir  = (((rng_next(rng) % 9) >> 1) << 1) as u8 % 8;

    let mut segs = VecDeque::new();
    for _ in 0..WORM_MIN_LEN { segs.push_back((col, row)); }

    Worm {
        segs,
        dir,
        length:        WORM_MIN_LEN,
        target_length: WORM_MIN_LEN,
        runlength:     WORM_MIN_LEN as u16,
        next_step:     Instant::now(),
    }
}

fn step_worm(worm: &mut Worm, w: u16, h: u16, rng: &mut u64) {
    let (dx, dy) = dir_delta(worm.dir);
    let head  = *worm.segs.front().unwrap();
    let mut nx  = head.0 as i16 + dx;
    let mut ny  = head.1 as i16 + dy;
    let mut dir = worm.dir as i16;

    let max_x = w as i16 - CELL_W;
    let max_y = h as i16 - 1;

    // ── boundary bouncing (Merkey's exact dir ± 4 logic) ─────────────────────
    if nx < 0 && worm.dir >= 5 {
        nx = 1; dir -= 4;
    } else if ny < 0 && (worm.dir >= 3 && worm.dir <= 5) {
        ny = 1; dir -= 4;
    } else if nx >= max_x && (worm.dir >= 1 && worm.dir <= 3) {
        nx = max_x; dir += 4;
    } else if ny >= h as i16 && (worm.dir == 7 || worm.dir == 0 || worm.dir == 1) {
        ny = max_y; dir += 4;
    }
    // ── voluntary direction changes ───────────────────────────────────────────
    else if worm.runlength == 0 {
        let rnd = rng_next(rng) % 128;
        if      rnd > 90 { dir += 2; }   // ~29% chance: 90° turn
        else if rnd == 1 { dir += 1; }
        else if rnd == 2 { dir -= 1; }
        worm.runlength = worm.length as u16;
    } else {
        worm.runlength -= 1;
        let rnd = rng_next(rng) % 128;
        if      rnd == 1 { dir += 1; }
        else if rnd == 2 { dir -= 1; }
    }

    dir = ((dir % 8) + 8) % 8;
    worm.dir = dir as u8;

    let nx = (nx.max(0) as u16).min(w.saturating_sub(CELL_W as u16));
    let ny = (ny.max(0) as u16).min(h.saturating_sub(1));

    worm.segs.push_front((nx, ny));
    while worm.segs.len() > worm.length { worm.segs.pop_back(); }
}

fn rng_next(s: &mut u64) -> u64 {
    *s = s.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
    *s >> 17
}


// ── worm rendering ────────────────────────────────────────────────────────────

fn draw(frame: &mut Frame, nw: &NetwareState) {
    let area = frame.area();

    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Black)),
        area,
    );

    let buf = frame.buffer_mut();

    // Draw tail-first so the head renders on top when worms overlap.
    for (i, worm) in nw.worms.iter().enumerate() {
        let (r, g, b) = COLORS[i % COLORS.len()];
        let length    = worm.segs.len();
        if length < WORM_MIN_LEN { continue; }

        for (n, &(sx, sy)) in worm.segs.iter().enumerate().rev() {
            if sy >= area.height { continue; }
            let ch    = WORM_CHARS[char_index(n, length)];
            let style = Style::default().fg(Color::Rgb(r, g, b)).bg(Color::Black);

            for cw in 0u16..CELL_W as u16 {
                let px = area.x + sx + cw;
                if px < area.x + area.width {
                    let cell = &mut buf[(px, area.y + sy)];
                    cell.set_symbol(ch);
                    cell.set_style(style);
                }
            }
        }
    }
}

// ── main ──────────────────────────────────────────────────────────────────────

fn main() -> io::Result<()> {
    enable_raw_mode()?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen)?;
    let backend  = CrosstermBackend::new(stdout);
    let mut term = Terminal::new(backend)?;

    let size      = term.size()?;
    let mut state = NetwareState::new(size.width, size.height);

    let result = run(&mut term, &mut state);

    disable_raw_mode()?;
    execute!(term.backend_mut(), LeaveAlternateScreen)?;
    result
}

fn run(
    terminal: &mut Terminal<CrosstermBackend<io::Stdout>>,
    state:    &mut NetwareState,
) -> io::Result<()> {
    loop {
        let size = terminal.size()?;
        state.step(size.width, size.height);
        terminal.draw(|frame| draw(frame, state))?;

        // Always render at ~60 fps; speed differences come from per-worm timers.
        if event::poll(Duration::from_millis(16))? {
            match event::read()? {
                Event::Key(_) => break,
                Event::Resize(_, _) => {
                    // Clear so stale cells from the old (larger) size are gone.
                    terminal.clear()?;
                    // Drain any further queued events (resize bursts on some terminals).
                    while event::poll(Duration::from_millis(0))? {
                        event::read()?;
                    }
                }
                _ => {}
            }
        }
    }
    Ok(())
}
