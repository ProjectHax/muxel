//! Opt-in main-thread timing for key → PTY → paint under load.
//!
//! Enable with `MUXEL_PROFILE_TERMINAL=1` (or `true` / `yes`). Stats dump to
//! stderr every ~500 ms while events are flowing, and a final line after 1 s of
//! quiet (e.g. after you release a held key).
//!
//! Example (PowerShell, second instance / sandbox):
//! ```text
//! $env:MUXEL_PROFILE_TERMINAL = "1"
//! $env:XDG_CONFIG_HOME = "…\sandbox\config"
//! $env:XDG_DATA_HOME = "…\sandbox\data"
//! .\target\debug\muxel.exe
//! ```
//! Hold a key in a terminal; watch stderr for `term-prof` lines.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

static ENABLED: OnceLock<bool> = OnceLock::new();

fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        std::env::var("MUXEL_PROFILE_TERMINAL")
            .map(|v| {
                let v = v.trim();
                v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
            })
            .unwrap_or(false)
    })
}

struct Counters {
    keys: AtomicU64,
    keys_held: AtomicU64,
    key_us: AtomicU64,
    notify: AtomicU64,
    process_batches: AtomicU64,
    process_bytes: AtomicU64,
    process_us: AtomicU64,
    paint_count: AtomicU64,
    paint_us: AtomicU64,
    paint_max_us: AtomicU64,
    process_max_us: AtomicU64,
    last_event: std::sync::Mutex<Option<Instant>>,
    window_start: std::sync::Mutex<Option<Instant>>,
    flusher_started: AtomicBool,
}

static C: OnceLock<Counters> = OnceLock::new();

fn counters() -> &'static Counters {
    C.get_or_init(|| Counters {
        keys: AtomicU64::new(0),
        keys_held: AtomicU64::new(0),
        key_us: AtomicU64::new(0),
        notify: AtomicU64::new(0),
        process_batches: AtomicU64::new(0),
        process_bytes: AtomicU64::new(0),
        process_us: AtomicU64::new(0),
        paint_count: AtomicU64::new(0),
        paint_us: AtomicU64::new(0),
        paint_max_us: AtomicU64::new(0),
        process_max_us: AtomicU64::new(0),
        last_event: std::sync::Mutex::new(None),
        window_start: std::sync::Mutex::new(None),
        flusher_started: AtomicBool::new(false),
    })
}

fn touch() {
    let c = counters();
    let now = Instant::now();
    if let Ok(mut g) = c.last_event.lock() {
        *g = Some(now);
    }
    if let Ok(mut g) = c.window_start.lock()
        && g.is_none()
    {
        *g = Some(now);
    }
    ensure_flusher();
}

fn ensure_flusher() {
    let c = counters();
    if c
        .flusher_started
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    std::thread::Builder::new()
        .name("muxel-term-prof".into())
        .spawn(|| {
            let mut last_dump = Instant::now();
            loop {
                std::thread::sleep(Duration::from_millis(100));
                let c = counters();
                let last = c.last_event.lock().ok().and_then(|g| *g);
                let Some(last) = last else {
                    continue;
                };
                let quiet = last.elapsed() >= Duration::from_millis(1000);
                let periodic = last_dump.elapsed() >= Duration::from_millis(500);
                if quiet || periodic {
                    dump(if quiet { "quiet" } else { "tick" });
                    last_dump = Instant::now();
                    if quiet {
                        // Reset window so the next hold starts a fresh sample.
                        if let Ok(mut g) = c.window_start.lock() {
                            *g = None;
                        }
                        if let Ok(mut g) = c.last_event.lock() {
                            *g = None;
                        }
                    }
                }
            }
        })
        .ok();
}

fn dump(tag: &str) {
    let c = counters();
    let keys = c.keys.swap(0, Ordering::Relaxed);
    let keys_held = c.keys_held.swap(0, Ordering::Relaxed);
    let key_us = c.key_us.swap(0, Ordering::Relaxed);
    let notify = c.notify.swap(0, Ordering::Relaxed);
    let batches = c.process_batches.swap(0, Ordering::Relaxed);
    let bytes = c.process_bytes.swap(0, Ordering::Relaxed);
    let process_us = c.process_us.swap(0, Ordering::Relaxed);
    let paints = c.paint_count.swap(0, Ordering::Relaxed);
    let paint_us = c.paint_us.swap(0, Ordering::Relaxed);
    let paint_max = c.paint_max_us.swap(0, Ordering::Relaxed);
    let process_max = c.process_max_us.swap(0, Ordering::Relaxed);

    if keys == 0 && batches == 0 && paints == 0 && notify == 0 {
        return;
    }

    let win_ms = c
        .window_start
        .lock()
        .ok()
        .and_then(|g| g.map(|t| t.elapsed().as_millis()))
        .unwrap_or(0)
        .max(1);

    let key_avg = key_us.checked_div(keys).unwrap_or(0);
    let proc_avg = process_us.checked_div(batches).unwrap_or(0);
    let paint_avg = paint_us.checked_div(paints).unwrap_or(0);
    let notify_hz = notify as u128 * 1000 / win_ms;
    let paint_hz = paints as u128 * 1000 / win_ms;
    let key_hz = keys as u128 * 1000 / win_ms;

    eprintln!(
        "term-prof[{tag}] win={win_ms}ms keys={keys} (held={keys_held}, ~{key_hz}/s, avg={key_avg}µs) \
         notify={notify} (~{notify_hz}/s) \
         process={batches} batches/{bytes}B avg={proc_avg}µs max={process_max}µs \
         paint={paints} (~{paint_hz}/s) avg={paint_avg}µs max={paint_max}µs"
    );
}

/// Time a key path that writes to the PTY.
pub fn key_handled(held: bool, elapsed: Duration) {
    if !enabled() {
        return;
    }
    let c = counters();
    c.keys.fetch_add(1, Ordering::Relaxed);
    if held {
        c.keys_held.fetch_add(1, Ordering::Relaxed);
    }
    c.key_us
        .fetch_add(elapsed.as_micros() as u64, Ordering::Relaxed);
    touch();
}

pub fn notify_scheduled() {
    if !enabled() {
        return;
    }
    counters().notify.fetch_add(1, Ordering::Relaxed);
    touch();
}

pub fn process_output(bytes: usize, elapsed: Duration) {
    if !enabled() {
        return;
    }
    let c = counters();
    c.process_batches.fetch_add(1, Ordering::Relaxed);
    c.process_bytes
        .fetch_add(bytes as u64, Ordering::Relaxed);
    let us = elapsed.as_micros() as u64;
    c.process_us.fetch_add(us, Ordering::Relaxed);
    c.process_max_us.fetch_max(us, Ordering::Relaxed);
    touch();
}

pub fn paint(elapsed: Duration) {
    if !enabled() {
        return;
    }
    let c = counters();
    c.paint_count.fetch_add(1, Ordering::Relaxed);
    let us = elapsed.as_micros() as u64;
    c.paint_us.fetch_add(us, Ordering::Relaxed);
    c.paint_max_us.fetch_max(us, Ordering::Relaxed);
    touch();
}

