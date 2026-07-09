import Foundation

/// Pure git-worktree naming — a Swift port of `crates/muxel-core/src/worktree.rs`.
/// The branch/dir names are part of the interop contract (desktop adopts iOS-created
/// worktrees from `workspace.json`), so they must match byte-for-byte.
enum WorktreeNaming {
    private static let adjectives = [
        "amber", "azure", "bold", "brave", "bright", "calm", "crisp", "deft", "eager", "fleet",
        "fresh", "glad", "keen", "kind", "lush", "merry", "nimble", "proud", "quick", "quiet", "rapid",
        "sage", "sharp", "solar", "steady", "swift", "teal", "warm", "wise", "zesty",
    ]
    private static let nouns = [
        "acorn", "beacon", "bloom", "brook", "cedar", "crest", "dawn", "delta", "dune", "echo", "fern",
        "fjord", "flint", "forge", "frost", "glade", "grove", "haven", "ledge", "mesa", "mist", "peak",
        "pine", "prism", "reef", "ridge", "spark", "spire", "tide", "vale",
    ]

    /// A filesystem-safe slug: ASCII-alnum kept, everything else `-`, trimmed, empty →
    /// `repo`. Port of `slug`.
    static func slug(_ name: String) -> String {
        var out = String.UnicodeScalarView()
        for scalar in name.unicodeScalars {
            let v = scalar.value
            let isAlnum = (v >= 0x30 && v <= 0x39) || (v >= 0x41 && v <= 0x5A) || (v >= 0x61 && v <= 0x7A)
            out.append(isAlnum ? scalar : "-")
        }
        let trimmed = String(out).trimmingCharacters(in: CharacterSet(charactersIn: "-"))
        return trimmed.isEmpty ? "repo" : trimmed
    }

    /// The branch muxel creates for an instance's worktree: `muxel/<id8>`.
    static func branchName(instanceId: String) -> String {
        "muxel/\(TmuxSession.uuid8(instanceId))"
    }

    /// Worktree directory name: `<repo-slug>_<id8>`.
    static func dirName(repoName: String, instanceId: String) -> String {
        "\(slug(repoName))_\(TmuxSession.uuid8(instanceId))"
    }

    /// Full worktree path under `base`.
    static func worktreePath(base: String, repoName: String, instanceId: String) -> String {
        "\(base)/\(dirName(repoName: repoName, instanceId: instanceId))"
    }

    /// A random `adjective-noun` display name (collisions are harmless — display only).
    static func randomName() -> String {
        let adj = adjectives.randomElement() ?? "swift"
        let noun = nouns.randomElement() ?? "pine"
        return "\(adj)-\(noun)"
    }

    /// The lowest unused color slot 0..<8 among a project's live (non-detached)
    /// worktrees, wrapping to 0. Port of `next_worktree_color`.
    static func nextColor(worktrees: [Worktree], projectId: String) -> Int {
        let used = Set(worktrees.filter { $0.projectId == projectId && !$0.detached }.map(\.color))
        return (0..<8).first { !used.contains($0) } ?? 0
    }
}
