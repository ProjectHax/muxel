import Foundation

/// Discovers muxel projects on a host by scanning the filesystem for the per-project
/// marker `<root>/.muxel/workspace.json` that desktop muxel writes when it syncs a
/// (remote) project's layout. Powers the "Scan for projects" action so a phone paired
/// with a remote dev box can pull in projects without typing each path by hand.
///
/// This is the remote-dev-box case: muxel desktop runs elsewhere (e.g. a laptop) and
/// treats this host as a remote, so the host has the per-project `.muxel/` markers but
/// no desktop muxel config dir to read.
enum ProjectDiscovery {
    /// A discovered project root on the host.
    struct Found: Identifiable, Hashable {
        let remoteRoot: String
        var id: String { remoteRoot }
        /// Display name = the root's last path component.
        var name: String { (remoteRoot as NSString).lastPathComponent }
    }

    /// Default search roots, shell-expanded on the remote. `$HOME` covers the common
    /// case; quoted so a home dir with spaces still works while `$HOME` expands.
    static let defaultRoots = ["\"$HOME\""]

    /// Path suffix of the marker we scan for.
    static let marker = "/.muxel/workspace.json"

    /// Heavy directories to prune so `find` stays fast over a one-shot exec channel.
    private static let pruned = ["node_modules", ".git", ".cache", ".cargo", ".rustup",
                                 ".npm", "target", "vendor", "Library", ".Trash"]

    /// Build the remote `find` command line. Exposed for testing the exact shell.
    static func findCommand(roots: [String] = defaultRoots) -> String {
        let pruneExpr = pruned.map { "-name \(Shell.quote($0))" }.joined(separator: " -o ")
        let rootsExpr = roots.joined(separator: " ")
        return "find \(rootsExpr) -maxdepth 7 \\( \(pruneExpr) \\) -prune -o "
            + "-type f -path \(Shell.quote("*\(marker)")) -print 2>/dev/null"
    }

    /// Parse `find` output (one marker path per line) into deduped, sorted roots.
    static func parse(_ output: String) -> [Found] {
        var seen = Set<String>()
        var found: [Found] = []
        for line in output.split(whereSeparator: \.isNewline) {
            let path = line.trimmingCharacters(in: .whitespaces)
            guard path.hasSuffix(marker) else { continue }
            let root = String(path.dropLast(marker.count))
            guard !root.isEmpty, seen.insert(root).inserted else { continue }
            found.append(Found(remoteRoot: root))
        }
        return found.sorted { $0.remoteRoot < $1.remoteRoot }
    }

    /// Scan `roots` on the host for project markers.
    static func scan(_ conn: SSHConnection, roots: [String] = defaultRoots) async throws -> [Found] {
        let output = try await conn.run(findCommand(roots: roots))
        return parse(output)
    }
}
