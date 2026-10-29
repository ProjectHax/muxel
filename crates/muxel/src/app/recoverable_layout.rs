use muxel_core::{LeafData, PaneNode, SplitDirection, remove};
use std::collections::HashSet;
use uuid::Uuid;

/// Restore held instances without replaying layout edits made during the hold.
/// Capture `original` from the durable layout (including earlier held groups),
/// so independent groups retain the topology needed for either restoration order.
pub(super) fn restore_layout(
    current: &mut Option<PaneNode>,
    original: &Option<PaneNode>,
    restoring: &HashSet<Uuid>,
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
    let mut expected = original.clone();
    for id in original_root.collect_instances() {
        if !present.contains(&id) {
            remove(&mut expected, id);
        }
    }
    if *current == expected {
        let mut restored = original.clone();
        for id in original_root.collect_instances() {
            if !present.contains(&id) && !restoring.contains(&id) {
                remove(&mut restored, id);
            }
        }
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
    use muxel_core::{LeafData, PaneNode, SplitDirection, remove, split};
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
        })
    }

    fn branch(direction: SplitDirection, children: Vec<PaneNode>) -> PaneNode {
        PaneNode::Split {
            direction,
            sizes: (1..=children.len()).map(|n| n as f32).collect(),
            children,
        }
    }

    fn close(layout: &mut Option<PaneNode>, ids: &[u128]) -> HashSet<Uuid> {
        ids.iter()
            .map(|&value| {
                remove(layout, id(value));
                id(value)
            })
            .collect()
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
        let restoring = close(&mut current, &[2, 3]);
        restore_layout(&mut current, &original, &restoring);
        assert_same(&current, &original);
        restore_layout(&mut current, &original, &restoring);
        assert_same(&current, &original);
    }

    #[test]
    fn whole_tab_group_restores_only_requested_members() {
        let original = Some(leaf(&[1, 2, 3], 1));
        let mut current = None;
        restore_layout(&mut current, &original, &HashSet::from([id(1), id(3)]));
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
        let restoring = close(&mut current, &[1, 2, 3]);
        restore_layout(&mut current, &original, &restoring);
        assert_same(&current, &original);
    }

    #[test]
    fn undo_preserves_new_splits_tabs_and_selected_survivor() {
        let original = Some(leaf(&[1, 2, 3], 0));
        let mut current = original.clone();
        let restoring = close(&mut current, &[2]);
        split(&mut current, id(1), SplitDirection::Vertical, id(4));
        muxel_core::add_tab(&mut current, id(1), id(5));
        restore_layout(&mut current, &original, &restoring);
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
        restore_layout(&mut current, &original, &HashSet::from([id(1), id(2)]));
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
            if reverse {
                restore_layout(&mut current, &second_original, &second);
                restore_layout(&mut current, &original, &first);
            } else {
                restore_layout(&mut current, &original, &first);
                restore_layout(&mut current, &second_original, &second);
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
            if reverse {
                restore_layout(&mut current, &second_original, &second);
                restore_layout(&mut current, &original, &first);
            } else {
                restore_layout(&mut current, &original, &first);
                restore_layout(&mut current, &second_original, &second);
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
            close(&mut current, &[2, 3]);
            for value in ids {
                restore_layout(&mut current, &original, &HashSet::from([id(value)]));
            }
            assert_same(&current, &original);
        }
    }

    #[test]
    fn separate_groups_restore_flat_split_sizes_in_either_order() {
        let original = Some(branch(
            SplitDirection::Horizontal,
            vec![leaf(&[1], 0), leaf(&[2], 0), leaf(&[3], 0)],
        ));
        let mut current = original.clone();
        let first = close(&mut current, &[2]);
        let second_original = current.clone();
        let second = close(&mut current, &[1]);
        restore_layout(&mut current, &original, &first);
        restore_layout(&mut current, &second_original, &second);
        assert_same(&current, &original);
    }
}
