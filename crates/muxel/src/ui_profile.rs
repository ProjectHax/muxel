//! Opt-in UI / present-pump health log (Windows soft-lag harness).
//!
//! **Off by default.** Complements `muxel_terminal::profile` (terminal
//! key→echo→paint). Covers the whole UI thread — settings typing, present
//! pump cost, message-queue latency — which term-prof cannot see.
//!
//! Enable with any of:
//! - `MUXEL_PROFILE_UI=1`
//! - `MUXEL_PROFILE_TERMINAL=1` (also enables UI profiling)
//! - `MUXEL_PROFILE=1` (terminal + UI in one switch)
//!
//! Log path (first match wins):
//! 1. `MUXEL_PROFILE_UI_LOG`
//! 2. sibling of `MUXEL_PROFILE_LOG` named `ui-prof.log` / `ui-prof-*.log`
//! 3. `$XDG_DATA_HOME/ui-prof.log`
//! 4. `ui-prof.log` in cwd
//!
//! When disabled: single OnceLock check, no flusher, no probe work.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

static ENABLED: OnceLock<bool> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_FILE: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
static STARTED: OnceLock<Instant> = OnceLock::new();
static FLUSHER: AtomicBool = AtomicBool::new(false);

const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Present-pump ticks handled on the UI thread.
static PUMP_N: AtomicU64 = AtomicU64::new(0);
static PUMP_US: AtomicU64 = AtomicU64::new(0);
static PUMP_MAX_US: AtomicU64 = AtomicU64::new(0);
static PUMP_SPIKE_8MS: AtomicU64 = AtomicU64::new(0);
static PUMP_SPIKE_30MS: AtomicU64 = AtomicU64::new(0);
/// Windows enumerated per tick (usually 1).
static PUMP_HWNDS: AtomicU64 = AtomicU64::new(0);
/// Times the pump thread skipped post because a present was still pending
/// (UI thread behind — coalescing).
static PUMP_COALESCE: AtomicU64 = AtomicU64::new(0);
/// Posts successfully queued.
static PUMP_POSTS: AtomicU64 = AtomicU64::new(0);

/// UI probe round-trips (PostMessage → wndproc): sum/max/n in µs.
static PROBE_N: AtomicU64 = AtomicU64::new(0);
static PROBE_US: AtomicU64 = AtomicU64::new(0);
static PROBE_MAX_US: AtomicU64 = AtomicU64::new(0);
static PROBE_SPIKE_50MS: AtomicU64 = AtomicU64::new(0);
static PROBE_SPIKE_200MS: AtomicU64 = AtomicU64::new(0);
/// Probes that never came back within the wait window.
static PROBE_TIMEOUT: AtomicU64 = AtomicU64::new(0);
/// Last probe send tick (GetTickCount64); 0 = none in flight / completed.
#[allow(dead_code)] // present-pump telemetry: only `present_pump` (Windows) writes it
static PROBE_SENT_TICK: AtomicU64 = AtomicU64::new(0);

fn env_truthy(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            let v = v.trim();
            v == "1" || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("yes")
        })
        .unwrap_or(false)
}

pub fn is_enabled() -> bool {
    *ENABLED.get_or_init(|| {
        env_truthy("MUXEL_PROFILE_UI")
            || env_truthy("MUXEL_PROFILE_TERMINAL")
            || env_truthy("MUXEL_PROFILE")
    })
}

fn log_path() -> &'static PathBuf {
    LOG_PATH.get_or_init(|| {
        if let Ok(p) = std::env::var("MUXEL_PROFILE_UI_LOG") {
            let p = p.trim();
            if !p.is_empty() {
                return PathBuf::from(p);
            }
        }
        if let Ok(p) = std::env::var("MUXEL_PROFILE_LOG") {
            let p = PathBuf::from(p.trim());
            if !p.as_os_str().is_empty() {
                // term-prof-system.log → ui-prof-system.log; else sibling ui-prof.log
                let name = p
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("term-prof.log");
                let ui_name = if name.starts_with("term-prof") {
                    name.replacen("term-prof", "ui-prof", 1)
                } else {
                    "ui-prof.log".into()
                };
                return p.with_file_name(ui_name);
            }
        }
        if let Ok(data) = std::env::var("XDG_DATA_HOME") {
            let data = data.trim();
            if !data.is_empty() {
                return PathBuf::from(data).join("ui-prof.log");
            }
        }
        PathBuf::from("ui-prof.log")
    })
}

fn open_log(path: &Path) -> Option<std::fs::File> {
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() >= LOG_MAX_BYTES
    {
        let mut rotated = path.as_os_str().to_owned();
        rotated.push(".1");
        let _ = std::fs::rename(path, PathBuf::from(rotated));
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

fn emit(line: &str) {
    if !is_enabled() {
        return;
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default();
    let line = format!("[{}.{:03}] {line}", now.as_secs(), now.subsec_millis());
    let path = log_path();
    let slot = LOG_FILE.get_or_init(|| Mutex::new(open_log(path)));
    if let Ok(mut g) = slot.lock() {
        let reopen = g.as_ref().is_none_or(|f| {
            f.metadata()
                .map(|m| m.len() >= LOG_MAX_BYTES)
                .unwrap_or(false)
        });
        if reopen {
            *g = open_log(path);
        }
        if let Some(f) = g.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

fn ensure_flusher() {
    if !is_enabled() {
        return;
    }
    if FLUSHER
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    STARTED.get_or_init(Instant::now);
    emit(&format!(
        "ui-prof[start] pid={} path={}",
        std::process::id(),
        log_path().display()
    ));
    std::thread::Builder::new()
        .name("muxel-ui-prof".into())
        .spawn(|| {
            let mut last_hour = Instant::now();
            let mut last_quarter = Instant::now();
            let mut last_tick = Instant::now();
            let mut last_heartbeat = Instant::now();
            // First snapshot after 5 minutes so a short session still leaves a baseline.
            let mut first_snap = false;
            loop {
                std::thread::sleep(Duration::from_secs(2));
                // Heartbeat before tick so a 60s boundary does not leave snapshot
                // with counters already drained by dump_interval.
                if last_heartbeat.elapsed() >= Duration::from_secs(60) {
                    dump_snapshot("1m");
                    last_heartbeat = Instant::now();
                    last_tick = Instant::now();
                } else if last_tick.elapsed() >= Duration::from_secs(10) {
                    // Spike-filtered interval dump (~10s).
                    dump_interval("tick");
                    last_tick = Instant::now();
                }
                let up = STARTED.get().map(|t| t.elapsed()).unwrap_or_default();
                if !first_snap && up >= Duration::from_secs(300) {
                    dump_snapshot("t+5m");
                    first_snap = true;
                    last_quarter = Instant::now();
                }
                // 15-minute snapshots for the multi-hour soft-lag curve.
                if last_quarter.elapsed() >= Duration::from_secs(900) {
                    dump_snapshot("15m");
                    last_quarter = Instant::now();
                }
                // Hourly full snapshot (memory + rates) even if quiet.
                if last_hour.elapsed() >= Duration::from_secs(3600) {
                    dump_snapshot("hourly");
                    last_hour = Instant::now();
                }
            }
        })
        .ok();
}

fn working_set_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;
        unsafe {
            let mut pmc = PROCESS_MEMORY_COUNTERS::default();
            pmc.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
            if GetProcessMemoryInfo(
                GetCurrentProcess(),
                &mut pmc,
                std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
            )
            .is_ok()
            {
                return Some(pmc.WorkingSetSize as u64);
            }
        }
        None
    }
    #[cfg(not(windows))]
    {
        None
    }
}

fn dump_interval(tag: &str) {
    let n = PUMP_N.swap(0, Ordering::Relaxed);
    let us = PUMP_US.swap(0, Ordering::Relaxed);
    let max = PUMP_MAX_US.swap(0, Ordering::Relaxed);
    let s8 = PUMP_SPIKE_8MS.swap(0, Ordering::Relaxed);
    let s30 = PUMP_SPIKE_30MS.swap(0, Ordering::Relaxed);
    let hwnds = PUMP_HWNDS.swap(0, Ordering::Relaxed);
    let coal = PUMP_COALESCE.swap(0, Ordering::Relaxed);
    let posts = PUMP_POSTS.swap(0, Ordering::Relaxed);
    let pn = PROBE_N.swap(0, Ordering::Relaxed);
    let pus = PROBE_US.swap(0, Ordering::Relaxed);
    let pmax = PROBE_MAX_US.swap(0, Ordering::Relaxed);
    let p50 = PROBE_SPIKE_50MS.swap(0, Ordering::Relaxed);
    let p200 = PROBE_SPIKE_200MS.swap(0, Ordering::Relaxed);
    let pto = PROBE_TIMEOUT.swap(0, Ordering::Relaxed);

    if n == 0 && pn == 0 && coal == 0 && posts == 0 && pto == 0 {
        return;
    }

    let pump_avg = us.checked_div(n).unwrap_or(0);
    let probe_avg = pus.checked_div(pn).unwrap_or(0);
    let hwnd_avg = hwnds.checked_div(n.max(1)).unwrap_or(0);

    // Only emit "interesting" ticks to keep multi-hour files small, unless tag
    // is snapshot (handled elsewhere). Always emit if probe/pump spikes.
    let interesting = s8 > 0
        || s30 > 0
        || p50 > 0
        || p200 > 0
        || pto > 0
        || pmax >= 50_000
        || max >= 8_000
        || coal > posts.saturating_add(n).max(1); // heavy coalesce

    if !interesting && tag == "tick" {
        return;
    }

    emit(&format!(
        "ui-prof[v1 {tag}] pump={n} avg={pump_avg}µs max={max}µs spikes(>8ms={s8} >30ms={s30}) \
         hwnds/tick≈{hwnd_avg} posts={posts} coalesce={coal} \
         probe n={pn} avg={probe_avg}µs max={pmax}µs spikes(>50ms={p50} >200ms={p200}) timeout={pto}"
    ));
}

fn dump_snapshot(tag: &str) {
    // Drain interval counters into the snapshot line.
    let n = PUMP_N.swap(0, Ordering::Relaxed);
    let us = PUMP_US.swap(0, Ordering::Relaxed);
    let max = PUMP_MAX_US.swap(0, Ordering::Relaxed);
    let s8 = PUMP_SPIKE_8MS.swap(0, Ordering::Relaxed);
    let s30 = PUMP_SPIKE_30MS.swap(0, Ordering::Relaxed);
    let coal = PUMP_COALESCE.swap(0, Ordering::Relaxed);
    let posts = PUMP_POSTS.swap(0, Ordering::Relaxed);
    let pn = PROBE_N.swap(0, Ordering::Relaxed);
    let pus = PROBE_US.swap(0, Ordering::Relaxed);
    let pmax = PROBE_MAX_US.swap(0, Ordering::Relaxed);
    let p50 = PROBE_SPIKE_50MS.swap(0, Ordering::Relaxed);
    let p200 = PROBE_SPIKE_200MS.swap(0, Ordering::Relaxed);
    let pto = PROBE_TIMEOUT.swap(0, Ordering::Relaxed);
    let _ = PUMP_HWNDS.swap(0, Ordering::Relaxed);

    let up_s = STARTED.get().map(|t| t.elapsed().as_secs()).unwrap_or(0);
    let ws = working_set_bytes()
        .map(|b| format!("{}MB", b / (1024 * 1024)))
        .unwrap_or_else(|| "?".into());
    let pump_avg = us.checked_div(n.max(1)).unwrap_or(0);
    let probe_avg = pus.checked_div(pn.max(1)).unwrap_or(0);

    emit(&format!(
        "ui-prof[v1 snapshot {tag}] up={up_s}s ws={ws} \
         pump={n} avg={pump_avg}µs max={max}µs spikes(>8ms={s8} >30ms={s30}) posts={posts} coalesce={coal} \
         probe n={pn} avg={probe_avg}µs max={pmax}µs spikes(>50ms={p50} >200ms={p200}) timeout={pto}"
    ));
}

// The recorders below are called only from `present_pump` (Windows-only), so on
// other platforms they are dead — kept compiled everywhere so the bodies stay
// under lint/type coverage on the Linux CI runner.

/// Record one present-pump handler invocation (UI thread).
#[allow(dead_code)]
pub fn pump_handled(elapsed: Duration, hwnd_count: u32) {
    if !is_enabled() {
        return;
    }
    ensure_flusher();
    let us = elapsed.as_micros() as u64;
    PUMP_N.fetch_add(1, Ordering::Relaxed);
    PUMP_US.fetch_add(us, Ordering::Relaxed);
    PUMP_MAX_US.fetch_max(us, Ordering::Relaxed);
    PUMP_HWNDS.fetch_add(hwnd_count as u64, Ordering::Relaxed);
    if us > 8_000 {
        PUMP_SPIKE_8MS.fetch_add(1, Ordering::Relaxed);
    }
    if us > 30_000 {
        PUMP_SPIKE_30MS.fetch_add(1, Ordering::Relaxed);
    }
}

#[allow(dead_code)]
pub fn pump_posted() {
    if !is_enabled() {
        return;
    }
    ensure_flusher();
    PUMP_POSTS.fetch_add(1, Ordering::Relaxed);
}

#[allow(dead_code)]
pub fn pump_coalesced() {
    if !is_enabled() {
        return;
    }
    ensure_flusher();
    PUMP_COALESCE.fetch_add(1, Ordering::Relaxed);
}

/// Wndproc side of the UI latency probe (µs of queue delay).
#[allow(dead_code)]
pub fn probe_completed(rtt_us: u64) {
    if !is_enabled() {
        return;
    }
    ensure_flusher();
    // Clear in-flight mark so the poster does not count a false timeout.
    PROBE_SENT_TICK.store(0, Ordering::Relaxed);
    PROBE_N.fetch_add(1, Ordering::Relaxed);
    PROBE_US.fetch_add(rtt_us, Ordering::Relaxed);
    PROBE_MAX_US.fetch_max(rtt_us, Ordering::Relaxed);
    if rtt_us > 50_000 {
        PROBE_SPIKE_50MS.fetch_add(1, Ordering::Relaxed);
    }
    if rtt_us > 200_000 {
        PROBE_SPIKE_200MS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Call from the probe poster when a prior probe never completed.
#[allow(dead_code)]
pub fn probe_timeout() {
    if !is_enabled() {
        return;
    }
    ensure_flusher();
    PROBE_TIMEOUT.fetch_add(1, Ordering::Relaxed);
}

/// Force an immediate snapshot (future hotkey / debugger). Best-effort.
#[allow(dead_code)]
pub fn force_snapshot(reason: &str) {
    if !is_enabled() {
        return;
    }
    ensure_flusher();
    dump_snapshot(reason);
}

/// Start the periodic flusher early (main), even before first pump tick.
pub fn init() {
    if is_enabled() {
        ensure_flusher();
    }
}

// --- Windows probe helpers (tick count) ------------------------------------

/// Store the tick used when posting a probe (for timeout detection).
#[allow(dead_code)]
pub fn probe_mark_sent(tick_ms: u64) {
    PROBE_SENT_TICK.store(tick_ms, Ordering::Relaxed);
}

#[allow(dead_code)]
pub fn probe_last_sent() -> u64 {
    PROBE_SENT_TICK.load(Ordering::Relaxed)
}
