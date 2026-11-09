//! Per-frame attribution for window draws (opt-in with the rest of `ui_profile`).
//!
//! Each draw of a view wrapped by `watch_render_build`/`finish_render` opens
//! one frame on the UI thread. The frame closes when GPUI clears its element
//! arena, after presenting. The root wrapper marks the phase boundaries, and
//! regions wrapped with `ui_profile::region` add their *self* time (their
//! work minus nested wrapped regions) per element phase. `other` is each root
//! interval minus the region time that ran in it, so nothing is counted twice
//! when GPUI lays a subtree out inside an ancestor's prepaint; `layout_late`
//! reports how much region request-layout ran there. A frame of at least
//! 50 ms emits one `ui-frame[v1 slow]` record. Every frame feeds a ten-second
//! `ui-frame[v1 sum]` summary, which also reports the UI thread's CPU time and
//! the frames' share of its cycles.
//!
//! Intervals, in GPUI's order. Each starts where the previous one ends, so a
//! frame's intervals add up to its `wall`:
//! - `pre`: `Window::draw` start to the root view's build (entity
//!   invalidation, gpui-component's Root render). Needs GPUI's frame trace.
//! - `build`: the wrapped view's `Render::render`.
//! - `layout`: request-layout of the app element tree. RenderOnce components
//!   (buttons, inputs) and uncached child views render here.
//! - `compute`: Root's request-layout outside the app element (before and
//!   after it) plus taffy's layout compute.
//! - `prepaint`: the app's prepaint, including the render, layout and
//!   prepaint of dirty cached views (background terminals), then GPUI's
//!   deferred draws (tooltips, popovers) up to paint.
//! - `paint`
//! - `tail`: paint end to `Window::draw` end (scene finish, focus and
//!   accessed-entity bookkeeping). Without GPUI's frame trace this interval
//!   runs to arena clear and is labelled `post`.
//! - `present`: `Window::draw` end to arena clear: the platform present plus
//!   dropping the elements allocated before the root wrapper.
//!
//! The summary's `per_frame_avg` averages `pre`, `tail` and `present` over
//! traced frames (`traced=T/F`), `post` over the rest, and the others over
//! every frame.
//!
//! `cycles` counts the UI thread's own cycles (Windows
//! `QueryThreadCycleTime`) over `cpu_span`, the frame from its build to arena
//! clear. `pre` is outside that span: GPUI gives no hook at draw start to
//! read the counters. Cycles are a count, not a duration. Microsoft warns
//! against converting them to elapsed time, and the rate can differ between
//! frames and the thread's other work, so nothing here converts them. The
//! summary's `ui_cpu` is the thread's measured CPU time from `GetThreadTimes`
//! (100 ns units, but charged per scheduler tick, so too coarse for one frame
//! and usable over ten seconds). `frame_cycle_share` is the frames' share of
//! the thread's cycles over the same interval, frames' `pre` outside it; it is
//! a share of cycles, not of `ui_cpu`. It says `unavailable` unless every
//! frame's cycles read, and `inconsistent` if the frames' cycles exceed the
//! interval's. `faults` is the process-wide page-fault count, soft faults
//! included. Off Windows, or when a read fails, these say `unavailable`.
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
    /// The span `cycles` and `faults` cover: build start to arena clear.
    cpu_span_us: u64,
    cycles: Option<u64>,
    faults: Option<u64>,
    region_ns: [[u64; PHASES]; REGIONS],
    /// Region self time by the root interval it ran in, which `other`
    /// subtracts. It differs from `region_ns`'s phase columns when GPUI lays a
    /// subtree out inside an ancestor's prepaint.
    ran_in_ns: [u64; PHASES],
    /// Region request-layout self time that ran outside the root's layout.
    layout_late_ns: u64,
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
            cpu_span_us: 0,
            cycles: None,
            faults: None,
            region_ns: [[0; PHASES]; REGIONS],
            ran_in_ns: [0; PHASES],
            layout_late_ns: 0,
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
    /// Traced frames' intervals, and every frame's build through paint. An
    /// untraced frame's `post` goes to `post_sum_us` instead, since it spans
    /// a traced frame's `tail` and `present`.
    intervals_sum_us: [u64; INTERVALS],
    post_sum_us: u64,
    traced: u32,
    queue_sum_us: u64,
    queue_max_us: u64,
    queue_n: u32,
    invalidations: u64,
    /// Cycles over the `cycles_n` frames whose counters read.
    frame_cycles: u64,
    cycles_n: u32,
    faults: u64,
    faults_n: u32,
    region_ns: [[u64; PHASES]; REGIONS],
    ran_in_ns: [u64; PHASES],
    layout_late_ns: u64,
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
            post_sum_us: 0,
            traced: 0,
            queue_sum_us: 0,
            queue_max_us: 0,
            queue_n: 0,
            invalidations: 0,
            frame_cycles: 0,
            cycles_n: 0,
            faults: 0,
            faults_n: 0,
            region_ns: [[0; PHASES]; REGIONS],
            ran_in_ns: [0; PHASES],
            layout_late_ns: 0,
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
        for (index, (sum, us)) in self
            .intervals_sum_us
            .iter_mut()
            .zip(frame.intervals_us)
            .enumerate()
        {
            if frame.traced || !matches!(index, IV_PRE | IV_TAIL | IV_PRESENT) {
                *sum += us;
            }
        }
        if !frame.traced {
            // An untraced frame's tail slot holds its post.
            self.post_sum_us += frame.intervals_us[IV_TAIL];
        }
        self.traced += u32::from(frame.traced);
        if let Some(queue) = frame.queue_us {
            self.queue_sum_us += queue;
            self.queue_max_us = self.queue_max_us.max(queue);
            self.queue_n += 1;
        }
        self.invalidations += frame.invalidations;
        if let Some(cycles) = frame.cycles {
            self.frame_cycles += cycles;
            self.cycles_n += 1;
        }
        if let Some(faults) = frame.faults {
            self.faults += faults;
            self.faults_n += 1;
        }
        for (sum, frame) in self.region_ns.iter_mut().zip(frame.region_ns.iter()) {
            for (sum, ns) in sum.iter_mut().zip(frame) {
                *sum += ns;
            }
        }
        for (sum, ns) in self.ran_in_ns.iter_mut().zip(frame.ran_in_ns) {
            *sum += ns;
        }
        self.layout_late_ns += frame.layout_late_ns;
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

    /// Close this interval now: its summary, with the thread's counters over
    /// the interval, and a fresh interval in its place.
    fn roll(&mut self) -> Box<Summary> {
        let next = Interval::start();
        let mut summary = std::mem::replace(&mut self.summary, Box::new(Summary::new()));
        summary.span_us = next
            .started
            .saturating_duration_since(self.started)
            .as_micros() as u64;
        summary.ui_cpu_us = self
            .cpu_100ns
            .zip(next.cpu_100ns)
            .map(|(start, end)| end.saturating_sub(start) / 10);
        summary.ui_cycles = self
            .cycles
            .zip(next.cycles)
            .map(|(start, end)| end.saturating_sub(start));
        *self = next;
        summary
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
    // The summary's counters start before the first frame's, so every frame
    // a summary counts falls inside the span its thread totals cover.
    INTERVAL.with(|interval| {
        interval.borrow_mut().get_or_insert_with(Interval::start);
    });
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
    // Token zero is a frame that was never opened (profiling off): no clock.
    if token == 0 {
        return;
    }
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
            let self_ns = elapsed.saturating_sub(scope.child_ns);
            frame.data.region_ns[scope.region][scope.phase] += self_ns;
            let running = running_phase(&frame.marks);
            if let Some(phase) = running {
                frame.data.ran_in_ns[phase] += self_ns;
            }
            if scope.phase == Phase::Layout as usize && running != Some(Phase::Layout as usize) {
                frame.data.layout_late_ns += self_ns;
            }
            if let Some(parent) = frame.scopes.last_mut() {
                parent.child_ns += elapsed;
            }
        });
    }
}

/// The root interval running now, from the marks set so far, or `None` in
/// `compute` or after paint, which have no region phase. Scopes close inside
/// the root's callbacks or GPUI's deferred draws, so this names the interval
/// a scope ran in. `prepaint` runs on to paint start, as [`close`] counts it.
fn running_phase(marks: &[Option<Instant>; 7]) -> Option<usize> {
    let set = |mark: Mark| marks[mark as usize].is_some();
    if set(Mark::PaintStart) {
        (!set(Mark::PaintEnd)).then_some(Phase::Paint as usize)
    } else if set(Mark::PrepaintStart) {
        Some(Phase::Prepaint as usize)
    } else if set(Mark::LayoutStart) {
        (!set(Mark::LayoutEnd)).then_some(Phase::Layout as usize)
    } else if set(Mark::BuildEnd) {
        None
    } else {
        Some(Phase::Build as usize)
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
    let trace = take_trace(acc.window).filter(|timing| draws_frame(timing, &acc));
    let data = close(acc, now, cycles_end, faults_end, trace.as_ref());

    let slow = data.wall_us >= super::SLOW_RENDER_US;
    INTERVAL.with(|interval| {
        // `begin` opened the interval before this frame's counters.
        let mut interval = interval.borrow_mut();
        let Some(current) = interval.as_mut() else {
            return;
        };
        current.summary.add(&data, slow);
        if current.started.elapsed() >= SUMMARY_EVERY {
            super::defer_frame_summary(current.roll());
        }
    });
    if slow {
        super::defer_frame(data);
    }
}

/// GPUI records a draw's timing at the end of `Window::draw`; the one for
/// this window that brackets our build and paint belongs to this frame.
fn draws_frame(timing: &DrawTiming, acc: &FrameAcc) -> bool {
    timing.draw_start <= acc.started
        && acc.marks[Mark::PaintEnd as usize].is_none_or(|end| timing.draw_end >= end)
}

/// The record of a frame closed at `now`. Each interval runs from where the
/// previous one ended to its own closing mark, so the intervals add up to
/// `wall` exactly; a missing mark leaves its interval empty. `cpu_span` is
/// `wall` minus `pre`.
fn close(
    acc: FrameAcc,
    now: Instant,
    cycles_end: Option<u64>,
    faults_end: Option<u32>,
    trace: Option<&DrawTiming>,
) -> Box<FrameData> {
    let FrameAcc {
        mut data,
        started,
        marks,
        cycles_start,
        faults_start,
        ..
    } = acc;
    let at = |mark: Mark| marks[mark as usize];
    let wall_from = trace.map_or(started, |timing| timing.draw_start.min(started));
    // Microseconds from `wall_from` to each boundary, never before the last.
    let mut last = 0u64;
    let mut upto = |boundary: Option<Instant>| {
        if let Some(boundary) = boundary {
            let us = boundary.saturating_duration_since(wall_from).as_micros() as u64;
            last = last.max(us);
        }
        last
    };
    let build_start = upto(Some(started));
    let build_end = upto(at(Mark::BuildEnd));
    let layout_start = upto(at(Mark::LayoutStart));
    let layout_end = upto(at(Mark::LayoutEnd));
    let prepaint_start = upto(at(Mark::PrepaintStart));
    // GPUI's deferred draws (tooltips, popovers) run after the app's prepaint
    // and before paint; prepaint keeps them.
    let paint_start = upto(at(Mark::PaintStart).or(at(Mark::PrepaintEnd)));
    let paint_end = upto(at(Mark::PaintEnd));
    let draw_end = upto(trace.map(|timing| timing.draw_end));
    let wall = upto(Some(now));

    let mut iv = [0u64; INTERVALS];
    iv[IV_PRE] = build_start;
    iv[IV_BUILD] = build_end - build_start;
    iv[IV_LAYOUT] = layout_end - layout_start;
    iv[IV_COMPUTE] = (layout_start - build_end) + (prepaint_start - layout_end);
    iv[IV_PREPAINT] = paint_start - prepaint_start;
    iv[IV_PAINT] = paint_end - paint_start;
    match trace {
        Some(timing) => {
            iv[IV_TAIL] = draw_end - paint_end;
            iv[IV_PRESENT] = wall - draw_end;
            data.traced = true;
            data.queue_us = timing.dirty_at.map(|dirty| {
                timing
                    .draw_start
                    .saturating_duration_since(dirty)
                    .as_micros() as u64
            });
            data.invalidations = timing.invalidations;
        }
        // `post`: paint end to arena clear.
        None => iv[IV_TAIL] = wall - paint_end,
    }
    data.intervals_us = iv;
    data.wall_us = wall;
    data.cpu_span_us = wall - build_start;
    data.cycles = cycles_start
        .zip(cycles_end)
        .map(|(start, end)| end.saturating_sub(start));
    data.faults = faults_start
        .zip(faults_end)
        .map(|(start, end)| u64::from(end.wrapping_sub(start)));
    data
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

// --- Record formatting (record worker) ---------------------------------------

fn interval_fields(out: &mut String, intervals_us: &[u64; INTERVALS], traced: bool) {
    use std::fmt::Write as _;
    for (index, &us) in intervals_us.iter().enumerate() {
        let label = match (index, traced) {
            (IV_PRE | IV_PRESENT, false) => continue,
            (IV_TAIL, false) => "post",
            _ => INTERVAL_LABELS[index],
        };
        let _ = write!(out, " {label}={us}µs");
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
/// then `other`: each interval's total minus the region self time that ran
/// in it, and `layout_late`: region request-layout that ran outside the
/// root's layout interval (inside an ancestor's prepaint).
fn region_fields(
    out: &mut String,
    region_ns: &[[u64; PHASES]; REGIONS],
    ran_in_ns: [u64; PHASES],
    layout_late_ns: u64,
    phase_us: [u64; PHASES],
    unit_ns: u64,
    min_ns: u64,
) {
    use std::fmt::Write as _;
    for (index, phases) in region_ns.iter().enumerate() {
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
    let other = |phase: usize| (phase_us[phase] * 1000).saturating_sub(ran_in_ns[phase]) / unit_ns;
    let _ = write!(
        out,
        " other={}/{}/{}/{} layout_late={}",
        other(0),
        other(1),
        other(2),
        other(3),
        layout_late_ns / unit_ns,
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

pub(super) fn frame_line(frame: &FrameData) -> String {
    use std::fmt::Write as _;
    let mut out = format!(
        "ui-frame[v1 slow] view={} token={} wall={}µs cpu_span={}µs",
        frame.view.label(),
        frame.token,
        frame.wall_us,
        frame.cpu_span_us,
    );
    match frame.cycles {
        Some(cycles) => {
            let _ = write!(out, " cycles={cycles}");
        }
        None => out.push_str(" cycles=unavailable"),
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
    interval_fields(&mut out, &frame.intervals_us, frame.traced);
    group_tabs(&mut out, &frame.group_tabs, frame.groups);
    out.push_str(" regions_us(build/layout/prepaint/paint):");
    region_fields(
        &mut out,
        &frame.region_ns,
        frame.ran_in_ns,
        frame.layout_late_ns,
        phase_totals(&frame.intervals_us),
        1000,
        50_000,
    );
    out
}

/// Average intervals per frame. `pre`, `tail` and `present` need GPUI's
/// frame trace, so they average over traced frames and `post` over the rest.
fn summary_interval_fields(out: &mut String, summary: &Summary) {
    use std::fmt::Write as _;
    let untraced = summary.frames - summary.traced;
    let mut field = |label: &str, sum_us: u64, frames: u32| {
        if frames > 0 {
            let _ = write!(out, " {label}={}µs", sum_us / u64::from(frames));
        }
    };
    for (index, &sum_us) in summary.intervals_sum_us.iter().enumerate() {
        let frames = match index {
            IV_PRE | IV_TAIL | IV_PRESENT => summary.traced,
            _ => summary.frames,
        };
        field(INTERVAL_LABELS[index], sum_us, frames);
    }
    field("post", summary.post_sum_us, untraced);
}

pub(super) fn summary_line(summary: &Summary) -> String {
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
    match summary.ui_cpu_us {
        Some(ui) => {
            let _ = write!(out, " ui_cpu={}ms", ui / 1000);
        }
        None => out.push_str(" ui_cpu=unavailable"),
    }
    // Every frame's cycles must have read for the share to be the frames'.
    match summary
        .ui_cycles
        .filter(|&cycles| cycles > 0 && summary.cycles_n == summary.frames)
    {
        // Frames' cycles beyond the interval's mean the two didn't cover the
        // same span; say so rather than print a share over 100%.
        Some(ui_cycles) if summary.frame_cycles > ui_cycles => {
            out.push_str(" frame_cycle_share=inconsistent");
        }
        Some(ui_cycles) => {
            let share = u128::from(summary.frame_cycles) * 100 / u128::from(ui_cycles);
            let _ = write!(out, " frame_cycle_share={share}%");
        }
        None => out.push_str(" frame_cycle_share=unavailable"),
    }
    match summary.faults_n {
        0 => out.push_str(" faults=unavailable"),
        n => {
            let _ = write!(out, " faults={}", summary.faults);
            if n < summary.frames {
                let _ = write!(out, " faults_frames={n}");
            }
        }
    }
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
    let _ = write!(out, " traced={}/{}", summary.traced, summary.frames);
    out.push_str(" per_frame_avg:");
    summary_interval_fields(&mut out, summary);
    group_tabs(&mut out, &summary.group_tabs, summary.groups);
    out.push_str(" regions_ms(build/layout/prepaint/paint):");
    region_fields(
        &mut out,
        &summary.region_ns,
        summary.ran_in_ns,
        summary.layout_late_ns,
        phase_totals(&summary.intervals_sum_us),
        1_000_000,
        500_000,
    );
    out
}

#[cfg(test)]
mod tests {
    use super::{
        DrawTiming, FRAMES, FrameAcc, FrameData, IV_BUILD, IV_COMPUTE, IV_LAYOUT, IV_PAINT, IV_PRE,
        IV_PREPAINT, IV_PRESENT, IV_TAIL, MAX_GROUPS, Mark, Phase, REGIONS, Region, Summary, close,
        frame_line, mark, open, region_label, running_phase, summary_line,
    };
    use crate::ui_profile::RenderView;
    use std::time::{Duration, Instant};

    /// A region laid out inside an ancestor's prepaint (GPUI does this for
    /// some subtrees) must leave `other`'s prepaint share: every interval is
    /// its regions' self time plus `other`, with nothing counted twice.
    #[test]
    fn layout_run_during_prepaint_leaves_the_prepaint_remainder() {
        let token = 9_001;
        FRAMES.with(|frames| {
            frames.borrow_mut().push(FrameAcc {
                data: Box::new(FrameData::new(RenderView::Main, token)),
                started: Instant::now(),
                marks: [None; 7],
                window: None,
                cycles_start: None,
                faults_start: None,
                scopes: Vec::new(),
            });
        });
        for at in [
            Mark::BuildEnd,
            Mark::LayoutStart,
            Mark::LayoutEnd,
            Mark::PrepaintStart,
        ] {
            mark(token, at);
        }
        let prepaint = Instant::now();
        {
            let _scope = open(Region::Sidebar, Phase::Layout);
            let spin = Instant::now();
            while spin.elapsed() < Duration::from_millis(3) {
                std::hint::spin_loop();
            }
        }
        let prepaint_us = prepaint.elapsed().as_micros() as u64;
        mark(token, Mark::PrepaintEnd);
        let mut data = FRAMES.with(|frames| {
            let mut frames = frames.borrow_mut();
            let index = frames
                .iter()
                .position(|frame| frame.data.token == token)
                .unwrap();
            frames.remove(index).data
        });
        data.intervals_us[IV_PREPAINT] = prepaint_us;
        let line = frame_line(&data);
        let other = line.rsplit(" other=").next().unwrap();
        let other_prepaint: u64 = other.split('/').nth(2).unwrap().parse().unwrap();
        assert!(
            other_prepaint < prepaint_us / 2,
            "the sidebar's {prepaint_us} µs of layout stayed in other: {line}"
        );
    }

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
        // Half the sidebar's layout ran inside the root's prepaint.
        frame.ran_in_ns = [20_040_000, 3_000_000, 4_000_000, 1_000_000];
        frame.layout_late_ns = 3_000_000;
        frame.groups = 2;
        frame.group_tabs[0] = 5;
        frame.group_tabs[1] = 12;
        assert_eq!(
            frame_line(&frame),
            "ui-frame[v1 slow] view=main token=7 wall=80000µs cpu_span=0µs cycles=unavailable \
             faults=unavailable \
             build=30000µs layout=10000µs compute=0µs prepaint=5000µs paint=4000µs post=0µs \
             groups=2 tabs=5/12 regions_us(build/layout/prepaint/paint): \
             sidebar=20000/6000/1000/1000 other=9960/7000/1000/3000 layout_late=3000"
        );
    }

    #[test]
    fn summary_line_reports_duty_and_the_frames_cycle_share() {
        let mut summary = Summary::new();
        let mut frame = FrameData::new(RenderView::Main, 1);
        frame.wall_us = 60_000;
        frame.cycles = Some(30_000 * 3000);
        summary.add(&frame, true);
        summary.add(&frame, true);
        summary.span_us = 1_000_000;
        summary.ui_cpu_us = Some(100_000);
        summary.ui_cycles = Some(100_000 * 3000);
        let line = summary_line(&summary);
        assert!(line.contains(" frames=2 main=2 popout=0 other_views=0 slow=2 duty=12% "));
        assert!(line.contains(" hist_ms(<8/<16/<33/<50/<100/<250/<1000/more)=0/0/0/0/2/0/0/0"));
        assert!(
            line.contains(" ui_cpu=100ms frame_cycle_share=60% "),
            "{line}"
        );

        // Frames' cycles beyond the interval's are a coverage error, not 100%.
        summary.ui_cycles = Some(50_000 * 3000);
        let line = summary_line(&summary);
        assert!(line.contains(" frame_cycle_share=inconsistent "), "{line}");
    }

    /// The first summary's thread counters start before its first frame's.
    /// A costly first frame, then as much work outside frames, is about half
    /// the interval's cycles, not all of them.
    #[cfg(windows)]
    #[test]
    fn the_first_summary_counts_its_first_frame_inside_its_counters() {
        use super::{INTERVAL, begin, end, thread_cycles};
        // A fresh thread, so this thread's interval doesn't exist yet.
        std::thread::spawn(|| {
            // Run on the CPU until this thread has used `cycles` more cycles.
            let spin = |cycles: u64| {
                let start = thread_cycles().unwrap();
                while thread_cycles().unwrap() - start < cycles {
                    std::hint::spin_loop();
                }
            };
            let token = 9_002;
            begin(RenderView::Main, token);
            spin(30_000_000);
            end(token);
            spin(30_000_000);
            let summary = INTERVAL.with(|interval| interval.borrow_mut().as_mut().unwrap().roll());
            let line = summary_line(&summary);
            let share = line
                .split(" frame_cycle_share=")
                .nth(1)
                .and_then(|rest| rest.split('%').next())
                .and_then(|share| share.parse::<u64>().ok());
            assert!(
                share.is_some_and(|share| (40..=60).contains(&share)),
                "{line}"
            );
        })
        .join()
        .unwrap();
    }

    /// Intervals telescope from draw start to arena clear, so a slow record
    /// accounts for its whole `wall` (regions plus `other` inside the four
    /// element phases), including the gaps between the root's marks: Root's
    /// request-layout before the app element, and GPUI's deferred draws
    /// (tooltips) after the app's prepaint. The counters' span is the same
    /// frame without `pre`.
    #[test]
    fn intervals_cover_the_frame_and_the_counters_cover_all_but_pre() {
        let draw_start = Instant::now();
        let at = |ms: u64| draw_start + Duration::from_millis(ms);
        let mut marks = [None; 7];
        for (mark, ms) in [
            (Mark::BuildEnd, 5),
            (Mark::LayoutStart, 7),
            (Mark::LayoutEnd, 11),
            (Mark::PrepaintStart, 12),
            (Mark::PrepaintEnd, 15),
            (Mark::PaintStart, 18),
            (Mark::PaintEnd, 20),
        ] {
            marks[mark as usize] = Some(at(ms));
        }
        // A tooltip's prepaint closes after the app's prepaint ended.
        let mut deferred = marks;
        deferred[Mark::PaintStart as usize] = None;
        deferred[Mark::PaintEnd as usize] = None;
        assert_eq!(running_phase(&deferred), Some(Phase::Prepaint as usize));

        let acc = FrameAcc {
            data: Box::new(FrameData::new(RenderView::Main, 3)),
            started: at(1),
            marks,
            window: Some(1),
            cycles_start: Some(1_000),
            faults_start: Some(10),
            scopes: Vec::new(),
        };
        let trace = DrawTiming {
            window: 1,
            draw_start,
            draw_end: at(22),
            dirty_at: None,
            invalidations: 0,
        };
        let data = close(acc, at(25), Some(4_000), Some(12), Some(&trace));
        let iv = data.intervals_us;
        assert_eq!(iv.iter().sum::<u64>(), data.wall_us);
        assert_eq!(data.wall_us, 25_000);
        assert_eq!(
            [
                iv[IV_PRE],
                iv[IV_BUILD],
                iv[IV_LAYOUT],
                iv[IV_COMPUTE],
                iv[IV_PREPAINT],
                iv[IV_PAINT],
                iv[IV_TAIL],
                iv[IV_PRESENT],
            ],
            [1_000, 4_000, 4_000, 3_000, 6_000, 2_000, 2_000, 3_000]
        );
        assert_eq!(data.cpu_span_us + iv[IV_PRE], data.wall_us);
        assert_eq!((data.cycles, data.faults), (Some(3_000), Some(2)));

        // An untraced frame starts at build; its `post` runs to arena clear.
        let untraced = close(
            FrameAcc {
                data: Box::new(FrameData::new(RenderView::Main, 4)),
                started: at(1),
                marks,
                window: None,
                cycles_start: None,
                faults_start: None,
                scopes: Vec::new(),
            },
            at(25),
            None,
            None,
            None,
        );
        let iv = untraced.intervals_us;
        assert_eq!(iv.iter().sum::<u64>(), untraced.wall_us);
        assert_eq!(untraced.wall_us, 24_000);
        assert_eq!((iv[IV_PRE], iv[IV_TAIL], iv[IV_PRESENT]), (0, 5_000, 0));
        assert_eq!(untraced.cpu_span_us, untraced.wall_us);
    }

    /// A counter that didn't read is `unavailable`, never zero, and a mix of
    /// traced and untraced frames keeps each kind's own intervals.
    #[test]
    fn summary_reports_only_what_was_recorded() {
        let mut summary = Summary::new();
        let frame = FrameData::new(RenderView::Main, 1);
        summary.add(&frame, false);
        let line = summary_line(&summary);
        assert!(line.contains(" faults=unavailable"), "{line}");
        assert!(line.contains(" frame_cycle_share=unavailable"), "{line}");

        let mut traced = FrameData::new(RenderView::Main, 2);
        traced.traced = true;
        traced.intervals_us[IV_PRE] = 2_000;
        traced.intervals_us[IV_TAIL] = 1_000;
        traced.intervals_us[IV_PRESENT] = 3_000;
        traced.faults = Some(4);
        let mut untraced = FrameData::new(RenderView::Main, 3);
        untraced.intervals_us[IV_TAIL] = 5_000;
        let mut summary = Summary::new();
        summary.add(&traced, false);
        summary.add(&untraced, false);
        let line = summary_line(&summary);
        assert!(line.contains(" faults=4 faults_frames=1"), "{line}");
        assert!(
            line.contains(" traced=1/2 per_frame_avg: pre=2000µs "),
            "{line}"
        );
        assert!(
            line.contains(" tail=1000µs present=3000µs post=5000µs "),
            "{line}"
        );
    }
}
