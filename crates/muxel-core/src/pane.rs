//! The recursive pane layout tree.
//!
//! A project's panes form a tree of [`PaneNode`]s: each `Leaf` is a *tab group*
//! holding one or more agent instances with one active, each `Split` arranges
//! children horizontally or vertically with relative `sizes`. Algorithms here are
//! pure and unit-tested; the renderer and persistence layers build on them.
//! Adapted from okena's `LayoutNode` (MIT).

use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// How a split arranges its children.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SplitDirection {
    /// Children laid out left-to-right (a vertical divider between them).
    Horizontal,
    /// Children laid out top-to-bottom (a horizontal divider between them).
    Vertical,
}

impl SplitDirection {
    pub fn flipped(self) -> Self {
        match self {
            Self::Horizontal => Self::Vertical,
            Self::Vertical => Self::Horizontal,
        }
    }
}

/// A spatial direction for moving keyboard focus between panes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FocusDir {
    Left,
    Right,
    Up,
    Down,
}

/// A leaf's relative rectangle within the layout ([0,1] in both axes).
#[derive(Clone, Copy, Debug)]
struct Rect {
    x: f32,
    y: f32,
    w: f32,
    h: f32,
}

impl Rect {
    fn cx(&self) -> f32 {
        self.x + self.w / 2.0
    }
    fn cy(&self) -> f32 {
        self.y + self.h / 2.0
    }
}

fn ranges_overlap(a0: f32, a1: f32, b0: f32, b1: f32) -> bool {
    a0 < b1 && b0 < a1
}

fn collect_leaf_rects(node: &PaneNode, rect: Rect, from: Uuid, out: &mut Vec<(Rect, Uuid, bool)>) {
    match node {
        PaneNode::Leaf(ld) => {
            out.push((rect, ld.active_instance(), ld.tabs.contains(&from)));
        }
        PaneNode::Split {
            direction,
            sizes,
            children,
        } => {
            let total: f32 = sizes.iter().sum();
            let total = if total > 0.0 {
                total
            } else {
                children.len().max(1) as f32
            };
            let mut off = 0.0;
            for (i, child) in children.iter().enumerate() {
                let frac = sizes.get(i).copied().unwrap_or(1.0) / total;
                let child_rect = match direction {
                    SplitDirection::Horizontal => Rect {
                        x: rect.x + off * rect.w,
                        w: frac * rect.w,
                        ..rect
                    },
                    SplitDirection::Vertical => Rect {
                        y: rect.y + off * rect.h,
                        h: frac * rect.h,
                        ..rect
                    },
                };
                collect_leaf_rects(child, child_rect, from, out);
                off += frac;
            }
        }
    }
}

/// The active instance of the nearest pane in `dir` from the pane currently
/// holding `from`. Panes overlapping the source's perpendicular span win (by
/// nearest edge); otherwise the nearest by straight-line distance. `None` if
/// there's no pane that way (or `from` isn't in the tree).
pub fn focus_in_direction(root: &PaneNode, from: Uuid, dir: FocusDir) -> Option<Uuid> {
    let mut leaves: Vec<(Rect, Uuid, bool)> = Vec::new();
    collect_leaf_rects(
        root,
        Rect {
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
        },
        from,
        &mut leaves,
    );
    let src = leaves.iter().find(|(_, _, is_src)| *is_src)?.0;

    let mut best_overlap: Option<(Uuid, f32)> = None; // (instance, primary distance)
    let mut best_any: Option<(Uuid, f32)> = None; // (instance, squared distance)
    for (rect, inst, is_src) in &leaves {
        if *is_src {
            continue;
        }
        let in_dir = match dir {
            FocusDir::Left => rect.cx() < src.cx() - 1e-4,
            FocusDir::Right => rect.cx() > src.cx() + 1e-4,
            FocusDir::Up => rect.cy() < src.cy() - 1e-4,
            FocusDir::Down => rect.cy() > src.cy() + 1e-4,
        };
        if !in_dir {
            continue;
        }
        let (primary, overlaps) = match dir {
            FocusDir::Left | FocusDir::Right => (
                (rect.cx() - src.cx()).abs(),
                ranges_overlap(src.y, src.y + src.h, rect.y, rect.y + rect.h),
            ),
            FocusDir::Up | FocusDir::Down => (
                (rect.cy() - src.cy()).abs(),
                ranges_overlap(src.x, src.x + src.w, rect.x, rect.x + rect.w),
            ),
        };
        if overlaps {
            if best_overlap.is_none_or(|(_, s)| primary < s) {
                best_overlap = Some((*inst, primary));
            }
        } else {
            let (dx, dy) = (rect.cx() - src.cx(), rect.cy() - src.cy());
            let euclid = dx * dx + dy * dy;
            if best_any.is_none_or(|(_, s)| euclid < s) {
                best_any = Some((*inst, euclid));
            }
        }
    }
    best_overlap.or(best_any).map(|(inst, _)| inst)
}

/// A tabbed pane: a non-empty, ordered list of instance `tabs` plus the index of
/// the currently active (visible/focused) one. Invariants, upheld by every
/// mutating function here: `tabs` is never empty and `active < tabs.len()`.
///
/// `Serialize` is derived (emits `{"tabs":[…],"active":N}`); `Deserialize` is
/// hand-written so legacy single-instance leaves (`{"instance":"<uuid>"}` from
/// before tabs existed) still load — see the impl below.
#[derive(Clone, Debug, PartialEq, Serialize)]
pub struct LeafData {
    pub tabs: Vec<Uuid>,
    pub active: usize,
}

impl LeafData {
    fn new(instance: Uuid) -> Self {
        Self {
            tabs: vec![instance],
            active: 0,
        }
    }

    /// The currently active instance (always valid given the invariants).
    pub fn active_instance(&self) -> Uuid {
        self.tabs[self.active.min(self.tabs.len().saturating_sub(1))]
    }
}

impl<'de> Deserialize<'de> for LeafData {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        use serde::de::{self, IgnoredAny, MapAccess, Visitor};
        use std::fmt;

        struct LeafVisitor;
        impl<'de> Visitor<'de> for LeafVisitor {
            type Value = LeafData;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a leaf pane: a 'tabs' array (or legacy 'instance' uuid)")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<LeafData, A::Error> {
                let mut tabs: Option<Vec<Uuid>> = None;
                let mut instance: Option<Uuid> = None;
                let mut active: usize = 0;
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "tabs" => tabs = Some(map.next_value()?),
                        "instance" => instance = Some(map.next_value()?),
                        "active" => active = map.next_value()?,
                        // Ignore unknowns — including the enum's "kind" tag, which
                        // serde leaves in the buffered map for newtype variants.
                        _ => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                let tabs = match (tabs, instance) {
                    (Some(t), _) if !t.is_empty() => t,
                    (_, Some(id)) => vec![id],
                    (Some(_empty), _) => return Err(de::Error::custom("leaf 'tabs' is empty")),
                    (None, None) => {
                        return Err(de::Error::custom("leaf needs 'tabs' or 'instance'"));
                    }
                };
                let active = active.min(tabs.len() - 1);
                Ok(LeafData { tabs, active })
            }
        }
        d.deserialize_map(LeafVisitor)
    }
}

/// A node in a project's pane layout.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum PaneNode {
    /// A single pane: a tab group of one or more agent instances.
    Leaf(LeafData),
    /// A split containing two or more children with relative `sizes`
    /// (`sizes.len() == children.len()`).
    Split {
        direction: SplitDirection,
        sizes: Vec<f32>,
        children: Vec<PaneNode>,
    },
}

impl PaneNode {
    pub fn leaf(instance: Uuid) -> Self {
        PaneNode::Leaf(LeafData::new(instance))
    }

    /// If this node is a leaf, its tabs and active index; otherwise `None`.
    pub fn tabs(&self) -> Option<(&[Uuid], usize)> {
        match self {
            PaneNode::Leaf(ld) => Some((&ld.tabs, ld.active)),
            PaneNode::Split { .. } => None,
        }
    }

    /// All instance ids in this subtree, in reading order (every tab of every
    /// leaf). Drives terminal spawning and the FocusNext/Prev cycle.
    pub fn collect_instances(&self) -> Vec<Uuid> {
        let mut out = Vec::new();
        self.collect_into(&mut out);
        out
    }

    fn collect_into(&self, out: &mut Vec<Uuid>) {
        match self {
            PaneNode::Leaf(ld) => out.extend_from_slice(&ld.tabs),
            PaneNode::Split { children, .. } => {
                for c in children {
                    c.collect_into(out);
                }
            }
        }
    }

    /// Stable key for a split node: its descendant instance ids, simple-joined.
    /// The renderer uses this as the split's element id, so size persistence can
    /// match a resize event back to the right split.
    pub fn split_key(&self) -> String {
        self.collect_instances()
            .iter()
            .map(|id| id.simple().to_string())
            .collect::<Vec<_>>()
            .join("-")
    }

    /// The first instance in reading order (the first tab of the first leaf). A
    /// stable anchor — it does not change as the user switches tabs.
    pub fn first_instance(&self) -> Option<Uuid> {
        match self {
            PaneNode::Leaf(ld) => ld.tabs.first().copied(),
            PaneNode::Split { children, .. } => children.first().and_then(|c| c.first_instance()),
        }
    }

    /// The instance in the last (rightmost/bottom-most) leaf — for appending a new
    /// pane at the end of the layout.
    pub fn last_instance(&self) -> Option<Uuid> {
        match self {
            PaneNode::Leaf(ld) => ld.tabs.last().copied(),
            PaneNode::Split { children, .. } => children.last().and_then(|c| c.last_instance()),
        }
    }

    /// Which instance the leaf containing `removing` would activate if `removing`
    /// were closed, or `None` if that leaf would disappear (it was the last tab)
    /// or `removing` isn't present. Lets the caller re-target focus before/after
    /// a close. Mirrors the active-index fixup in [`remove`].
    pub fn surviving_active_after_remove(&self, removing: Uuid) -> Option<Uuid> {
        let path = self.find_path(removing)?;
        let PaneNode::Leaf(ld) = self.get_at_path(&path)? else {
            return None;
        };
        if ld.tabs.len() <= 1 {
            return None;
        }
        let idx = ld.tabs.iter().position(|&id| id == removing)?;
        let new_active = if idx < ld.active {
            ld.active - 1
        } else if idx == ld.active {
            ld.active.min(ld.tabs.len() - 2)
        } else {
            ld.active
        };
        ld.tabs
            .iter()
            .copied()
            .filter(|&id| id != removing)
            .nth(new_active)
    }

    /// Find an adjacent pane to the leaf holding `instance`, to remember where a
    /// popped-out terminal sat so it can re-dock in place. Returns `(neighbor,
    /// direction, before)` where `before` is true when `instance` sat *before*
    /// the neighbor (so re-dock must insert on that side). `None` if `instance`
    /// is the whole tree (no neighbor).
    pub fn neighbor_of(&self, instance: Uuid) -> Option<(Uuid, SplitDirection, bool)> {
        if let PaneNode::Split {
            direction,
            children,
            ..
        } = self
        {
            // Is `instance` a direct leaf child of this split?
            if let Some(ix) = children
                .iter()
                .position(|c| matches!(c, PaneNode::Leaf(ld) if ld.tabs.contains(&instance)))
            {
                // Prefer the previous sibling (instance sat after it → before=false);
                // otherwise the next sibling (instance sat before it → before=true).
                if let Some(left) = ix.checked_sub(1)
                    && let Some(anchor) = children[left].first_instance()
                {
                    return Some((anchor, *direction, false));
                }
                if ix + 1 < children.len()
                    && let Some(anchor) = children[ix + 1].first_instance()
                {
                    return Some((anchor, *direction, true));
                }
            }
            // Otherwise recurse.
            for c in children {
                if let Some(found) = c.neighbor_of(instance) {
                    return Some(found);
                }
            }
        }
        None
    }

    /// Path (child indices) from this node to the leaf holding `instance` (as any
    /// of its tabs).
    pub fn find_path(&self, instance: Uuid) -> Option<Vec<usize>> {
        let mut path = Vec::new();
        if self.find_path_into(instance, &mut path) {
            Some(path)
        } else {
            None
        }
    }

    fn find_path_into(&self, instance: Uuid, path: &mut Vec<usize>) -> bool {
        match self {
            PaneNode::Leaf(ld) => ld.tabs.contains(&instance),
            PaneNode::Split { children, .. } => {
                for (i, child) in children.iter().enumerate() {
                    path.push(i);
                    if child.find_path_into(instance, path) {
                        return true;
                    }
                    path.pop();
                }
                false
            }
        }
    }

    pub fn get_at_path(&self, path: &[usize]) -> Option<&PaneNode> {
        match path.split_first() {
            None => Some(self),
            Some((&i, rest)) => match self {
                PaneNode::Leaf(..) => None,
                PaneNode::Split { children, .. } => children.get(i)?.get_at_path(rest),
            },
        }
    }

    pub fn get_at_path_mut(&mut self, path: &[usize]) -> Option<&mut PaneNode> {
        match path.split_first() {
            None => Some(self),
            Some((&i, rest)) => match self {
                PaneNode::Leaf(..) => None,
                PaneNode::Split { children, .. } => children.get_mut(i)?.get_at_path_mut(rest),
            },
        }
    }

    /// Remove the child at `path`, collapsing a split that's left with one child.
    /// Returns the removed node, or `None` if the path is invalid/empty.
    pub fn remove_at_path(&mut self, path: &[usize]) -> Option<PaneNode> {
        let (&idx, parent_path) = path.split_last()?;
        let parent = self.get_at_path_mut(parent_path)?;
        match parent {
            PaneNode::Leaf(..) => None,
            PaneNode::Split {
                children, sizes, ..
            } => {
                if idx >= children.len() {
                    return None;
                }
                let removed = children.remove(idx);
                if idx < sizes.len() {
                    sizes.remove(idx);
                }
                if children.len() == 1 {
                    let only = children.remove(0);
                    *parent = only;
                }
                Some(removed)
            }
        }
    }

    /// Normalize in place: recurse, fix `sizes` length, unwrap single-child
    /// splits, and flatten nested same-direction splits (merging sizes). Leaves
    /// (tab groups) are left untouched — their non-empty invariant is upheld by
    /// the mutating functions, not here.
    pub fn normalize(&mut self) {
        if let PaneNode::Split { children, .. } = self {
            for c in children.iter_mut() {
                c.normalize();
            }
        }

        // Keep sizes and children the same length.
        if let PaneNode::Split {
            sizes, children, ..
        } = self
            && sizes.len() != children.len()
        {
            *sizes = vec![1.0; children.len()];
        }

        // Unwrap a split with a single child, then re-normalize the result.
        let single_child = matches!(self, PaneNode::Split { children, .. } if children.len() == 1);
        if single_child {
            if let PaneNode::Split { children, .. } = self {
                let only = children.remove(0);
                *self = only;
            }
            self.normalize();
            return;
        }

        // Flatten nested splits with the same direction.
        if let PaneNode::Split {
            direction,
            sizes,
            children,
        } = self
        {
            let dir = *direction;
            let has_same_dir = children
                .iter()
                .any(|c| matches!(c, PaneNode::Split { direction: d, .. } if *d == dir));
            if has_same_dir {
                let mut new_children = Vec::new();
                let mut new_sizes = Vec::new();
                for (i, child) in std::mem::take(children).into_iter().enumerate() {
                    let parent_size = sizes.get(i).copied().unwrap_or(1.0);
                    match child {
                        PaneNode::Split {
                            direction: cd,
                            sizes: cs,
                            children: gc,
                        } if cd == dir => {
                            let total: f32 = cs.iter().sum();
                            let total = if total > 0.0 {
                                total
                            } else {
                                gc.len().max(1) as f32
                            };
                            for (j, g) in gc.into_iter().enumerate() {
                                new_children.push(g);
                                new_sizes
                                    .push(parent_size * cs.get(j).copied().unwrap_or(1.0) / total);
                            }
                        }
                        other => {
                            new_children.push(other);
                            new_sizes.push(parent_size);
                        }
                    }
                }
                *children = new_children;
                *sizes = new_sizes;
            }
        }
    }
}

/// Split the pane holding `target` in two, placing `new_instance` alongside it.
/// The new pane takes half of the target's space. Returns `false` if `target`
/// isn't present. Splitting a multi-tab pane keeps the whole tab group together
/// as one child and adds a fresh single-tab pane beside it.
pub fn split(
    tree: &mut Option<PaneNode>,
    target: Uuid,
    direction: SplitDirection,
    new_instance: Uuid,
) -> bool {
    split_beside(tree, target, direction, new_instance, false)
}

/// Split `target`'s pane, inserting `new_instance` before or after it.
/// `before == true` places the new pane ahead of the target (left/top).
pub fn split_beside(
    tree: &mut Option<PaneNode>,
    target: Uuid,
    direction: SplitDirection,
    new_instance: Uuid,
    before: bool,
) -> bool {
    let Some(root) = tree.as_mut() else {
        return false;
    };
    let Some(path) = root.find_path(target) else {
        return false;
    };
    let Some(node) = root.get_at_path_mut(&path) else {
        return false;
    };
    let old = node.clone();
    let children = if before {
        vec![PaneNode::leaf(new_instance), old]
    } else {
        vec![old, PaneNode::leaf(new_instance)]
    };
    *node = PaneNode::Split {
        direction,
        sizes: vec![1.0, 1.0],
        children,
    };
    root.normalize();
    true
}

/// Pull the single tab `dragged` out of its current pane and place it as a new
/// pane split beside `target`'s pane (Zed-style "drag a tab to an edge").
/// `before == true` puts the new pane ahead of the target (left/top).
///
/// No-op (returns false) if `dragged == target`, or `dragged` is already the sole
/// tab of `target`'s pane (it's its own pane — nothing to pull out).
pub fn move_into_split(
    tree: &mut Option<PaneNode>,
    dragged: Uuid,
    target: Uuid,
    direction: SplitDirection,
    before: bool,
) -> bool {
    if dragged == target {
        return false;
    }
    let (Some(pd), Some(pt)) = (
        tree.as_ref().and_then(|r| r.find_path(dragged)),
        tree.as_ref().and_then(|r| r.find_path(target)),
    ) else {
        return false;
    };
    // Same leaf: only valid when there are ≥2 tabs (target survives the remove).
    // A sole-tab leaf is already its own pane, so there's nothing to do.
    if pd == pt {
        let sole = tree
            .as_ref()
            .and_then(|r| r.get_at_path(&pd))
            .and_then(|n| n.tabs())
            .map(|(tabs, _)| tabs.len() == 1)
            .unwrap_or(true);
        if sole {
            return false;
        }
    }
    if !remove(tree, dragged) {
        return false;
    }
    // `target` is guaranteed to survive: it's either in a different leaf, or in
    // the same leaf that still holds ≥1 tab after the removal.
    debug_assert!(
        tree.as_ref().and_then(|r| r.find_path(target)).is_some(),
        "move_into_split: target vanished after remove",
    );
    split_beside(tree, target, direction, dragged, before)
}

/// Move the whole pane (every tab + active index) holding `src_anchor` to a new
/// split beside `target`'s pane. Like [`split_beside`] but relocates an existing
/// leaf instead of creating a fresh one. No-op if the two are the same pane.
pub fn move_pane_beside(
    tree: &mut Option<PaneNode>,
    src_anchor: Uuid,
    target: Uuid,
    direction: SplitDirection,
    before: bool,
) -> bool {
    if src_anchor == target {
        return false;
    }
    let (Some(src_path), Some(tgt_path)) = (
        tree.as_ref().and_then(|r| r.find_path(src_anchor)),
        tree.as_ref().and_then(|r| r.find_path(target)),
    ) else {
        return false;
    };
    if src_path == tgt_path {
        return false; // same leaf
    }
    // Snapshot the source leaf before mutating the tree.
    let src_leaf = match tree.as_ref().and_then(|r| r.get_at_path(&src_path)) {
        Some(node @ PaneNode::Leaf(_)) => node.clone(),
        _ => return false,
    };
    // Detach the source leaf node (collapses its parent split if left singular).
    {
        let Some(root) = tree.as_mut() else {
            return false;
        };
        root.remove_at_path(&src_path);
        root.normalize();
    }
    // Re-find `target`: the removal + normalize may have shifted its path.
    let Some(root) = tree.as_mut() else {
        return false;
    };
    let Some(tgt_path) = root.find_path(target) else {
        return false;
    };
    let Some(node) = root.get_at_path_mut(&tgt_path) else {
        return false;
    };
    let old = node.clone();
    let children = if before {
        vec![src_leaf, old]
    } else {
        vec![old, src_leaf]
    };
    *node = PaneNode::Split {
        direction,
        sizes: vec![1.0, 1.0],
        children,
    };
    root.normalize();
    true
}

/// Add `new_instance` as the last tab of the pane holding `target`, and make it
/// active. Returns `false` if `target` isn't present or `new_instance` is already
/// a tab in that group.
pub fn add_tab(tree: &mut Option<PaneNode>, target: Uuid, new_instance: Uuid) -> bool {
    add_tab_at(tree, target, new_instance, usize::MAX)
}

/// Insert `new_instance` as a tab at `index` (clamped to `0..=len`) in the pane
/// holding `target`, and make it active. Returns `false` if `target` isn't
/// present or `new_instance` is already somewhere in the tree.
pub fn add_tab_at(
    tree: &mut Option<PaneNode>,
    target: Uuid,
    new_instance: Uuid,
    index: usize,
) -> bool {
    let Some(root) = tree.as_mut() else {
        return false;
    };
    if root.find_path(new_instance).is_some() {
        return false; // never allow the same instance twice in the tree
    }
    let Some(path) = root.find_path(target) else {
        return false;
    };
    let Some(PaneNode::Leaf(ld)) = root.get_at_path_mut(&path) else {
        return false;
    };
    let at = index.min(ld.tabs.len());
    ld.tabs.insert(at, new_instance);
    ld.active = at;
    true
}

/// Replace the tab order of the pane holding `anchor` with `ordered` (which must
/// be a permutation of that leaf's current tabs), keeping the same active tab.
/// Returns `false` if `anchor` isn't in a leaf or `ordered` isn't a permutation.
pub fn set_tab_order(tree: &mut Option<PaneNode>, anchor: Uuid, ordered: &[Uuid]) -> bool {
    let Some(root) = tree.as_mut() else {
        return false;
    };
    let Some(path) = root.find_path(anchor) else {
        return false;
    };
    let Some(PaneNode::Leaf(ld)) = root.get_at_path_mut(&path) else {
        return false;
    };
    // `ordered` must be exactly the same multiset (and, since tabs are unique,
    // set) of instances. Compare sorted copies.
    let mut a = ld.tabs.clone();
    let mut b = ordered.to_vec();
    a.sort();
    b.sort();
    if a != b {
        return false;
    }
    let was_active = ld.tabs[ld.active.min(ld.tabs.len() - 1)];
    ld.tabs = ordered.to_vec();
    ld.active = ld.tabs.iter().position(|&id| id == was_active).unwrap_or(0);
    true
}

/// Make `instance` the active tab of its pane. Returns `false` if not present.
pub fn set_active_tab(tree: &mut Option<PaneNode>, instance: Uuid) -> bool {
    let Some(root) = tree.as_mut() else {
        return false;
    };
    let Some(path) = root.find_path(instance) else {
        return false;
    };
    let Some(PaneNode::Leaf(ld)) = root.get_at_path_mut(&path) else {
        return false;
    };
    let Some(idx) = ld.tabs.iter().position(|&id| id == instance) else {
        return false;
    };
    ld.active = idx;
    true
}

/// Move `dragged` out of wherever it sits and append it as the active tab of the
/// pane holding `target` (drag-to-tabify). No-op (`false`) if `dragged == target`
/// or both are already in the same pane; otherwise see [`move_tab_to`].
pub fn move_into_tabs(tree: &mut Option<PaneNode>, dragged: Uuid, target: Uuid) -> bool {
    if dragged == target {
        return false;
    }
    let same_pane = matches!(
        (
            tree.as_ref().and_then(|r| r.find_path(dragged)),
            tree.as_ref().and_then(|r| r.find_path(target)),
        ),
        (Some(pd), Some(pt)) if pd == pt
    );
    if same_pane {
        return false;
    }
    move_tab_to(tree, dragged, target, usize::MAX)
}

/// Move `dragged` to position `index` in the pane holding `target_anchor`, and
/// make it active. Two modes:
/// - **Same pane** (`dragged` and `target_anchor` share a leaf): reorder within
///   the leaf. `index` is the desired final position (clamped to `0..=len-1`); a
///   move to the same slot is a no-op (`false`).
/// - **Different pane**: detach `dragged` (collapsing an emptied source) and
///   insert it at `index` in the target leaf.
///
/// Returns `false` if `dragged`/`target_anchor` is absent or the move is a no-op.
pub fn move_tab_to(
    tree: &mut Option<PaneNode>,
    dragged: Uuid,
    target_anchor: Uuid,
    index: usize,
) -> bool {
    let (Some(pd), Some(pt)) = (
        tree.as_ref().and_then(|r| r.find_path(dragged)),
        tree.as_ref().and_then(|r| r.find_path(target_anchor)),
    ) else {
        return false;
    };
    if pd == pt {
        // Same-leaf reorder. Insert position == desired final index; removing the
        // element first then inserting at the clamped index lands it there.
        let Some(root) = tree.as_mut() else {
            return false;
        };
        let Some(PaneNode::Leaf(ld)) = root.get_at_path_mut(&pd) else {
            return false;
        };
        let Some(src) = ld.tabs.iter().position(|&id| id == dragged) else {
            return false;
        };
        let dst = index.min(ld.tabs.len() - 1);
        if dst == src {
            return false;
        }
        ld.tabs.remove(src);
        ld.tabs.insert(dst, dragged);
        ld.active = dst;
        return true;
    }
    // Cross-leaf: detach (fixes source active / collapses empty source), then
    // re-find the target (a collapse can shift paths) and insert at `index`.
    if !remove(tree, dragged) {
        return false;
    }
    add_tab_at(tree, target_anchor, dragged, index)
}

/// Remove `target` from the tree. If it's one of several tabs in its pane, only
/// that tab is removed (and the pane's active index fixed up); if it's the last
/// tab, the pane is removed and the tree collapses. If it was the last pane, the
/// tree becomes empty (`None`). Returns `false` if `target` is absent.
pub fn remove(tree: &mut Option<PaneNode>, target: Uuid) -> bool {
    let Some(path) = tree.as_ref().and_then(|r| r.find_path(target)) else {
        return false;
    };
    // Remove the tab from its leaf; learn whether the leaf is now empty.
    let emptied = {
        let Some(root) = tree.as_mut() else {
            return false;
        };
        let Some(PaneNode::Leaf(ld)) = root.get_at_path_mut(&path) else {
            return false;
        };
        let Some(idx) = ld.tabs.iter().position(|&id| id == target) else {
            return false;
        };
        ld.tabs.remove(idx);
        if ld.tabs.is_empty() {
            true
        } else {
            if idx < ld.active {
                ld.active -= 1;
            } else if idx == ld.active {
                ld.active = ld.active.min(ld.tabs.len() - 1);
            }
            false
        }
    };
    if emptied {
        if path.is_empty() {
            *tree = None;
        } else if let Some(root) = tree.as_mut() {
            root.remove_at_path(&path);
            root.normalize();
        }
    }
    true
}

/// Swap the positions of two instances wherever they sit (including across tab
/// groups). Returns true only if both instances were found.
pub fn swap_instances(tree: &mut Option<PaneNode>, a: Uuid, b: Uuid) -> bool {
    if a == b {
        return false;
    }
    let Some(root) = tree.as_mut() else {
        return false;
    };
    let mut found_a = false;
    let mut found_b = false;
    swap_walk(root, a, b, &mut found_a, &mut found_b);
    found_a && found_b
}

fn swap_walk(node: &mut PaneNode, a: Uuid, b: Uuid, found_a: &mut bool, found_b: &mut bool) {
    match node {
        PaneNode::Leaf(ld) => {
            for id in ld.tabs.iter_mut() {
                if *id == a {
                    *id = b;
                    *found_a = true;
                } else if *id == b {
                    *id = a;
                    *found_b = true;
                }
            }
        }
        PaneNode::Split { children, .. } => {
            for child in children.iter_mut() {
                swap_walk(child, a, b, found_a, found_b);
            }
        }
    }
}

/// Swap two whole panes (the leaves holding `a` and `b`) in the layout — every
/// tab moves with its pane. The split structure is unchanged; only the two
/// leaves' contents trade places. Returns false if `a`/`b` share a pane or
/// either is absent. Use this for "drag a pane onto another to swap them".
pub fn swap_panes(tree: &mut Option<PaneNode>, a: Uuid, b: Uuid) -> bool {
    if a == b {
        return false;
    }
    let Some(root) = tree.as_mut() else {
        return false;
    };
    let (Some(pa), Some(pb)) = (root.find_path(a), root.find_path(b)) else {
        return false;
    };
    if pa == pb {
        return false; // same pane
    }
    let (Some(PaneNode::Leaf(da)), Some(PaneNode::Leaf(db))) = (
        root.get_at_path(&pa).cloned(),
        root.get_at_path(&pb).cloned(),
    ) else {
        return false;
    };
    if let Some(PaneNode::Leaf(slot)) = root.get_at_path_mut(&pa) {
        *slot = db;
    }
    if let Some(PaneNode::Leaf(slot)) = root.get_at_path_mut(&pb) {
        *slot = da;
    }
    true
}

/// Record the (pixel) sizes of the split identified by `key` (see
/// [`PaneNode::split_key`]) so the layout restores at those proportions.
/// Returns true if a matching split was found.
pub fn set_split_sizes(tree: &mut Option<PaneNode>, key: &str, sizes: &[f32]) -> bool {
    fn walk(node: &mut PaneNode, key: &str, sizes: &[f32]) -> bool {
        match node {
            PaneNode::Leaf(..) => false,
            PaneNode::Split {
                sizes: node_sizes,
                children,
                ..
            } => {
                let this_key: String = children
                    .iter()
                    .flat_map(|c| c.collect_instances())
                    .map(|id| id.simple().to_string())
                    .collect::<Vec<_>>()
                    .join("-");
                if this_key == key {
                    if sizes.len() == children.len() {
                        *node_sizes = sizes.to_vec();
                    }
                    return true;
                }
                children.iter_mut().any(|c| walk(c, key, sizes))
            }
        }
    }
    match tree {
        Some(root) => walk(root, key, sizes),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id() -> Uuid {
        Uuid::new_v4()
    }

    /// Build a multi-tab leaf for tests.
    fn tabs_leaf(tabs: Vec<Uuid>, active: usize) -> PaneNode {
        PaneNode::Leaf(LeafData { tabs, active })
    }

    /// Tabs of the leaf holding `instance`, in order.
    fn leaf_tabs(tree: &Option<PaneNode>, instance: Uuid) -> Vec<Uuid> {
        let root = tree.as_ref().unwrap();
        let path = root.find_path(instance).unwrap();
        root.get_at_path(&path).unwrap().tabs().unwrap().0.to_vec()
    }

    #[test]
    fn last_instance_returns_rightmost_leaf() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        assert!(split(&mut tree, a, SplitDirection::Horizontal, b)); // [a | b]
        assert!(split(&mut tree, b, SplitDirection::Horizontal, c)); // [a | [b | c]]
        let root = tree.as_ref().unwrap();
        assert_eq!(root.first_instance(), Some(a));
        assert_eq!(root.last_instance(), Some(c));
    }

    // ---- move_into_split -------------------------------------------------

    #[test]
    fn move_into_split_pulls_tab_from_two_tab_leaf() {
        let (a, b) = (id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b], 0));
        // Pull b out to the right of a.
        assert!(move_into_split(
            &mut tree,
            b,
            a,
            SplitDirection::Horizontal,
            false
        ));
        match tree.as_ref().unwrap() {
            PaneNode::Split {
                direction,
                children,
                ..
            } => {
                assert_eq!(*direction, SplitDirection::Horizontal);
                assert_eq!(children.len(), 2);
                assert_eq!(children[0].tabs().unwrap().0.to_vec(), vec![a]);
                assert_eq!(children[1].tabs().unwrap().0.to_vec(), vec![b]);
            }
            _ => panic!("expected a split"),
        }
    }

    #[test]
    fn move_into_split_from_three_tab_leaf_keeps_group() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 0));
        // Pull c out above a (Vertical, before).
        assert!(move_into_split(
            &mut tree,
            c,
            a,
            SplitDirection::Vertical,
            true
        ));
        let root = tree.as_ref().unwrap();
        assert_ne!(root.find_path(a), root.find_path(c));
        // a and b stay together; c is on its own.
        assert_eq!(leaf_tabs(&tree, a), vec![a, b]);
        assert_eq!(leaf_tabs(&tree, c), vec![c]);
    }

    #[test]
    fn move_into_split_from_other_pane() {
        let (a, b) = (id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        // Pull a (its own pane) to the left of b.
        assert!(move_into_split(
            &mut tree,
            a,
            b,
            SplitDirection::Horizontal,
            true
        ));
        let root = tree.as_ref().unwrap();
        assert_eq!(root.collect_instances(), vec![a, b]);
        assert_ne!(root.find_path(a), root.find_path(b));
    }

    #[test]
    fn move_into_split_same_instance_noop() {
        let a = id();
        let mut tree = Some(PaneNode::leaf(a));
        assert!(!move_into_split(
            &mut tree,
            a,
            a,
            SplitDirection::Horizontal,
            false
        ));
        assert_eq!(tree, Some(PaneNode::leaf(a)));
    }

    #[test]
    fn move_into_split_path_revalidation_three_pane() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        split(&mut tree, b, SplitDirection::Horizontal, c);
        // Tree: [a | b | c]. Pull a to the left of c.
        assert!(move_into_split(
            &mut tree,
            a,
            c,
            SplitDirection::Horizontal,
            true
        ));
        let root = tree.as_ref().unwrap();
        // remove(a) → [b | c]; wrap c with a-before → normalize → [b, a, c].
        assert_eq!(root.collect_instances(), vec![b, a, c]);
        assert_ne!(root.find_path(a), root.find_path(c));
    }

    // ---- move_pane_beside ------------------------------------------------

    #[test]
    fn move_pane_beside_relocates_pane() {
        let (a, b) = (id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        // Move pane(a) to the right of pane(b).
        assert!(move_pane_beside(
            &mut tree,
            a,
            b,
            SplitDirection::Horizontal,
            false
        ));
        assert_eq!(tree.as_ref().unwrap().collect_instances(), vec![b, a]);
    }

    #[test]
    fn move_pane_beside_preserves_tabs_and_active() {
        let (a, b, c, d) = (id(), id(), id(), id());
        let mut tree = Some(PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![tabs_leaf(vec![a, b, c], 2), PaneNode::leaf(d)],
        });
        // Move the abc pane (anchor a) below d.
        assert!(move_pane_beside(
            &mut tree,
            a,
            d,
            SplitDirection::Vertical,
            false
        ));
        let root = tree.as_ref().unwrap();
        let path = root.find_path(a).unwrap();
        match root.get_at_path(&path).unwrap() {
            PaneNode::Leaf(ld) => {
                assert_eq!(ld.tabs, vec![a, b, c]);
                assert_eq!(ld.active, 2);
            }
            _ => panic!("expected a leaf"),
        }
    }

    #[test]
    fn move_pane_beside_same_pane_noop() {
        let (a, b) = (id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b], 0));
        assert!(!move_pane_beside(
            &mut tree,
            a,
            b,
            SplitDirection::Horizontal,
            false
        ));
    }

    #[test]
    fn move_pane_beside_missing_is_noop() {
        let (a, b) = (id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        assert!(!move_pane_beside(
            &mut tree,
            id(),
            b,
            SplitDirection::Horizontal,
            false
        ));
        assert!(!move_pane_beside(
            &mut tree,
            a,
            id(),
            SplitDirection::Horizontal,
            false
        ));
    }

    #[test]
    fn move_pane_beside_path_revalidation_three_pane() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        split(&mut tree, b, SplitDirection::Horizontal, c);
        // Tree: [a | b | c]. Move pane(b) to the left of c.
        assert!(move_pane_beside(
            &mut tree,
            b,
            c,
            SplitDirection::Horizontal,
            true
        ));
        assert_eq!(tree.as_ref().unwrap().collect_instances(), vec![a, b, c]);
    }

    #[test]
    fn move_pane_beside_first_to_right_of_last() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        split(&mut tree, b, SplitDirection::Horizontal, c);
        // Move pane(a) to the right of c.
        assert!(move_pane_beside(
            &mut tree,
            a,
            c,
            SplitDirection::Horizontal,
            false
        ));
        assert_eq!(tree.as_ref().unwrap().collect_instances(), vec![b, c, a]);
    }

    #[test]
    fn neighbor_of_finds_adjacent_pane_and_direction() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        assert!(split(&mut tree, a, SplitDirection::Horizontal, b));
        assert!(split(&mut tree, b, SplitDirection::Horizontal, c));
        let root = tree.as_ref().unwrap();
        // c's previous sibling is b (c sat after b → before=false).
        assert_eq!(
            root.neighbor_of(c),
            Some((b, SplitDirection::Horizontal, false))
        );
        // a is first; its neighbor is the next sibling and a sat before it.
        let (_, _, a_before) = root.neighbor_of(a).expect("a has a neighbor");
        assert!(a_before);
        // Unknown instance has no neighbor.
        assert_eq!(root.neighbor_of(id()), None);
    }

    #[test]
    fn split_beside_inserts_on_the_requested_side() {
        let (a, b) = (id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        assert!(split_beside(
            &mut tree,
            a,
            SplitDirection::Horizontal,
            b,
            true
        ));
        // b inserted before a.
        assert_eq!(tree.as_ref().unwrap().collect_instances(), vec![b, a]);
    }

    #[test]
    fn set_split_sizes_records_sizes_by_key() {
        let (a, b) = (id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        assert!(split(&mut tree, a, SplitDirection::Horizontal, b));
        let key = tree.as_ref().unwrap().split_key();
        assert!(set_split_sizes(&mut tree, &key, &[700.0, 300.0]));
        if let Some(PaneNode::Split { sizes, .. }) = &tree {
            assert_eq!(sizes, &vec![700.0, 300.0]);
        } else {
            panic!("expected a split");
        }
        // Unknown key + wrong-length sizes are no-ops.
        assert!(!set_split_sizes(&mut tree, "nope", &[1.0, 1.0]));
    }

    #[test]
    fn swap_instances_swaps_two_leaves() {
        let (a, b) = (id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        assert!(split(&mut tree, a, SplitDirection::Horizontal, b));
        assert_eq!(tree.as_ref().unwrap().collect_instances(), vec![a, b]);
        assert!(swap_instances(&mut tree, a, b));
        assert_eq!(tree.as_ref().unwrap().collect_instances(), vec![b, a]);
        assert!(!swap_instances(&mut tree, a, id()));
    }

    #[test]
    fn split_a_leaf_creates_a_two_child_split() {
        let a = id();
        let b = id();
        let mut tree = Some(PaneNode::leaf(a));
        assert!(split(&mut tree, a, SplitDirection::Horizontal, b));
        match tree.unwrap() {
            PaneNode::Split {
                direction,
                children,
                sizes,
            } => {
                assert_eq!(direction, SplitDirection::Horizontal);
                assert_eq!(children.len(), 2);
                assert_eq!(sizes.len(), 2);
            }
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn same_direction_splits_flatten_to_n_way() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        split(&mut tree, b, SplitDirection::Horizontal, c);
        let root = tree.unwrap();
        assert_eq!(root.collect_instances(), vec![a, b, c]);
        match root {
            PaneNode::Split {
                children, sizes, ..
            } => {
                assert_eq!(children.len(), 3, "should flatten into one 3-way split");
                assert_eq!(sizes.len(), 3);
            }
            _ => panic!("expected split"),
        }
    }

    #[test]
    fn cross_direction_splits_nest() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        split(&mut tree, b, SplitDirection::Vertical, c);
        let root = tree.unwrap();
        match root {
            PaneNode::Split {
                direction: SplitDirection::Horizontal,
                children,
                ..
            } => {
                assert_eq!(children.len(), 2);
                assert!(matches!(
                    &children[1],
                    PaneNode::Split {
                        direction: SplitDirection::Vertical,
                        ..
                    }
                ));
            }
            _ => panic!("expected nested split"),
        }
    }

    #[test]
    fn remove_collapses_two_child_split() {
        let (a, b) = (id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        assert!(remove(&mut tree, a));
        assert_eq!(tree, Some(PaneNode::leaf(b)));
    }

    #[test]
    fn remove_last_pane_empties_tree() {
        let a = id();
        let mut tree = Some(PaneNode::leaf(a));
        assert!(remove(&mut tree, a));
        assert_eq!(tree, None);
    }

    #[test]
    fn remove_middle_of_three_keeps_two() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        split(&mut tree, b, SplitDirection::Horizontal, c);
        assert!(remove(&mut tree, b));
        let root = tree.unwrap();
        assert_eq!(root.collect_instances(), vec![a, c]);
    }

    #[test]
    fn remove_absent_is_noop() {
        let a = id();
        let mut tree = Some(PaneNode::leaf(a));
        assert!(!remove(&mut tree, id()));
        assert_eq!(tree, Some(PaneNode::leaf(a)));
    }

    #[test]
    fn find_path_and_get() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        split(&mut tree, b, SplitDirection::Vertical, c);
        let root = tree.unwrap();
        let path = root.find_path(c).unwrap();
        assert_eq!(root.get_at_path(&path), Some(&PaneNode::leaf(c)));
    }

    #[test]
    fn serde_round_trip() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        split(&mut tree, b, SplitDirection::Vertical, c);
        let json = serde_json::to_string(&tree).unwrap();
        let back: Option<PaneNode> = serde_json::from_str(&json).unwrap();
        assert_eq!(back, tree);
    }

    // ---- tabs: serde backward/forward compatibility ----

    #[test]
    fn legacy_leaf_deserializes() {
        let u = Uuid::new_v4();
        let json = format!(r#"{{"kind":"leaf","instance":"{u}"}}"#);
        let node: PaneNode = serde_json::from_str(&json).unwrap();
        assert_eq!(
            node,
            PaneNode::Leaf(LeafData {
                tabs: vec![u],
                active: 0
            })
        );
    }

    #[test]
    fn legacy_split_of_leaves_deserializes() {
        let (a, b) = (id(), id());
        let json = format!(
            r#"{{"kind":"split","direction":"horizontal","sizes":[1.0,1.0],
                 "children":[
                   {{"kind":"leaf","instance":"{a}"}},
                   {{"kind":"leaf","instance":"{b}"}}]}}"#
        );
        let node: PaneNode = serde_json::from_str(&json).unwrap();
        assert_eq!(node.collect_instances(), vec![a, b]);
    }

    #[test]
    fn new_leaf_round_trips_with_multiple_tabs() {
        let (a, b) = (id(), id());
        let node = tabs_leaf(vec![a, b], 1);
        let json = serde_json::to_string(&node).unwrap();
        assert!(json.contains("\"tabs\""));
        let back: PaneNode = serde_json::from_str(&json).unwrap();
        assert_eq!(back, node);
    }

    #[test]
    fn new_leaf_active_defaults_to_zero() {
        let u = Uuid::new_v4();
        let json = format!(r#"{{"kind":"leaf","tabs":["{u}"]}}"#);
        let node: PaneNode = serde_json::from_str(&json).unwrap();
        assert_eq!(node, tabs_leaf(vec![u], 0));
    }

    #[test]
    fn out_of_range_active_is_clamped() {
        let u = Uuid::new_v4();
        let json = format!(r#"{{"kind":"leaf","tabs":["{u}"],"active":5}}"#);
        let node: PaneNode = serde_json::from_str(&json).unwrap();
        assert_eq!(node, tabs_leaf(vec![u], 0));
    }

    #[test]
    fn leaf_empty_tabs_is_error() {
        assert!(serde_json::from_str::<PaneNode>(r#"{"kind":"leaf","tabs":[]}"#).is_err());
    }

    #[test]
    fn leaf_missing_fields_is_error() {
        assert!(serde_json::from_str::<PaneNode>(r#"{"kind":"leaf"}"#).is_err());
    }

    // ---- tabs: mutation ----

    #[test]
    fn add_tab_appends_and_activates() {
        let (a, b) = (id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        assert!(add_tab(&mut tree, a, b));
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[a, b][..], 1)));
        assert_eq!(tree.as_ref().unwrap().collect_instances(), vec![a, b]);
    }

    #[test]
    fn add_tab_target_not_found_is_false() {
        let a = id();
        let mut tree = Some(PaneNode::leaf(a));
        assert!(!add_tab(&mut tree, id(), id()));
    }

    #[test]
    fn add_tab_into_a_pane_of_a_split() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        assert!(add_tab(&mut tree, b, c));
        // b's leaf now holds [b, c]; a's leaf untouched.
        let root = tree.as_ref().unwrap();
        let path = root.find_path(c).unwrap();
        assert_eq!(
            root.get_at_path(&path).unwrap().tabs(),
            Some((&[b, c][..], 1))
        );
        assert_eq!(root.collect_instances(), vec![a, b, c]);
    }

    #[test]
    fn add_tab_duplicate_is_noop() {
        let a = id();
        let mut tree = Some(PaneNode::leaf(a));
        assert!(!add_tab(&mut tree, a, a));
    }

    #[test]
    fn set_active_tab_updates_index() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 0));
        assert!(set_active_tab(&mut tree, b));
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[a, b, c][..], 1)));
        assert!(!set_active_tab(&mut tree, id()));
    }

    #[test]
    fn remove_tab_keeps_group() {
        let (a, b) = (id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b], 0));
        assert!(remove(&mut tree, a));
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[b][..], 0)));
    }

    #[test]
    fn remove_active_tab_clamps_active() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 2));
        assert!(remove(&mut tree, c));
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[a, b][..], 1)));
    }

    #[test]
    fn remove_middle_tab_shifts_active_down() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 2));
        assert!(remove(&mut tree, b));
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[a, c][..], 1)));
    }

    #[test]
    fn remove_last_tab_collapses_pane() {
        let (a, b, c) = (id(), id(), id());
        // split: [ leaf(a,b) | leaf(c) ]
        let mut tree = Some(PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![tabs_leaf(vec![a, b], 0), PaneNode::leaf(c)],
        });
        assert!(remove(&mut tree, a));
        assert!(remove(&mut tree, b)); // empties the first leaf
        assert_eq!(tree, Some(PaneNode::leaf(c)));
    }

    #[test]
    fn surviving_active_after_remove_picks_neighbor() {
        let (a, b, c) = (id(), id(), id());
        let root = tabs_leaf(vec![a, b, c], 1); // active = b
        assert_eq!(root.surviving_active_after_remove(b), Some(c));
        // Sole tab → leaf would vanish.
        assert_eq!(PaneNode::leaf(a).surviving_active_after_remove(a), None);
    }

    #[test]
    fn move_into_tabs_merges_two_panes() {
        let (a, b) = (id(), id());
        let mut tree = Some(PaneNode::leaf(a));
        split(&mut tree, a, SplitDirection::Horizontal, b);
        assert!(move_into_tabs(&mut tree, a, b));
        // Collapsed to a single tabbed leaf [b, a] with a active.
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[b, a][..], 1)));
    }

    #[test]
    fn move_into_tabs_from_multi_tab_source() {
        let (a, b, c) = (id(), id(), id());
        // [ leaf(a,b) | leaf(c) ]
        let mut tree = Some(PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![tabs_leaf(vec![a, b], 1), PaneNode::leaf(c)],
        });
        assert!(move_into_tabs(&mut tree, b, c));
        let root = tree.as_ref().unwrap();
        // Source keeps [a] (active fixed to 0); target becomes [c, b].
        let pa = root.find_path(a).unwrap();
        assert_eq!(root.get_at_path(&pa).unwrap().tabs(), Some((&[a][..], 0)));
        let pc = root.find_path(c).unwrap();
        assert_eq!(
            root.get_at_path(&pc).unwrap().tabs(),
            Some((&[c, b][..], 1))
        );
    }

    #[test]
    fn move_into_tabs_no_ops() {
        let (a, b) = (id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b], 0));
        assert!(!move_into_tabs(&mut tree, a, a)); // same instance
        assert!(!move_into_tabs(&mut tree, a, b)); // already same pane
        assert!(!move_into_tabs(&mut tree, a, id())); // target missing
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[a, b][..], 0)));
    }

    #[test]
    fn collect_instances_yields_all_tabs() {
        let (a, b, c) = (id(), id(), id());
        let node = PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![tabs_leaf(vec![a, b], 0), PaneNode::leaf(c)],
        };
        assert_eq!(node.collect_instances(), vec![a, b, c]);
    }

    #[test]
    fn swap_within_same_group() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 0));
        assert!(swap_instances(&mut tree, a, c));
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[c, b, a][..], 0)));
    }

    #[test]
    fn swap_panes_trades_whole_groups() {
        let (a, b, c, d) = (id(), id(), id(), id());
        // split: [ leaf(a,b) active=1 | leaf(c,d) active=0 ]
        let mut tree = Some(PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![tabs_leaf(vec![a, b], 1), tabs_leaf(vec![c, d], 0)],
        });
        // Drag pane A (by tab a) onto pane B (by tab c): the groups trade places.
        assert!(swap_panes(&mut tree, a, c));
        if let Some(PaneNode::Split { children, .. }) = &tree {
            assert_eq!(children[0].tabs(), Some((&[c, d][..], 0)));
            assert_eq!(children[1].tabs(), Some((&[a, b][..], 1)));
        } else {
            panic!("expected split");
        }
        // Same-pane / missing are no-ops.
        assert!(!swap_panes(&mut tree, a, b));
        assert!(!swap_panes(&mut tree, a, id()));
    }

    // ---- add_tab_at ----

    #[test]
    fn add_tab_at_prepend_middle_append() {
        let (a, b, c, d) = (id(), id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 0));
        assert!(add_tab_at(&mut tree, a, d, 1)); // middle
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[a, d, b, c][..], 1)));
        let e = id();
        assert!(add_tab_at(&mut tree, a, e, 0)); // prepend
        assert_eq!(
            tree.as_ref().unwrap().tabs(),
            Some((&[e, a, d, b, c][..], 0))
        );
        let f = id();
        assert!(add_tab_at(&mut tree, a, f, usize::MAX)); // clamp → append
        assert_eq!(tree.as_ref().unwrap().tabs().unwrap().1, 5);
    }

    #[test]
    fn add_tab_at_rejects_duplicate_and_missing() {
        let (a, b) = (id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b], 0));
        assert!(!add_tab_at(&mut tree, a, b, 0)); // b already present
        assert!(!add_tab_at(&mut tree, id(), id(), 0)); // target missing
    }

    #[test]
    fn add_tab_appends_via_wrapper() {
        let a = id();
        let b = id();
        let mut tree = Some(PaneNode::leaf(a));
        assert!(add_tab(&mut tree, a, b));
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[a, b][..], 1)));
    }

    // ---- move_tab_to: same-leaf reorder ----

    #[test]
    fn move_tab_to_same_leaf_forward() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 0));
        assert!(move_tab_to(&mut tree, a, a, 2));
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[b, c, a][..], 2)));
    }

    #[test]
    fn move_tab_to_same_leaf_backward() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 2));
        assert!(move_tab_to(&mut tree, c, c, 0));
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[c, a, b][..], 0)));
    }

    #[test]
    fn move_tab_to_same_leaf_noop() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 1));
        assert!(!move_tab_to(&mut tree, b, b, 1)); // same slot
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[a, b, c][..], 1)));
    }

    // ---- move_tab_to: cross-leaf precise insert ----

    #[test]
    fn move_tab_to_cross_leaf_at_index() {
        let (a, b, c, d) = (id(), id(), id(), id());
        let mut tree = Some(PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![PaneNode::leaf(a), tabs_leaf(vec![b, c, d], 0)],
        });
        assert!(move_tab_to(&mut tree, a, b, 2));
        // left pane collapsed; a inserted at index 2 of [b, c, d] → [b, c, a, d]
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[b, c, a, d][..], 2)));
    }

    #[test]
    fn move_tab_to_cross_leaf_keeps_source_when_multi() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![tabs_leaf(vec![a, b], 0), PaneNode::leaf(c)],
        });
        assert!(move_tab_to(&mut tree, a, c, 0));
        let root = tree.as_ref().unwrap();
        let pb = root.find_path(b).unwrap();
        assert_eq!(root.get_at_path(&pb).unwrap().tabs(), Some((&[b][..], 0)));
        let pc = root.find_path(c).unwrap();
        assert_eq!(
            root.get_at_path(&pc).unwrap().tabs(),
            Some((&[a, c][..], 0))
        );
    }

    #[test]
    fn move_tab_to_not_found() {
        let a = id();
        let mut tree = Some(PaneNode::leaf(a));
        assert!(!move_tab_to(&mut tree, id(), a, 0));
        assert!(!move_tab_to(&mut tree, a, id(), 0));
    }

    // ---- set_tab_order ----

    #[test]
    fn set_tab_order_permutes_and_keeps_active() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 1)); // b active
        assert!(set_tab_order(&mut tree, a, &[c, a, b]));
        assert_eq!(tree.as_ref().unwrap().tabs(), Some((&[c, a, b][..], 2)));
    }

    #[test]
    fn set_tab_order_rejects_non_permutation() {
        let (a, b, c) = (id(), id(), id());
        let mut tree = Some(tabs_leaf(vec![a, b, c], 0));
        assert!(!set_tab_order(&mut tree, a, &[a, b])); // wrong count
        assert!(!set_tab_order(&mut tree, a, &[a, b, id()])); // wrong member
        assert!(!set_tab_order(&mut tree, id(), &[a, b, c])); // anchor missing
    }

    fn vsplit(a: PaneNode, b: PaneNode) -> PaneNode {
        PaneNode::Split {
            direction: SplitDirection::Vertical,
            sizes: vec![1.0, 1.0],
            children: vec![a, b],
        }
    }
    fn hsplit(a: PaneNode, b: PaneNode) -> PaneNode {
        PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![1.0, 1.0],
            children: vec![a, b],
        }
    }

    #[test]
    fn focus_direction_horizontal_split() {
        let (l, r) = (id(), id());
        let tree = hsplit(PaneNode::leaf(l), PaneNode::leaf(r));
        assert_eq!(focus_in_direction(&tree, l, FocusDir::Right), Some(r));
        assert_eq!(focus_in_direction(&tree, r, FocusDir::Left), Some(l));
        assert_eq!(focus_in_direction(&tree, l, FocusDir::Up), None);
        assert_eq!(focus_in_direction(&tree, l, FocusDir::Down), None);
    }

    #[test]
    fn focus_direction_grid_2x2() {
        // Horizontal split of two vertical columns → a 2x2 grid (tl/bl | tr/br).
        let (tl, bl, tr, br) = (id(), id(), id(), id());
        let tree = hsplit(
            vsplit(PaneNode::leaf(tl), PaneNode::leaf(bl)),
            vsplit(PaneNode::leaf(tr), PaneNode::leaf(br)),
        );
        assert_eq!(focus_in_direction(&tree, tl, FocusDir::Right), Some(tr));
        assert_eq!(focus_in_direction(&tree, tl, FocusDir::Down), Some(bl));
        assert_eq!(focus_in_direction(&tree, br, FocusDir::Left), Some(bl));
        assert_eq!(focus_in_direction(&tree, br, FocusDir::Up), Some(tr));
        assert_eq!(focus_in_direction(&tree, tr, FocusDir::Left), Some(tl));
    }

    #[test]
    fn focus_direction_single_leaf_is_none() {
        let a = id();
        let tree = PaneNode::leaf(a);
        assert_eq!(focus_in_direction(&tree, a, FocusDir::Left), None);
    }

    #[test]
    fn focus_direction_targets_neighbor_active_tab() {
        // The right pane is a multi-tab leaf; focusing into it returns its ACTIVE tab.
        let l = id();
        let (r0, r1) = (id(), id());
        let tree = hsplit(PaneNode::leaf(l), tabs_leaf(vec![r0, r1], 1));
        assert_eq!(focus_in_direction(&tree, l, FocusDir::Right), Some(r1));
    }
}
