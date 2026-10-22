import Foundation

/// tmux command builders. Each returns a tmux argument array; `commandLine(_:)`
/// turns it into a shell-quoted `tmux …` string to run over an SSH exec channel —
/// on muxel's own server, or on the server holding a session
/// (`commandLine(_:session:)`), or on both (`commandLineOnBothServers`).
///
/// Ports `new-session`/`kill-session` from `crates/muxel-core/src/tmux.rs`, plus
/// the read-only status/capture commands the iOS poller needs (these have no
/// desktop equivalent because the desktop reads status from its live PTY grid).
enum TmuxCommands {
    /// Dirs appended to a remote command's `PATH` so that `tmux` resolves. Port of
    /// `REMOTE_TMUX_PATH_DIRS` in `crates/muxel-core/src/ssh.rs` — keep the two in step.
    ///
    /// An SSH exec channel runs its command through a *non-interactive, non-login* shell,
    /// whose `PATH` is sshd's bare default (`/usr/bin:/bin:/usr/sbin:/sbin`) — no profile,
    /// no rc file. A Mac's tmux comes from Homebrew, which on Apple silicon lives in
    /// `/opt/homebrew/bin` and is on none of them: against such a host *every* tmux command
    /// fails with `command not found: tmux`, so a pane dies the moment it launches and the
    /// poller reads the host as having no sessions at all. Linux hosts never showed it — a
    /// distro tmux is `/usr/bin/tmux`, already on the bare PATH.
    ///
    /// The remote OS is unknown here, so macOS and Linux prefixes share a list; a `PATH`
    /// entry that doesn't exist is harmless. **Appended, never prepended**: a host that
    /// resolves `tmux` today keeps resolving the exact binary it resolves now.
    static let pathDirs = [
        "/opt/homebrew/bin",  // Homebrew, Apple silicon
        "/usr/local/bin",  // Homebrew, Intel macOS — and the usual local prefix
        "/opt/local/bin",  // MacPorts
        "/home/linuxbrew/.linuxbrew/bin",  // Linuxbrew
        "/snap/bin",
        "$HOME/.local/bin",
        "$HOME/bin",
        "$HOME/.nix-profile/bin",  // Nix
    ]

    /// The `PATH` assignment that must lead **every** remote command line naming `tmux`.
    /// Port of `ssh::tmux_path_prelude`; see `pathDirs` for why.
    ///
    /// Only `tmux` is resolved this way. A *program* (the agent) still goes through the
    /// user's login+interactive shell — see `launchAgent` — because it needs the user's
    /// whole environment, not merely a findable binary; tmux only needs to be found, and
    /// sourcing a user's rc files to find it would fold their startup output into the
    /// command results this app parses.
    ///
    /// `$PATH` and `$HOME` are expanded by the *remote* shell, so this is left unquoted.
    static var pathPrelude: String {
        "export PATH=\"$PATH:\(pathDirs.joined(separator: ":"))\""
    }

    // MARK: Servers

    /// The name of muxel's own tmux socket: `tmux -L mxl`. Port of `SOCKET_NAME` in
    /// `crates/muxel-core/src/tmux.rs` — keep the two in step.
    ///
    /// On the default socket muxel's sessions shared one server with the user's own tmux
    /// and with every tool that runs a bare `tmux …`, any of which could take that server
    /// down with every agent in it: RHEL 10's tmux 3.3a aborts in `capture-pane`. Every
    /// session this app creates goes on muxel's server. One still running on the default
    /// server from before muxel had its own is used where it is (`selectSocket`).
    ///
    /// Not `muxel`: the server keeps the command line of the client that started it
    /// (`startServer`), which names this socket, so a socket called `muxel` would let a
    /// `pkill -f muxel` on the host kill the server and every agent in it.
    static let socketName = "mxl"

    /// The client flags selecting muxel's server. Like `-u`, they go before the command.
    static var socketArgs: [String] { ["-L", socketName] }

    /// A shell fragment pointing `"$@"` at the server holding `session` — muxel's own,
    /// unless the session is running on the default server and not on muxel's — so the
    /// tmux words after it can be `tmux "$@" …`. Port of `ssh::select_tmux_socket`; both
    /// must produce the same text. Positional parameters, because zsh won't split a
    /// variable holding `-L mxl` into two words, and a PTY command has to `exec` tmux
    /// itself. Must follow `pathPrelude`.
    static func selectSocket(_ session: String, socket: String? = nil) -> String {
        if let socket {
            return socket == "default" ? "set --" : "set -- -L \(Shell.quote(socket))"
        }
        let target = Shell.quote("=\(session)")
        return "if ! tmux -L \(socketName) has-session -t \(target) 2>/dev/null "
            + "&& tmux has-session -t \(target) 2>/dev/null; "
            + "then set --; else set -- -L \(socketName); fi"
    }

    /// `tmux <args…>` on muxel's own server, as a single shell-quoted command line,
    /// behind the PATH that makes tmux resolvable (`pathPrelude`).
    static func commandLine(_ args: [String]) -> String {
        "\(pathPrelude); " + Shell.command(["tmux"] + socketArgs + args)
    }

    /// `tmux <args…>` on the server holding `session` (`selectSocket`).
    static func commandLine(_ args: [String], session: String, socket: String? = nil) -> String {
        "\(pathPrelude); \(selectSocket(session, socket: socket)); tmux \"$@\" " + Shell.command(args)
    }

    /// `tmux <args…>` on both servers, muxel's own first: listings, which must find a
    /// pane from before muxel had its own server. For read-only discovery only. A
    /// server that isn't running contributes nothing; the line fails only when there is
    /// no tmux to run at all.
    static func commandLineOnBothServers(_ args: [String]) -> String {
        "\(pathPrelude); { " + Shell.command(["tmux"] + socketArgs + args) + "; "
            + Shell.command(["tmux"] + args) + "; } 2>/dev/null; command -v tmux >/dev/null"
    }

    struct SessionTarget: Equatable {
        var name: String
        var socket: String
    }

    struct DefaultMigration: Equatable {
        let recordedSession: String?
        let sessionName: String
    }

    enum ProbeError: LocalizedError, Equatable {
        case incomplete
        case ownershipRefreshRequired

        var errorDescription: String? {
            switch self {
            case .incomplete:
                return "Couldn't confirm both tmux servers. Reconnect and try again."
            case .ownershipRefreshRequired:
                return "A session with this name is already running on another tmux server, so this pane was not started. Try again once another muxel sharing this project's layout has assigned that session to it, or after stopping that session."
            }
        }
    }

    /// Whether tmux can use `name` as `-L <name>`: a plain file name in its socket
    /// directory, so not empty, `.` or `..`, and holding no `/` or NUL (`-L` joins
    /// the name onto that directory, so a path would reach a server elsewhere). A
    /// recorded owner that isn't one decodes as none (`Instance`), which means
    /// legacy discovery. Port of `Socket::try_from` in `crates/muxel-core/src/tmux.rs`.
    static func isSocketName(_ name: String) -> Bool {
        !name.isEmpty && name != "." && name != ".." && !name.contains("/") && !name.contains("\0")
    }

    /// The checked listing a pane recorded on the default server needs: muxel's
    /// own server, then the default one, each answer closed by its exit status.
    /// Names only, one per line, so no session name is split. Port of
    /// `ssh::list_session_names_command_on` over both servers.
    static func confirmedTargetsCommand() -> String {
        "export LC_ALL=C; " + [socketName, "default"].map { server in
            commandLine(["list-sessions", "-F", "#{session_name}"], session: "", socket: server)
                + " 2>&1; echo \"\(listingEnd)$?\""
        }.joined(separator: "; ")
    }

    private static let listingEnd = ":muxel-listed:"

    /// `confirmedTargetsCommand`'s output, one answer per server (muxel's own, then
    /// the default one): the sessions it listed, `[]` for a server confirmed not
    /// running, or `nil` for any other reply, one cut off before that server
    /// answered included. A server's answer stands whatever the other said.
    /// `complete` is whether both answered and nothing but whitespace followed.
    ///
    /// A listing that exited 0 holds one exact session name per line. An empty
    /// line is no name tmux lists, so it spoils that server's answer. Lines end in
    /// `\n` or `\r\n`, as Rust's `str::lines` reads them. Port of
    /// `ssh::checked_session_listings_on` — keep both in step.
    static func parseConfirmedListings(_ output: String) -> (answers: [[SessionTarget]?], complete: Bool) {
        let servers = [socketName, "default"]
        var answers: [[SessionTarget]?] = []
        var listing: [String] = []
        var complete = true
        // "\r\n" is one Character, so it needs naming beside "\n".
        for raw in output.split(omittingEmptySubsequences: false, whereSeparator: { $0 == "\n" || $0 == "\r\n" }) {
            let line = String(raw)
            guard line.hasPrefix(listingEnd) else {
                listing.append(line); continue
            }
            if answers.count < servers.count {
                answers.append(checkedListing(listing, status: String(line.dropFirst(listingEnd.count)),
                                              server: servers[answers.count]))
            } else {
                complete = false  // more answers than servers asked
            }
            listing = []
        }
        complete = complete && answers.count == servers.count
            && listing.allSatisfy { $0.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty }
        while answers.count < servers.count { answers.append(nil) }
        return (answers, complete)
    }

    private static func checkedListing(_ listing: [String], status: String, server: String) -> [SessionTarget]? {
        let status = status.trimmingCharacters(in: .whitespacesAndNewlines)
        if status == "0" {
            guard !listing.contains(where: { $0.isEmpty }) else { return nil }
            return listing.map { SessionTarget(name: $0, socket: server) }
        }
        // A malformed status is not a failed tmux command, even when its output
        // resembles an absent server.
        guard let code = UInt8(status), code != 0,
              confirmedNoServer(listing.joined(separator: "\n").trimmingCharacters(in: .whitespacesAndNewlines))
        else { return nil }
        return []
    }

    /// Default migration needs both complete replies. Ordinary best-effort
    /// discovery cannot distinguish an empty server from a permission failure.
    /// Port of `ssh::parse_session_listings_for_migration`.
    static func parseConfirmedTargets(_ output: String) throws -> [SessionTarget] {
        let (answers, complete) = parseConfirmedListings(output)
        let confirmed = answers.compactMap { $0 }
        guard complete, confirmed.count == answers.count else { throw ProbeError.incomplete }
        return confirmed.flatMap { $0 }
    }

    /// What a pane recorded on the default server attaches to or moves from. A
    /// session still listed there under `name` is reattached on that answer alone,
    /// whatever muxel's own server replied (an rc file's echo, an unfamiliar
    /// error): only moving it off Default (`targetForNewSession`) needs both
    /// servers' checked answers. Port of `ssh::still_on_default`.
    static func defaultOwnerTargets(_ output: String, name: String) throws -> [SessionTarget] {
        if let onDefault = parseConfirmedListings(output).answers[1],
           onDefault.contains(where: { $0.name == name }) {
            return onDefault
        }
        return try parseConfirmedTargets(output)
    }

    private static func confirmedNoServer(_ message: String) -> Bool {
        guard !message.contains("\n") else { return false }
        let absent = "no server running on "
        if message.hasPrefix(absent) { return message.count > absent.count }
        let connecting = "error connecting to "
        guard message.hasPrefix(connecting) else { return false }
        return [" (No such file or directory)", " (Connection refused)"].contains { suffix in
            message.hasSuffix(suffix) && message.count > connecting.count + suffix.count
        }
    }

    /// Called only after checked inventory. A Private namesake stops the plan
    /// until saved ownership is refreshed; inventory alone cannot identify it.
    static func targetForNewSession(_ instance: Instance, name: String, confirmedTargets: [SessionTarget])
        throws -> (target: SessionTarget, migration: DefaultMigration?) {
        let owner = instance.tmuxSocket ?? socketName
        if owner == "default",
           !confirmedTargets.contains(where: { $0.name == name && $0.socket == "default" }) {
            guard !confirmedTargets.contains(where: { $0.name == name && $0.socket == socketName }) else {
                throw ProbeError.ownershipRefreshRequired
            }
            return (.init(name: name, socket: socketName),
                    DefaultMigration(recordedSession: instance.tmuxSession, sessionName: name))
        }
        return (.init(name: name, socket: owner), nil)
    }

    /// Keep server identity beside every name, including duplicate names.
    static func listTargetsCommand(socket: String? = nil) -> String {
        let servers = socket.map { [$0] } ?? [socketName, "default"]
        let commands = servers.map { server in
            commandLine(["list-sessions", "-F", "\(server)\t#{session_name}"],
                        session: "", socket: server) + " 2>/dev/null"
        }
        return commands.joined(separator: "; ") + "; command -v tmux >/dev/null"
    }

    static func parseTargets(_ output: String) -> [SessionTarget] {
        output.split(separator: "\n").compactMap { line in
            let fields = line.split(separator: "\t", maxSplits: 1)
            guard fields.count == 2 else { return nil }
            return SessionTarget(name: String(fields[1]), socket: String(fields[0]))
        }
    }

    static func resolveTarget(_ instance: Instance, in targets: [SessionTarget]) -> SessionTarget? {
        resolveTarget(instance, in: targets, name: { $0.name }, socket: { $0.socket })
    }

    static func resolveTarget<Row>(_ instance: Instance, in rows: [Row],
                                   name: (Row) -> String, socket: (Row) -> String?) -> Row? {
        let eligible = rows.filter { instance.tmuxSocket == nil || socket($0) == instance.tmuxSocket }
        return eligible.first { name($0) == instance.tmuxSession }
            ?? eligible.first { TmuxSession.session(name($0), matchesInstanceId: instance.id) }
    }

    // MARK: Lifecycle

    /// Start the remote tmux server *before* any session exists, from a command line
    /// that names no project. Port of `start_server_args` in `crates/muxel-core/src/tmux.rs`
    /// — both must produce the same argv.
    ///
    /// tmux forks its server from whichever client first needs one, and the server keeps
    /// that client's command line. One server hosts every muxel session on the host —
    /// shared by this app and muxel desktop's SSH panes (muxel's own server, run through
    /// `commandLine`, which names `socketName`). So if the first
    /// client is `tmux new-session -A -s muxel_<project>_… -c <project root>`, the shared
    /// server's argv names a project, and an agent on that host running
    /// `pkill -f <project>` to clear its dev server SIGKILLs the server and every agent
    /// in every session with it.
    ///
    /// `exit-empty off` is required: by default a server with no sessions exits at once,
    /// so `start-server` alone would evaporate before the first `new-session` and the
    /// server would be re-forked with the project name back in its argv.
    ///
    /// `Shell.quote(";")` renders as `';'`, so the remote shell hands tmux a literal `;`
    /// argument (its command separator) rather than splitting the line itself.
    static func startServer() -> [String] {
        ["start-server", ";", "set", "-s", "exit-empty", "off"]
    }

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

    /// The variable that tells a program desktop muxel starts which pane it runs in.
    /// Port of `INSTANCE_ID_ENV` in `crates/muxel-core/src/control.rs`.
    static let instanceIdEnv = "MUXEL_INSTANCE_ID"

    /// `set-environment -g -u MUXEL_INSTANCE_ID ';'`: the tmux commands that lead every
    /// `new-session` this app sends, removing `instanceIdEnv` from the server's global
    /// environment first. Port of the `unset` half of `launch_session_args_with_env`
    /// (`crates/muxel-core/src/tmux.rs`), which desktop's SSH panes run before theirs.
    ///
    /// tmux starts a new session's program from its *server's* environment, not the
    /// client's. A host's one server is shared, and one first started from inside a
    /// desktop muxel pane on that host holds that pane's id — which an agent launched
    /// here would then take as its own, recording its conversation against that other
    /// pane, and typing into it when it asks `muxel ctl` to type into "itself".
    ///
    /// Harmless when the variable isn't set, and needs no server: `new-session` in the
    /// same command list starts one. A session `-A` attaches to keeps the environment it
    /// started with. Shell text: the `';'` reaches tmux as its command separator.
    static var clearInstanceId: String {
        "set-environment -g -u \(instanceIdEnv) ';'"
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
    /// it relies on the remote shell expanding `$SHELL`. Clears a stale pane id from the
    /// server first (`clearInstanceId`).
    static func launchAgent(session: String, cwd: String, program: String?, args: [String]) -> String {
        var cmd =
            "\(pathPrelude); \(selectSocket(session)); tmux \"$@\" \(clearInstanceId) "
            + "new-session -d -s \(Shell.quote(session)) -c \(Shell.quote(cwd))"
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
    /// don't bleed through under tmux's redraw. On the server holding the session
    /// (`selectSocket`).
    static func attachPTYCommand(session: String, socket: String? = nil) -> String {
        "\(pathPrelude); \(selectSocket(session, socket: socket)); clear; "
            + "exec tmux \"$@\" attach-session -t \(Shell.quote("=\(session)"))"
    }

    /// Raw PTY command: `exec tmux new-session -A` **attached** (no `-d`), running
    /// `program` in a login shell. Attached-at-init is the whole point: interactive
    /// TUI agents (claude) crash if they initialize with no client; over a live PTY
    /// they get one, exactly like desktop's `ssh -t`. `program == nil` → default shell.
    /// Clears a stale pane id from the server first (`clearInstanceId`). A new session
    /// goes on muxel's own server; `-A` reattaches one wherever it runs (`selectSocket`).
    static func newAttachedPTYCommand(session: String, cwd: String, program: String?, args: [String], socket: String? = nil) -> String {
        // `clear;` wipes the login banner + echoed command before tmux takes over.
        var cmd =
            "\(pathPrelude); \(selectSocket(session, socket: socket)); clear; exec tmux \"$@\" \(clearInstanceId) "
            + "new-session -A -s \(Shell.quote(session)) -c \(Shell.quote(cwd))"
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
    static func allPaneStatuses(socket: String? = nil) -> [String] {
        ["list-panes", "-a", "-F",
         "#{session_name}\t#{pane_dead}\t#{window_bell_flag}\t#{window_activity}" + (socket.map { "\t\($0)" } ?? "")]
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
