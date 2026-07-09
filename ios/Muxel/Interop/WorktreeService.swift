import Foundation

enum WorktreeError: Error, Equatable {
    case notGitRepo
    case git(String)

    var message: String {
        switch self {
        case .notGitRepo: return "not a git repository"
        case let .git(msg): return msg
        }
    }
}

/// Creates git worktrees on the remote over the SSH exec channel. Desktop has no
/// remote-creation path today, so iOS *defines* the convention (base dir + `muxel/<id8>`
/// branch); desktop then adopts the `Worktree` record from `workspace.json` and can
/// delete it by its stored path. Naming comes from `WorktreeNaming` (the interop port).
enum WorktreeService {
    /// One shell script that creates the worktree and **always exits 0**, printing a
    /// parseable `MUXEL_WT_OK <path>` / `MUXEL_WT_ERR <msg>` line — because
    /// `SSHConnection.run` throws only when stdout is empty, never on a non-zero exit.
    /// The base dir is resolved on the remote (`$XDG_DATA_HOME` / `$HOME`), so the full
    /// path is echoed back in the OK line for iOS to store.
    static func createCommand(root: String, dirName: String, branch: String) -> String {
        let r = Shell.quote(root)
        let b = Shell.quote(branch)
        let d = Shell.quote(dirName)
        return """
        root=\(r); branch=\(b); dir=\(d)
        if ! git -C "$root" rev-parse --is-inside-work-tree >/dev/null 2>&1; then printf 'MUXEL_WT_ERR %s' 'not a git repository'; exit 0; fi
        base="${XDG_DATA_HOME:-$HOME/.local/share}/muxel/worktrees"
        mkdir -p "$base" || { printf 'MUXEL_WT_ERR %s' "cannot create $base"; exit 0; }
        path="$base/$dir"
        if err=$(git -C "$root" worktree add -b "$branch" "$path" 2>&1 >/dev/null); then
          # Best-effort: trust the new worktree for mise/direnv so their configs (and the
          # tool PATHs they set up) load instead of erroring on an "untrusted" new path.
          # Runs a login shell so mise/direnv are found; all output suppressed so it can't
          # corrupt the result line; failures (tool absent) ignored.
          "${SHELL:-/bin/sh}" -lc "mise trust $path 2>/dev/null; direnv allow $path 2>/dev/null" >/dev/null 2>&1 || true
          printf 'MUXEL_WT_OK %s' "$path"
        else
          printf 'MUXEL_WT_ERR %s' "$err"
        fi
        """
    }

    static func parseResult(_ output: String) -> Result<String, WorktreeError> {
        let s = output.trimmingCharacters(in: .whitespacesAndNewlines)
        if let r = s.range(of: "MUXEL_WT_OK ") {
            return .success(String(s[r.upperBound...]).trimmingCharacters(in: .whitespacesAndNewlines))
        }
        if let r = s.range(of: "MUXEL_WT_ERR ") {
            let msg = String(s[r.upperBound...]).trimmingCharacters(in: .whitespacesAndNewlines)
            return .failure(msg == "not a git repository" ? .notGitRepo : .git(msg))
        }
        return .failure(.git(s.isEmpty ? "unknown error" : s))
    }

    /// Create the worktree; returns its absolute remote path or throws `WorktreeError`.
    static func create(_ conn: SSHConnection, root: String, dirName: String, branch: String) async throws -> String {
        let out = try await conn.run(createCommand(root: root, dirName: dirName, branch: branch))
        switch parseResult(out) {
        case let .success(path): return path
        case let .failure(err): throw err
        }
    }

    /// Best-effort rollback if the launch fails after the worktree was created. Full
    /// worktree deletion UX stays desktop-side. Always exits 0.
    static func removeCommand(root: String, path: String, branch: String) -> String {
        let r = Shell.quote(root), p = Shell.quote(path), b = Shell.quote(branch)
        return "git -C \(r) worktree remove --force \(p) 2>/dev/null; "
            + "git -C \(r) worktree prune 2>/dev/null; git -C \(r) branch -D \(b) 2>/dev/null; true"
    }
}
