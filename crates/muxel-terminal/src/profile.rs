//! Opt-in main-thread timing for key → PTY → paint under load.
//!
//! **Off by default.** Enable with `MUXEL_PROFILE_TERMINAL=1` or
//! `MUXEL_PROFILE=1` (`true` / `yes` also work). Stats dump to a log file
//! every ~500 ms while **interesting** events are flowing (keypresses, paint
//! spikes, high felt latency), and a final line after 1 s of quiet.
//!
//! ## Overhead
//! When disabled: one cached boolean branch per call site. When enabled:
//! timestamps and relaxed counters on the PTY threads, a bounded nonblocking
//! handoff for startup/slow records, a small per-pane state lock on the UI thread, and
//! background log formatting every 500 ms (append + rotate). The worker is
//! initialized during session setup; PTY and GPUI paint paths never perform
//! log I/O. The channel and worker each retain at most 128 records; saturation
//! is reported as `deferred_dropped=` in the next interval line.
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
//! Lines are `term-prof[v8 …]` and include PTY writer queue/write timing, PTY
//! reader/drain timing, and paint phase splits
//! (`build=` / `shape=` / `submit=` / `runs=` / `reuse=`) plus felt-latency
//! samples split at `key→next-read`, `read→process`, and `process→paint`.
//! Slow chains emit per-pane `term-lat[v8]` with the channel-drain and UI-update
//! stages; slow PTY writes emit `term-write[v1]`. Together those boundaries
//! distinguish child/ConPTY delay from Muxel queueing and paint delay.

use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, TryRecvError, sync_channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use uuid::Uuid;

use crate::session::PtyReadTiming;

static ENABLED: OnceLock<bool> = OnceLock::new();
static LOG_STDERR: OnceLock<bool> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_FILE: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();

/// Rotate when the profile log reaches this size (keep one `.1` backup).
const PROFILE_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Latest cursor position of the focused pane, pushed by the drain after each
/// processed batch. Terminal contents never enter profiler state or logs.
static LAST_CURSOR: Mutex<Option<(Uuid, usize, i32)>> = Mutex::new(None);

/// Whether profiling is on — callers gate probe collection on this.
pub fn is_enabled() -> bool {
    enabled()
}

/// Initialize all enabled-profiler state and the bounded log worker away from
/// PTY reader/writer and GPUI paint paths.
pub(crate) fn start() {
    if !enabled() {
        return;
    }
    let _ = counters();
    let _ = now_us();
    let _ = deferred_sender();
}

/// Record the focused pane's cursor position (drain thread, after process_output).
pub fn cursor_probe_update(instance_id: Uuid, col: usize, row: i32) {
    if !enabled() {
        return;
    }
    if let Ok(mut g) = LAST_CURSOR.lock() {
        *g = Some((instance_id, col, row));
    }
}

/// Record an actual GPUI terminal focus edge with pane identity.
pub fn focus_changed(instance_id: Uuid, focused: bool) {
    if !enabled() {
        return;
    }
    if !focused {
        clear_pane_state(instance_id);
    }
    emit_line(&format!(
        "term-focus[v1] pane={instance_id} focused={focused}"
    ));
}

pub(crate) fn pane_closed(instance_id: Uuid) {
    if enabled() {
        clear_pane_state(instance_id);
    }
}

fn clear_pane_state(instance_id: Uuid) {
    if let Ok(mut panes) = pane_latency().lock() {
        panes.remove(&instance_id);
    }
    if let Ok(mut cursor) = LAST_CURSOR.lock()
        && cursor.is_some_and(|(pane, _, _)| pane == instance_id)
    {
        *cursor = None;
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

/// One-shot terminal startup milestone. These lines share the profiler's file,
/// rotation, timestamp, and opt-in gate so a workspace trace can correlate UI
/// activation with ConPTY creation and the agent's first visible frame.
pub fn startup_event(
    instance_id: Uuid,
    _program: &str,
    phase: &str,
    elapsed: Duration,
    bytes: usize,
) {
    if !enabled() {
        return;
    }
    defer(DeferredRecord::Startup(StartupRecord {
        instance_id,
        phase: startup_phase_label(phase),
        elapsed_us: elapsed.as_micros() as u64,
        bytes,
    }));
    touch();
}

fn startup_line(
    instance_id: Uuid,
    _program: &str,
    phase: &str,
    elapsed: Duration,
    bytes: usize,
) -> String {
    format!(
        "term-start pane={instance_id} phase={phase} elapsed={}ms bytes={bytes}",
        elapsed.as_millis()
    )
}

fn startup_phase_label(phase: &str) -> &'static str {
    match phase {
        "automation-armed" => "automation-armed",
        "automation-first-output" => "automation-first-output",
        "automation-settled" => "automation-settled",
        "automation-pasted" => "automation-pasted",
        "automation-submitted" => "automation-submitted",
        "first-output" => "first-output",
        "first-screen" => "first-screen",
        "openpty" => "openpty",
        "program-resolution" => "program-resolution",
        "child-spawn" => "child-spawn",
        "pty-setup" => "pty-setup",
        "pty-spawn" => "pty-spawn",
        _ => "other",
    }
}

#[derive(Clone, Copy, Debug)]
struct SlowWrite {
    instance_id: Uuid,
    bytes: usize,
    queue_us: u64,
    write_us: u64,
    at_us: u64,
}

#[derive(Clone, Copy, Debug)]
struct SlowLatency {
    instance_id: Uuid,
    sample: LatencySample,
    paint_us: u64,
}

#[derive(Clone, Copy, Debug)]
struct StartupRecord {
    instance_id: Uuid,
    phase: &'static str,
    elapsed_us: u64,
    bytes: usize,
}

#[derive(Clone, Copy, Debug)]
enum DeferredRecord {
    Startup(StartupRecord),
    SlowWrite(SlowWrite),
    SlowLatency(SlowLatency),
}

const MAX_PENDING_RECORDS: usize = 128;

struct Counters {
    keys: AtomicU64,
    keys_held: AtomicU64,
    key_us: AtomicU64,
    writes: AtomicU64,
    write_bytes: AtomicU64,
    write_queue_us: AtomicU64,
    write_queue_max_us: AtomicU64,
    write_us: AtomicU64,
    write_max_us: AtomicU64,
    reads: AtomicU64,
    read_bytes: AtomicU64,
    read_drain_us: AtomicU64,
    read_drain_max_us: AtomicU64,
    read_drain_n: AtomicU64,
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
    key_read_lat_us: AtomicU64,
    key_read_lat_max: AtomicU64,
    key_read_lat_n: AtomicU64,
    read_process_lat_us: AtomicU64,
    read_process_lat_max: AtomicU64,
    read_process_lat_n: AtomicU64,
    process_paint_lat_us: AtomicU64,
    process_paint_lat_max: AtomicU64,
    process_paint_lat_n: AtomicU64,
    /// Synchronized-update (DECSET 2026) windows force-expired at deadline.
    sync_expired: AtomicU64,
    deferred_dropped: AtomicU64,
    last_event_us: AtomicU64,
    interval_start_us: AtomicU64,
}

static C: OnceLock<Counters> = OnceLock::new();

fn counters() -> &'static Counters {
    C.get_or_init(|| Counters {
        keys: AtomicU64::new(0),
        keys_held: AtomicU64::new(0),
        key_us: AtomicU64::new(0),
        writes: AtomicU64::new(0),
        write_bytes: AtomicU64::new(0),
        write_queue_us: AtomicU64::new(0),
        write_queue_max_us: AtomicU64::new(0),
        write_us: AtomicU64::new(0),
        write_max_us: AtomicU64::new(0),
        reads: AtomicU64::new(0),
        read_bytes: AtomicU64::new(0),
        read_drain_us: AtomicU64::new(0),
        read_drain_max_us: AtomicU64::new(0),
        read_drain_n: AtomicU64::new(0),
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
        key_read_lat_us: AtomicU64::new(0),
        key_read_lat_max: AtomicU64::new(0),
        key_read_lat_n: AtomicU64::new(0),
        read_process_lat_us: AtomicU64::new(0),
        read_process_lat_max: AtomicU64::new(0),
        read_process_lat_n: AtomicU64::new(0),
        process_paint_lat_us: AtomicU64::new(0),
        process_paint_lat_max: AtomicU64::new(0),
        process_paint_lat_n: AtomicU64::new(0),
        sync_expired: AtomicU64::new(0),
        deferred_dropped: AtomicU64::new(0),
        last_event_us: AtomicU64::new(0),
        interval_start_us: AtomicU64::new(0),
    })
}

static DEFERRED: OnceLock<SyncSender<DeferredRecord>> = OnceLock::new();

fn deferred_sender() -> &'static SyncSender<DeferredRecord> {
    DEFERRED.get_or_init(|| {
        let (tx, rx) = sync_channel(MAX_PENDING_RECORDS);
        let _ = std::thread::Builder::new()
            .name("muxel-term-prof".into())
            .spawn(move || flusher_loop(rx));
        tx
    })
}

fn defer(record: DeferredRecord) {
    if !try_defer_to(DEFERRED.get(), record) {
        counters().deferred_dropped.fetch_add(1, Ordering::Relaxed);
    }
}

fn try_defer_to(tx: Option<&SyncSender<DeferredRecord>>, record: DeferredRecord) -> bool {
    tx.is_some_and(|tx| tx.try_send(record).is_ok())
}

/// Process-lifetime epoch for lock-free latency timestamps (µs since first use).
static EPOCH: OnceLock<Instant> = OnceLock::new();

/// Convert a monotonic instant to µs since the profiler epoch; never 0 (so 0
/// can mean "no sample pending").
fn instant_us(at: Instant) -> u64 {
    let epoch = EPOCH.get_or_init(|| at);
    at.saturating_duration_since(*epoch).as_micros().max(1) as u64
}

fn now_us() -> u64 {
    instant_us(Instant::now())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaintRequest {
    Now,
    Timer,
}

impl PaintRequest {
    fn label(self) -> &'static str {
        match self {
            Self::Now => "now",
            Self::Timer => "timer",
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct PaneLatency {
    pending_key_at: u64,
    key_at: u64,
    read_sequence: u64,
    read_at: u64,
    drain_at: u64,
    update_requested_at: u64,
    process_at: u64,
    notify_at: u64,
    request: Option<PaintRequest>,
    min_interval_us: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct LatencySample {
    key_read_us: u64,
    read_drain_us: Option<u64>,
    drain_request_us: Option<u64>,
    request_process_us: Option<u64>,
    read_process_us: u64,
    process_notify_us: Option<u64>,
    notify_paint_us: Option<u64>,
    process_paint_us: u64,
    request: Option<PaintRequest>,
    min_interval_us: u64,
}

impl PaneLatency {
    fn key(&mut self, now: u64) {
        // One chain at a time. A key received after this chain's read cannot be
        // correlated with a later read without skipping output that followed it.
        if self.read_at != 0 {
            return;
        }
        if self.pending_key_at == 0 || now.saturating_sub(self.pending_key_at) >= LATENCY_STALE_US {
            self.pending_key_at = now;
        }
    }

    fn read(&mut self, sequence: u64, now: u64) {
        if self.read_at != 0 || self.pending_key_at == 0 {
            return;
        }
        // A batch already waiting in the async channel can drain after a key.
        // Its reader timestamp still precedes the key and must not claim it.
        if now < self.pending_key_at {
            return;
        }
        let key_at = std::mem::take(&mut self.pending_key_at);
        if now.saturating_sub(key_at) >= LATENCY_STALE_US {
            return;
        }
        self.key_at = key_at;
        self.read_sequence = sequence;
        self.read_at = now;
    }

    fn drained(&mut self, timing: PtyReadTiming, now: u64) {
        if self.read_at != 0
            && self.drain_at == 0
            && timing.sequence == self.read_sequence
            && timing.read_at_us == self.read_at
        {
            self.drain_at = now;
        }
    }

    fn update_requested(&mut self, now: u64) {
        if self.drain_at != 0 && self.update_requested_at == 0 {
            self.update_requested_at = now;
        }
    }

    fn processed(&mut self, now: u64) {
        if self.drain_at != 0 && self.process_at == 0 {
            self.process_at = now;
        }
    }

    fn notified(&mut self, now: u64, request: PaintRequest, min_interval: Duration) {
        if self.process_at != 0 && self.notify_at == 0 {
            self.notify_at = now;
            self.request = Some(request);
            self.min_interval_us = min_interval.as_micros() as u64;
        }
    }

    fn painted(&mut self, now: u64) -> Option<LatencySample> {
        if self.process_at == 0 {
            return None;
        }
        let process_paint_us = now.saturating_sub(self.process_at);
        let sample = (process_paint_us < LATENCY_STALE_US).then(|| LatencySample {
            key_read_us: self.read_at.saturating_sub(self.key_at),
            read_drain_us: (self.drain_at != 0).then(|| self.drain_at.saturating_sub(self.read_at)),
            drain_request_us: (self.drain_at != 0 && self.update_requested_at != 0)
                .then(|| self.update_requested_at.saturating_sub(self.drain_at)),
            request_process_us: (self.update_requested_at != 0)
                .then(|| self.process_at.saturating_sub(self.update_requested_at)),
            read_process_us: self.process_at.saturating_sub(self.read_at),
            process_notify_us: (self.notify_at != 0)
                .then(|| self.notify_at.saturating_sub(self.process_at)),
            notify_paint_us: (self.notify_at != 0).then(|| now.saturating_sub(self.notify_at)),
            process_paint_us,
            request: self.request,
            min_interval_us: self.min_interval_us,
        });
        self.key_at = 0;
        self.read_sequence = 0;
        self.read_at = 0;
        self.drain_at = 0;
        self.update_requested_at = 0;
        self.process_at = 0;
        self.notify_at = 0;
        self.request = None;
        self.min_interval_us = 0;
        sample
    }
}

static PANE_LATENCY: OnceLock<std::sync::Mutex<HashMap<Uuid, PaneLatency>>> = OnceLock::new();

fn pane_latency() -> &'static std::sync::Mutex<HashMap<Uuid, PaneLatency>> {
    PANE_LATENCY.get_or_init(|| std::sync::Mutex::new(HashMap::new()))
}

/// Update an existing key chain without creating state for background output.
/// Remove a chain once it returns to its empty state.
fn update_latency_pane<R>(
    panes: &mut HashMap<Uuid, PaneLatency>,
    instance_id: Uuid,
    update: impl FnOnce(&mut PaneLatency) -> R,
) -> Option<R> {
    let result = panes.get_mut(&instance_id).map(update);
    if panes
        .get(&instance_id)
        .is_some_and(|pane| *pane == PaneLatency::default())
    {
        panes.remove(&instance_id);
    }
    result
}

/// Samples older than this are dropped as stale — the key had no echo (arrow
/// keys in some TUIs), or the echo was for something else entirely.
const LATENCY_STALE_US: u64 = 500_000;

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
    let now = now_us();
    let previous = c.last_event_us.swap(now, Ordering::Relaxed);
    if previous == 0 {
        c.interval_start_us.store(now, Ordering::Relaxed);
    } else {
        let _ = c
            .interval_start_us
            .compare_exchange(0, now, Ordering::Relaxed, Ordering::Relaxed);
    }
}

fn flusher_loop(rx: Receiver<DeferredRecord>) {
    let mut records = Vec::with_capacity(MAX_PENDING_RECORDS);
    let mut last_dump = Instant::now();
    loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(record) if records.len() < MAX_PENDING_RECORDS => records.push(record),
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => return,
        }
        while records.len() < MAX_PENDING_RECORDS {
            match rx.try_recv() {
                Ok(record) => records.push(record),
                Err(TryRecvError::Empty) => break,
                Err(TryRecvError::Disconnected) => return,
            }
        }

        let c = counters();
        let last = c.last_event_us.load(Ordering::Relaxed);
        if last == 0 {
            continue;
        }
        let quiet = now_us().saturating_sub(last) >= 1_000_000;
        let periodic = last_dump.elapsed() >= Duration::from_millis(500);
        if quiet || periodic {
            dump(if quiet { "quiet" } else { "tick" }, &mut records);
            last_dump = Instant::now();
            if quiet {
                let _ =
                    c.last_event_us
                        .compare_exchange(last, 0, Ordering::Relaxed, Ordering::Relaxed);
            }
        }
    }
}

fn dump(tag: &str, records: &mut Vec<DeferredRecord>) {
    let c = counters();
    let keys = c.keys.swap(0, Ordering::Relaxed);
    let keys_held = c.keys_held.swap(0, Ordering::Relaxed);
    let key_us = c.key_us.swap(0, Ordering::Relaxed);
    let writes = c.writes.swap(0, Ordering::Relaxed);
    let write_bytes = c.write_bytes.swap(0, Ordering::Relaxed);
    let write_queue_us = c.write_queue_us.swap(0, Ordering::Relaxed);
    let write_queue_max = c.write_queue_max_us.swap(0, Ordering::Relaxed);
    let write_us = c.write_us.swap(0, Ordering::Relaxed);
    let write_max = c.write_max_us.swap(0, Ordering::Relaxed);
    let reads = c.reads.swap(0, Ordering::Relaxed);
    let read_bytes = c.read_bytes.swap(0, Ordering::Relaxed);
    let read_drain_us = c.read_drain_us.swap(0, Ordering::Relaxed);
    let read_drain_max = c.read_drain_max_us.swap(0, Ordering::Relaxed);
    let read_drain_n = c.read_drain_n.swap(0, Ordering::Relaxed);
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
    let key_read_us = c.key_read_lat_us.swap(0, Ordering::Relaxed);
    let key_read_max = c.key_read_lat_max.swap(0, Ordering::Relaxed);
    let key_read_n = c.key_read_lat_n.swap(0, Ordering::Relaxed);
    let read_process_us = c.read_process_lat_us.swap(0, Ordering::Relaxed);
    let read_process_max = c.read_process_lat_max.swap(0, Ordering::Relaxed);
    let read_process_n = c.read_process_lat_n.swap(0, Ordering::Relaxed);
    let process_paint_us = c.process_paint_lat_us.swap(0, Ordering::Relaxed);
    let process_paint_max = c.process_paint_lat_max.swap(0, Ordering::Relaxed);
    let process_paint_n = c.process_paint_lat_n.swap(0, Ordering::Relaxed);
    let sync_exp = c.sync_expired.swap(0, Ordering::Relaxed);
    let deferred_dropped = c.deferred_dropped.swap(0, Ordering::Relaxed);
    if keys == 0
        && writes == 0
        && reads == 0
        && batches == 0
        && paints == 0
        && notify == 0
        && sync_exp == 0
        && deferred_dropped == 0
        && records.is_empty()
    {
        return;
    }

    // Roll the interval whenever its counters are consumed, including ticks
    // filtered out below. Otherwise a later interesting tick reports rates over
    // a window that contains counters already discarded by an earlier tick.
    let dump_at = now_us();
    let interval_start = c.interval_start_us.swap(dump_at, Ordering::Relaxed);
    let win_ms = if interval_start == 0 {
        500
    } else {
        (dump_at.saturating_sub(interval_start) as u128 / 1000).max(1)
    };

    // Always-on corpus filter: skip pure background paint/notify ticks. Those
    // flood the log under multi-agent load and do not explain typing lag.
    // Keep intervals with keypresses, paint spikes, or high felt latency.
    let interesting = keys > 0
        || spikes_3 > 0
        || spikes_8 > 0
        || key_read_max >= 80_000 // ≥80ms before the PTY reader receives output
        || read_process_max >= 30_000 // ≥30ms in Muxel's output channel/drain
        || process_paint_max >= 30_000 // ≥30ms from parse start through paint
        || read_drain_max >= 30_000 // ≥30ms channel delay on any PTY batch
        || write_queue_max >= 20_000 // ≥20ms waiting behind an earlier PTY write
        || write_max >= 20_000 // ≥20ms blocked in the ConPTY write/flush
        || !records.is_empty()
        || deferred_dropped > 0
        || paint_max >= 8_000 // ≥8ms single paint
        || tag == "quiet"; // end-of-burst summary still useful after typing
    if !interesting {
        // Counters already swapped to zero above; drop the interval.
        return;
    }

    let key_avg = key_us.checked_div(keys).unwrap_or(0);
    let write_queue_avg = write_queue_us.checked_div(writes).unwrap_or(0);
    let write_avg = write_us.checked_div(writes).unwrap_or(0);
    let read_drain_avg = read_drain_us.checked_div(read_drain_n).unwrap_or(0);
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

    let key_read_avg = key_read_us.checked_div(key_read_n).unwrap_or(0);
    let read_process_avg = read_process_us.checked_div(read_process_n).unwrap_or(0);
    let process_paint_avg = process_paint_us.checked_div(process_paint_n).unwrap_or(0);

    // v8: v7 + PTY reader, channel-drain, and UI-update boundaries.
    let line = format!(
        "term-prof[v8 {tag}] Δ={win_ms}ms keys={keys} (held={keys_held}, ~{key_hz}/s, avg={key_avg}µs) \
         writer={writes}/{write_bytes}B queue_avg={write_queue_avg}µs queue_max={write_queue_max}µs \
         write_avg={write_avg}µs write_max={write_max}µs \
         reader={reads}/{read_bytes}B drain_avg={read_drain_avg}µs drain_max={read_drain_max}µs \
         notify={notify} (~{notify_hz}/s) \
         process={batches} batches/{bytes}B avg={proc_avg}µs max={process_max}µs \
         paint={paints} (focus={paints_f} bg={paints_bg} full={paint_full} replay={paint_replay}, ~{paint_hz}/s) \
         avg={paint_avg}µs max={paint_max}µs sum={paint_total_ms}ms (~{paint_pct}% of interval) \
         spikes(>3ms={spikes_3} >8ms={spikes_8}) \
         full-phases: build_avg={build_avg}µs shape_avg={shape_avg}µs submit_avg={submit_avg}µs \
         runs={runs_total} reuse={runs_reused} ({reuse_pct}%) \
         lat: key→next-read avg={key_read_avg}µs max={key_read_max}µs (n={key_read_n}) \
         read→process avg={read_process_avg}µs max={read_process_max}µs (n={read_process_n}) \
         process→paint avg={process_paint_avg}µs max={process_paint_max}µs (n={process_paint_n}) \
         sync_exp={sync_exp} deferred_dropped={deferred_dropped}"
    );
    let cursor = LAST_CURSOR.lock().ok().and_then(|g| *g);
    let line = match cursor {
        Some((pane, col, row)) => format!("{line} cur_pane={pane} cur={col},{row}"),
        None => line,
    };
    emit_line(&line);
    for record in records.drain(..) {
        match record {
            DeferredRecord::Startup(startup) => emit_line(&startup_line(
                startup.instance_id,
                "",
                startup.phase,
                Duration::from_micros(startup.elapsed_us),
                startup.bytes,
            )),
            DeferredRecord::SlowWrite(write) => emit_line(&format!(
                "term-write[v1] pane={} bytes={} queue={}ms write={}ms age={}ms",
                write.instance_id,
                write.bytes,
                write.queue_us / 1000,
                write.write_us / 1000,
                dump_at.saturating_sub(write.at_us) / 1000,
            )),
            DeferredRecord::SlowLatency(latency) => {
                let stage = |value: Option<u64>| {
                    value.map_or_else(|| "missing".to_string(), |us| format!("{}ms", us / 1000))
                };
                let sample = latency.sample;
                let read_drain = stage(sample.read_drain_us);
                let drain_request = stage(sample.drain_request_us);
                let request_process = stage(sample.request_process_us);
                let process_notify = stage(sample.process_notify_us);
                let notify_paint = stage(sample.notify_paint_us);
                let request = sample.request.map_or("missing", PaintRequest::label);
                emit_line(&format!(
                    "term-lat[v8] pane={} key_next_read={}ms read_drain={read_drain} drain_request={drain_request} request_process={request_process} process_notify={process_notify} notify_paint={notify_paint} process_paint={}ms paint={}ms request={request} min={}ms focused=true",
                    latency.instance_id,
                    sample.key_read_us / 1000,
                    sample.process_paint_us / 1000,
                    latency.paint_us / 1000,
                    sample.min_interval_us / 1000,
                ));
            }
        }
    }
}

/// Arm a latency chain before input is queued to the PTY writer. `started_at`
/// is the start of the GPUI key callback, so key→read includes handler work.
pub fn key_started(instance_id: Uuid, started_at: Instant) {
    if !enabled() {
        return;
    }
    if let Ok(mut panes) = pane_latency().lock() {
        panes
            .entry(instance_id)
            .or_default()
            .key(instant_us(started_at));
    }
}

/// Finish timing a key path after its bytes have been queued.
pub fn key_finished(held: bool, elapsed: Duration) {
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

/// Record one packet leaving the asynchronous PTY writer. Queue delay proves
/// whether earlier writes blocked this packet; write time measures the ConPTY
/// write/flush itself. Slow per-pane lines are deferred to the profiler thread;
/// the PTY writer itself never performs file I/O.
pub fn pty_write(instance_id: Uuid, bytes: usize, queue_delay: Duration, elapsed: Duration) {
    if !enabled() {
        return;
    }
    let c = counters();
    let queue_us = queue_delay.as_micros() as u64;
    let write_us = elapsed.as_micros() as u64;
    c.writes.fetch_add(1, Ordering::Relaxed);
    c.write_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    c.write_queue_us.fetch_add(queue_us, Ordering::Relaxed);
    c.write_queue_max_us.fetch_max(queue_us, Ordering::Relaxed);
    c.write_us.fetch_add(write_us, Ordering::Relaxed);
    c.write_max_us.fetch_max(write_us, Ordering::Relaxed);
    if queue_us >= 50_000 || write_us >= 50_000 {
        defer(DeferredRecord::SlowWrite(SlowWrite {
            instance_id,
            bytes,
            queue_us,
            write_us,
            at_us: now_us(),
        }));
    }
    touch();
}

/// Record the instant the blocking PTY reader receives a byte batch. This is
/// the first boundary after the child/ConPTY path and therefore the decisive
/// split for delayed terminal echo.
pub(crate) fn pty_read(bytes: usize, sequence: u64) -> PtyReadTiming {
    let read_at_us = now_us();
    let timing = PtyReadTiming {
        sequence,
        read_at_us,
    };
    let c = counters();
    c.reads.fetch_add(1, Ordering::Relaxed);
    c.read_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    timing
}

/// Record when one identified reader batch leaves the async channel. Every
/// batch contributes to the reader→drain aggregate; the sequence also attaches
/// the exact post-key batch to its per-pane latency chain.
pub(crate) fn output_drained(instance_id: Uuid, timing: PtyReadTiming) {
    if !enabled() {
        return;
    }
    let now = now_us();
    let elapsed = now.saturating_sub(timing.read_at_us);
    let c = counters();
    c.read_drain_us.fetch_add(elapsed, Ordering::Relaxed);
    c.read_drain_max_us.fetch_max(elapsed, Ordering::Relaxed);
    c.read_drain_n.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut panes) = pane_latency().lock() {
        update_latency_pane(&mut panes, instance_id, |pane| {
            pane.read(timing.sequence, timing.read_at_us);
            pane.drained(timing, now);
        });
    }
    touch();
}

/// The drain has finished its intentional coalescing and is about to request
/// the GPUI entity update that parses this output batch.
pub(crate) fn output_update_requested(instance_id: Uuid) {
    if !enabled() {
        return;
    }
    if let Ok(mut panes) = pane_latency().lock() {
        update_latency_pane(&mut panes, instance_id, |pane| {
            pane.update_requested(now_us())
        });
    }
    touch();
}

/// The GPUI update closure started. Any time since
/// [`output_update_requested`] is UI-executor queueing, not PTY or child delay.
pub(crate) fn output_update_started(instance_id: Uuid) {
    if !enabled() {
        return;
    }
    if let Ok(mut panes) = pane_latency().lock() {
        update_latency_pane(&mut panes, instance_id, |pane| pane.processed(now_us()));
    }
    touch();
}

/// Record that a terminal pointer listener ran and whether the child currently
/// owns mouse input. A missing line during a reported mouse outage distinguishes
/// lost GPUI listeners from a TUI that enabled mouse reporting.
pub fn pointer_routed(instance_id: Uuid, event: &str, mouse_reporting: bool, shift: bool) {
    if !enabled() {
        return;
    }
    emit_line(&format!(
        "term-pointer[v1] pane={instance_id} event={event} mouse_reporting={mouse_reporting} shift={shift}"
    ));
    touch();
}

pub fn notify_scheduled(instance_id: Uuid, request: PaintRequest, min_interval: Duration) {
    if !enabled() {
        return;
    }
    counters().notify.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut panes) = pane_latency().lock() {
        update_latency_pane(&mut panes, instance_id, |pane| {
            pane.notified(now_us(), request, min_interval)
        });
    }
    touch();
}

pub fn process_output(_instance_id: Uuid, bytes: usize, elapsed: Duration, _focused: bool) {
    if !enabled() {
        return;
    }
    let c = counters();
    c.process_batches.fetch_add(1, Ordering::Relaxed);
    c.process_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
    let us = elapsed.as_micros() as u64;
    c.process_us.fetch_add(us, Ordering::Relaxed);
    c.process_max_us.fetch_max(us, Ordering::Relaxed);
    touch();
}

pub fn paint_with_phases(
    instance_id: Uuid,
    elapsed: Duration,
    focused: bool,
    mode: PaintMode,
    phases: PaintPhases,
) {
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
        let sample = pane_latency().lock().ok().and_then(|mut panes| {
            update_latency_pane(&mut panes, instance_id, |pane| pane.painted(now_us())).flatten()
        });
        if let Some(sample) = sample {
            c.key_read_lat_us
                .fetch_add(sample.key_read_us, Ordering::Relaxed);
            c.key_read_lat_max
                .fetch_max(sample.key_read_us, Ordering::Relaxed);
            c.key_read_lat_n.fetch_add(1, Ordering::Relaxed);
            c.read_process_lat_us
                .fetch_add(sample.read_process_us, Ordering::Relaxed);
            c.read_process_lat_max
                .fetch_max(sample.read_process_us, Ordering::Relaxed);
            c.read_process_lat_n.fetch_add(1, Ordering::Relaxed);
            c.process_paint_lat_us
                .fetch_add(sample.process_paint_us, Ordering::Relaxed);
            c.process_paint_lat_max
                .fetch_max(sample.process_paint_us, Ordering::Relaxed);
            c.process_paint_lat_n.fetch_add(1, Ordering::Relaxed);
            if sample.key_read_us >= 50_000
                || sample.read_process_us >= 50_000
                || sample.process_paint_us >= 50_000
                || sample.read_drain_us.is_some_and(|us| us >= 30_000)
                || sample.drain_request_us.is_some_and(|us| us >= 30_000)
                || sample.request_process_us.is_some_and(|us| us >= 30_000)
                || sample.notify_paint_us.is_some_and(|us| us >= 30_000)
            {
                defer(DeferredRecord::SlowLatency(SlowLatency {
                    instance_id,
                    sample,
                    paint_us: elapsed.as_micros() as u64,
                }));
            }
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pane_latency_keeps_each_chain_self_contained() {
        let mut first = PaneLatency::default();
        let mut second = PaneLatency::default();
        first.key(10);
        second.key(20);
        first.read(1, 14);
        first.drained(
            PtyReadTiming {
                sequence: 1,
                read_at_us: 14,
            },
            15,
        );
        first.update_requested(16);
        first.processed(18);
        first.notified(20, PaintRequest::Now, Duration::from_millis(8));

        assert_eq!(second.painted(30), None);
        assert_eq!(
            first.painted(40),
            Some(LatencySample {
                key_read_us: 4,
                read_drain_us: Some(1),
                drain_request_us: Some(1),
                request_process_us: Some(2),
                read_process_us: 4,
                process_notify_us: Some(2),
                notify_paint_us: Some(20),
                process_paint_us: 22,
                request: Some(PaintRequest::Now),
                min_interval_us: 8_000,
            })
        );
        second.read(2, 50);
        second.drained(
            PtyReadTiming {
                sequence: 2,
                read_at_us: 50,
            },
            52,
        );
        second.update_requested(54);
        second.processed(56);
        assert_eq!(second.painted(60).unwrap().key_read_us, 30);
    }

    #[test]
    fn repeated_keys_keep_earliest_unanswered_key() {
        let mut pane = PaneLatency::default();
        pane.key(10);
        pane.key(12);
        pane.read(1, 20);
        pane.drained(
            PtyReadTiming {
                sequence: 1,
                read_at_us: 20,
            },
            21,
        );
        pane.update_requested(22);
        pane.processed(23);
        assert_eq!(pane.painted(25).unwrap().key_read_us, 10);
    }

    #[test]
    fn key_during_an_active_chain_is_not_deferred_to_later_output() {
        let mut pane = PaneLatency::default();
        pane.key(10);
        pane.read(1, 20);
        pane.key(22);
        pane.drained(
            PtyReadTiming {
                sequence: 1,
                read_at_us: 20,
            },
            23,
        );
        pane.update_requested(24);
        pane.processed(25);
        assert!(pane.painted(30).is_some());

        pane.read(2, 40);
        pane.drained(
            PtyReadTiming {
                sequence: 2,
                read_at_us: 40,
            },
            41,
        );
        pane.update_requested(42);
        pane.processed(43);
        assert_eq!(pane.painted(44), None);
    }

    #[test]
    fn profiler_state_is_created_only_by_a_key_and_removed_when_idle() {
        let pane_id = Uuid::new_v4();
        let timing = PtyReadTiming {
            sequence: 1,
            read_at_us: 10,
        };
        let mut panes = HashMap::new();
        update_latency_pane(&mut panes, pane_id, |pane| pane.read(1, 10));
        update_latency_pane(&mut panes, pane_id, |pane| pane.drained(timing, 11));
        update_latency_pane(&mut panes, pane_id, |pane| pane.update_requested(12));
        update_latency_pane(&mut panes, pane_id, |pane| pane.processed(13));
        assert!(panes.is_empty());

        panes.entry(pane_id).or_default().key(5);
        update_latency_pane(&mut panes, pane_id, |pane| pane.read(1, 10));
        update_latency_pane(&mut panes, pane_id, |pane| pane.drained(timing, 11));
        update_latency_pane(&mut panes, pane_id, |pane| pane.update_requested(12));
        update_latency_pane(&mut panes, pane_id, |pane| pane.processed(13));
        let sample = update_latency_pane(&mut panes, pane_id, |pane| pane.painted(14)).flatten();
        assert!(sample.is_some());
        assert!(panes.is_empty());
    }

    #[test]
    fn pane_close_removes_a_pending_no_output_key_and_cursor() {
        let instance_id = Uuid::new_v4();
        pane_latency().lock().unwrap().insert(
            instance_id,
            PaneLatency {
                pending_key_at: 7,
                ..PaneLatency::default()
            },
        );
        *LAST_CURSOR.lock().unwrap() = Some((instance_id, 4, 2));

        clear_pane_state(instance_id);

        assert!(!pane_latency().lock().unwrap().contains_key(&instance_id));
        assert_ne!(
            *LAST_CURSOR.lock().unwrap(),
            Some((instance_id, 4, 2)),
            "a destroyed pane must not retain cursor attribution"
        );
    }

    #[test]
    fn startup_log_omits_program_and_terminal_content() {
        let secret = "agent-SECRET_ARG-typed-terminal-row";
        let line = startup_line(
            Uuid::nil(),
            secret,
            "child-spawn",
            Duration::from_millis(3),
            secret.len(),
        );
        assert!(!line.contains(secret));
        assert_eq!(
            line,
            format!(
                "term-start pane={} phase=child-spawn elapsed=3ms bytes={}",
                Uuid::nil(),
                secret.len()
            )
        );
    }

    #[test]
    fn deferred_record_saturation_is_observable_to_the_producer() {
        let (tx, _rx) = sync_channel(1);
        let record = DeferredRecord::Startup(StartupRecord {
            instance_id: Uuid::nil(),
            phase: "first-output",
            elapsed_us: 1,
            bytes: 1,
        });
        assert!(try_defer_to(Some(&tx), record));
        assert!(!try_defer_to(Some(&tx), record));
        assert!(!try_defer_to(None, record));
    }

    #[test]
    fn missing_notify_and_stale_samples_are_explicit() {
        let mut pane = PaneLatency::default();
        pane.key(10);
        pane.read(1, 20);
        pane.drained(
            PtyReadTiming {
                sequence: 1,
                read_at_us: 20,
            },
            21,
        );
        pane.update_requested(22);
        pane.processed(23);
        let sample = pane.painted(30).unwrap();
        assert_eq!(sample.process_notify_us, None);
        assert_eq!(sample.notify_paint_us, None);

        pane.key(1);
        pane.read(2, LATENCY_STALE_US + 1);
        assert_eq!(pane.painted(LATENCY_STALE_US + 2), None);
    }

    #[test]
    fn queued_output_before_the_key_cannot_claim_the_drain_stage() {
        let mut pane = PaneLatency::default();
        pane.key(10);
        pane.read(1, 5);
        pane.drained(
            PtyReadTiming {
                sequence: 1,
                read_at_us: 5,
            },
            21,
        );
        pane.update_requested(22);
        pane.processed(23);
        assert_eq!(pane.painted(24), None);

        pane.read(2, 20);
        pane.drained(
            PtyReadTiming {
                sequence: 2,
                read_at_us: 20,
            },
            30,
        );
        pane.update_requested(31);
        pane.processed(32);
        assert_eq!(pane.painted(40).unwrap().read_drain_us, Some(10));
    }
}
