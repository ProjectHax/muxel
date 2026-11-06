use muxel_core::{LeafData, PaneNode, SplitDirection, remove};
use std::collections::HashSet;
use uuid::Uuid;

/// Restore held instances without replaying layout edits made during the hold.
/// Capture `original` from the durable layout (including earlier held groups),
/// so independent groups retain the topology needed for either restoration order.
/// `closed` lists every held pane, across groups, in the order it was closed.
pub(super) fn restore_layout(
    current: &mut Option<PaneNode>,
    original: &Option<PaneNode>,
    restoring: &HashSet<Uuid>,
    closed: &[Uuid],
) {
    let Some(original_root) = original else {
        return;
    };
    let present: HashSet<_> = current
        .as_ref()
        .map(PaneNode::collect_instances)
        .unwrap_or_default()
        .into_iter()
        .collect();
    if !restoring.iter().any(|id| !present.contains(id)) {
        return;
    }
    // The usual path also recovers exact split proportions and active tabs.
    // Closing hands a pane's size to a sibling, and which sibling can depend
    // on what else has closed, so replay the closes in the order they happened.
    let mut expected = original.clone();
    remove_in_close_order(&mut expected, |id| !present.contains(id), closed);
    // Sizes added in another order can still round differently. Compare up to
    // that rounding.
    let unchanged = match (current.as_ref(), expected.as_ref()) {
        (Some(current), Some(expected)) => current.same_layout(expected),
        (current, expected) => current.is_none() && expected.is_none(),
    };
    if unchanged {
        let mut restored = original.clone();
        remove_in_close_order(
            &mut restored,
            |id| !present.contains(id) && !restoring.contains(id),
            closed,
        );
        *current = restored;
        return;
    }
    let original_ids: HashSet<_> = original_root.collect_instances().into_iter().collect();
    restore_leaves(
        current,
        original_root,
        original_root,
        restoring,
        &original_ids,
    );
    if let Some(root) = current.as_mut() {
        // A pane put back beside a survivor joins the survivor's row rather
        // than nesting a row in its place.
        root.normalize();
        // Panes put back name the panes they were split from, which may be gone.
        root.repoint_missing_origins(original_root);
    }
}

/// Remove the panes `leaving` picks from `layout`: those that left without a
/// held close first, in reading order, then held ones in the order they closed.
fn remove_in_close_order(
    layout: &mut Option<PaneNode>,
    leaving: impl Fn(&Uuid) -> bool,
    closed: &[Uuid],
) {
    let held: HashSet<_> = closed.iter().copied().collect();
    let others: Vec<_> = layout
        .as_ref()
        .map(PaneNode::collect_instances)
        .unwrap_or_default()
        .into_iter()
        .filter(|id| !held.contains(id))
        .collect();
    for id in others.iter().chain(closed) {
        if leaving(id) {
            remove(layout, *id);
        }
    }
}

fn restore_leaves(
    current: &mut Option<PaneNode>,
    node: &PaneNode,
    original: &PaneNode,
    restoring: &HashSet<Uuid>,
    original_ids: &HashSet<Uuid>,
) {
    let PaneNode::Leaf(leaf) = node else {
        if let PaneNode::Split { children, .. } = node {
            for child in children {
                restore_leaves(current, child, original, restoring, original_ids);
            }
        }
        return;
    };
    let missing: Vec<_> = leaf
        .tabs
        .iter()
        .copied()
        .filter(|id| {
            restoring.contains(id)
                && current
                    .as_ref()
                    .is_none_or(|root| root.find_path(*id).is_none())
        })
        .collect();
    if missing.is_empty() {
        return;
    }
    if let Some(target) = current.as_mut().and_then(|root| matching_leaf(root, leaf)) {
        let active = target.active_instance();
        for id in missing {
            let index = leaf.tabs.iter().position(|tab| *tab == id).unwrap();
            let before = leaf.tabs[index + 1..]
                .iter()
                .find_map(|next| target.tabs.iter().position(|tab| tab == next));
            target.tabs.insert(before.unwrap_or(target.tabs.len()), id);
        }
        let active = if restoring.contains(&leaf.active_instance()) {
            leaf.active_instance()
        } else {
            active
        };
        target.active = target.tabs.iter().position(|id| *id == active).unwrap_or(0);
        return;
    }
    let active = missing
        .iter()
        .position(|id| *id == leaf.active_instance())
        .unwrap_or(0);
    let restored = PaneNode::Leaf(LeafData {
        pane_id: leaf.pane_id,
        tabs: missing,
        active,
        split_from: leaf.split_from,
    });
    let Some(root) = current.as_mut() else {
        *current = Some(restored);
        return;
    };
    let path = original.find_path(leaf.tabs[0]).unwrap();
    // Walk outward until an original sibling still has a visible representative.
    for depth in (0..path.len()).rev() {
        let PaneNode::Split {
            direction,
            sizes,
            children,
        } = original.get_at_path(&path[..depth]).unwrap()
        else {
            continue;
        };
        let own = path[depth];
        let siblings = (own + 1..children.len()).chain((0..own).rev());
        for sibling in siblings {
            let sibling_tabs = children[sibling].collect_instances();
            let Some(anchor) = sibling_tabs.iter().find_map(|id| root.find_path(*id)) else {
                continue;
            };
            let sibling_ids: HashSet<_> = sibling_tabs.into_iter().collect();
            let mut anchor = anchor;
            // Include edits belonging to the surviving subtree. Stop before an
            // original pane outside this sibling would be moved with it.
            while !anchor.is_empty() {
                let parent = root.get_at_path(&anchor[..anchor.len() - 1]).unwrap();
                if parent
                    .collect_instances()
                    .iter()
                    .any(|id| original_ids.contains(id) && !sibling_ids.contains(id))
                {
                    break;
                }
                anchor.pop();
            }
            let target = root.get_at_path_mut(&anchor).unwrap();
            insert_beside(
                target,
                restored,
                *direction,
                own < sibling,
                sizes.get(own).copied().unwrap_or(1.0),
                sizes.get(sibling).copied().unwrap_or(1.0),
            );
            return;
        }
    }
    insert_beside(root, restored, SplitDirection::Horizontal, false, 1.0, 1.0);
}

fn matching_leaf<'a>(node: &'a mut PaneNode, original: &LeafData) -> Option<&'a mut LeafData> {
    match node {
        PaneNode::Leaf(leaf) => (leaf.pane_id == original.pane_id
            || leaf.tabs.iter().any(|id| original.tabs.contains(id)))
        .then_some(leaf),
        PaneNode::Split { children, .. } => children
            .iter_mut()
            .find_map(|child| matching_leaf(child, original)),
    }
}

fn insert_beside(
    target: &mut PaneNode,
    restored: PaneNode,
    direction: SplitDirection,
    before: bool,
    restored_size: f32,
    survivor_size: f32,
) {
    if let PaneNode::Split {
        direction: existing,
        sizes,
        children,
    } = target
        && *existing == direction
    {
        let index = if before { 0 } else { children.len() };
        children.insert(index, restored);
        sizes.insert(index, restored_size);
        return;
    }
    let survivor = std::mem::replace(
        target,
        PaneNode::Split {
            direction,
            sizes: Vec::new(),
            children: Vec::new(),
        },
    );
    *target = PaneNode::Split {
        direction,
        sizes: if before {
            vec![restored_size, survivor_size]
        } else {
            vec![survivor_size, restored_size]
        },
        children: if before {
            vec![restored, survivor]
        } else {
            vec![survivor, restored]
        },
    };
}

#[cfg(test)]
mod tests {
    use super::restore_layout;
    use muxel_core::{LeafData, PaneNode, SplitDirection, remove, split, split_beside};
    use std::collections::HashSet;
    use uuid::Uuid;

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn leaf(tabs: &[u128], active: usize) -> PaneNode {
        PaneNode::Leaf(LeafData {
            pane_id: id(100 + tabs[0]),
            tabs: tabs.iter().copied().map(id).collect(),
            active,
            split_from: None,
        })
    }

    fn branch(direction: SplitDirection, children: Vec<PaneNode>) -> PaneNode {
        PaneNode::Split {
            direction,
            sizes: (1..=children.len()).map(|n| n as f32).collect(),
            children,
        }
    }

    /// Close `ids` in this order, as `hold_closes` does, returning that order.
    fn close(layout: &mut Option<PaneNode>, ids: &[u128]) -> Vec<Uuid> {
        ids.iter()
            .map(|&value| {
                remove(layout, id(value));
                id(value)
            })
            .collect()
    }

    fn set(ids: &[Uuid]) -> HashSet<Uuid> {
        ids.iter().copied().collect()
    }

    fn assert_same(actual: &Option<PaneNode>, expected: &Option<PaneNode>) {
        // PaneNode equality intentionally excludes stable pane identity.
        assert_eq!(
            serde_json::to_value(actual).unwrap(),
            serde_json::to_value(expected).unwrap()
        );
    }

    #[test]
    fn idle_agent_close_undo_restores_tab_order_and_pane_identity() {
        let original = Some(leaf(&[1, 2, 3, 4], 2));
        let mut current = original.clone();
        let closed = close(&mut current, &[2, 3]);
        restore_layout(&mut current, &original, &set(&closed), &closed);
        assert_same(&current, &original);
        restore_layout(&mut current, &original, &set(&closed), &closed);
        assert_same(&current, &original);
    }

    #[test]
    fn whole_tab_group_restores_only_requested_members() {
        let original = Some(leaf(&[1, 2, 3], 1));
        let mut current = None;
        let closed = [id(1), id(2), id(3)];
        restore_layout(
            &mut current,
            &original,
            &HashSet::from([id(1), id(3)]),
            &closed,
        );
        let mut expected = original.clone();
        remove(&mut expected, id(2));
        assert_same(&current, &expected);
    }

    #[test]
    fn restores_nested_split_shape_and_sizes() {
        let original = Some(branch(
            SplitDirection::Horizontal,
            vec![
                leaf(&[1], 0),
                branch(
                    SplitDirection::Vertical,
                    vec![leaf(&[2, 3], 1), leaf(&[4], 0)],
                ),
            ],
        ));
        let mut current = original.clone();
        let closed = close(&mut current, &[1, 2, 3]);
        restore_layout(&mut current, &original, &set(&closed), &closed);
        assert_same(&current, &original);
    }

    #[test]
    fn undo_preserves_new_splits_tabs_and_selected_survivor() {
        let original = Some(leaf(&[1, 2, 3], 0));
        let mut current = original.clone();
        let closed = close(&mut current, &[2]);
        split(&mut current, id(1), SplitDirection::Vertical, id(4));
        muxel_core::add_tab(&mut current, id(1), id(5));
        restore_layout(&mut current, &original, &set(&closed), &closed);
        let root = current.as_ref().unwrap();
        let pane = root.leaf_containing(id(1)).unwrap();
        assert_eq!(pane.tabs().unwrap().0, &[id(1), id(2), id(3), id(5)]);
        assert_eq!(pane.tabs().unwrap().1, 3);
        assert!(root.find_path(id(4)).is_some());
    }

    #[test]
    fn missing_group_preserves_new_layout_and_original_identity() {
        let original = Some(leaf(&[1, 2], 0));
        let mut current = Some(leaf(&[3], 0));
        let closed = [id(1), id(2)];
        restore_layout(&mut current, &original, &set(&closed), &closed);
        let root = current.unwrap();
        assert!(root.find_path(id(3)).is_some());
        let PaneNode::Leaf(restored) = root.leaf_containing(id(1)).unwrap() else {
            panic!("expected a tab group");
        };
        assert_eq!(restored.pane_id, id(101));
        assert_eq!(restored.tabs, [id(1), id(2)]);
    }

    #[test]
    fn separate_groups_restore_in_either_order() {
        let original = Some(branch(
            SplitDirection::Horizontal,
            vec![
                leaf(&[1], 0),
                branch(SplitDirection::Vertical, vec![leaf(&[2], 0), leaf(&[3], 0)]),
            ],
        ));
        for reverse in [false, true] {
            let mut current = original.clone();
            let first = close(&mut current, &[2]);
            let second_original = current.clone();
            let second = close(&mut current, &[1]);
            let closed = [first.clone(), second.clone()].concat();
            if reverse {
                restore_layout(&mut current, &second_original, &set(&second), &closed);
                restore_layout(&mut current, &original, &set(&first), &first);
            } else {
                restore_layout(&mut current, &original, &set(&first), &closed);
                restore_layout(&mut current, &second_original, &set(&second), &second);
            }
            assert_same(&current, &original);
        }
    }

    #[test]
    fn separate_tab_groups_restore_in_either_order() {
        let original = Some(leaf(&[1, 2, 3], 0));
        for reverse in [false, true] {
            let mut current = original.clone();
            let first = close(&mut current, &[2]);
            let second_original = current.clone();
            let second = close(&mut current, &[3]);
            let closed = [first.clone(), second.clone()].concat();
            if reverse {
                restore_layout(&mut current, &second_original, &set(&second), &closed);
                restore_layout(&mut current, &original, &set(&first), &first);
            } else {
                restore_layout(&mut current, &original, &set(&first), &closed);
                restore_layout(&mut current, &second_original, &set(&second), &second);
            }
            assert_same(&current, &original);
        }
    }

    #[test]
    fn durable_snapshots_restore_nested_groups_in_either_order() {
        let original = Some(branch(
            SplitDirection::Horizontal,
            vec![
                leaf(&[1], 0),
                branch(SplitDirection::Vertical, vec![leaf(&[2], 0), leaf(&[3], 0)]),
            ],
        ));
        for ids in [[2, 3], [3, 2]] {
            let mut current = original.clone();
            let closed = close(&mut current, &[2, 3]);
            for value in ids {
                restore_layout(
                    &mut current,
                    &original,
                    &HashSet::from([id(value)]),
                    &closed,
                );
            }
            assert_same(&current, &original);
        }
    }

    #[test]
    fn held_pane_comes_back_where_an_adopted_peer_layout_put_it() {
        // While 2 was held, a peer moved it into its own split below 3.
        let adopted = Some(branch(
            SplitDirection::Horizontal,
            vec![
                leaf(&[1], 0),
                branch(SplitDirection::Vertical, vec![leaf(&[3], 0), leaf(&[2], 0)]),
            ],
        ));
        let mut current = adopted.clone();
        let gone = muxel_core::recoverable_close::rehide_held(&mut current, &[id(2)]);
        assert!(gone.is_empty());
        assert!(current.as_ref().unwrap().find_path(id(2)).is_none());
        restore_layout(&mut current, &adopted, &HashSet::from([id(2)]), &[id(2)]);
        assert_same(&current, &adopted);
    }

    #[test]
    fn a_group_closed_out_of_reading_order_restores_exact_sizes() {
        // Closing 3 and then 2 hands their widths to 1 in a different order than
        // removing them in reading order does, and f32 addition rounds the two
        // sums differently (1027.8999 against 1027.9). Undo must still see that
        // nothing else changed and restore the layout exactly.
        let original = Some(PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![412.7, 333.3, 281.9, 500.0],
            children: vec![leaf(&[1], 0), leaf(&[2], 0), leaf(&[3], 0), leaf(&[4], 0)],
        });
        let mut current = original.clone();
        let closed = close(&mut current, &[3, 2]);
        restore_layout(&mut current, &original, &set(&closed), &closed);
        assert_same(&current, &original);
    }

    #[test]
    fn separate_groups_restore_flat_split_sizes_in_either_order() {
        let original = Some(branch(
            SplitDirection::Horizontal,
            vec![leaf(&[1], 0), leaf(&[2], 0), leaf(&[3], 0)],
        ));
        for reverse in [false, true] {
            let mut current = original.clone();
            let first = close(&mut current, &[2]);
            // A group captures the durable layout, where an earlier held pane
            // is still in place (`hold_closes` → `workspace_with_closes_restored`).
            let mut second_original = current.clone();
            restore_layout(&mut second_original, &original, &set(&first), &first);
            let second = close(&mut current, &[1]);
            let closed = [first.clone(), second.clone()].concat();
            if reverse {
                restore_layout(&mut current, &second_original, &set(&second), &closed);
                restore_layout(&mut current, &original, &set(&first), &first);
            } else {
                restore_layout(&mut current, &original, &set(&first), &closed);
                restore_layout(&mut current, &second_original, &set(&second), &second);
            }
            assert_same(&current, &original);
        }
    }

    // ---- close order, with a pane that has no split history ----------------

    /// The row `a_pane_brought_back_beside_a_new_split_joins_its_row` reaches:
    /// 3, saved before split history existed, sits between 2 and the pane 2 was
    /// split from, 4. Closing 3 hands its width to 2; closing 2 hands its width
    /// to 4. So the same panes closed in another order end at other widths.
    fn mixed_history_row() -> Option<PaneNode> {
        let pane = |tab: u128, from: Option<u128>| {
            PaneNode::Leaf(LeafData {
                pane_id: id(100 + tab),
                tabs: vec![id(tab)],
                active: 0,
                split_from: from.map(|tab| id(100 + tab)),
            })
        };
        Some(PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![600.0, 300.0, 75.0, 225.0],
            children: vec![
                pane(1, None),
                pane(2, Some(4)),
                pane(3, None),
                pane(4, None),
            ],
        })
    }

    #[test]
    fn a_pane_brought_back_beside_a_new_split_joins_its_row() {
        // A row saved before split history existed.
        let original = Some(PaneNode::Split {
            direction: SplitDirection::Horizontal,
            sizes: vec![400.0, 200.0, 600.0],
            children: vec![leaf(&[1], 0), leaf(&[3], 0), leaf(&[4], 0)],
        });
        let mut current = original.clone();
        let closed = close(&mut current, &[3]);
        // While 3 is held, 4 is split leftward, so undo can't put back the old row.
        assert!(split_beside(
            &mut current,
            id(4),
            SplitDirection::Horizontal,
            id(2),
            true
        ));
        restore_layout(&mut current, &original, &set(&closed), &closed);
        // 3 comes back between 2 and 4 in the row itself, at its old share of
        // 4's place, rather than in a row nested where 4 was.
        let restored = current.unwrap();
        assert!(
            restored.same_layout(mixed_history_row().as_ref().unwrap()),
            "{restored:#?}"
        );
    }

    #[test]
    fn panes_closed_together_restore_exactly_in_either_close_order() {
        let original = mixed_history_row();
        for first in 1..=4 {
            for second in (1..=4).filter(|&second| second != first) {
                let mut current = original.clone();
                let closed = close(&mut current, &[first, second]);
                restore_layout(&mut current, &original, &set(&closed), &closed);
                assert_eq!(
                    serde_json::to_value(&current).unwrap(),
                    serde_json::to_value(&original).unwrap(),
                    "closing {first} then {second}"
                );
            }
        }
    }

    #[test]
    fn groups_closed_one_after_another_restore_exactly_in_either_undo_order() {
        let original = mixed_history_row();
        for older_first in [false, true] {
            let mut current = original.clone();
            let first = close(&mut current, &[3]);
            // The second group captures the durable layout, with 3 still in place.
            let mut second_original = current.clone();
            restore_layout(&mut second_original, &original, &set(&first), &first);
            let second = close(&mut current, &[2]);
            let closed = [first.clone(), second.clone()].concat();
            if older_first {
                // Undo the older close while the newer one is still held.
                restore_layout(&mut current, &original, &set(&first), &closed);
                restore_layout(&mut current, &second_original, &set(&second), &second);
            } else {
                restore_layout(&mut current, &second_original, &set(&second), &closed);
                restore_layout(&mut current, &original, &set(&first), &first);
            }
            assert_same(&current, &original);
        }
    }

    #[test]
    fn a_pane_brought_back_by_hand_gives_its_width_where_its_origin_went() {
        let sizes = |layout: &Option<PaneNode>| match layout {
            Some(PaneNode::Split { sizes, .. }) => sizes.clone(),
            other => panic!("expected a split, got {other:?}"),
        };
        // 2 is split off 4, then 3 off 2.
        let mut layout = Some(leaf(&[4], 0));
        assert!(split(&mut layout, id(4), SplitDirection::Horizontal, id(2)));
        assert!(split(&mut layout, id(2), SplitDirection::Horizontal, id(3)));
        let original = layout.clone();
        let mut current = layout;
        // Closing 2 hands its width to 4, and 3 now counts as split from 4.
        let first = close(&mut current, &[2]);
        let mut second_original = current.clone();
        restore_layout(&mut second_original, &original, &set(&first), &first);
        let second = close(&mut current, &[3]);
        // A new pane while 3 is held, so undo rebuilds the layout by hand.
        assert!(split(
            &mut current,
            id(4),
            SplitDirection::Horizontal,
            id(5)
        ));
        assert_eq!(sizes(&current), [1.0, 1.0]);
        let closed = [first, second.clone()].concat();
        restore_layout(&mut current, &second_original, &set(&second), &closed);
        assert_eq!(
            current.as_ref().unwrap().collect_instances(),
            [id(4), id(5), id(3)]
        );
        // 2 is still held, so 3's width belongs where 2's went: to 4, not to
        // the new pane beside it.
        assert!(remove(&mut current, id(3)));
        assert_eq!(sizes(&current), [1.5, 1.0]);
    }
}
