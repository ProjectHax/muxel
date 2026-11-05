//! Where a file, diff or web page opens when the request names no tab or split.

use crate::InstanceKind;
use crate::pane::{PaneNode, SplitDirection, add_tab, side_neighbor, split};
use uuid::Uuid;

/// Who asked for a resource (a file, diff or web page) to open.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ResourceOrigin {
    /// A pane opened it: a link clicked in it, its diff button, its HTML
    /// preview. The opener's id travels with the request to where the
    /// resource's instance is created.
    Pane(Uuid),
    /// muxel's own chrome opened it: the file browser, palette, find panel,
    /// sidebar or git panel. `near`, usually the focused pane, decides which
    /// group of the same kind is closest.
    Chrome { near: Option<Uuid> },
}

impl ResourceOrigin {
    /// The pane that opened the resource, if a pane did.
    pub fn opener(self) -> Option<Uuid> {
        match self {
            Self::Pane(opener) => Some(opener),
            Self::Chrome { .. } => None,
        }
    }

    /// The pane an explicit tab or split attaches to.
    pub fn anchor(self) -> Option<Uuid> {
        match self {
            Self::Pane(opener) => Some(opener),
            Self::Chrome { near } => near,
        }
    }
}

/// Place resource `iid` of `kind`, which asked for no particular tab or split.
/// An empty layout takes it as its only pane.
///
/// A resource a pane opened goes beside that pane, so the opener stays in view
/// and what it opened sits next to it: as a tab in the pane on its right, or on
/// its left when the opener is at the right edge. An opener alone in its row
/// splits once to its right; its later opens then join that new column.
///
/// Anything else (chrome opens, or an opener no longer in the layout) joins the
/// group of the same kind nearest `origin`'s pane in reading order, or splits
/// beside that pane when no such group exists. `kind_of` reports the kind of
/// each instance in the layout. Returns false if nothing was placed.
pub fn place_resource(
    layout: &mut Option<PaneNode>,
    iid: Uuid,
    kind: InstanceKind,
    origin: ResourceOrigin,
    kind_of: impl Fn(Uuid) -> Option<InstanceKind>,
) -> bool {
    let Some(root) = layout.as_ref() else {
        *layout = Some(PaneNode::leaf(iid));
        return true;
    };
    if let Some(opener) = origin
        .opener()
        .filter(|opener| root.leaf_containing(*opener).is_some())
    {
        return match side_neighbor(root, opener) {
            Some(neighbor) => add_tab(layout, neighbor, iid),
            None => split(layout, opener, SplitDirection::Horizontal, iid),
        };
    }
    let instances: Vec<_> = root
        .collect_instances()
        .into_iter()
        .filter_map(|candidate| kind_of(candidate).map(|kind| (candidate, kind)))
        .collect();
    let near = origin.anchor();
    if let Some(group) = nearest_kind_instance(&instances, kind, near) {
        return add_tab(layout, group, iid);
    }
    let anchor = near
        .filter(|near| root.leaf_containing(*near).is_some())
        .or_else(|| root.first_instance());
    anchor.is_some_and(|anchor| split(layout, anchor, SplitDirection::Horizontal, iid))
}

/// The instance of `kind` closest to `near` in reading order (the first one when
/// `near` isn't listed).
fn nearest_kind_instance(
    instances: &[(Uuid, InstanceKind)],
    kind: InstanceKind,
    near: Option<Uuid>,
) -> Option<Uuid> {
    let near_index = near.and_then(|near| {
        instances
            .iter()
            .position(|(candidate, _)| *candidate == near)
    });
    instances
        .iter()
        .enumerate()
        .filter(|(_, (_, candidate_kind))| *candidate_kind == kind)
        .min_by_key(|(index, _)| near_index.map_or(*index, |near| index.abs_diff(near)))
        .map(|(_, (candidate, _))| *candidate)
}

#[cfg(test)]
mod tests {
    use super::{InstanceKind, PaneNode, ResourceOrigin, SplitDirection, Uuid, place_resource};
    use crate::pane::{add_tab, split};
    use std::collections::HashMap;

    use InstanceKind::{Browser, Editor, Terminal};

    /// A layout plus the kind of every instance in it.
    #[derive(Default)]
    struct Fixture {
        layout: Option<PaneNode>,
        kinds: HashMap<Uuid, InstanceKind>,
    }

    impl Fixture {
        /// One pane holding a single instance of `kind`.
        fn with(kind: InstanceKind) -> (Self, Uuid) {
            let mut fixture = Self::default();
            let iid = fixture.instance(kind);
            fixture.layout = Some(PaneNode::leaf(iid));
            (fixture, iid)
        }

        fn instance(&mut self, kind: InstanceKind) -> Uuid {
            let iid = Uuid::new_v4();
            self.kinds.insert(iid, kind);
            iid
        }

        /// A new pane of `kind`, split from `beside`'s pane.
        fn split(&mut self, beside: Uuid, direction: SplitDirection, kind: InstanceKind) -> Uuid {
            let iid = self.instance(kind);
            assert!(split(&mut self.layout, beside, direction, iid));
            iid
        }

        /// A new tab of `kind` in `group`'s pane.
        fn tab(&mut self, group: Uuid, kind: InstanceKind) -> Uuid {
            let iid = self.instance(kind);
            assert!(add_tab(&mut self.layout, group, iid));
            iid
        }

        /// Open a resource of `kind` with no explicit placement.
        fn open(&mut self, kind: InstanceKind, origin: ResourceOrigin) -> Uuid {
            let iid = Uuid::new_v4();
            let kinds = &self.kinds;
            assert!(place_resource(&mut self.layout, iid, kind, origin, |id| {
                kinds.get(&id).copied()
            }));
            self.kinds.insert(iid, kind);
            iid
        }

        fn group(&self, iid: Uuid) -> Vec<Uuid> {
            self.root().tab_group(iid).expect("placed").to_vec()
        }

        fn root(&self) -> &PaneNode {
            self.layout.as_ref().expect("layout")
        }

        /// The panes left to right when the root is one row of columns.
        fn columns(&self) -> usize {
            match self.root() {
                PaneNode::Split {
                    direction: SplitDirection::Horizontal,
                    children,
                    ..
                } => children.len(),
                _ => 1,
            }
        }
    }

    #[test]
    fn a_pane_opens_its_resource_in_the_column_to_its_right() {
        // Claude | Codex | a browser group, in one row.
        let (mut f, claude) = Fixture::with(Terminal);
        let codex = f.split(claude, SplitDirection::Horizontal, Terminal);
        let page = f.split(codex, SplitDirection::Horizontal, Browser);

        // Claude's page lands next to Claude, not in the browser group two columns over.
        let from_claude = f.open(Browser, ResourceOrigin::Pane(claude));
        assert_eq!(f.group(from_claude), [codex, from_claude]);

        // A file from Codex joins the browser column, its right-hand neighbour.
        let from_codex = f.open(Editor, ResourceOrigin::Pane(codex));
        assert_eq!(f.group(from_codex), [page, from_codex]);
        assert_eq!(f.columns(), 3);
    }

    #[test]
    fn the_rightmost_pane_opens_its_resource_in_the_column_to_its_left() {
        let (mut f, left) = Fixture::with(Terminal);
        let middle = f.split(left, SplitDirection::Horizontal, Terminal);
        let right = f.split(middle, SplitDirection::Horizontal, Terminal);

        let file = f.open(Editor, ResourceOrigin::Pane(right));
        assert_eq!(f.group(file), [middle, file]);
        assert_eq!(f.columns(), 3);
    }

    #[test]
    fn a_pane_alone_in_its_row_splits_once_beside_itself() {
        // The top row holds a terminal and an editor; the bottom row is one wide terminal.
        let (mut f, top) = Fixture::with(Terminal);
        let bottom = f.split(top, SplitDirection::Vertical, Terminal);
        let notes = f.split(top, SplitDirection::Horizontal, Editor);

        // The bottom pane's file stays in the bottom row rather than joining the
        // editor above it.
        let first = f.open(Editor, ResourceOrigin::Pane(bottom));
        assert_eq!(f.group(first), [first]);
        assert_eq!(f.group(notes), [notes]);
        let PaneNode::Split { children, .. } = f.root() else {
            panic!("two rows");
        };
        let bottom_row = &children[1];
        assert_eq!(bottom_row.collect_instances(), [bottom, first]);

        // The split happens once: the next file from the same pane joins the new column.
        let second = f.open(Editor, ResourceOrigin::Pane(bottom));
        assert_eq!(f.group(second), [first, second]);
    }

    #[test]
    fn the_neighbour_is_the_pane_touching_the_opener_on_that_side() {
        // [ (A | B) over C ] | D: B's right edge meets D, and A has B to its right.
        let (mut f, a) = Fixture::with(Terminal);
        let d = f.split(a, SplitDirection::Horizontal, Terminal);
        let c = f.split(a, SplitDirection::Vertical, Terminal);
        let b = f.split(a, SplitDirection::Horizontal, Terminal);

        let from_b = f.open(Editor, ResourceOrigin::Pane(b));
        assert_eq!(f.group(from_b), [d, from_b]);
        let from_c = f.open(Browser, ResourceOrigin::Pane(c));
        assert_eq!(f.group(from_c), [d, from_b, from_c]);
        let from_a = f.open(Browser, ResourceOrigin::Pane(a));
        assert_eq!(f.group(from_a), [b, from_a]);
    }

    #[test]
    fn a_stacked_neighbour_column_takes_the_resource_in_its_top_pane() {
        // A | (B over C)
        let (mut f, a) = Fixture::with(Terminal);
        let b = f.split(a, SplitDirection::Horizontal, Terminal);
        let c = f.split(b, SplitDirection::Vertical, Terminal);

        let from_a = f.open(Editor, ResourceOrigin::Pane(a));
        assert_eq!(f.group(from_a), [b, from_a]);
        // C is at the right edge, so its neighbour is A on the left.
        let from_c = f.open(Editor, ResourceOrigin::Pane(c));
        assert_eq!(f.group(from_c), [a, from_c]);
    }

    #[test]
    fn chrome_opens_join_the_same_kind_group_nearest_the_focused_pane() {
        // browser | terminal | terminal | browser
        let (mut f, left_browser) = Fixture::with(Browser);
        let left = f.split(left_browser, SplitDirection::Horizontal, Terminal);
        let right = f.split(left, SplitDirection::Horizontal, Terminal);
        let right_browser = f.split(right, SplitDirection::Horizontal, Browser);

        let near_right = f.open(Browser, ResourceOrigin::Chrome { near: Some(right) });
        assert_eq!(f.group(near_right), [right_browser, near_right]);
        let near_left = f.open(Browser, ResourceOrigin::Chrome { near: Some(left) });
        assert_eq!(f.group(near_left), [left_browser, near_left]);

        // The same page opened by the left terminal goes to its right-hand neighbour.
        let from_left = f.open(Browser, ResourceOrigin::Pane(left));
        assert_eq!(f.group(from_left), [right, from_left]);
    }

    #[test]
    fn an_opener_gone_from_the_layout_falls_back_to_the_same_kind_group() {
        // A browser popup arrives a tick after its pane has closed.
        let (mut f, terminal) = Fixture::with(Terminal);
        let page = f.split(terminal, SplitDirection::Horizontal, Browser);
        f.tab(terminal, Terminal);

        let popup = f.open(Browser, ResourceOrigin::Pane(Uuid::new_v4()));
        assert_eq!(f.group(popup), [page, popup]);
        // With no group of its kind, it splits beside the first pane.
        let file = f.open(Editor, ResourceOrigin::Pane(Uuid::new_v4()));
        assert_eq!(f.columns(), 3);
        assert_eq!(f.group(file), [file]);

        // An empty layout takes it as its only pane.
        let mut empty = Fixture::default();
        let only = empty.open(Editor, ResourceOrigin::Pane(Uuid::new_v4()));
        assert_eq!(empty.root().collect_instances(), [only]);
    }
}
