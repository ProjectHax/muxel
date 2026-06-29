import Foundation

/// tmux command builders. Each returns a tmux argument array; `commandLine(_:)`
/// turns it into a shell-quoted `tmux …` string to run over an SSH exec channel.
///
/// Ports `new-session`/`kill-session` from `crates/muxel-core/src/tmux.rs`, plus
/// the read-only status/capture commands the iOS poller needs (these have no
/// desktop equivalent because the desktop reads status from its live PTY grid).
enum TmuxCommands {
    /// `tmux <args…>` as a single shell-quoted command line.
    static func commandLine(_ args: [String]) -> String {
        Shell.command(["tmux"] + args)
    }

    // MARK: Lifecycle

    /// Create a session running `program`+`args` in `cwd`.
    ///
    /// - `detached`: add `-d` so the session persists with no attached client
    ///   (the iOS app attaches separately). The desktop omits `-d` (it attaches
    ///   its foreground PTY directly).
    /// - `attachOrCreate`: add `-A` (create-or-attach idempotency). Leave `false`
    ///   for a brand-new instance id — combining `-A` with `-d` can detach other
    ///   clients (e.g. the desktop) from an existing session.
    static func newSession(
        session: String,
        cwd: String?,
        program: String?,
        args: [String] = [],
        detached: Bool = true,
        attachOrCreate: Bool = false
    ) -> [String] {
        var v = ["new-session"]
        if attachOrCreate { v.append("-A") }
        if detached { v.append("-d") }
        v += ["-s", session]
        if let cwd { v += ["-c", cwd] }
        if let program {
            v.append("--")
            v.append(program)
            v += args
        }
        return v
    }

    /// Kill a session (exact-match `=` target). Port of `kill_session_args`.
    static func killSession(_ session: String) -> [String] {
        ["kill-session", "-t", "=\(session)"]
    }

    /// Attach to a session interactively (used on a PTY channel for live view).
    static func attach(session: String) -> [String] {
        ["attach-session", "-t", "=\(session)"]
    }

    // MARK: Read-only status (for polling without an attached PTY)

    /// List muxel-owned session names, one per line.
    static func listSessions() -> [String] {
        ["list-sessions", "-F", "#{session_name}"]
    }

    /// The visible pane content as plain text — the marker-scan input
    /// (`visible_text` equivalent).
    static func capturePane(session: String) -> [String] {
        ["capture-pane", "-p", "-t", "=\(session)"]
    }

    /// Tab-delimited `pane_dead<TAB>window_bell_flag<TAB>window_activity` for a
    /// session: exited flag, bell flag, and last-activity unix seconds.
    static func paneStatus(session: String) -> [String] {
        ["display-message", "-p", "-t", "=\(session)",
         "#{pane_dead}\t#{window_bell_flag}\t#{window_activity}"]
    }

    /// Clear a window's bell flag once we've acted on it (attend equivalent).
    static func clearBell(session: String) -> [String] {
        ["set-option", "-w", "-t", "=\(session)", "monitor-bell", "off"]
    }

    // MARK: Input

    /// Send literal text to a session's active pane (`-l` = literal, no key-name
    /// translation). Used when driving a pane without a full PTY attach.
    static func sendLiteral(session: String, text: String) -> [String] {
        ["send-keys", "-t", "=\(session)", "-l", text]
    }

    /// Send a named key (e.g. "Enter", "Escape", "C-c") to a session's pane.
    static func sendKey(session: String, key: String) -> [String] {
        ["send-keys", "-t", "=\(session)", key]
    }
}
