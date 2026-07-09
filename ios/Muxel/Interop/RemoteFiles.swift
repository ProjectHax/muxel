import Foundation

/// Read-only remote file + git-diff fetchers over the SSH exec channel, for the
/// editor/diff pane viewers. Ports `read_remote_file` / `git_diff`
/// (`crates/muxel/src/integrations.rs`).
enum RemoteFiles {
    static let maxBytes = 2_000_000

    /// Size-guarded cat of a remote file — same shape as `read_remote_file` (only cat
    /// when it exists and is ≤ 2 MB). Empty output for absent/too-large files.
    static func readCommand(path: String) -> String {
        let f = Shell.quote(path)
        return "f=\(f); if [ -f \"$f\" ] && [ \"$(wc -c < \"$f\")\" -le \(maxBytes) ]; then cat \"$f\"; fi"
    }

    static func read(_ conn: SSHConnection, path: String) async throws -> String {
        try await conn.run(readCommand(path: path))
    }

    /// Git diff for `dir`, mirroring `git_diff`: a `rev-parse --show-toplevel` probe
    /// (doubles as the "is this a repo?" check), a header noting a subfolder, then
    /// `diff HEAD` (falling back to the worktree diff when there are no commits),
    /// capped at `maxBytes`. Always exits 0 and prints its result.
    static func diffCommand(dir: String) -> String {
        let d = Shell.quote(dir)
        return """
        dir=\(d)
        if ! top=$(git -C "$dir" rev-parse --show-toplevel 2>/dev/null); then printf '# %s\\n\\nNot a git repository.\\n' "$dir"; exit 0; fi
        out=$(git -C "$dir" --no-pager diff HEAD --no-color -- . 2>/dev/null)
        if [ -z "$out" ]; then out=$(git -C "$dir" --no-pager diff --no-color -- . 2>/dev/null); fi
        out=$(printf '%s' "$out" | head -c \(maxBytes))
        printf '# Changes in %s\\n' "$dir"
        if [ "$top" != "$dir" ]; then printf '# (subfolder of git repo %s — changes under this folder only)\\n' "$top"; fi
        printf '\\n'
        if [ -z "$out" ]; then printf 'No changes.\\n'; else printf '%s\\n' "$out"; fi
        """
    }

    static func diff(_ conn: SSHConnection, dir: String) async throws -> String {
        try await conn.run(diffCommand(dir: dir))
    }
}
