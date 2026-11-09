//! The grid each pane gives its terminal, measured where the pane is laid out.
//!
//! A terminal's PTY opens before its view exists, and the view isn't painted
//! until the program has drawn something visible: until then the pane shows
//! "Starting terminal…". A shell's first prompt is therefore drawn at whatever
//! size the PTY opened with. A prompt drawn at any other width than its pane's
//! is reflowed by the later resize, and PowerShell's line editor then draws the
//! first command over it. So a launch opens its PTY at the size measured here,
//! waiting at most one frame for its pane to be laid out.

use gpui::{App, IntoElement, SharedString, Styled, Window, canvas};
use muxel_terminal::TerminalSession;
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// The longest a launch waits for its project's next layout. A shown project
/// is laid out in its window's next frame, so this only bounds a window that
/// draws nothing (one started hidden, say); the launch then opens at its best
/// earlier guess, as it always did.
pub(super) const PANE_LAYOUT_WAIT: Duration = Duration::from_millis(250);

/// Shared with the measuring elements, which run while a window draws.
pub(super) type SharedPaneGrids = Rc<RefCell<PaneGrids>>;

#[derive(Default)]
pub(super) struct PaneGrids {
    /// The grid each tab's pane gave its terminal at the pane's latest layout.
    grids: HashMap<Uuid, (u16, u16)>,
    /// Launches waiting for their project's next layout, by project. Dropping
    /// a sender wakes its launch.
    waiting: HashMap<Uuid, Vec<async_channel::Sender<()>>>,
}

impl PaneGrids {
    /// The grid `iid`'s pane gave its terminal when it was last laid out.
    pub(super) fn grid(&self, iid: Uuid) -> Option<(u16, u16)> {
        self.grids.get(&iid).copied()
    }

    /// Resolves once `pid`'s panes have next been laid out.
    pub(super) fn next_layout(&mut self, pid: Uuid) -> async_channel::Receiver<()> {
        let (wake, woken) = async_channel::bounded(1);
        self.waiting.entry(pid).or_default().push(wake);
        woken
    }

    /// Forget panes that no longer exist, and launches that stopped waiting: one
    /// that timed out (its window drew nothing) dropped its receiver, and its
    /// project may never draw again to clear it.
    pub(super) fn prune(&mut self, live: &HashSet<Uuid>) {
        self.grids.retain(|iid, _| live.contains(iid));
        self.waiting.retain(|_, wakes| {
            wakes.retain(|wake| !wake.is_closed());
            !wakes.is_empty()
        });
    }
}

/// Measures a pane's body as it is laid out: records the grid its tabs'
/// terminals get there, and moves any of its terminals that hasn't drawn yet
/// to that size, so a pane resized while its program starts still opens at the
/// right size. Place it inside the pane body, which must be `relative`.
pub(super) fn measure_pane(
    grids: SharedPaneGrids,
    tabs: Vec<Uuid>,
    starting: Vec<Arc<TerminalSession>>,
    font_family: SharedString,
    font_size: f32,
) -> impl IntoElement {
    canvas(
        move |bounds, window: &mut Window, cx: &mut App| {
            let grid = muxel_terminal::pane_grid(window, &font_family, font_size, bounds.size);
            let mut state = grids.borrow_mut();
            for tab in tabs {
                state.grids.insert(tab, grid);
            }
            drop(state);
            // A drag churns the layout; the terminal element waits it out too.
            if cx.has_active_drag() {
                return;
            }
            let mut settling = false;
            for session in starting {
                settling |= session.resize(grid.0, grid.1);
            }
            if settling {
                window.request_animation_frame();
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .size_full()
}

/// Wakes the launches waiting on `pid`'s layout. Place it after the pane area
/// in the element tree: elements are measured in tree order, so every pane the
/// window shows has been measured by the time this runs.
pub(super) fn layout_done(grids: SharedPaneGrids, pid: Uuid) -> impl IntoElement {
    canvas(
        move |_, _, _| {
            grids.borrow_mut().waiting.remove(&pid);
        },
        |_, _, _, _| {},
    )
    .absolute()
}

#[cfg(test)]
mod tests {
    use super::PaneGrids;
    use std::collections::HashSet;
    use uuid::Uuid;

    #[test]
    fn pruning_forgets_gone_panes_and_launches_that_stopped_waiting() {
        let mut grids = PaneGrids::default();
        let (kept, gone) = (Uuid::new_v4(), Uuid::new_v4());
        grids.grids.insert(kept, (120, 30));
        grids.grids.insert(gone, (120, 30));
        let project = Uuid::new_v4();
        let waiting = grids.next_layout(project);
        // A launch whose window drew nothing gives up and drops its receiver.
        drop(grids.next_layout(project));
        let live = HashSet::from([kept]);

        grids.prune(&live);
        assert_eq!(grids.grid(kept), Some((120, 30)));
        assert_eq!(grids.grid(gone), None);
        assert_eq!(grids.waiting.get(&project).map(Vec::len), Some(1));

        drop(waiting);
        grids.prune(&live);
        assert!(grids.waiting.is_empty());
    }
}
