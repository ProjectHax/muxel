import Foundation

/// Configured JSON coders for the muxel remote-layout interop format. Formatting
/// need not match Rust byte-for-byte — desktop parses the document and recomputes
/// its change-detection key from the parsed values (UUIDs are lowercase strings, so
/// values match). Pretty + sorted keys keeps our own diffs stable.
enum MuxelJSON {
    static let decoder = JSONDecoder()
    static let encoder: JSONEncoder = {
        let e = JSONEncoder()
        e.outputFormatting = [.prettyPrinted, .sortedKeys, .withoutEscapingSlashes]
        return e
    }()
}

/// The per-project layout snapshot shared with desktop muxel at
/// `<remote_root>/.muxel/workspace.json`. Port of `RemoteLayout`
/// (`crates/muxel-core/src/lib.rs`). `version` must be 1 and `remoteRoot` must match
/// before consuming; `updatedAt` (unix seconds) drives newer-wins merge.
struct RemoteLayout: Codable, Equatable {
    static let currentVersion = 1

    var version: Int = RemoteLayout.currentVersion
    var updatedAt: Int = 0
    var remoteRoot: String
    var layout: PaneNode?
    var instances: [Instance] = []
    var worktrees: [Worktree] = []

    private enum CodingKeys: String, CodingKey {
        case version
        case updatedAt = "updated_at"
        case remoteRoot = "remote_root"
        case layout, instances, worktrees
    }

    init(remoteRoot: String, layout: PaneNode? = nil, instances: [Instance] = [], worktrees: [Worktree] = []) {
        self.remoteRoot = remoteRoot
        self.layout = layout
        self.instances = instances
        self.worktrees = worktrees
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        version = try c.decode(Int.self, forKey: .version)
        updatedAt = (try c.decodeIfPresent(Int.self, forKey: .updatedAt)) ?? 0
        remoteRoot = try c.decode(String.self, forKey: .remoteRoot)
        layout = try c.decodeIfPresent(PaneNode.self, forKey: .layout)
        instances = (try c.decodeIfPresent([Instance].self, forKey: .instances)) ?? []
        worktrees = (try c.decodeIfPresent([Worktree].self, forKey: .worktrees)) ?? []
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(version, forKey: .version)
        try c.encode(updatedAt, forKey: .updatedAt)
        try c.encode(remoteRoot, forKey: .remoteRoot)
        try c.encode(layout, forKey: .layout)
        try c.encode(instances, forKey: .instances)
        try c.encode(worktrees, forKey: .worktrees)
    }

    /// Whether this document is consumable for `root` (correct version + root).
    func isValid(forRoot root: String) -> Bool {
        version == RemoteLayout.currentVersion && remoteRoot == root
    }

    /// The terminal instances referenced by the layout, in tab order — what the
    /// sidebar/detail lists.
    var orderedTerminalInstances: [Instance] {
        let byId = Dictionary(instances.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        return (layout?.allTabs ?? []).compactMap { byId[$0] }.filter { $0.kind == .terminal }
    }

    // MARK: Mutations (for launching/closing from the phone)

    /// Append `instance` and add it as a new active tab in the first leaf (or seed
    /// the layout if empty). Stamps `updatedAt` so newer-wins picks it up. The MVP
    /// adds to the main pane; rearranging is left to desktop.
    mutating func addInstanceAsTab(_ instance: Instance, now: Int) {
        instances.append(instance)
        if let existing = layout {
            layout = existing.addingTab(instance.id)
        } else {
            layout = .leaf(tabs: [instance.id], active: 0)
        }
        updatedAt = now
    }

    /// Drop an instance + its tab (e.g. after killing its session).
    mutating func removeInstance(id: String, now: Int) {
        instances.removeAll { $0.id == id }
        layout = layout?.removingTab(id)
        updatedAt = now
    }

    /// Set an instance's custom display name (a nil/blank name clears it, falling
    /// back to the title). Stamps `updatedAt` so newer-wins picks it up.
    mutating func renameInstance(id: String, name: String?, now: Int) {
        guard let idx = instances.firstIndex(where: { $0.id == id }) else { return }
        let trimmed = name?.trimmingCharacters(in: .whitespacesAndNewlines)
        instances[idx].customName = (trimmed?.isEmpty ?? true) ? nil : trimmed
        updatedAt = now
    }
}

extension PaneNode {
    /// Add `id` as a new tab in the first leaf (depth-first), made active.
    func addingTab(_ id: String) -> PaneNode {
        switch self {
        case let .leaf(tabs, _):
            let next = tabs + [id]
            return .leaf(tabs: next, active: next.count - 1)
        case let .split(direction, sizes, children):
            guard let first = children.first else { return .leaf(tabs: [id], active: 0) }
            return .split(direction: direction, sizes: sizes,
                          children: [first.addingTab(id)] + children.dropFirst())
        }
    }

    /// Remove `id` wherever it appears; returns nil if the subtree becomes empty.
    func removingTab(_ id: String) -> PaneNode? {
        switch self {
        case let .leaf(tabs, active):
            let next = tabs.filter { $0 != id }
            if next.isEmpty { return nil }
            return .leaf(tabs: next, active: min(active, next.count - 1))
        case let .split(direction, sizes, children):
            let kept = zip(children, sizes).compactMap { child, size -> (PaneNode, Double)? in
                guard let pruned = child.removingTab(id) else { return nil }
                return (pruned, size)
            }
            if kept.isEmpty { return nil }
            if kept.count == 1 { return kept[0].0 }
            return .split(direction: direction, sizes: kept.map(\.1), children: kept.map(\.0))
        }
    }
}
