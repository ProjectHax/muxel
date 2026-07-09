import Foundation

/// Current unix seconds (the `updated_at` clock).
func unixNow() -> Int { Int(Date().timeIntervalSince1970) }

/// Reads/writes the shared `<remote_root>/.muxel/workspace.json` over an SSH
/// connection — the peer-with-desktop interop. Ports muxel's remote-sync
/// (`crates/muxel/src/integrations.rs`): the size-guarded read, the `.muxel/` prep
/// (mkdir + backup + gitignore), and an atomic write. Mutations re-read immediately
/// before writing (newer-wins).
enum RemoteLayoutStore {
    static func path(_ root: String) -> String { "\(root)/.muxel/workspace.json" }

    /// Read + parse the layout if present and valid for `root`; nil if absent/empty.
    static func read(_ conn: SSHConnection, root: String) async throws -> RemoteLayout? {
        let f = path(root)
        // Port of read_remote_file: only cat when present and <= 2 MB.
        let cmd = "f=\(Shell.quote(f)); if [ -f \"$f\" ] && [ \"$(wc -c < \"$f\")\" -le 2000000 ]; then cat \"$f\"; fi"
        let json = try await conn.run(cmd).trimmingCharacters(in: .whitespacesAndNewlines)
        guard !json.isEmpty, let data = json.data(using: .utf8) else { return nil }
        let layout = try MuxelJSON.decoder.decode(RemoteLayout.self, from: data)
        guard layout.isValid(forRoot: root) else { return nil }
        return layout
    }

    /// Prepare `.muxel/` and atomically write `layout`. Port of remote_push_prep_cmd
    /// + write_remote_file (using a quoted heredoc instead of ssh stdin so it works
    /// over a one-shot exec channel; portable POSIX sh).
    static func write(_ conn: SSHConnection, _ layout: RemoteLayout) async throws {
        let root = layout.remoteRoot
        let prep = "cd \(Shell.quote(root)) && mkdir -p .muxel"
            + " && { test -f .muxel/workspace.json && cp -f .muxel/workspace.json .muxel/workspace.bak.json || true; }"
            + " && { grep -qxF .muxel/ .gitignore 2>/dev/null || printf '%s\\n' .muxel/ >> .gitignore; }"
        _ = try await conn.run(prep)

        let data = try MuxelJSON.encoder.encode(layout)
        let json = String(data: data, encoding: .utf8) ?? "{}"
        // Random delimiter avoids any collision with the JSON body; quoted heredoc
        // prevents shell expansion so the bytes are written verbatim.
        let delim = "MUXEL_EOF_\(UUID().uuidString.replacingOccurrences(of: "-", with: "").prefix(8))"
        let cmd = "cat > \(Shell.quote(path(root))) <<'\(delim)'\n\(json)\n\(delim)\n"
        _ = try await conn.run(cmd)
    }

    /// Generic read-modify-write. Reads the freshest layout (seeding an empty one for
    /// `root`), applies `transform` (returns `false` → nothing changed, skip the
    /// write), stamps a **strictly increasing** `updatedAt`, and writes. Returns the
    /// written layout, or nil if the transform declined.
    ///
    /// Concurrency: the read-immediately-before-write + the `.bak` backup + desktop's
    /// strictly-newer-wins adoption bound the lost-update window to one round trip
    /// (the loser survives in `workspace.bak.json`) — the same guarantee the shipped
    /// add/rename/close paths already rely on. AppState additionally serializes the
    /// phone's own mutations per project so two rapid ones can't interleave.
    @discardableResult
    static func mutate(_ conn: SSHConnection, root: String, seedIfMissing: Bool = true,
                       transform: (inout RemoteLayout) -> Bool) async throws -> RemoteLayout? {
        let existing = try await read(conn, root: root)
        // Only the append path seeds a document; remove/rename/split no-op on a missing
        // layout rather than creating an empty workspace.json.
        guard var layout = existing ?? (seedIfMissing ? RemoteLayout(remoteRoot: root) : nil) else { return nil }
        let before = layout.updatedAt
        guard transform(&layout) else { return nil }
        // Monotonic: two mutations in the same wall-clock second must still increase,
        // or desktop's `remote.updated_at > local` newer-wins would drop the second.
        layout.updatedAt = max(unixNow(), before + 1)
        try await write(conn, layout)
        return layout
    }

    /// Read-modify-write: append a freshly-launched instance as a tab, optionally into
    /// a specific leaf (`targetLeafAnchor`). Seeds the document if none exists yet.
    @discardableResult
    static func appendInstance(_ conn: SSHConnection, root: String, instance: Instance,
                               targetLeafAnchor: String? = nil,
                               worktree: Worktree? = nil) async throws -> RemoteLayout {
        let written = try await mutate(conn, root: root) {
            $0.addInstanceAsTab(instance, now: unixNow(), targetLeafAnchor: targetLeafAnchor)
            // The instance and its worktree land in ONE write, so desktop adopts both
            // together (never an instance referencing a not-yet-written worktree).
            if let worktree { $0.worktrees.append(worktree) }
            return true
        }
        return written ?? RemoteLayout(remoteRoot: root)
    }

    /// Read-modify-write: drop an instance + its tab (after killing its session).
    @discardableResult
    static func removeInstance(_ conn: SSHConnection, root: String, instanceId: String) async throws -> RemoteLayout? {
        try await mutate(conn, root: root, seedIfMissing: false) {
            $0.removeInstance(id: instanceId, now: unixNow()); return true
        }
    }

    /// Read-modify-write: set an instance's custom display name (nil/blank clears it).
    @discardableResult
    static func renameInstance(_ conn: SSHConnection, root: String, instanceId: String, name: String?) async throws -> RemoteLayout? {
        try await mutate(conn, root: root, seedIfMissing: false) {
            $0.renameInstance(id: instanceId, name: name, now: unixNow()); return true
        }
    }

    /// Read-modify-write: relocate `dragged`'s tab into a new split beside `target`
    /// (the iPad "open in split" action). No-op on a missing layout or an invalid move.
    @discardableResult
    static func moveIntoSplit(_ conn: SSHConnection, root: String, dragged: String, target: String,
                              direction: SplitDirection, before: Bool) async throws -> RemoteLayout? {
        try await mutate(conn, root: root, seedIfMissing: false) {
            PaneTree.moveIntoSplit(&$0.layout, dragged: dragged, target: target,
                                   direction: direction, before: before)
        }
    }

    /// Read-modify-write: move `dragged`'s tab into `target`'s pane (drag-to-tabify).
    @discardableResult
    static func moveIntoTabs(_ conn: SSHConnection, root: String, dragged: String,
                             target: String) async throws -> RemoteLayout? {
        try await mutate(conn, root: root, seedIfMissing: false) {
            PaneTree.moveIntoTabs(&$0.layout, dragged: dragged, target: target)
        }
    }

    /// Read-modify-write: move `dragged` to `index` in `targetAnchor`'s pane (reorder
    /// within a group, or insert-at-position when dropped on another group's tab).
    @discardableResult
    static func moveTabTo(_ conn: SSHConnection, root: String, dragged: String,
                          targetAnchor: String, index: Int) async throws -> RemoteLayout? {
        try await mutate(conn, root: root, seedIfMissing: false) {
            PaneTree.moveTabTo(&$0.layout, dragged: dragged, targetAnchor: targetAnchor, index: index)
        }
    }

    /// Read-modify-write: set the sizes of the split identified by `key`.
    @discardableResult
    static func setSplitSizes(_ conn: SSHConnection, root: String, key: String,
                              sizes: [Double]) async throws -> RemoteLayout? {
        try await mutate(conn, root: root, seedIfMissing: false) {
            PaneTree.setSplitSizes(&$0.layout, key: key, sizes: sizes)
        }
    }
}
