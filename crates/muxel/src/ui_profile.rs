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

use std::cell::RefCell;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, SyncSender, sync_channel};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};
use uuid::Uuid;

static ENABLED: OnceLock<bool> = OnceLock::new();
static LOG_PATH: OnceLock<PathBuf> = OnceLock::new();
static LOG_FILE: OnceLock<Mutex<Option<std::fs::File>>> = OnceLock::new();
static STARTED: OnceLock<Instant> = OnceLock::new();
static FLUSHER: AtomicBool = AtomicBool::new(false);
static DEFERRED: OnceLock<SyncSender<DeferredRecord>> = OnceLock::new();
static DEFERRED_DROPPED: AtomicU64 = AtomicU64::new(0);
static NEXT_SPAN_ID: AtomicU64 = AtomicU64::new(0);
static NEXT_RENDER_TOKEN: AtomicU64 = AtomicU64::new(0);
static RENDER_TOKEN: AtomicU64 = AtomicU64::new(0);
static RENDER_STARTED_US: AtomicU64 = AtomicU64::new(0);
static RENDER_VIEW: AtomicU64 = AtomicU64::new(0);
static RENDER_STAGE: AtomicU64 = AtomicU64::new(0);

const LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
const MAX_PENDING_RECORDS: usize = 4096;
const SLOW_RENDER_US: u64 = 50_000;

enum DeferredRecord {
    Line {
        at: SystemTime,
        line: String,
    },
    StaticLine {
        at: SystemTime,
        line: &'static str,
    },
    Phase {
        at: SystemTime,
        boundary: PhaseBoundary,
        id: u64,
        category: &'static str,
        phase: &'static str,
        pane: Option<Uuid>,
        elapsed_us: Option<u64>,
    },
    RenderBlocked {
        at: SystemTime,
        token: u64,
        view: RenderView,
        stage: RenderStage,
        active_us: u64,
    },
    SlowRender {
        at: SystemTime,
        token: u64,
        view: RenderView,
        stage: RenderStage,
        elapsed_us: u64,
    },
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RenderView {
    Main = 1,
    Workspace = 2,
    Popout = 3,
    DevConsole = 4,
    FileDiff = 5,
}

impl RenderView {
    fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::Main),
            2 => Some(Self::Workspace),
            3 => Some(Self::Popout),
            4 => Some(Self::DevConsole),
            5 => Some(Self::FileDiff),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Workspace => "workspace",
            Self::Popout => "popout",
            Self::DevConsole => "dev-console",
            Self::FileDiff => "file-diff",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum RenderStage {
    Build = 1,
    Frame = 2,
    RequestLayout = 3,
    RootLayoutOrCompute = 4,
    Prepaint = 5,
    Paint = 6,
    RootPaintOrPresent = 7,
}

impl RenderStage {
    fn from_code(code: u64) -> Option<Self> {
        match code {
            1 => Some(Self::Build),
            2 => Some(Self::Frame),
            3 => Some(Self::RequestLayout),
            4 => Some(Self::RootLayoutOrCompute),
            5 => Some(Self::Prepaint),
            6 => Some(Self::Paint),
            7 => Some(Self::RootPaintOrPresent),
            _ => None,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Build => "build",
            Self::Frame => "frame",
            Self::RequestLayout => "request-layout",
            Self::RootLayoutOrCompute => "root-layout-or-compute",
            Self::Prepaint => "prepaint",
            Self::Paint => "paint",
            Self::RootPaintOrPresent => "root-paint-or-present",
        }
    }
}

#[derive(Clone, Copy)]
enum PhaseBoundary {
    Begin,
    End,
}

impl PhaseBoundary {
    fn label(self) -> &'static str {
        match self {
            Self::Begin => "begin",
            Self::End => "end",
        }
    }
}

/// A content-free phase boundary around synchronous work that may occupy the UI
/// thread. Records are timestamped at the call site and written by a bounded
/// background worker, so profiling cannot add file I/O or lock waits to the
/// measured path.
pub struct PhaseSpan {
    id: u64,
    category: &'static str,
    phase: &'static str,
    pane: Option<Uuid>,
    started: Instant,
}

/// High-frequency render watch. Normal frames only touch atomics and emit
/// nothing; slow frames and probe timeouts enqueue one typed record.
pub struct RenderWatch {
    token: u64,
    view: RenderView,
    stage: RenderStage,
    started: Instant,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct RenderSnapshot {
    token: u64,
    started_us: u64,
    view: RenderView,
    stage: RenderStage,
}

thread_local! {
    /// Render watches are created and dropped on GPUI's UI thread. A vector is
    /// required instead of saved previous values because re-entrant Windows
    /// draws can defer a nested arena clear and therefore destroy watches out
    /// of LIFO order.
    static RENDER_STACK: RefCell<Vec<RenderSnapshot>> = const { RefCell::new(Vec::new()) };
}

struct ProfiledElement {
    inner: AnyElement,
    view: RenderView,
    frame: Option<RenderWatch>,
}

impl Drop for PhaseSpan {
    fn drop(&mut self) {
        let elapsed_us = self.started.elapsed().as_micros() as u64;
        defer_record(DeferredRecord::Phase {
            at: SystemTime::now(),
            boundary: PhaseBoundary::End,
            id: self.id,
            category: self.category,
            phase: self.phase,
            pane: self.pane,
            elapsed_us: Some(elapsed_us),
        });
    }
}

impl Drop for RenderWatch {
    fn drop(&mut self) {
        let elapsed_us = self.started.elapsed().as_micros() as u64;
        RENDER_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            remove_render_snapshot(&mut stack, self.token);
            publish_render_snapshot(stack.last().copied());
        });
        if elapsed_us >= SLOW_RENDER_US {
            defer_record(DeferredRecord::SlowRender {
                at: SystemTime::now(),
                token: self.token,
                view: self.view,
                stage: self.stage,
                elapsed_us,
            });
        }
    }
}

impl RenderWatch {
    fn mark_stage(&self, stage: RenderStage) {
        RENDER_STACK.with(|stack| {
            let mut stack = stack.borrow_mut();
            if mark_render_stage(&mut stack, self.token, stage) {
                publish_render_snapshot(stack.last().copied());
            }
        });
    }
}

fn remove_render_snapshot(stack: &mut Vec<RenderSnapshot>, token: u64) -> bool {
    let Some(index) = stack.iter().rposition(|snapshot| snapshot.token == token) else {
        return false;
    };
    stack.remove(index);
    true
}

/// Update one live watch. Returns true only when it is the published top.
fn mark_render_stage(stack: &mut [RenderSnapshot], token: u64, stage: RenderStage) -> bool {
    let Some(index) = stack.iter().rposition(|snapshot| snapshot.token == token) else {
        return false;
    };
    stack[index].stage = stage;
    index + 1 == stack.len()
}

fn publish_render_snapshot(snapshot: Option<RenderSnapshot>) {
    // Invalidate the previous snapshot first. Readers that caught its token
    // will reject the record on their second token read.
    RENDER_TOKEN.store(0, Ordering::Release);
    if let Some(snapshot) = snapshot {
        RENDER_STARTED_US.store(snapshot.started_us, Ordering::Relaxed);
        RENDER_VIEW.store(snapshot.view as u64, Ordering::Relaxed);
        RENDER_STAGE.store(snapshot.stage as u64, Ordering::Relaxed);
        RENDER_TOKEN.store(snapshot.token, Ordering::Release);
    }
}

impl IntoElement for ProfiledElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ProfiledElement {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let layout = {
            let _watch = watch_render(self.view, RenderStage::RequestLayout);
            self.inner.request_layout(window, cx)
        };
        if let Some(frame) = &self.frame {
            // gpui-component's thin Root still finishes its own request-layout
            // callback before GPUI computes the window layout. This stage names
            // that combined, otherwise hookless interval without overstating it.
            frame.mark_stage(RenderStage::RootLayoutOrCompute);
        }
        (layout, ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) {
        {
            let _watch = watch_render(self.view, RenderStage::Prepaint);
            self.inner.prepaint(window, cx);
        }
        if let Some(frame) = &self.frame {
            frame.mark_stage(RenderStage::Frame);
        }
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        {
            let _watch = watch_render(self.view, RenderStage::Paint);
            self.inner.paint(window, cx);
        }
        if let Some(frame) = &self.frame {
            // gpui-component's Root may still paint after this child returns;
            // GPUI then finalizes the scene and presents before clearing the
            // arena. Keep the sentinel through that combined hookless interval.
            frame.mark_stage(RenderStage::RootPaintOrPresent);
        }
    }
}

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
/// Probes that could not be posted at all (not evidence of a busy UI thread).
static PROBE_POST_FAILURE: AtomicU64 = AtomicU64::new(0);
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
    if !prepare_log_path(path) {
        return None;
    }
    OpenOptions::new().create(true).append(true).open(path).ok()
}

fn reopen_log(slot: &mut Option<std::fs::File>, path: &Path) {
    *slot = None;
    *slot = open_log(path);
}

fn rotated_log_path(path: &Path) -> PathBuf {
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    PathBuf::from(rotated)
}

fn prepare_log_path(path: &Path) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return true;
    };
    if meta.len() < LOG_MAX_BYTES {
        return true;
    }
    let rotated = rotated_log_path(path);
    match std::fs::remove_file(&rotated) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => {}
    }
    if std::fs::rename(path, &rotated).is_ok() {
        return true;
    }
    OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(path)
        .is_ok()
}

fn emit_at(at: SystemTime, line: &str) {
    if !is_enabled() {
        return;
    }
    let now = at.duration_since(UNIX_EPOCH).unwrap_or_default();
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
            reopen_log(&mut g, path);
        }
        if let Some(f) = g.as_mut() {
            let _ = writeln!(f, "{line}");
            let _ = f.flush();
        }
    }
}

fn emit(line: &str) {
    emit_at(SystemTime::now(), line);
}

fn deferred_sender() -> &'static SyncSender<DeferredRecord> {
    DEFERRED.get_or_init(|| {
        let (tx, rx) = sync_channel(MAX_PENDING_RECORDS);
        let _ = std::thread::Builder::new()
            .name("muxel-ui-prof-records".into())
            .spawn(move || deferred_writer(rx));
        tx
    })
}

fn deferred_writer(rx: Receiver<DeferredRecord>) {
    while let Ok(record) = rx.recv() {
        match record {
            DeferredRecord::Line { at, line } => emit_at(at, &line),
            DeferredRecord::StaticLine { at, line } => emit_at(at, line),
            DeferredRecord::Phase {
                at,
                boundary,
                id,
                category,
                phase,
                pane,
                elapsed_us,
            } => emit_at(
                at,
                &phase_line(boundary.label(), id, category, phase, pane, elapsed_us),
            ),
            DeferredRecord::RenderBlocked {
                at,
                token,
                view,
                stage,
                active_us,
            } => emit_at(
                at,
                &render_line("blocked", token, view, stage, "active", active_us),
            ),
            DeferredRecord::SlowRender {
                at,
                token,
                view,
                stage,
                elapsed_us,
            } => emit_at(
                at,
                &render_line("slow", token, view, stage, "elapsed", elapsed_us),
            ),
        }
    }
}

fn try_defer_to(tx: Option<&SyncSender<DeferredRecord>>, record: DeferredRecord) -> bool {
    tx.is_some_and(|tx| tx.try_send(record).is_ok())
}

fn defer_record(record: DeferredRecord) {
    if !try_defer_to(DEFERRED.get(), record) {
        DEFERRED_DROPPED.fetch_add(1, Ordering::Relaxed);
    }
}

fn phase_line(
    boundary: &str,
    id: u64,
    category: &str,
    phase: &str,
    pane: Option<Uuid>,
    elapsed_us: Option<u64>,
) -> String {
    let pane = pane.map_or_else(|| "none".to_string(), |pane| pane.to_string());
    let elapsed = elapsed_us.map_or_else(String::new, |us| format!(" elapsed={us}µs"));
    format!(
        "ui-prof[phase {boundary}] span={id} category={category} phase={phase} pane={pane}{elapsed}"
    )
}

fn render_line(
    event: &str,
    token: u64,
    view: RenderView,
    stage: RenderStage,
    duration_label: &str,
    duration_us: u64,
) -> String {
    format!(
        "ui-prof[render {event}] token={token} view={} stage={} {duration_label}={duration_us}µs",
        view.label(),
        stage.label(),
    )
}

fn start_line(pid: u32) -> String {
    format!("ui-prof[start] pid={pid}")
}

fn ensure_flusher() {
    if !is_enabled() {
        return;
    }
    // Every caller that can enqueue a record first observes an initialized
    // sender. This closes the tiny race where a second thread saw `FLUSHER=true`
    // before the winning thread had published `DEFERRED`.
    let _ = deferred_sender();
    if FLUSHER
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    STARTED.get_or_init(Instant::now);
    defer_record(DeferredRecord::Line {
        at: SystemTime::now(),
        line: start_line(std::process::id()),
    });
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

/// Begin one synchronous profiler phase. The returned guard writes its end
/// boundary on drop. `category` and `phase` must be fixed labels; `pane` is the
/// only correlation value, keeping command lines, paths, and session ids out of
/// the log.
pub fn phase(category: &'static str, phase: &'static str, pane: Option<Uuid>) -> Option<PhaseSpan> {
    if !is_enabled() {
        return None;
    }
    ensure_flusher();
    let id = NEXT_SPAN_ID.fetch_add(1, Ordering::Relaxed).wrapping_add(1);
    let started = Instant::now();
    defer_record(DeferredRecord::Phase {
        at: SystemTime::now(),
        boundary: PhaseBoundary::Begin,
        id,
        category,
        phase,
        pane,
        elapsed_us: None,
    });
    Some(PhaseSpan {
        id,
        category,
        phase,
        pane,
        started,
    })
}

fn profiler_elapsed_us() -> u64 {
    STARTED.get().map_or(0, |started| {
        started.elapsed().as_micros().min(u64::MAX as u128) as u64
    })
}

fn watch_render(view: RenderView, stage: RenderStage) -> Option<RenderWatch> {
    if !is_enabled() {
        return None;
    }
    ensure_flusher();
    let token = NEXT_RENDER_TOKEN
        .fetch_add(1, Ordering::Relaxed)
        .wrapping_add(1);
    let started = Instant::now();
    let snapshot = RenderSnapshot {
        token,
        started_us: profiler_elapsed_us(),
        view,
        stage,
    };
    RENDER_STACK.with(|stack| {
        let mut stack = stack.borrow_mut();
        stack.push(snapshot);
        publish_render_snapshot(stack.last().copied());
    });
    Some(RenderWatch {
        token,
        view,
        stage,
        started,
    })
}

/// Mark root-view element construction as the current UI-thread render stage.
/// Normal frames touch atomics only and produce no log record.
pub fn watch_render_build(view: RenderView) -> Option<RenderWatch> {
    watch_render(view, RenderStage::Build)
}

/// Wrap the application element beneath gpui-component's thin window Root so
/// request-layout, the remaining root/layout interval, prepaint, paint, and the
/// remaining root/present interval stay observable after `Render::render`.
pub fn finish_render(
    view: RenderView,
    build: Option<RenderWatch>,
    element: impl IntoElement,
) -> AnyElement {
    let inner = element.into_any_element();
    drop(build);
    if !is_enabled() {
        inner
    } else {
        ProfiledElement {
            inner,
            view,
            frame: watch_render(view, RenderStage::Frame),
        }
        .into_any_element()
    }
}

fn working_set_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        use windows::Win32::System::ProcessStatus::{
            GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
        };
        use windows::Win32::System::Threading::GetCurrentProcess;
        unsafe {
            let mut pmc = PROCESS_MEMORY_COUNTERS {
                cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
                ..Default::default()
            };
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
    let post_fail = PROBE_POST_FAILURE.swap(0, Ordering::Relaxed);
    let deferred_dropped = DEFERRED_DROPPED.swap(0, Ordering::Relaxed);

    if n == 0
        && pn == 0
        && coal == 0
        && posts == 0
        && pto == 0
        && post_fail == 0
        && deferred_dropped == 0
    {
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
        || post_fail > 0
        || pmax >= 50_000
        || max >= 8_000
        || coal > posts.saturating_add(n).max(1)
        || deferred_dropped > 0; // heavy coalesce / saturated record worker

    if !interesting && tag == "tick" {
        return;
    }

    emit(&format!(
        "ui-prof[v1 {tag}] pump={n} avg={pump_avg}µs max={max}µs spikes(>8ms={s8} >30ms={s30}) \
         hwnds/tick≈{hwnd_avg} posts={posts} coalesce={coal} \
         probe n={pn} avg={probe_avg}µs max={pmax}µs spikes(>50ms={p50} >200ms={p200}) timeout={pto} post_fail={post_fail} \
         deferred_dropped={deferred_dropped}"
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
    let post_fail = PROBE_POST_FAILURE.swap(0, Ordering::Relaxed);
    let deferred_dropped = DEFERRED_DROPPED.swap(0, Ordering::Relaxed);
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
         probe n={pn} avg={probe_avg}µs max={pmax}µs spikes(>50ms={p50} >200ms={p200}) timeout={pto} post_fail={post_fail} \
         deferred_dropped={deferred_dropped}"
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

/// Mark entry to and exit from Windows' modal move/size loop.
#[allow(dead_code)]
pub fn move_size_changed(active: bool) {
    if !is_enabled() {
        return;
    }
    ensure_flusher();
    defer_record(DeferredRecord::StaticLine {
        at: SystemTime::now(),
        line: if active {
            "ui-prof[move-size enter]"
        } else {
            "ui-prof[move-size exit]"
        },
    });
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
    let token = RENDER_TOKEN.load(Ordering::Acquire);
    if token == 0 {
        defer_record(DeferredRecord::StaticLine {
            at: SystemTime::now(),
            line: "ui-prof[probe timeout] no-render-active",
        });
        return;
    }
    let started_us = RENDER_STARTED_US.load(Ordering::Relaxed);
    let view = RenderView::from_code(RENDER_VIEW.load(Ordering::Relaxed));
    let stage = RenderStage::from_code(RENDER_STAGE.load(Ordering::Relaxed));
    std::sync::atomic::fence(Ordering::Acquire);
    // Re-read the token after the associated fields. A changed or cleared token
    // means the render stage ended while the probe thread sampled it.
    if token != RENDER_TOKEN.load(Ordering::Acquire) {
        return;
    }
    if let (Some(view), Some(stage)) = (view, stage) {
        defer_record(DeferredRecord::RenderBlocked {
            at: SystemTime::now(),
            token,
            view,
            stage,
            active_us: profiler_elapsed_us().saturating_sub(started_us),
        });
    }
}

/// Record failure to enqueue the probe itself. This is transport evidence, not
/// proof that the UI thread failed to answer, so it must not sample render state.
#[allow(dead_code)]
pub fn probe_post_failed() {
    if !is_enabled() {
        return;
    }
    ensure_flusher();
    PROBE_SENT_TICK.store(0, Ordering::Relaxed);
    PROBE_POST_FAILURE.fetch_add(1, Ordering::Relaxed);
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deferred_record_queue_is_bounded_and_nonblocking() {
        let (tx, _rx) = sync_channel(1);
        assert!(try_defer_to(
            Some(&tx),
            DeferredRecord::Line {
                at: UNIX_EPOCH,
                line: "first".into(),
            }
        ));
        assert!(!try_defer_to(
            Some(&tx),
            DeferredRecord::Line {
                at: UNIX_EPOCH,
                line: "second".into(),
            }
        ));
    }

    #[test]
    fn ui_rotation_drops_the_live_handle_and_replaces_an_existing_backup() {
        let dir = std::env::temp_dir().join(format!("muxel-ui-prof-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).expect("create rotation fixture");
        let path = dir.join("ui-prof.log");
        let rotated = rotated_log_path(&path);
        std::fs::write(&path, vec![b'x'; LOG_MAX_BYTES as usize + 1])
            .expect("write oversized live log");
        std::fs::write(&rotated, b"stale backup").expect("write stale backup");
        let mut slot = Some(
            OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("hold live log open"),
        );

        reopen_log(&mut slot, &path);

        assert_eq!(
            std::fs::metadata(&rotated).expect("rotated log").len(),
            LOG_MAX_BYTES + 1
        );
        assert_eq!(std::fs::metadata(&path).expect("new live log").len(), 0);
        writeln!(slot.as_mut().expect("reopened live log"), "bounded").unwrap();
        drop(slot);
        assert!(std::fs::metadata(&path).unwrap().len() < LOG_MAX_BYTES);
        std::fs::remove_dir_all(dir).expect("remove rotation fixture");
    }

    #[test]
    fn phase_records_only_fixed_labels_and_pane_identity() {
        let pane = Uuid::nil();
        assert_eq!(
            phase_line("end", 7, "activation", "resume-scan", Some(pane), Some(42)),
            "ui-prof[phase end] span=7 category=activation phase=resume-scan pane=00000000-0000-0000-0000-000000000000 elapsed=42µs"
        );
    }

    #[test]
    fn start_record_does_not_disclose_the_log_path() {
        let line = start_line(42);
        assert_eq!(line, "ui-prof[start] pid=42");
        assert!(!line.contains("path="));
    }

    #[test]
    fn render_records_use_bounded_view_and_stage_labels() {
        assert_eq!(
            render_line(
                "blocked",
                9,
                RenderView::Workspace,
                RenderStage::RootLayoutOrCompute,
                "active",
                15_281_000,
            ),
            "ui-prof[render blocked] token=9 view=workspace stage=root-layout-or-compute active=15281000µs"
        );
        assert_eq!(RenderView::from_code(99), None);
        assert_eq!(RenderStage::from_code(99), None);
    }

    #[test]
    fn reentrant_render_drops_never_resurrect_a_dead_watch() {
        let snapshot = |token, stage| RenderSnapshot {
            token,
            started_us: token * 10,
            view: RenderView::Main,
            stage,
        };
        let mut stack = vec![
            snapshot(1, RenderStage::Frame),
            snapshot(2, RenderStage::Paint),
            snapshot(3, RenderStage::Frame),
        ];

        // A nested draw survives while an outer stage guard is destroyed.
        assert!(remove_render_snapshot(&mut stack, 2));
        assert_eq!(stack.last().map(|watch| watch.token), Some(3));
        // The hidden outer frame can progress without replacing the nested top.
        assert!(!mark_render_stage(
            &mut stack,
            1,
            RenderStage::RootPaintOrPresent,
        ));
        assert_eq!(stack.last().map(|watch| watch.token), Some(3));
        // Once the nested draw dies, only the still-live outer frame resurfaces.
        assert!(remove_render_snapshot(&mut stack, 3));
        assert_eq!(
            stack.last(),
            Some(&snapshot(1, RenderStage::RootPaintOrPresent))
        );
        assert!(remove_render_snapshot(&mut stack, 1));
        assert!(stack.is_empty());
        assert!(!remove_render_snapshot(&mut stack, 2));
    }
}
