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

    /// Raw `tmux new-session` command line that launches `program` inside the user's
    /// **login + interactive** shell (`$SHELL -ilc 'exec …'`), so the agent resolves
    /// on the user's real PATH (nvm / npm-global / `~/.local/bin`, …).
    ///
    /// iOS runs commands over a *no-PTY* SSH exec, which has only a bare login PATH —
    /// so launching the agent directly (`-- claude`) fails to find it and the pane
    /// exits instantly, taking the session (and, if it's the only one, the server)
    /// with it. With no `program`, tmux starts its own default login shell, so no
    /// wrapping is needed. Returned as a shell string (not a quoted arg array) because
    /// it relies on the remote shell expanding `$SHELL`.
    static func launchAgent(session: String, cwd: String, program: String?, args: [String]) -> String {
        var cmd = "tmux new-session -d -s \(Shell.quote(session)) -c \(Shell.quote(cwd))"
        if let program {
            let inner = "exec " + Shell.command([program] + args)
            cmd += " -- \"${SHELL:-/bin/sh}\" -ilc \(Shell.quote(inner))"
        }
        return cmd
    }

    /// Attach to a session interactively (used on a PTY channel for live view).
    static func attach(session: String) -> [String] {
        ["attach-session", "-t", "=\(session)"]
    }

    /// Raw PTY command: `exec tmux attach` to an existing session, so the PTY's shell
    /// *becomes* tmux (the pane is the live terminal). For a session that already
    /// exists (created by desktop or a prior launch). The PTY is opened at the phone's
    /// real grid size (see `TerminalSession`), so the attaching client is correctly
    /// sized from the start. `clear;` wipes the login banner + echoed command so they
    /// don't bleed through under tmux's redraw.
    static func attachPTYCommand(session: String) -> String {
        "clear; exec tmux attach-session -t \(Shell.quote("=\(session)"))"
    }

    /// Raw PTY command: `exec tmux new-session -A` **attached** (no `-d`), running
    /// `program` in a login shell. Attached-at-init is the whole point: interactive
    /// TUI agents (claude) crash if they initialize with no client; over a live PTY
    /// they get one, exactly like desktop's `ssh -t`. `program == nil` → default shell.
    static func newAttachedPTYCommand(session: String, cwd: String, program: String?, args: [String]) -> String {
        // `clear;` wipes the login banner + echoed command before tmux takes over.
        var cmd = "clear; exec tmux new-session -A -s \(Shell.quote(session)) -c \(Shell.quote(cwd))"
        if let program {
            let inner = "exec " + Shell.command([program] + args)
            cmd += " -- \"${SHELL:-/bin/sh}\" -ilc \(Shell.quote(inner))"
        }
        return cmd
    }

    // MARK: Read-only status (for polling without an attached PTY)

    /// Pane/window target for an existing session: exact-match the session (`=`) and
    /// resolve its active window + pane (trailing `:`). The bare `=name` form fails
    /// for pane/window targets ("can't find pane" / "no such window") and makes
    /// `display-message` return empty fields — the `:` is required. Session-target
    /// commands (kill/attach) use the bare `=name` instead.
    static func paneTarget(_ session: String) -> String { "=\(session):" }

    /// List muxel-owned session names, one per line.
    static func listSessions() -> [String] {
        ["list-sessions", "-F", "#{session_name}"]
    }

    /// The visible pane content as plain text — the marker-scan input
    /// (`visible_text` equivalent).
    static func capturePane(session: String) -> [String] {
        ["capture-pane", "-p", "-t", paneTarget(session)]
    }

    /// Tab-delimited `pane_dead<TAB>window_bell_flag<TAB>window_activity` for a
    /// session: exited flag, bell flag, and last-activity unix seconds.
    static func paneStatus(session: String) -> [String] {
        ["display-message", "-p", "-t", paneTarget(session),
         "#{pane_dead}\t#{window_bell_flag}\t#{window_activity}"]
    }

    /// Tab-delimited status for **every** pane on the host in a single round trip:
    /// `session_name<TAB>pane_dead<TAB>window_bell_flag<TAB>window_activity`. Since
    /// muxel sessions are host-global (one tmux server per host user), one call
    /// classifies every project on the host — the batched replacement for a
    /// `list-sessions` + N per-session `paneStatus` reads. `list-panes -a` needs no
    /// target; a host with no running server exits non-zero (treat as empty).
    static func allPaneStatuses() -> [String] {
        ["list-panes", "-a", "-F",
         "#{session_name}\t#{pane_dead}\t#{window_bell_flag}\t#{window_activity}"]
    }

    /// Clear a window's bell flag once we've acted on it (attend equivalent).
    static func clearBell(session: String) -> [String] {
        ["set-option", "-w", "-t", paneTarget(session), "monitor-bell", "off"]
    }

    /// Enable tmux mouse mode for the session so a touch drag scrolls the pane's
    /// scrollback (tmux copy mode) — the panes run full-screen (alternate screen), so
    /// there's no terminal-native scrollback to swipe; the history lives in tmux.
    /// `mouse` is a session option; the `=name:` target resolves it to that session (a
    /// bare `=name` fails with "no such session" for set-option).
    static func setMouseOn(session: String) -> [String] {
        ["set-option", "-t", paneTarget(session), "mouse", "on"]
    }

    /// Let apps inside tmux put text on the phone clipboard via OSC 52 — tmux
    /// forwards the escape to the attached client, where SwiftTerm's
    /// `clipboardCopy` delegate lands it in `UIPasteboard`. A server option
    /// (`-s`), enabled best-effort on attach alongside `setMouseOn`.
    static func setClipboardOn() -> [String] {
        ["set-option", "-s", "set-clipboard", "on"]
    }

    // MARK: Input

    /// Send literal text to a session's active pane (`-l` = literal, no key-name
    /// translation). Used when driving a pane without a full PTY attach.
    static func sendLiteral(session: String, text: String) -> [String] {
        ["send-keys", "-t", paneTarget(session), "-l", text]
    }

    /// Send a named key (e.g. "Enter", "Escape", "C-c") to a session's pane.
    static func sendKey(session: String, key: String) -> [String] {
        ["send-keys", "-t", paneTarget(session), key]
    }
}
