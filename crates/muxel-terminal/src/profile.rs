//! Opt-in main-thread timing for key → PTY → paint under load.
//!
//! **Off by default.** Enable with `MUXEL_PROFILE_TERMINAL=1` or
//! `MUXEL_PROFILE=1` (`true` / `yes` also work). Stats dump to a log file
//! every ~500 ms while **interesting** events are flowing (keypresses, paint
//! spikes, high felt latency), and a final line after 1 s of quiet.
//!
//! ## Overhead
//! When disabled: one OnceLock bool check per call site (effectively free).
//! When enabled: atomics + occasional `Instant::now()`; a background thread
//! formats one line every 500 ms (append + rotate).
//!
//! Log path (first match wins):
//! 1. `MUXEL_PROFILE_LOG` — absolute or relative path
//! 2. `$XDG_DATA_HOME/term-prof.log` when that env is set
//! 3. `term-prof.log` in the process cwd
//!
//! `MUXEL_PROFILE_STDERR=1` also echoes dump lines to stderr (default: file only
//! — keeps GUI launches quiet and avoids console I/O on the hot path).
//!
//! Example (PowerShell, second instance / sandbox):
//! ```text
//! $env:MUXEL_PROFILE_TERMINAL = "1"
//! $env:MUXEL_PROFILE_LOG = "…\term-prof.log"
//! .\target\debug\muxel.exe
//! ```
//! Hold a key in a terminal; open the log file (no paste needed).
//!
//! Lines are `term-prof[v5 …]` and include paint phase splits
//! (`build=` / `shape=` / `submit=` / `runs=` / `reuse=`) plus felt-latency
//! samples: `key→echo` (keypress until the focused pane's PTY echo is parsed —
//! high here = ConPTY/agent/scheduling, not paint) and `echo→paint` (parsed
//! echo until the focused pane finishes painting — high here = muxel).

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static ENABLED: OnceLock<bool> = OnceLock::new();
static LOG_STDERR: OnceLock<bool> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_FILE: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();

/// Rotate when the profile log reaches this size (keep one `.1` backup).
const PROFILE_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Latest cursor position + cursor-row text of the focused pane, pushed by the
/// drain after each processed batch. Dumps append it so the log shows whether
/// typed characters actually reached the grid during a visually frozen hang.
static LAST_PROBE: Mutex<Option<(usize, i32, String)>> = Mutex::new(None);

/// Whether profiling is on — callers gate probe collection on this.
pub fn is_enabled() -> bool {
    enabled()
}

/// Record the focused pane's cursor row (drain thread, after process_output).
pub fn screen_probe_update(col: usize, row: i32, text: String) {
    if !enabled() {
        return;
    }
    if let Ok(mut g) = LAST_PROBE.lock() {
        *g = Some((col, row, text));
    }
}

fn env_truthy(key: &str) -> Option<bool> {
    std::env::var(key).ok().map(|v| {
        let v = v.trim();
        v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
    })
}

fn enabled() -> bool {
    *ENABLED.get_or_init(|| {
        env_truthy("MUXEL_PROFILE_TERMINAL")
            .or_else(|| env_truthy("MUXEL_PROFILE"))
            .unwrap_or(false)
    })
}

fn log_stderr() -> bool {
    *LOG_STDERR.get_or_init(|| env_truthy("MUXEL_PROFILE_STDERR").unwrap_or(false))
}

fn log_path() -> &'static PathBuf {
    LOG_PATH.get_or_init(|| {
        if let Ok(p) = std::env::var("MUXEL_PROFILE_LOG") {
            let p = p.trim();
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        if let Ok(data) = std::env::var("XDG_DATA_HOME") {
            let data = data.trim();
            if !data.is_empty() {
                return PathBuf::from(data).join("term-prof.log");
            }
        }
        PathBuf::from("term-prof.log")
    })
}

fn open_log_file(path: &Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    // Append across restarts so a long session (or many short ones) builds a
    // corpus; rotate when large so a multi-day run cannot fill the disk.
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() >= PROFILE_LOG_MAX_BYTES
    {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, PathBuf::from(rotated));
    }
    match OpenOptions::new().create(true).append(true).open(path) {
        Ok(f) => Some(f),
        Err(e) => {
            if log_stderr() {
                eprintln!("term-prof: could not open log {}: {e}", path.display());
            }
            None
        }
    }
}

fn emit_line(line: &str) {
    // Wall-clock prefix (epoch seconds.millis) so dump lines correlate with
    // external instruments (capture-window.ps1 hashes, PresentMon traces).
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let line = format!("[{}.{:03}] {line}", now.as_secs(), now.subsec_millis());
    let line = line.as_str();
    if log_stderr() {
        eprintln!("{line}");
    }
    let path = log_path();
    let slot = LOG_FILE.get_or_init(|| {
        let f = open_log_file(path);
        if f.is_some() && log_stderr() {
            eprintln!("term-prof: writing {}", path.display());
        }
        Mutex::new(f)
    });
    if let Ok(mut g) = slot.lock() {
        // Re-open after rotation mid-process when the live file grew too large.
        let needs_reopen = g.as_ref().is_none_or(|f| {
            f.metadata()
                .map(|m| m.len() >= PROFILE_LOG_MAX_BYTES)
                .unwrap_or(false)
        });
        if needs_reopen {
            *g = open_log_file(path);
        }
        if let Some(f) = g.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
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
    paint_focused: AtomicU64,
    paint_full: AtomicU64,
    paint_replay: AtomicU64,
    paint_us: AtomicU64,
    paint_max_us: AtomicU64,
    process_max_us: AtomicU64,
    /// Full-path only: cell walk + batching.
    build_us: AtomicU64,
    /// Full-path only: shape_line work (after reuse).
    shape_us: AtomicU64,
    /// Full-path only: submitting quads/glyphs.
    submit_us: AtomicU64,
    runs_total: AtomicU64,
    runs_reused: AtomicU64,
    /// Paints whose total time exceeded 3ms (hang tails).
    paint_spikes_3ms: AtomicU64,
    /// Paints whose total time exceeded 8ms.
    paint_spikes_8ms: AtomicU64,
    /// µs-epoch of the earliest key not yet answered by a focused echo (0 = none).
    pending_echo: AtomicU64,
    /// µs-epoch of the last focused echo not yet painted (0 = none).
    pending_paint: AtomicU64,
    echo_lat_us: AtomicU64,
    echo_lat_max: AtomicU64,
    echo_lat_n: AtomicU64,
    paint_lat_us: AtomicU64,
    paint_lat_max: AtomicU64,
    paint_lat_n: AtomicU64,
    /// Synchronized-update (DECSET 2026) windows force-expired at deadline.
    sync_expired: AtomicU64,
    last_event: std::sync::Mutex<Option<Instant>>,
    interval_start: std::sync::Mutex<Option<Instant>>,
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
        paint_focused: AtomicU64::new(0),
        paint_full: AtomicU64::new(0),
        paint_replay: AtomicU64::new(0),
        paint_us: AtomicU64::new(0),
        paint_max_us: AtomicU64::new(0),
        process_max_us: AtomicU64::new(0),
        build_us: AtomicU64::new(0),
        shape_us: AtomicU64::new(0),
        submit_us: AtomicU64::new(0),
        runs_total: AtomicU64::new(0),
        runs_reused: AtomicU64::new(0),
        paint_spikes_3ms: AtomicU64::new(0),
        paint_spikes_8ms: AtomicU64::new(0),
        pending_echo: AtomicU64::new(0),
        pending_paint: AtomicU64::new(0),
        echo_lat_us: AtomicU64::new(0),
        echo_lat_max: AtomicU64::new(0),
        echo_lat_n: AtomicU64::new(0),
        paint_lat_us: AtomicU64::new(0),
        paint_lat_max: AtomicU64::new(0),
        paint_lat_n: AtomicU64::new(0),
        sync_expired: AtomicU64::new(0),
        last_event: std::sync::Mutex::new(None),
        interval_start: std::sync::Mutex::new(None),
        flusher_started: AtomicBool::new(false),
    })
}

/// Process-lifetime epoch for lock-free latency timestamps (µs since first use).
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// µs since the profiler epoch; never 0 (so 0 can mean "no sample pending").
fn now_us() -> u64 {
    (EPOCH.get_or_init(Instant::now).elapsed().as_micros() as u64).max(1)
}

/// Samples older than this are dropped as stale — the key had no echo (arrow
/// keys in some TUIs), or the echo was for something else entirely.
const LATENCY_STALE_US: u64 = 500_000;

/// Record `now - t0` into a sum/max/count triple, dropping stale samples.
fn record_latency(t0: u64, sum: &AtomicU64, max: &AtomicU64, n: &AtomicU64) {
    let d = now_us().saturating_sub(t0);
    if t0 == 0 || d >= LATENCY_STALE_US {
        return;
    }
    sum.fetch_add(d, Ordering::Relaxed);
    max.fetch_max(d, Ordering::Relaxed);
    n.fetch_add(1, Ordering::Relaxed);
}

/// Whether a terminal paint walked the grid or replayed a cached draw list.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaintMode {
    Full,
    Replay,
}

/// Phase timings for a full (rebuild) paint. All zero for replay.
#[derive(Clone, Copy, Debug, Default)]
pub struct PaintPhases {
    pub build: Duration,
    pub shape: Duration,
    pub submit: Duration,
    pub runs: u64,
    pub runs_reused: u64,
}

fn touch() {
    let c = counters();
    let now = Instant::now();
    if let Ok(mut g) = c.last_event.lock() {
        *g = Some(now);
    }
    if let Ok(mut g) = c.interval_start.lock()
        && g.is_none()
    {
        *g = Some(now);
    }
    ensure_flusher();
}

fn ensure_flusher() {
    let c = counters();
    if c.flusher_started
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
                    if quiet && let Ok(mut g) = c.last_event.lock() {
                        *g = None;
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
    let paints_f = c.paint_focused.swap(0, Ordering::Relaxed);
    let paint_full = c.paint_full.swap(0, Ordering::Relaxed);
    let paint_replay = c.paint_replay.swap(0, Ordering::Relaxed);
    let paint_us = c.paint_us.swap(0, Ordering::Relaxed);
    let paint_max = c.paint_max_us.swap(0, Ordering::Relaxed);
    let process_max = c.process_max_us.swap(0, Ordering::Relaxed);
    let build_us = c.build_us.swap(0, Ordering::Relaxed);
    let shape_us = c.shape_us.swap(0, Ordering::Relaxed);
    let submit_us = c.submit_us.swap(0, Ordering::Relaxed);
    let runs_total = c.runs_total.swap(0, Ordering::Relaxed);
    let runs_reused = c.runs_reused.swap(0, Ordering::Relaxed);
    let spikes_3 = c.paint_spikes_3ms.swap(0, Ordering::Relaxed);
    let spikes_8 = c.paint_spikes_8ms.swap(0, Ordering::Relaxed);
    let echo_us = c.echo_lat_us.swap(0, Ordering::Relaxed);
    let echo_max = c.echo_lat_max.swap(0, Ordering::Relaxed);
    let echo_n = c.echo_lat_n.swap(0, Ordering::Relaxed);
    let plat_us = c.paint_lat_us.swap(0, Ordering::Relaxed);
    let plat_max = c.paint_lat_max.swap(0, Ordering::Relaxed);
    let plat_n = c.paint_lat_n.swap(0, Ordering::Relaxed);
    let sync_exp = c.sync_expired.swap(0, Ordering::Relaxed);

    if keys == 0 && batches == 0 && paints == 0 && notify == 0 {
        return;
    }

    // Always-on corpus filter: skip pure background paint/notify ticks. Those
    // flood the log under multi-agent load and do not explain typing lag.
    // Keep intervals with keypresses, paint spikes, or high felt latency.
    let interesting = keys > 0
        || spikes_3 > 0
        || spikes_8 > 0
        || echo_max >= 80_000 // ≥80ms key→echo
        || plat_max >= 30_000 // ≥30ms echo→paint
        || paint_max >= 8_000 // ≥8ms single paint
        || tag == "quiet"; // end-of-burst summary still useful after typing
    if !interesting {
        // Counters already swapped to zero above; drop the interval.
        return;
    }

    let win_ms = c
        .interval_start
        .lock()
        .ok()
        .and_then(|mut g| {
            let start = g.take();
            *g = Some(Instant::now());
            start.map(|t| t.elapsed().as_millis())
        })
        .unwrap_or(500)
        .max(1);

    let key_avg = key_us.checked_div(keys).unwrap_or(0);
    let proc_avg = process_us.checked_div(batches).unwrap_or(0);
    let paint_avg = paint_us.checked_div(paints).unwrap_or(0);
    let notify_hz = notify as u128 * 1000 / win_ms;
    let paint_hz = paints as u128 * 1000 / win_ms;
    let key_hz = keys as u128 * 1000 / win_ms;
    let paints_bg = paints.saturating_sub(paints_f);
    let paint_total_ms = paint_us / 1000;
    let paint_pct = paint_us as u128 * 100 / (win_ms * 1000);

    let full_n = paint_full.max(1);
    let build_avg = build_us / full_n;
    let shape_avg = shape_us / full_n;
    let submit_avg = submit_us / full_n;
    let reuse_pct = runs_reused
        .checked_mul(100)
        .and_then(|n| n.checked_div(runs_total))
        .unwrap_or(0);

    let echo_avg = echo_us.checked_div(echo_n).unwrap_or(0);
    let plat_avg = plat_us.checked_div(plat_n).unwrap_or(0);

    // v5: v4 + felt latency (key→echo = ConPTY/agent side, echo→paint = ours).
    let line = format!(
        "term-prof[v5 {tag}] Δ={win_ms}ms keys={keys} (held={keys_held}, ~{key_hz}/s, avg={key_avg}µs) \
         notify={notify} (~{notify_hz}/s) \
         process={batches} batches/{bytes}B avg={proc_avg}µs max={process_max}µs \
         paint={paints} (focus={paints_f} bg={paints_bg} full={paint_full} replay={paint_replay}, ~{paint_hz}/s) \
         avg={paint_avg}µs max={paint_max}µs sum={paint_total_ms}ms (~{paint_pct}% of interval) \
         spikes(>3ms={spikes_3} >8ms={spikes_8}) \
         full-phases: build_avg={build_avg}µs shape_avg={shape_avg}µs submit_avg={submit_avg}µs \
         runs={runs_total} reuse={runs_reused} ({reuse_pct}%) \
         lat: key→echo avg={echo_avg}µs max={echo_max}µs (n={echo_n}) \
         echo→paint avg={plat_avg}µs max={plat_max}µs (n={plat_n}) \
         sync_exp={sync_exp}"
    );
    // Focused-pane grid probe: proves whether typed chars reached the grid.
    let probe = LAST_PROBE.lock().ok().and_then(|g| g.clone());
    let line = match probe {
        Some((col, row, text)) => {
            let clean: String = text
                .chars()
                .map(|c| if c.is_control() { '·' } else { c })
                .take(100)
                .collect();
            format!("{line} cur={col},{row} row=\"{clean}\"")
        }
        None => line,
    };
    emit_line(&line);
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
    // Arm the key→echo latency sample with the FIRST unanswered key (a later
    // key must not shrink an in-flight measurement).
    let _ = c
        .pending_echo
        .compare_exchange(0, now_us(), Ordering::Relaxed, Ordering::Relaxed);
    touch();
}

pub fn notify_scheduled() {
    if !enabled() {
        return;
    }
    counters().notify.fetch_add(1, Ordering::Relaxed);
    touch();
}

pub fn process_output(bytes: usize, elapsed: Duration, focused: bool) {
    if !enabled() {
        return;
    }
    let c = counters();
    c.process_batches.fetch_add(1, Ordering::Relaxed);
    c.process_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    let us = elapsed.as_micros() as u64;
    c.process_us.fetch_add(us, Ordering::Relaxed);
    c.process_max_us.fetch_max(us, Ordering::Relaxed);
    // Focused-pane echo: close the key→echo sample and arm echo→paint. Only the
    // focused pane — a background agent's stream must not answer for a keypress.
    if focused && bytes > 0 {
        let t0 = c.pending_echo.swap(0, Ordering::Relaxed);
        record_latency(t0, &c.echo_lat_us, &c.echo_lat_max, &c.echo_lat_n);
        let _ = c
            .pending_paint
            .compare_exchange(0, now_us(), Ordering::Relaxed, Ordering::Relaxed);
    }
    touch();
}

pub fn paint_with_phases(elapsed: Duration, focused: bool, mode: PaintMode, phases: PaintPhases) {
    if !enabled() {
        return;
    }
    let c = counters();
    c.paint_count.fetch_add(1, Ordering::Relaxed);
    if focused {
        c.paint_focused.fetch_add(1, Ordering::Relaxed);
    }
    match mode {
        PaintMode::Full => {
            c.paint_full.fetch_add(1, Ordering::Relaxed);
            c.build_us
                .fetch_add(phases.build.as_micros() as u64, Ordering::Relaxed);
            c.shape_us
                .fetch_add(phases.shape.as_micros() as u64, Ordering::Relaxed);
            c.submit_us
                .fetch_add(phases.submit.as_micros() as u64, Ordering::Relaxed);
            c.runs_total.fetch_add(phases.runs, Ordering::Relaxed);
            c.runs_reused
                .fetch_add(phases.runs_reused, Ordering::Relaxed);
        }
        PaintMode::Replay => {
            c.paint_replay.fetch_add(1, Ordering::Relaxed);
        }
    }
    if focused {
        let t0 = c.pending_paint.swap(0, Ordering::Relaxed);
        record_latency(t0, &c.paint_lat_us, &c.paint_lat_max, &c.paint_lat_n);
    }
    let us = elapsed.as_micros() as u64;
    c.paint_us.fetch_add(us, Ordering::Relaxed);
    c.paint_max_us.fetch_max(us, Ordering::Relaxed);
    if us > 3000 {
        c.paint_spikes_3ms.fetch_add(1, Ordering::Relaxed);
    }
    if us > 8000 {
        c.paint_spikes_8ms.fetch_add(1, Ordering::Relaxed);
    }
    touch();
}

/// A synchronized-update window (DECSET 2026) was force-expired at its
/// deadline — the TUI held BSU open past the timeout; buffered bytes applied.
pub fn sync_expired() {
    if !enabled() {
        return;
    }
    counters().sync_expired.fetch_add(1, Ordering::Relaxed);
    touch();
}
