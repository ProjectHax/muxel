//! Per-frame attribution for window draws (opt-in with the rest of `ui_profile`).
//!
//! Each draw of a view wrapped by `watch_render_build`/`finish_render` opens
//! one frame on the UI thread. The frame closes when GPUI clears its element
//! arena, after presenting. The root wrapper marks the phase boundaries, and
//! regions wrapped with `ui_profile::region` add their *self* time (their
//! work minus nested wrapped regions) per phase, so region and `other` values
//! in one phase sum to that phase. A frame of at least 50 ms emits one
//! `ui-frame[v1 slow]` record. Every frame feeds a ten-second
//! `ui-frame[v1 sum]` summary, which also reports the UI thread's CPU time
//! inside and outside frames.
//!
//! Intervals, in GPUI's order:
//! - `pre`: `Window::draw` start to the root view's build (entity
//!   invalidation, gpui-component's Root render). Needs GPUI's frame trace.
//! - `build`: the wrapped view's `Render::render`.
//! - `layout`: request-layout of the app element tree. RenderOnce components
//!   (buttons, inputs) and uncached child views render here.
//! - `compute`: the rest of Root's request-layout plus taffy's layout compute.
//! - `prepaint`: includes the render, layout and prepaint of dirty cached
//!   views (background terminals).
//! - `paint`
//! - `tail`: paint end to `Window::draw` end (scene finish, focus and
//!   accessed-entity bookkeeping). Without GPUI's frame trace this interval
//!   runs to arena clear and is labelled `post`.
//! - `present`: `Window::draw` end to arena clear: the platform present plus
//!   dropping the elements allocated before the root wrapper.
//!
//! `cpu` counts the UI thread's own cycles over the frame (Windows
//! `QueryThreadCycleTime`), so `wall - cpu` is time the thread did not run:
//! blocked on the GPU, a lock or a hard page fault, or runnable but not
//! scheduled. `faults` is the process-wide page-fault count over the frame,
//! soft faults included. Neither is available off Windows.
//!
//! Records carry fixed labels, counts and durations only.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use gpui::{
    AnyElement, App, Bounds, Element, ElementId, GlobalElementId, InspectorElementId, IntoElement,
    LayoutId, Pixels, Window,
};

use super::RenderView;

/// Tab groups attributed individually; later groups fold into the last slot.
pub(crate) const MAX_GROUPS: usize = 16;
const FIXED: usize = 10;
pub(super) const REGIONS: usize = FIXED + 3 * MAX_GROUPS;
pub(super) const PHASES: usize = 4;
const INTERVALS: usize = 8;
const SUMMARY_EVERY: Duration = Duration::from_secs(10);
/// Frames whose arena clear never arrives (a panicking draw) are evicted.
const MAX_OPEN_FRAMES: usize = 4;
const HIST_BOUNDS_MS: [u64; 7] = [8, 16, 33, 50, 100, 250, 1000];

const FIXED_LABELS: [&str; FIXED] = [
    "prep", "actions", "titlebar", "sidebar", "toolbar", "hotbar", "panes", "overlay", "shell",
    "panes_sb",
];
const INTERVAL_LABELS: [&str; INTERVALS] = [
    "pre", "build", "layout", "compute", "prepaint", "paint", "tail", "present",
];
const IV_PRE: usize = 0;
const IV_BUILD: usize = 1;
const IV_LAYOUT: usize = 2;
const IV_COMPUTE: usize = 3;
const IV_PREPAINT: usize = 4;
const IV_PAINT: usize = 5;
const IV_TAIL: usize = 6;
const IV_PRESENT: usize = 7;

/// A part of a window whose frame cost is attributed separately.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Region {
    /// Main render's preamble: browser visibility sync and re-dock drains.
    Prep,
    /// Root key-context and workspace action registration.
    Actions,
    Titlebar,
    Sidebar,
    Toolbar,
    Hotbar,
    /// The pane tree's containers (scroller, split groups) outside tab groups.
    Panes,
    /// Modals, palettes, menus and bars drawn over the window.
    Overlay,
    /// The window body's containers outside wrapped regions: the sidebar and
    /// side-panel resizable groups and the main column.
    Shell,
    /// The pane tree's horizontal scrollbar.
    PaneScrollbar,
    /// One tab group's strip (pills, `+`, worktree badge, pane controls).
    Strip(usize),
    /// One tab group's card outside its strip and content.
    Body(usize),
    /// One tab group's shown tab: terminal, editor, browser or placeholder.
    Content(usize),
}

impl Region {
    fn index(self) -> usize {
        let group = |g: usize| g.min(MAX_GROUPS - 1);
        match self {
            Self::Prep => 0,
            Self::Actions => 1,
            Self::Titlebar => 2,
            Self::Sidebar => 3,
            Self::Toolbar => 4,
            Self::Hotbar => 5,
            Self::Panes => 6,
            Self::Overlay => 7,
            Self::Shell => 8,
            Self::PaneScrollbar => 9,
            Self::Strip(g) => FIXED + group(g),
            Self::Body(g) => FIXED + MAX_GROUPS + group(g),
            Self::Content(g) => FIXED + 2 * MAX_GROUPS + group(g),
        }
    }
}

fn region_label(index: usize) -> String {
    if index < FIXED {
        FIXED_LABELS[index].to_string()
    } else {
        let group = (index - FIXED) % MAX_GROUPS;
        let kind = match (index - FIXED) / MAX_GROUPS {
            0 => "strip",
            1 => "body",
            _ => "content",
        };
        format!("{kind}{group}")
    }
}

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Build = 0,
    Layout = 1,
    Prepaint = 2,
    Paint = 3,
}

#[derive(Clone, Copy)]
pub(super) enum Mark {
    BuildEnd = 0,
    LayoutStart = 1,
    LayoutEnd = 2,
    PrepaintStart = 3,
    PrepaintEnd = 4,
    PaintStart = 5,
    PaintEnd = 6,
}

/// One closed frame, handed to the record worker when slow.
#[derive(Clone)]
pub(super) struct FrameData {
    view: RenderView,
    token: u64,
    wall_us: u64,
    intervals_us: [u64; INTERVALS],
    traced: bool,
    queue_us: Option<u64>,
    invalidations: u64,
    cycles: Option<u64>,
    faults: Option<u64>,
    region_ns: [[u64; PHASES]; REGIONS],
    group_tabs: [u16; MAX_GROUPS],
    groups: usize,
}

impl FrameData {
    fn new(view: RenderView, token: u64) -> Self {
        Self {
            view,
            token,
            wall_us: 0,
            intervals_us: [0; INTERVALS],
            traced: false,
            queue_us: None,
            invalidations: 0,
            cycles: None,
            faults: None,
            region_ns: [[0; PHASES]; REGIONS],
            group_tabs: [0; MAX_GROUPS],
            groups: 0,
        }
    }
}

struct OpenScope {
    region: usize,
    phase: usize,
    started: Instant,
    child_ns: u64,
}

struct FrameAcc {
    data: Box<FrameData>,
    started: Instant,
    marks: [Option<Instant>; 7],
    window: Option<u64>,
    cycles_start: Option<u64>,
    faults_start: Option<u32>,
    scopes: Vec<OpenScope>,
}

/// Ten seconds of frames on the UI thread, every window included.
#[derive(Clone)]
pub(super) struct Summary {
    span_us: u64,
    ui_cpu_us: Option<u64>,
    ui_cycles: Option<u64>,
    frames: u32,
    slow: u32,
    per_view: [u32; 6],
    wall_sum_us: u64,
    wall_max_us: u64,
    hist: [u32; HIST_BOUNDS_MS.len() + 1],
    intervals_sum_us: [u64; INTERVALS],
    traced: u32,
    queue_sum_us: u64,
    queue_max_us: u64,
    queue_n: u32,
    invalidations: u64,
    frame_cycles: u64,
    faults: u64,
    region_ns: [[u64; PHASES]; REGIONS],
    group_tabs: [u16; MAX_GROUPS],
    groups: usize,
}

impl Summary {
    fn new() -> Self {
        Self {
            span_us: 0,
            ui_cpu_us: None,
            ui_cycles: None,
            frames: 0,
            slow: 0,
            per_view: [0; 6],
            wall_sum_us: 0,
            wall_max_us: 0,
            hist: [0; HIST_BOUNDS_MS.len() + 1],
            intervals_sum_us: [0; INTERVALS],
            traced: 0,
            queue_sum_us: 0,
            queue_max_us: 0,
            queue_n: 0,
            invalidations: 0,
            frame_cycles: 0,
            faults: 0,
            region_ns: [[0; PHASES]; REGIONS],
            group_tabs: [0; MAX_GROUPS],
            groups: 0,
        }
    }

    fn add(&mut self, frame: &FrameData, slow: bool) {
        self.frames += 1;
        self.slow += u32::from(slow);
        self.per_view[(frame.view as usize).min(5)] += 1;
        self.wall_sum_us += frame.wall_us;
        self.wall_max_us = self.wall_max_us.max(frame.wall_us);
        let bucket = HIST_BOUNDS_MS
            .iter()
            .position(|&ms| frame.wall_us < ms * 1000)
            .unwrap_or(HIST_BOUNDS_MS.len());
        self.hist[bucket] += 1;
        for (sum, us) in self.intervals_sum_us.iter_mut().zip(frame.intervals_us) {
            *sum += us;
        }
        self.traced += u32::from(frame.traced);
        if let Some(queue) = frame.queue_us {
            self.queue_sum_us += queue;
            self.queue_max_us = self.queue_max_us.max(queue);
            self.queue_n += 1;
        }
        self.invalidations += frame.invalidations;
        self.frame_cycles += frame.cycles.unwrap_or(0);
        self.faults += frame.faults.unwrap_or(0);
        for (sum, frame) in self.region_ns.iter_mut().zip(frame.region_ns.iter()) {
            for (sum, ns) in sum.iter_mut().zip(frame) {
                *sum += ns;
            }
        }
        if frame.view == RenderView::Main && frame.groups > 0 {
            self.group_tabs = frame.group_tabs;
            self.groups = frame.groups;
        }
    }
}

struct Interval {
    started: Instant,
    cpu_100ns: Option<u64>,
    cycles: Option<u64>,
    summary: Box<Summary>,
}

impl Interval {
    fn start() -> Self {
        Self {
            started: Instant::now(),
            cpu_100ns: thread_cpu_100ns(),
            cycles: thread_cycles(),
            summary: Box::new(Summary::new()),
        }
    }
}

thread_local! {
    static FRAMES: RefCell<Vec<FrameAcc>> = const { RefCell::new(Vec::new()) };
    static INTERVAL: RefCell<Option<Interval>> = const { RefCell::new(None) };
    static TRACE: RefCell<Option<gpui::profiler::FrameTimingCollector>> = const { RefCell::new(None) };
}

static TRACE_ENABLED: AtomicBool = AtomicBool::new(false);

/// GPUI's own record of one `Window::draw`.
struct DrawTiming {
    window: u64,
    draw_start: Instant,
    draw_end: Instant,
    dirty_at: Option<Instant>,
    invalidations: u64,
}

/// Turn on GPUI's frame trace and start collecting from this frame on. The
/// GPUI dependency of this module is this function and [`take_draw_timings`].
fn start_gpui_trace() {
    if !TRACE_ENABLED.swap(true, Ordering::Relaxed) {
        gpui::profiler::set_frame_trace_enabled(true);
    }
    TRACE.with(|trace| {
        trace
            .borrow_mut()
            .get_or_insert_with(gpui::profiler::FrameTimingCollector::new);
    });
}

/// GPUI draws recorded since the last call, oldest first.
fn take_draw_timings() -> Vec<DrawTiming> {
    TRACE.with(|trace| {
        trace
            .borrow_mut()
            .as_mut()
            .map_or_else(Vec::new, |collector| {
                collector
                    .collect_unseen()
                    .into_iter()
                    .map(|timing| DrawTiming {
                        window: timing.window_id.as_u64(),
                        draw_start: timing.draw_start,
                        draw_end: timing.draw_end,
                        dirty_at: timing.dirty_at,
                        invalidations: timing.invalidations,
                    })
                    .collect()
            })
    })
}

/// Open a frame for one root-view render. Called on the UI thread when
/// profiling is enabled.
pub(super) fn begin(view: RenderView, token: u64) {
    start_gpui_trace();
    let acc = FrameAcc {
        data: Box::new(FrameData::new(view, token)),
        cycles_start: thread_cycles(),
        faults_start: process_faults(),
        started: Instant::now(),
        marks: [None; 7],
        window: None,
        scopes: Vec::with_capacity(16),
    };
    FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        if frames.len() >= MAX_OPEN_FRAMES {
            frames.remove(0);
        }
        frames.push(acc);
    });
}

pub(super) fn mark(token: u64, mark: Mark) {
    let now = Instant::now();
    with_frame(token, |frame| frame.marks[mark as usize] = Some(now));
}

pub(super) fn set_window(token: u64, window: u64) {
    with_frame(token, |frame| frame.window = Some(window));
}

fn with_frame(token: u64, f: impl FnOnce(&mut FrameAcc)) {
    if token == 0 {
        return;
    }
    FRAMES.with(|frames| {
        if let Some(frame) = frames
            .borrow_mut()
            .iter_mut()
            .rev()
            .find(|frame| frame.data.token == token)
        {
            f(frame);
        }
    });
}

/// Claim the next tab-group slot in the frame being built, recording its tab
/// count. Leaves are numbered in render order.
pub(super) fn next_group(tabs: usize) -> usize {
    FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        let Some(frame) = frames.last_mut() else {
            return 0;
        };
        let group = frame.data.groups;
        frame.data.groups += 1;
        if group < MAX_GROUPS {
            frame.data.group_tabs[group] = tabs.min(u16::MAX as usize) as u16;
        }
        group
    })
}

/// A region's open interval in the current frame; closes on drop.
pub(crate) struct RegionScope {
    token: u64,
}

pub(super) fn open(region: Region, phase: Phase) -> Option<RegionScope> {
    FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        let frame = frames.last_mut()?;
        frame.scopes.push(OpenScope {
            region: region.index(),
            phase: phase as usize,
            started: Instant::now(),
            child_ns: 0,
        });
        Some(RegionScope {
            token: frame.data.token,
        })
    })
}

impl Drop for RegionScope {
    fn drop(&mut self) {
        let now = Instant::now();
        with_frame(self.token, |frame| {
            // Scopes are lexical (build guards and element callbacks), so the
            // last one open is this one.
            let Some(scope) = frame.scopes.pop() else {
                return;
            };
            let elapsed = now.saturating_duration_since(scope.started).as_nanos() as u64;
            frame.data.region_ns[scope.region][scope.phase] +=
                elapsed.saturating_sub(scope.child_ns);
            if let Some(parent) = frame.scopes.last_mut() {
                parent.child_ns += elapsed;
            }
        });
    }
}

fn micros(from: Option<Instant>, to: Option<Instant>) -> u64 {
    match (from, to) {
        (Some(from), Some(to)) => to.saturating_duration_since(from).as_micros() as u64,
        _ => 0,
    }
}

fn take_trace(window: Option<u64>) -> Option<DrawTiming> {
    let draws = take_draw_timings();
    let window = window?;
    draws
        .into_iter()
        .rev()
        .find(|timing| timing.window == window)
}

/// Close a frame at arena clear. Slow frames go to the record worker whole;
/// every frame feeds the interval summary.
pub(super) fn end(token: u64) {
    let cycles_end = thread_cycles();
    let faults_end = process_faults();
    let now = Instant::now();
    let Some(acc) = FRAMES.with(|frames| {
        let mut frames = frames.borrow_mut();
        let index = frames.iter().rposition(|frame| frame.data.token == token)?;
        Some(frames.remove(index))
    }) else {
        return;
    };
    let FrameAcc {
        mut data,
        started,
        marks,
        window,
        cycles_start,
        faults_start,
        ..
    } = acc;
    let at = |mark: Mark| marks[mark as usize];
    let mut iv = [0u64; INTERVALS];
    iv[IV_BUILD] = micros(Some(started), at(Mark::BuildEnd));
    iv[IV_LAYOUT] = micros(at(Mark::LayoutStart), at(Mark::LayoutEnd));
    iv[IV_COMPUTE] = micros(at(Mark::LayoutEnd), at(Mark::PrepaintStart));
    iv[IV_PREPAINT] = micros(at(Mark::PrepaintStart), at(Mark::PrepaintEnd));
    iv[IV_PAINT] = micros(at(Mark::PaintStart), at(Mark::PaintEnd));
    let mut wall_from = started;
    // GPUI records a draw's timing at the end of `Window::draw`; the one for
    // this window that brackets our build and paint belongs to this frame.
    let ours = |timing: &DrawTiming| {
        timing.draw_start <= started && at(Mark::PaintEnd).is_none_or(|end| timing.draw_end >= end)
    };
    match take_trace(window).filter(ours) {
        Some(timing) => {
            iv[IV_PRE] = micros(Some(timing.draw_start), Some(started));
            iv[IV_TAIL] = micros(at(Mark::PaintEnd), Some(timing.draw_end));
            iv[IV_PRESENT] = micros(Some(timing.draw_end), Some(now));
            wall_from = timing.draw_start;
            data.traced = true;
            data.queue_us = timing.dirty_at.map(|dirty| {
                timing
                    .draw_start
                    .saturating_duration_since(dirty)
                    .as_micros() as u64
            });
            data.invalidations = timing.invalidations;
        }
        None => iv[IV_TAIL] = micros(at(Mark::PaintEnd), Some(now)),
    }
    data.intervals_us = iv;
    data.wall_us = now.saturating_duration_since(wall_from).as_micros() as u64;
    data.cycles = cycles_start
        .zip(cycles_end)
        .map(|(start, end)| end.saturating_sub(start));
    data.faults = faults_start
        .zip(faults_end)
        .map(|(start, end)| u64::from(end.wrapping_sub(start)));

    let slow = data.wall_us >= super::SLOW_RENDER_US;
    INTERVAL.with(|interval| {
        let mut interval = interval.borrow_mut();
        let current = interval.get_or_insert_with(Interval::start);
        current.summary.add(&data, slow);
        if current.started.elapsed() >= SUMMARY_EVERY {
            let next = Interval::start();
            let mut summary = std::mem::replace(&mut current.summary, Box::new(Summary::new()));
            summary.span_us = next
                .started
                .saturating_duration_since(current.started)
                .as_micros() as u64;
            summary.ui_cpu_us = current
                .cpu_100ns
                .zip(next.cpu_100ns)
                .map(|(start, end)| end.saturating_sub(start) / 10);
            summary.ui_cycles = current
                .cycles
                .zip(next.cycles)
                .map(|(start, end)| end.saturating_sub(start));
            *current = next;
            super::defer_frame_summary(summary);
        }
    });
    if slow {
        super::defer_frame(data);
    }
}

/// Closes its frame when GPUI drops the root wrapper at arena clear.
pub(super) struct FrameClose(pub u64);

impl Drop for FrameClose {
    fn drop(&mut self) {
        end(self.0);
    }
}

/// Layout-transparent wrapper timing one region's element callbacks.
pub(super) struct ProfiledRegion {
    pub inner: AnyElement,
    pub region: Region,
}

impl IntoElement for ProfiledRegion {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ProfiledRegion {
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
        let _scope = open(self.region, Phase::Layout);
        (self.inner.request_layout(window, cx), ())
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
        let _scope = open(self.region, Phase::Prepaint);
        self.inner.prepaint(window, cx);
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
        let _scope = open(self.region, Phase::Paint);
        self.inner.paint(window, cx);
    }
}

// --- UI-thread counters ------------------------------------------------------

#[cfg(windows)]
fn thread_cycles() -> Option<u64> {
    use windows::Win32::System::Threading::GetCurrentThread;
    use windows::Win32::System::WindowsProgramming::QueryThreadCycleTime;
    let mut cycles = 0u64;
    unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) }.ok()?;
    Some(cycles)
}

#[cfg(not(windows))]
fn thread_cycles() -> Option<u64> {
    None
}

#[cfg(windows)]
fn thread_cpu_100ns() -> Option<u64> {
    use windows::Win32::Foundation::FILETIME;
    use windows::Win32::System::Threading::{GetCurrentThread, GetThreadTimes};
    let mut creation = FILETIME::default();
    let mut exit = FILETIME::default();
    let mut kernel = FILETIME::default();
    let mut user = FILETIME::default();
    unsafe {
        GetThreadTimes(
            GetCurrentThread(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    }
    .ok()?;
    let ticks =
        |time: FILETIME| (u64::from(time.dwHighDateTime) << 32) | u64::from(time.dwLowDateTime);
    Some(ticks(kernel) + ticks(user))
}

#[cfg(not(windows))]
fn thread_cpu_100ns() -> Option<u64> {
    None
}

#[cfg(windows)]
fn process_faults() -> Option<u32> {
    use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows::Win32::System::Threading::GetCurrentProcess;
    let mut counters = PROCESS_MEMORY_COUNTERS {
        cb: std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    unsafe {
        GetProcessMemoryInfo(
            GetCurrentProcess(),
            &mut counters,
            std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        )
    }
    .ok()?;
    Some(counters.PageFaultCount)
}

#[cfg(not(windows))]
fn process_faults() -> Option<u32> {
    None
}

/// Thread cycles per microsecond of a spinning thread (the invariant TSC
/// rate on current x86 hardware). The best of three 10 ms spins, so a
/// preempted spin cannot lower it. Runs once on the record worker.
pub(super) fn calibrate_cycles_per_us() -> Option<f64> {
    let mut best: Option<f64> = None;
    for _ in 0..3 {
        let start_cycles = thread_cycles()?;
        let started = Instant::now();
        while started.elapsed() < Duration::from_millis(10) {
            std::hint::spin_loop();
        }
        let cycles = thread_cycles()?.saturating_sub(start_cycles);
        let us = started.elapsed().as_secs_f64() * 1e6;
        if us > 0.0 {
            let rate = cycles as f64 / us;
            best = Some(best.map_or(rate, |best| best.max(rate)));
        }
    }
    best
}

// --- Record formatting (record worker) ---------------------------------------

fn cycles_us(cycles: u64, rate: Option<f64>) -> Option<u64> {
    rate.filter(|rate| *rate > 0.0)
        .map(|rate| (cycles as f64 / rate) as u64)
}

fn interval_fields(out: &mut String, intervals_us: &[u64; INTERVALS], traced: bool, divisor: u64) {
    use std::fmt::Write as _;
    for (index, &us) in intervals_us.iter().enumerate() {
        let label = match (index, traced) {
            (IV_PRE | IV_PRESENT, false) => continue,
            (IV_TAIL, false) => "post",
            _ => INTERVAL_LABELS[index],
        };
        let _ = write!(out, " {label}={}µs", us / divisor.max(1));
    }
}

fn group_tabs(out: &mut String, tabs: &[u16; MAX_GROUPS], groups: usize) {
    use std::fmt::Write as _;
    let _ = write!(out, " groups={groups} tabs=");
    for (index, count) in tabs.iter().take(groups.min(MAX_GROUPS)).enumerate() {
        if index > 0 {
            out.push('/');
        }
        let _ = write!(out, "{count}");
    }
}

/// Region self times (`build/layout/prepaint/paint`) in `unit` nanoseconds,
/// then `other`: each phase's total minus every region's self time.
fn region_fields(
    out: &mut String,
    region_ns: &[[u64; PHASES]; REGIONS],
    phase_us: [u64; PHASES],
    unit_ns: u64,
    min_ns: u64,
) {
    use std::fmt::Write as _;
    let mut attributed = [0u64; PHASES];
    for (index, phases) in region_ns.iter().enumerate() {
        for (sum, ns) in attributed.iter_mut().zip(phases) {
            *sum += ns;
        }
        if phases.iter().sum::<u64>() < min_ns {
            continue;
        }
        let _ = write!(
            out,
            " {}={}/{}/{}/{}",
            region_label(index),
            phases[0] / unit_ns,
            phases[1] / unit_ns,
            phases[2] / unit_ns,
            phases[3] / unit_ns,
        );
    }
    let other = |phase: usize| (phase_us[phase] * 1000).saturating_sub(attributed[phase]) / unit_ns;
    let _ = write!(
        out,
        " other={}/{}/{}/{}",
        other(0),
        other(1),
        other(2),
        other(3)
    );
}

fn phase_totals(intervals_us: &[u64; INTERVALS]) -> [u64; PHASES] {
    [
        intervals_us[IV_BUILD],
        intervals_us[IV_LAYOUT],
        intervals_us[IV_PREPAINT],
        intervals_us[IV_PAINT],
    ]
}

pub(super) fn frame_line(frame: &FrameData, rate: Option<f64>) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "ui-frame[v1 slow] view={} token={} wall={}µs",
        frame.view.label(),
        frame.token,
        frame.wall_us,
    );
    match frame.cycles.and_then(|cycles| cycles_us(cycles, rate)) {
        Some(cpu) => {
            let _ = write!(out, " cpu={cpu}µs");
        }
        None => out.push_str(" cpu=unavailable"),
    }
    match frame.faults {
        Some(faults) => {
            let _ = write!(out, " faults={faults}");
        }
        None => out.push_str(" faults=unavailable"),
    }
    if let Some(queue) = frame.queue_us {
        let _ = write!(out, " queue={queue}µs");
    }
    if frame.traced {
        let _ = write!(out, " invalidations={}", frame.invalidations);
    }
    interval_fields(&mut out, &frame.intervals_us, frame.traced, 1);
    group_tabs(&mut out, &frame.group_tabs, frame.groups);
    out.push_str(" regions_us(build/layout/prepaint/paint):");
    region_fields(
        &mut out,
        &frame.region_ns,
        phase_totals(&frame.intervals_us),
        1000,
        50_000,
    );
    out
}

pub(super) fn summary_line(summary: &Summary, rate: Option<f64>) -> String {
    use std::fmt::Write as _;
    let span_ms = summary.span_us / 1000;
    let duty = (summary.wall_sum_us * 100)
        .checked_div(summary.span_us)
        .unwrap_or(0);
    let mut out = format!(
        "ui-frame[v1 sum] span={span_ms}ms frames={} main={} popout={} other_views={} slow={} duty={duty}% wall_sum={}ms wall_max={}ms",
        summary.frames,
        summary.per_view[RenderView::Main as usize],
        summary.per_view[RenderView::Popout as usize],
        summary.frames
            - summary.per_view[RenderView::Main as usize]
            - summary.per_view[RenderView::Popout as usize],
        summary.slow,
        summary.wall_sum_us / 1000,
        summary.wall_max_us / 1000,
    );
    out.push_str(" hist_ms(<8/<16/<33/<50/<100/<250/<1000/more)=");
    for (index, count) in summary.hist.iter().enumerate() {
        if index > 0 {
            out.push('/');
        }
        let _ = write!(out, "{count}");
    }
    let frame_cpu = cycles_us(summary.frame_cycles, rate);
    match (summary.ui_cpu_us, frame_cpu) {
        (Some(ui), Some(frames)) => {
            let _ = write!(
                out,
                " ui_cpu={}ms frame_cpu={}ms outside_cpu={}ms",
                ui / 1000,
                frames / 1000,
                ui.saturating_sub(frames) / 1000,
            );
        }
        _ => out.push_str(" ui_cpu=unavailable"),
    }
    if let Some(cycles) = summary.ui_cycles {
        let share = (summary.frame_cycles * 100)
            .checked_div(cycles)
            .unwrap_or(0);
        let _ = write!(out, " frame_cycle_share={share}%");
    }
    let _ = write!(out, " faults={}", summary.faults);
    if summary.queue_n > 0 {
        let _ = write!(
            out,
            " queue_avg={}µs queue_max={}µs",
            summary.queue_sum_us / u64::from(summary.queue_n),
            summary.queue_max_us,
        );
    }
    if summary.traced > 0 {
        let _ = write!(
            out,
            " invalidations_avg={}",
            summary.invalidations / u64::from(summary.traced)
        );
    }
    out.push_str(" per_frame_avg:");
    let traced = summary.traced > 0 && summary.traced == summary.frames;
    interval_fields(
        &mut out,
        &summary.intervals_sum_us,
        traced,
        u64::from(summary.frames),
    );
    group_tabs(&mut out, &summary.group_tabs, summary.groups);
    out.push_str(" regions_ms(build/layout/prepaint/paint):");
    region_fields(
        &mut out,
        &summary.region_ns,
        phase_totals(&summary.intervals_sum_us),
        1_000_000,
        500_000,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::{
        FrameData, IV_BUILD, IV_LAYOUT, IV_PAINT, IV_PREPAINT, MAX_GROUPS, REGIONS, Region,
        Summary, frame_line, region_label, summary_line,
    };
    use crate::ui_profile::RenderView;

    #[test]
    fn region_slots_are_distinct_and_fold_late_groups() {
        let mut seen = std::collections::HashSet::new();
        let fixed = [
            Region::Prep,
            Region::Actions,
            Region::Titlebar,
            Region::Sidebar,
            Region::Toolbar,
            Region::Hotbar,
            Region::Panes,
            Region::Overlay,
            Region::Shell,
            Region::PaneScrollbar,
        ];
        for region in fixed {
            assert!(seen.insert(region.index()));
        }
        for group in 0..MAX_GROUPS {
            for region in [
                Region::Strip(group),
                Region::Body(group),
                Region::Content(group),
            ] {
                assert!(region.index() < REGIONS);
                assert!(seen.insert(region.index()));
            }
        }
        assert_eq!(seen.len(), REGIONS);
        assert_eq!(
            Region::Strip(MAX_GROUPS + 3).index(),
            Region::Strip(MAX_GROUPS - 1).index()
        );
        assert_eq!(region_label(Region::Content(2).index()), "content2");
        assert_eq!(region_label(Region::Sidebar.index()), "sidebar");
    }

    #[test]
    fn slow_frame_line_attributes_the_remainder_to_other() {
        let mut frame = FrameData::new(RenderView::Main, 7);
        frame.wall_us = 80_000;
        frame.intervals_us[IV_BUILD] = 30_000;
        frame.intervals_us[IV_LAYOUT] = 10_000;
        frame.intervals_us[IV_PREPAINT] = 5_000;
        frame.intervals_us[IV_PAINT] = 4_000;
        frame.region_ns[Region::Sidebar.index()] = [20_000_000, 6_000_000, 1_000_000, 1_000_000];
        frame.region_ns[Region::Strip(1).index()] = [40_000, 0, 0, 0];
        frame.groups = 2;
        frame.group_tabs[0] = 5;
        frame.group_tabs[1] = 12;
        assert_eq!(
            frame_line(&frame, None),
            "ui-frame[v1 slow] view=main token=7 wall=80000µs cpu=unavailable faults=unavailable \
             build=30000µs layout=10000µs compute=0µs prepaint=5000µs paint=4000µs post=0µs \
             groups=2 tabs=5/12 regions_us(build/layout/prepaint/paint): \
             sidebar=20000/6000/1000/1000 other=9960/4000/4000/3000"
        );
    }

    #[test]
    fn summary_line_reports_duty_and_cpu_outside_frames() {
        let mut summary = Summary::new();
        let mut frame = FrameData::new(RenderView::Main, 1);
        frame.wall_us = 60_000;
        frame.cycles = Some(30_000 * 3000);
        summary.add(&frame, true);
        summary.add(&frame, true);
        summary.span_us = 1_000_000;
        summary.ui_cpu_us = Some(100_000);
        summary.ui_cycles = Some(100_000 * 3000);
        let line = summary_line(&summary, Some(3000.0));
        assert!(line.contains(" frames=2 main=2 popout=0 other_views=0 slow=2 duty=12% "));
        assert!(line.contains(" hist_ms(<8/<16/<33/<50/<100/<250/<1000/more)=0/0/0/0/2/0/0/0"));
        assert!(
            line.contains(" ui_cpu=100ms frame_cpu=60ms outside_cpu=40ms frame_cycle_share=60%")
        );
    }
}
