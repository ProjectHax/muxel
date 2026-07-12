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
    /// Whether the project's shared memory (`.muxel/MEMORY.md`) is switched on.
    /// Shared state: the file lives on the host and every agent there — desktop's
    /// panes and this app's — reads and writes the same one, so the flag travels with
    /// the project rather than living on one machine.
    ///
    /// Optional, and it must be *carried through even when we don't set it*: a doc
    /// written before the field existed says "no opinion" (desktop then infers it
    /// from the memory file), and re-encoding without it would erase an opinion
    /// desktop had already recorded.
    var memoryEnabled: Bool?

    private enum CodingKeys: String, CodingKey {
        case version
        case updatedAt = "updated_at"
        case remoteRoot = "remote_root"
        case memoryEnabled = "memory_enabled"
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
        memoryEnabled = try c.decodeIfPresent(Bool.self, forKey: .memoryEnabled)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(version, forKey: .version)
        try c.encode(updatedAt, forKey: .updatedAt)
        try c.encode(remoteRoot, forKey: .remoteRoot)
        try c.encode(layout, forKey: .layout)
        try c.encode(instances, forKey: .instances)
        try c.encode(worktrees, forKey: .worktrees)
        // `encodeIfPresent`: absent stays absent, so "no opinion" is preserved rather
        // than being written back as `false`.
        try c.encodeIfPresent(memoryEnabled, forKey: .memoryEnabled)
    }

    /// Whether this document is consumable for `root` (correct version + root).
    func isValid(forRoot root: String) -> Bool {
        version == RemoteLayout.currentVersion && remoteRoot == root
    }

    /// The terminal instances referenced by the layout, in tab order — the input for
    /// polling / status / Live Activity (terminal-only).
    var orderedTerminalInstances: [Instance] {
        let byId = Dictionary(instances.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        return (layout?.allTabs ?? []).compactMap { byId[$0] }.filter { $0.kind == .terminal }
    }

    /// Every pane instance in tab order, including editor/diff/browser — what the UI
    /// lists so desktop-created editor/diff panes appear as tabs too.
    var orderedPaneInstances: [Instance] {
        let byId = Dictionary(instances.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        return (layout?.allTabs ?? []).compactMap { byId[$0] }
    }

    // MARK: Mutations (for launching/closing from the phone)

    /// Append `instance` and add it as a new active tab in the first leaf (or seed
    /// the layout if empty). Optionally into a specific `targetLeafAnchor` (any tab in
    /// the destination leaf). Stamps `updatedAt` so newer-wins picks it up.
    mutating func addInstanceAsTab(_ instance: Instance, now: Int, targetLeafAnchor: String? = nil) {
        instances.append(instance)
        // Prefer the requested leaf; else the first leaf; else seed a fresh tree.
        if let anchor = targetLeafAnchor ?? layout?.firstInstance {
            _ = PaneTree.addTab(&layout, target: anchor, newInstance: instance.id)
        } else {
            layout = .leaf(tabs: [instance.id], active: 0)
        }
        updatedAt = now
    }

    /// Drop an instance + its tab (e.g. after killing its session). Uses the ported
    /// `remove` so the active-index fixup + split collapse match desktop exactly.
    mutating func removeInstance(id: String, now: Int) {
        instances.removeAll { $0.id == id }
        _ = PaneTree.remove(&layout, target: id)
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

