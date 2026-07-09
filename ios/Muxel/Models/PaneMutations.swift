import Foundation

/// Pure pane-tree read helpers + mutations — a Swift port of the subset of
/// `crates/muxel-core/src/pane.rs` the iOS split UI needs. `PaneNode` is a value
/// enum, so where Rust mutates through `&mut Option<PaneNode>` these operate on
/// `inout PaneNode?` and rebuild the affected spine. Semantics mirror Rust exactly
/// (active-index fixup, the no-duplicate guard, single-child collapse, same-direction
/// flatten) so trees the phone writes converge with what desktop would produce.
extension PaneNode {
    /// First tab of the first leaf (reading order) — a stable anchor that doesn't move
    /// as the user switches tabs. Port of `first_instance`.
    var firstInstance: String? { allTabs.first }

    /// A stable SwiftUI identity for this subtree — its instance ids in order. Changes
    /// only when the subtree's tab set changes (the analogue of Rust's `split_key`), so
    /// terminals survive re-renders that don't touch their leaf's membership. Also the
    /// key `set_split_sizes` matches a split by.
    var stableKey: String { allTabs.joined(separator: "-") }

    /// Number of leaves (panes) in this subtree — for the pane cap.
    var leafCount: Int {
        switch self {
        case .leaf: return 1
        case let .split(_, _, children): return children.reduce(0) { $0 + $1.leafCount }
        }
    }

    /// Return a copy of the tree with the split matching `key` given `sizes` (if the
    /// count matches), plus whether a match was found. Port of `set_split_sizes`.
    func settingSplitSizes(key: String, sizes: [Double]) -> (node: PaneNode, changed: Bool) {
        guard case let .split(dir, curSizes, children) = self else { return (self, false) }
        if stableKey == key {
            let newSizes = sizes.count == children.count ? sizes : curSizes
            return (.split(direction: dir, sizes: newSizes, children: children), true)
        }
        var newChildren = children
        for i in children.indices {
            let (n, c) = children[i].settingSplitSizes(key: key, sizes: sizes)
            if c {
                newChildren[i] = n
                return (.split(direction: dir, sizes: curSizes, children: newChildren), true)
            }
        }
        return (self, false)
    }

    /// This node's `(tabs, active)` iff it is a leaf. Port of `tabs()`.
    var leafTabs: (tabs: [String], active: Int)? {
        if case let .leaf(tabs, active) = self { return (tabs, active) }
        return nil
    }

    /// Child-index path from this node to the leaf holding `instance`. Port of
    /// `find_path`.
    func findPath(_ instance: String) -> [Int]? {
        var path: [Int] = []
        return findPathInto(instance, &path) ? path : nil
    }

    private func findPathInto(_ instance: String, _ path: inout [Int]) -> Bool {
        switch self {
        case let .leaf(tabs, _):
            return tabs.contains(instance)
        case let .split(_, _, children):
            for (i, child) in children.enumerated() {
                path.append(i)
                if child.findPathInto(instance, &path) { return true }
                path.removeLast()
            }
            return false
        }
    }

    /// The node at `path`, or nil if the path is invalid. Port of `get_at_path`.
    func node(atPath path: [Int]) -> PaneNode? {
        guard let first = path.first else { return self }
        guard case let .split(_, _, children) = self, first < children.count else { return nil }
        return children[first].node(atPath: Array(path.dropFirst()))
    }

    /// Return a copy of the tree with `transform` applied to the node at `path`.
    /// The value-type analogue of `get_at_path_mut`.
    func replacingNode(atPath path: [Int], _ transform: (PaneNode) -> PaneNode) -> PaneNode {
        guard let first = path.first else { return transform(self) }
        guard case let .split(direction, sizes, children) = self, first < children.count else { return self }
        var newChildren = children
        newChildren[first] = children[first].replacingNode(atPath: Array(path.dropFirst()), transform)
        return .split(direction: direction, sizes: sizes, children: newChildren)
    }

    /// Remove the child node at `path`, collapsing a split left with a single child.
    /// Port of `remove_at_path`.
    mutating func removeAtPath(_ path: [Int]) {
        guard let idx = path.last else { return }
        let parentPath = Array(path.dropLast())
        self = replacingNode(atPath: parentPath) { parent in
            guard case let .split(direction, sizes, children) = parent, idx < children.count else { return parent }
            var newChildren = children
            var newSizes = sizes
            newChildren.remove(at: idx)
            if idx < newSizes.count { newSizes.remove(at: idx) }
            if newChildren.count == 1 { return newChildren[0] }
            return .split(direction: direction, sizes: newSizes, children: newChildren)
        }
    }

    /// Normalize in place: recurse, fix `sizes` length, unwrap single-child splits, and
    /// flatten nested same-direction splits (merging weights). Port of `normalize`.
    mutating func normalize() {
        guard case let .split(direction, sizes, children) = self else { return }

        var newChildren = children.map { child -> PaneNode in
            var c = child
            c.normalize()
            return c
        }
        var newSizes = sizes
        if newSizes.count != newChildren.count {
            newSizes = Array(repeating: 1.0, count: newChildren.count)
        }

        // Unwrap a split with a single child, then re-normalize.
        if newChildren.count == 1 {
            self = newChildren[0]
            normalize()
            return
        }

        // Flatten nested splits with the same direction (weighted).
        let hasSameDir = newChildren.contains {
            if case let .split(d, _, _) = $0 { return d == direction } else { return false }
        }
        if hasSameDir {
            var flatChildren: [PaneNode] = []
            var flatSizes: [Double] = []
            for (i, child) in newChildren.enumerated() {
                let parentSize = i < newSizes.count ? newSizes[i] : 1.0
                if case let .split(cd, cs, gc) = child, cd == direction {
                    let total = cs.reduce(0, +)
                    let denom = total > 0 ? total : Double(max(gc.count, 1))
                    for (j, g) in gc.enumerated() {
                        flatChildren.append(g)
                        flatSizes.append(parentSize * (j < cs.count ? cs[j] : 1.0) / denom)
                    }
                } else {
                    flatChildren.append(child)
                    flatSizes.append(parentSize)
                }
            }
            self = .split(direction: direction, sizes: flatSizes, children: flatChildren)
        } else {
            self = .split(direction: direction, sizes: newSizes, children: newChildren)
        }
    }

    /// Which instance the leaf holding `removing` would activate if `removing` closed,
    /// or nil if that leaf would vanish / `removing` isn't present. Port of
    /// `surviving_active_after_remove` — used to retarget focus before a close.
    func survivingActiveAfterRemove(_ removing: String) -> String? {
        guard let path = findPath(removing) else { return nil }
        guard case let .leaf(tabs, active)? = node(atPath: path) else { return nil }
        guard tabs.count > 1, let idx = tabs.firstIndex(of: removing) else { return nil }
        let newActive: Int
        if idx < active { newActive = active - 1 }
        else if idx == active { newActive = min(active, tabs.count - 2) }
        else { newActive = active }
        let surviving = tabs.filter { $0 != removing }
        guard newActive >= 0, newActive < surviving.count else { return nil }
        return surviving[newActive]
    }
}

/// Namespaced pane-tree mutations. Each returns whether it changed the tree.
enum PaneTree {
    /// Split the pane holding `target`, placing a fresh single-tab pane for
    /// `newInstance` alongside it. Port of `split`.
    @discardableResult
    static func split(_ tree: inout PaneNode?, target: String,
                      direction: SplitDirection, newInstance: String) -> Bool {
        splitBeside(&tree, target: target, direction: direction, newInstance: newInstance, before: false)
    }

    /// Split `target`'s pane, inserting a fresh single-tab pane before/after it.
    /// Port of `split_beside`.
    @discardableResult
    static func splitBeside(_ tree: inout PaneNode?, target: String,
                            direction: SplitDirection, newInstance: String, before: Bool) -> Bool {
        guard var root = tree, let path = root.findPath(target) else { return false }
        root = root.replacingNode(atPath: path) { node in
            let leaf = PaneNode.leaf(tabs: [newInstance], active: 0)
            let children = before ? [leaf, node] : [node, leaf]
            return .split(direction: direction, sizes: [1.0, 1.0], children: children)
        }
        root.normalize()
        tree = root
        return true
    }

    /// Pull the tab `dragged` out of its pane and place it as a new pane split beside
    /// `target`'s pane. Port of `move_into_split`. No-op if `dragged == target` or
    /// `dragged` is already the sole tab of `target`'s pane.
    @discardableResult
    static func moveIntoSplit(_ tree: inout PaneNode?, dragged: String, target: String,
                              direction: SplitDirection, before: Bool) -> Bool {
        if dragged == target { return false }
        guard let root = tree,
              let pd = root.findPath(dragged),
              let pt = root.findPath(target) else { return false }
        if pd == pt {
            let sole = (root.node(atPath: pd)?.leafTabs?.tabs.count ?? 1) == 1
            if sole { return false }
        }
        guard remove(&tree, target: dragged) else { return false }
        // `target` is guaranteed to survive (different leaf, or same leaf still ≥1 tab).
        return splitBeside(&tree, target: target, direction: direction, newInstance: dragged, before: before)
    }

    /// Move `dragged` to position `index` in `targetAnchor`'s pane, made active. Port
    /// of `move_tab_to`: same-leaf → reorder within the leaf; cross-leaf → detach and
    /// insert at `index`. No-op if absent or a same-slot move.
    @discardableResult
    static func moveTabTo(_ tree: inout PaneNode?, dragged: String, targetAnchor: String, index: Int) -> Bool {
        guard let root = tree,
              let pd = root.findPath(dragged),
              let pt = root.findPath(targetAnchor) else { return false }
        if pd == pt {
            guard case let .leaf(tabs, _)? = root.node(atPath: pd),
                  let src = tabs.firstIndex(of: dragged) else { return false }
            let dst = min(max(index, 0), tabs.count - 1)
            if dst == src { return false }
            var next = tabs
            next.remove(at: src)
            next.insert(dragged, at: dst)
            guard var newRoot = tree else { return false }
            newRoot = newRoot.replacingNode(atPath: pd) { _ in .leaf(tabs: next, active: dst) }
            tree = newRoot
            return true
        }
        guard remove(&tree, target: dragged) else { return false }
        return addTabAt(&tree, target: targetAnchor, newInstance: dragged, index: index)
    }

    /// Move `dragged` out of its pane and append it as the active tab of `target`'s
    /// pane (drag-to-tabify). Port of `move_into_tabs`. No-op if they're the same tab
    /// or already in the same pane.
    @discardableResult
    static func moveIntoTabs(_ tree: inout PaneNode?, dragged: String, target: String) -> Bool {
        if dragged == target { return false }
        guard let root = tree,
              let pd = root.findPath(dragged),
              let pt = root.findPath(target), pd != pt else { return false }
        guard remove(&tree, target: dragged) else { return false }
        return addTab(&tree, target: target, newInstance: dragged)
    }

    /// Append `newInstance` as the active last tab of `target`'s pane. Port of `add_tab`.
    @discardableResult
    static func addTab(_ tree: inout PaneNode?, target: String, newInstance: String) -> Bool {
        addTabAt(&tree, target: target, newInstance: newInstance, index: Int.max)
    }

    /// Insert `newInstance` at `index` (clamped) in `target`'s pane, made active.
    /// Port of `add_tab_at`, including the never-the-same-instance-twice guard.
    @discardableResult
    static func addTabAt(_ tree: inout PaneNode?, target: String, newInstance: String, index: Int) -> Bool {
        guard var root = tree else { return false }
        if root.findPath(newInstance) != nil { return false }  // no dupes anywhere in the tree
        guard let path = root.findPath(target) else { return false }
        guard case .leaf? = root.node(atPath: path) else { return false }
        root = root.replacingNode(atPath: path) { node in
            guard case let .leaf(tabs, _) = node else { return node }
            let at = min(max(index, 0), tabs.count)
            var next = tabs
            next.insert(newInstance, at: at)
            return .leaf(tabs: next, active: at)
        }
        tree = root
        return true
    }

    /// Set the sizes of the split matching `key` (its `stableKey`). Port of
    /// `set_split_sizes`.
    @discardableResult
    static func setSplitSizes(_ tree: inout PaneNode?, key: String, sizes: [Double]) -> Bool {
        guard let root = tree else { return false }
        let (node, changed) = root.settingSplitSizes(key: key, sizes: sizes)
        if changed { tree = node }
        return changed
    }

    /// Make `instance` the active tab of its pane. Port of `set_active_tab`.
    @discardableResult
    static func setActiveTab(_ tree: inout PaneNode?, instance: String) -> Bool {
        guard var root = tree, let path = root.findPath(instance) else { return false }
        guard case let .leaf(tabs, _)? = root.node(atPath: path) else { return false }
        guard let idx = tabs.firstIndex(of: instance) else { return false }
        root = root.replacingNode(atPath: path) { node in
            guard case let .leaf(tabs, _) = node else { return node }
            return .leaf(tabs: tabs, active: idx)
        }
        tree = root
        return true
    }

    /// Remove `target`: one tab of several (with active fixup), or the whole pane if
    /// it's the last tab (collapsing the tree, → nil if it was the last pane). Port of
    /// `remove`.
    @discardableResult
    static func remove(_ tree: inout PaneNode?, target: String) -> Bool {
        guard var root = tree, let path = root.findPath(target) else { return false }
        guard case let .leaf(tabs, active)? = root.node(atPath: path) else { return false }
        guard let idx = tabs.firstIndex(of: target) else { return false }
        var next = tabs
        next.remove(at: idx)
        if next.isEmpty {
            if path.isEmpty {
                tree = nil
                return true
            }
            root.removeAtPath(path)
            root.normalize()
            tree = root
            return true
        }
        let newActive: Int
        if idx < active { newActive = active - 1 }
        else if idx == active { newActive = min(active, next.count - 1) }
        else { newActive = active }
        root = root.replacingNode(atPath: path) { _ in .leaf(tabs: next, active: newActive) }
        tree = root
        return true
    }
}
