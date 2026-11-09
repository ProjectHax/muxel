//! Seeded fuzz over every mutating pane op, asserting the tree invariants the
//! app renders against.
//!
//! Chiefly: **no leaf ever carries zero tabs**. `render_pane` and
//! `visible_browser_ids` index `tabs[active]` for every leaf on every frame, so
//! a single empty leaf would panic the whole UI. `LeafData`'s deserializer
//! rejects `"tabs": []`, which means an empty leaf can only ever be produced by
//! a mutation here — exactly what this fuzz rules out.
//!
//! The two control tests are load-bearing: without them a green fuzz could mean
//! "the checker is blind" or "the fuzz only ever built a single leaf".

use std::collections::{HashMap, HashSet};

use muxel_core::{
    LeafData, PaneNode, SplitDirection, Uuid, add_tab, add_tab_at, move_into_split, move_into_tabs,
    move_pane_beside, move_tab_to, remove, set_active_tab, split, split_beside, swap_instances,
    swap_panes,
};

/// Deterministic LCG, so a failure reproduces from its seed.
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        self.0 >> 33
    }
    fn below(&mut self, n: usize) -> usize {
        (self.next() as usize) % n.max(1)
    }
}

fn collect_leaves<'a>(node: &'a PaneNode, out: &mut Vec<&'a LeafData>) {
    match node {
        PaneNode::Leaf(ld) => out.push(ld),
        PaneNode::Split { children, .. } => {
            for c in children {
                collect_leaves(c, out);
            }
        }
    }
}

fn all_ids(tree: &Option<PaneNode>) -> Vec<Uuid> {
    let mut leaves = Vec::new();
    if let Some(root) = tree {
        collect_leaves(root, &mut leaves);
    }
    leaves.iter().flat_map(|l| l.tabs.iter().copied()).collect()
}

fn check_splits(node: &PaneNode, step: usize, op: &str) {
    if let PaneNode::Split {
        children, sizes, ..
    } = node
    {
        assert!(
            children.len() >= 2,
            "step {step} after {op}: split with {} children",
            children.len()
        );
        assert_eq!(
            children.len(),
            sizes.len(),
            "step {step} after {op}: sizes/children mismatch"
        );
        for c in children {
            check_splits(c, step, op);
        }
    }
}

/// Every invariant the rendering code assumes about a layout tree.
fn check(tree: &Option<PaneNode>, step: usize, op: &str) {
    let Some(root) = tree else { return };
    let mut leaves = Vec::new();
    collect_leaves(root, &mut leaves);

    for ld in &leaves {
        assert!(
            !ld.tabs.is_empty(),
            "step {step} after {op}: EMPTY LEAF — tabs[active] would index-panic"
        );
        assert!(
            ld.active < ld.tabs.len(),
            "step {step} after {op}: active {} out of range (len {})",
            ld.active,
            ld.tabs.len()
        );
    }
    let pane_ids: Vec<_> = leaves.iter().map(|leaf| leaf.pane_id).collect();
    let mut unique_pane_ids = pane_ids.clone();
    unique_pane_ids.sort();
    unique_pane_ids.dedup();
    assert_eq!(
        unique_pane_ids.len(),
        pane_ids.len(),
        "step {step} after {op}: two leaves share one pane identity"
    );
    check_splits(root, step, op);

    let ids = all_ids(tree);
    let mut sorted = ids.clone();
    sorted.sort();
    sorted.dedup();
    assert_eq!(
        sorted.len(),
        ids.len(),
        "step {step} after {op}: an instance appears in two panes"
    );
}

/// CONTROL: the checker must actually catch an empty leaf, or the fuzz below is
/// asserting nothing.
#[test]
#[should_panic(expected = "EMPTY LEAF")]
fn checker_detects_an_empty_leaf() {
    let bad = PaneNode::Split {
        direction: SplitDirection::Horizontal,
        sizes: vec![0.5, 0.5],
        children: vec![
            PaneNode::leaf(Uuid::new_v4()),
            PaneNode::Leaf(LeafData {
                pane_id: Uuid::new_v4(),
                tabs: vec![],
                active: 0,
                split_from: None,
            }),
        ],
    };
    check(&Some(bad), 0, "control");
}

/// CONTROL: the fuzz must build interesting trees, not sit on a single leaf
/// where "no empty leaf" is trivially true.
#[test]
fn fuzz_reaches_splits_and_tab_groups() {
    let pool: Vec<Uuid> = (0..12).map(|_| Uuid::new_v4()).collect();
    let (mut saw_split, mut saw_nested, mut saw_multi_tab) = (false, false, false);

    for seed in 0..400u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF);
        let mut tree: Option<PaneNode> = Some(PaneNode::leaf(pool[0]));
        for _ in 1..=60 {
            let live = all_ids(&tree);
            if live.is_empty() {
                tree = Some(PaneNode::leaf(pool[rng.below(pool.len())]));
                continue;
            }
            let a = live[rng.below(live.len())];
            let b = live[rng.below(live.len())];
            let fresh = pool
                .iter()
                .copied()
                .find(|id| !live.contains(id))
                .unwrap_or_else(Uuid::new_v4);
            match rng.below(5) {
                0 => {
                    split(&mut tree, a, SplitDirection::Horizontal, fresh);
                }
                1 => {
                    add_tab(&mut tree, a, fresh);
                }
                2 => {
                    move_into_tabs(&mut tree, a, b);
                }
                3 => {
                    move_into_split(&mut tree, a, b, SplitDirection::Vertical, false);
                }
                _ => {
                    remove(&mut tree, a);
                }
            }
            let Some(root) = &tree else { continue };
            if let PaneNode::Split { children, .. } = root {
                saw_split = true;
                saw_nested |= children.iter().any(|c| matches!(c, PaneNode::Split { .. }));
            }
            let mut leaves = Vec::new();
            collect_leaves(root, &mut leaves);
            saw_multi_tab |= leaves.iter().any(|l| l.tabs.len() >= 2);
        }
    }
    assert!(saw_split, "fuzz never produced a split");
    assert!(saw_nested, "fuzz never produced a nested split");
    assert!(saw_multi_tab, "fuzz never produced a multi-tab leaf");
}

/// One random mutation of `tree`, drawn from every op, with targets drawn from
/// its live instances (and `pool` for fresh ones). Returns the op's name.
fn random_op(tree: &mut Option<PaneNode>, rng: &mut Rng, pool: &[Uuid]) -> &'static str {
    let live = all_ids(tree);
    let a = live[rng.below(live.len())];
    let b = live[rng.below(live.len())];
    let dir = if rng.next().is_multiple_of(2) {
        SplitDirection::Horizontal
    } else {
        SplitDirection::Vertical
    };
    let before = rng.next().is_multiple_of(2);
    // An id not currently in the tree, for the insert-style ops.
    let fresh = pool
        .iter()
        .copied()
        .find(|id| !live.contains(id))
        .unwrap_or_else(Uuid::new_v4);

    match rng.below(11) {
        0 => {
            split(tree, a, dir, fresh);
            "split"
        }
        1 => {
            split_beside(tree, a, dir, fresh, before);
            "split_beside"
        }
        2 => {
            add_tab(tree, a, fresh);
            "add_tab"
        }
        3 => {
            add_tab_at(tree, a, fresh, rng.below(4));
            "add_tab_at"
        }
        4 => {
            move_into_tabs(tree, a, b);
            "move_into_tabs"
        }
        5 => {
            move_tab_to(tree, a, b, rng.below(4));
            "move_tab_to"
        }
        6 => {
            move_into_split(tree, a, b, dir, before);
            "move_into_split"
        }
        7 => {
            move_pane_beside(tree, a, b, dir, before);
            "move_pane_beside"
        }
        8 => {
            remove(tree, a);
            "remove"
        }
        9 => {
            swap_instances(tree, a, b);
            "swap_instances"
        }
        _ => {
            if rng.next().is_multiple_of(2) {
                swap_panes(tree, a, b);
                "swap_panes"
            } else {
                set_active_tab(tree, a);
                "set_active_tab"
            }
        }
    }
}

#[test]
fn no_op_ever_leaves_an_empty_leaf() {
    let pool: Vec<Uuid> = (0..12).map(|_| Uuid::new_v4()).collect();

    for seed in 0..2_000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0xDEAD_BEEF);
        let mut tree: Option<PaneNode> = Some(PaneNode::leaf(pool[0]));
        check(&tree, 0, "init");

        for step in 1..=60 {
            if all_ids(&tree).is_empty() {
                tree = Some(PaneNode::leaf(pool[rng.below(pool.len())]));
                check(&tree, step, "reseed");
                continue;
            }
            let op = random_op(&mut tree, &mut rng, &pool);
            check(&tree, step, &format!("{op} (seed {seed})"));
        }
    }
}

/// Each pane's rectangle as fractions of the layout, keyed by pane identity.
/// Splits share their extent in proportion to `sizes`, as the renderer does
/// when no pane is held at its minimum.
fn pane_rects(tree: &Option<PaneNode>) -> HashMap<Uuid, [f32; 4]> {
    fn walk(node: &PaneNode, rect: [f32; 4], out: &mut HashMap<Uuid, [f32; 4]>) {
        match node {
            PaneNode::Leaf(leaf) => {
                out.insert(leaf.pane_id, rect);
            }
            PaneNode::Split {
                direction,
                sizes,
                children,
            } => {
                let total: f32 = sizes.iter().sum();
                let [x, y, w, h] = rect;
                let mut offset = 0.0;
                for (child, size) in children.iter().zip(sizes) {
                    let share = size / total;
                    let child_rect = match direction {
                        SplitDirection::Horizontal => [x + offset * w, y, share * w, h],
                        SplitDirection::Vertical => [x, y + offset * h, w, share * h],
                    };
                    walk(child, child_rect, out);
                    offset += share;
                }
            }
        }
    }
    let mut out = HashMap::new();
    if let Some(root) = tree {
        walk(root, [0.0, 0.0, 1.0, 1.0], &mut out);
    }
    out
}

/// Opening a pane and then closing it, or dragging its tab into another pane,
/// leaves every other pane exactly where it was, in any layout the ops reach.
#[test]
fn a_pane_opened_then_gone_leaves_every_other_pane_in_place() {
    let pool: Vec<Uuid> = (0..12).map(|_| Uuid::new_v4()).collect();
    let mut crowded = 0;

    for seed in 0..2_000u64 {
        let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) ^ 0x5EED_5EED);
        let mut tree: Option<PaneNode> = Some(PaneNode::leaf(pool[0]));
        for _ in 0..rng.below(40) {
            if all_ids(&tree).is_empty() {
                tree = Some(PaneNode::leaf(pool[rng.below(pool.len())]));
            }
            random_op(&mut tree, &mut rng, &pool);
        }
        let live = all_ids(&tree);
        if live.is_empty() {
            continue;
        }
        let before = pane_rects(&tree);
        crowded += usize::from(before.len() > 2);

        let target = live[rng.below(live.len())];
        let other = live[rng.below(live.len())];
        let dir = if rng.next().is_multiple_of(2) {
            SplitDirection::Horizontal
        } else {
            SplitDirection::Vertical
        };
        let side = rng.next().is_multiple_of(2);
        let new = Uuid::new_v4();
        let mut after = tree.clone();
        assert!(split_beside(&mut after, target, dir, new, side));
        let how = match rng.below(3) {
            0 => {
                assert!(remove(&mut after, new));
                "closed"
            }
            1 => {
                assert!(move_tab_to(&mut after, new, other, rng.below(4)));
                "dragged into another pane"
            }
            _ => {
                assert!(move_into_tabs(&mut after, new, other));
                "merged into another pane"
            }
        };

        let after = pane_rects(&after);
        assert_eq!(
            before.keys().collect::<HashSet<_>>(),
            after.keys().collect::<HashSet<_>>(),
            "seed {seed}: the same panes remain after the new one is {how}"
        );
        for (pane, was) in &before {
            let now = after[pane];
            assert!(
                was.iter().zip(now).all(|(a, b)| (a - b).abs() < 1e-4),
                "seed {seed}: splitting {dir:?} (before: {side}) then the new pane {how} \
                 moved pane {pane} from {was:?} to {now:?}"
            );
        }
    }
    // CONTROL: most cases must have neighbours the new pane could disturb.
    assert!(
        crowded > 1_000,
        "only {crowded} layouts had three or more panes"
    );
}
