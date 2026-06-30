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

    /// Read-modify-write: append a freshly-launched instance as a tab. Seeds the
    /// document if none exists yet. Returns the written layout.
    @discardableResult
    static func appendInstance(_ conn: SSHConnection, root: String, instance: Instance) async throws -> RemoteLayout {
        var layout = (try await read(conn, root: root)) ?? RemoteLayout(remoteRoot: root)
        layout.addInstanceAsTab(instance, now: unixNow())
        try await write(conn, layout)
        return layout
    }

    /// Read-modify-write: drop an instance + its tab (after killing its session).
    @discardableResult
    static func removeInstance(_ conn: SSHConnection, root: String, instanceId: String) async throws -> RemoteLayout? {
        guard var layout = try await read(conn, root: root) else { return nil }
        layout.removeInstance(id: instanceId, now: unixNow())
        try await write(conn, layout)
        return layout
    }

    /// Read-modify-write: set an instance's custom display name (nil/blank clears it).
    @discardableResult
    static func renameInstance(_ conn: SSHConnection, root: String, instanceId: String, name: String?) async throws -> RemoteLayout? {
        guard var layout = try await read(conn, root: root) else { return nil }
        layout.renameInstance(id: instanceId, name: name, now: unixNow())
        try await write(conn, layout)
        return layout
    }
}
