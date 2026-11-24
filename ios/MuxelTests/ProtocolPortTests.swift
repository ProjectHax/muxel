import XCTest
@testable import muxel

/// These mirror the Rust unit tests for the interop-critical ports. If they drift
/// from `muxel-core`, the iOS app and desktop will compute different tmux session
/// names / agent statuses and stop peering correctly.
final class ProtocolPortTests: XCTestCase {

    // MARK: tmux session naming (port of tmux.rs tests)

    func testSessionNameSanitizedAndStable() {
        let id = UUID(uuidString: "00000000-0000-0000-0000-000000000000")!
        let name = TmuxSession.name(hostName: "My Project!", instanceId: id)
        XCTAssertTrue(name.hasPrefix("muxel_My_Project_"))
        XCTAssertEqual(name, TmuxSession.name(hostName: "My Project!", instanceId: id))
    }

    func testSessionNameEmptySlugFallsBackToP() {
        let id = UUID(uuidString: "12345678-0000-0000-0000-000000000000")!
        let name = TmuxSession.name(hostName: "!!!", instanceId: id)
        XCTAssertEqual(name, "muxel_p_12345678")
    }

    func testSessionNameUsesLowercaseUuid8() {
        let id = UUID(uuidString: "ABCDEF12-3456-7890-0000-000000000000")!
        let name = TmuxSession.name(hostName: "h", instanceId: id)
        XCTAssertEqual(name, "muxel_h_abcdef12")
    }

    func testSessionMatchesInstanceBySuffix() {
        let id = UUID(uuidString: "ABCDEF12-3456-7890-0000-000000000000")!
        XCTAssertTrue(TmuxSession.session("muxel_DesktopHost_abcdef12", matchesInstance: id))
        XCTAssertTrue(TmuxSession.session("muxel_phone_abcdef12", matchesInstance: id))
        XCTAssertFalse(TmuxSession.session("muxel_h_deadbeef", matchesInstance: id))
        XCTAssertFalse(TmuxSession.session("other_h_abcdef12", matchesInstance: id))
    }

    // MARK: tmux commands (port of tmux.rs tests)

    func testNewSessionWrapsProgramAfterSeparator() {
        let args = TmuxCommands.newSession(
            session: "muxel_p_123", cwd: "/work",
            program: "claude", args: ["--flag", "x"]
        )
        XCTAssertEqual(args, [
            "new-session", "-d", "-s", "muxel_p_123",
            "-c", "/work", "--", "claude", "--flag", "x",
        ])
    }

    func testNewSessionWithoutProgram() {
        let args = TmuxCommands.newSession(session: "s", cwd: "/work", program: nil)
        XCTAssertEqual(args, ["new-session", "-d", "-s", "s", "-c", "/work"])
        XCTAssertFalse(args.contains("--"))
    }

    func testKillUsesExactMatchTarget() {
        XCTAssertEqual(TmuxCommands.killSession("s"), ["kill-session", "-t", "=s"])
    }

    // Port of `start_server_args`. The tmux server inherits the command line of the
    // client that forks it, and one server hosts every session on the host — so it must
    // be started from an argv that names no project, before any `new-session` runs.
    func testStartServerNamesNoProject() {
        XCTAssertEqual(
            TmuxCommands.startServer(),
            ["start-server", ";", "set", "-s", "exit-empty", "off"])
        // The `;` has to reach tmux as a literal argument (tmux's command separator);
        // if the remote shell ate it, `set -s exit-empty off` would run as a shell
        // command and the server would exit the moment it had no sessions.
        // Started on muxel's own server (`-L mxl`), whose name is chosen so that the
        // server's command line still names no project, no session, and not muxel.
        XCTAssertEqual(
            TmuxCommands.commandLine(TmuxCommands.startServer()),
            "\(TmuxCommands.pathPrelude); 'tmux' '-L' 'mxl' 'start-server' ';' 'set' '-s' 'exit-empty' 'off'")
        XCTAssertFalse(TmuxCommands.commandLine(TmuxCommands.startServer()).contains("muxel"))
    }

    // Port of `SOCKET_NAME` / `ssh::select_tmux_socket` (tmux.rs, ssh.rs). Desktop and
    // the phone must agree on the server: a session one creates on muxel's own socket
    // is invisible to a client still asking the default one.
    func testSessionsLiveOnMuxelsOwnServer() {
        XCTAssertEqual(TmuxCommands.socketName, "mxl")
        // Byte-for-byte the desktop's fragment: the default server only for a session
        // running there and not on muxel's own (a pane from before), else muxel's own.
        XCTAssertEqual(
            TmuxCommands.selectSocket("muxel_app_1"),
            "if ! tmux -L mxl has-session -t '=muxel_app_1' 2>/dev/null "
                + "&& tmux has-session -t '=muxel_app_1' 2>/dev/null; "
                + "then set --; else set -- -L mxl; fi")
        let select = TmuxCommands.selectSocket("s")
        XCTAssertEqual(
            TmuxCommands.commandLine(TmuxCommands.setMouseOn(session: "s"), session: "s"),
            "\(TmuxCommands.pathPrelude); \(select); tmux \"$@\" 'set-option' '-t' '=s:' 'mouse' 'on'")
        XCTAssertEqual(
            TmuxCommands.attachPTYCommand(session: "s"),
            "\(TmuxCommands.pathPrelude); \(TmuxCommands.utf8ColorPrelude); \(select); "
                + "\(TmuxCommands.enableTrueColorCommand); clear; exec tmux -u \"$@\" attach-session -t '=s'")
        for socket in ["mxl", "default"] {
            let cmd = TmuxCommands.commandLine(TmuxCommands.killSession("s"), session: "s", socket: socket)
            XCTAssertFalse(cmd.contains("has-session"))
            XCTAssertEqual(cmd.components(separatedBy: "kill-session").count - 1, 1)
            XCTAssertTrue(cmd.contains("; \(TmuxCommands.selectSocket("s", socket: socket)); tmux \"$@\""))
        }
    }

    func testSocketOwnerSurvivesRoundTripAndDuplicateNames() throws {
        let raw = #"{"id":"abcdef12-0000-4000-8000-000000000000","project_id":"p","title":"work","tmux_session":"work","tmux_socket":"default"}"#
        let instance = try JSONDecoder().decode(Instance.self, from: Data(raw.utf8))
        let restored = try JSONDecoder().decode(Instance.self, from: JSONEncoder().encode(instance))
        XCTAssertEqual(restored.tmuxSocket, "default")
        let targets = TmuxCommands.parseTargets("mxl\twork\ndefault\twork\n")
        XCTAssertEqual(targets.count, 2)
        XCTAssertEqual(TmuxCommands.resolveTarget(restored, in: targets), targets[1])
        XCTAssertNil(TmuxCommands.resolveTarget(restored, in: [targets[0]]))
        let cmd = TmuxCommands.attachPTYCommand(session: "work", socket: restored.tmuxSocket)
        XCTAssertFalse(cmd.contains("has-session"))
        XCTAssertFalse(cmd.contains("-L"))
    }

    // Port of `a_session_another_pane_still_shows_outlives_the_close`: two panes on
    // one session, their records disagreeing about the server. Closing either leaves
    // it to the other; only the last pane to close stops it.
    func testASessionAnotherPaneStillShowsOutlivesTheClose() {
        let name = "muxel_p_12345678"
        var original = Instance(id: "12345678-0000-4000-8000-000000000000", projectId: "p",
                                title: "Claude", program: "claude", args: [])
        original.tmuxSession = name
        original.tmuxSocket = "default"
        var adopted = Instance(id: "abcdef12-0000-4000-8000-000000000000", projectId: "p",
                               title: "Claude", program: "claude", args: [])
        adopted.tmuxSession = name
        adopted.tmuxSocket = TmuxCommands.socketName
        let both = [original, adopted]
        XCTAssertTrue(TmuxCommands.sessionStillShown(name, closing: [adopted.id], in: both))
        XCTAssertTrue(TmuxCommands.sessionStillShown(name, closing: [original.id], in: both))
        XCTAssertFalse(TmuxCommands.sessionStillShown(name, closing: [original.id, adopted.id], in: both))
        XCTAssertFalse(TmuxCommands.sessionStillShown("muxel_p_87654321", closing: [original.id], in: both))
        var literal = adopted
        literal.literalArgv = true
        XCTAssertFalse(TmuxCommands.sessionStillShown(name, closing: [original.id], in: [original, literal]))
    }

    // sshd runs an exec channel's command with its bare default PATH and no profile, so a
    // Mac's Homebrew tmux (`/opt/homebrew/bin/tmux` on Apple silicon) is not on it and every
    // tmux command fails with `command not found: tmux`. Port of `ssh::tmux_path_prelude`.
    func testTmuxResolvesWhenTheSSHPathIsBare() {
        let prelude = TmuxCommands.pathPrelude
        // Appended, so a host that already resolves tmux keeps the binary it has.
        XCTAssertTrue(prelude.hasPrefix("export PATH=\"$PATH:"))
        XCTAssertTrue(prelude.contains("/opt/homebrew/bin"))
        // `$HOME` is expanded by the remote shell, so it must stay unquoted.
        XCTAssertTrue(prelude.contains("$HOME/.local/bin"))

        // Every command line that names tmux is behind it — exec channels and PTY alike.
        for cmd in [
            TmuxCommands.commandLine(TmuxCommands.listSessions()),
            TmuxCommands.launchAgent(session: "s", cwd: "/w", program: "claude", args: []),
            TmuxCommands.attachPTYCommand(session: "s"),
            TmuxCommands.newAttachedPTYCommand(session: "s", cwd: "/w", program: nil, args: []),
        ] {
            XCTAssertTrue(cmd.hasPrefix("\(prelude); "), "no PATH prelude: \(cmd)")
            // Nothing names tmux before the PATH it is looked up on is set.
            XCTAssertFalse(cmd.dropLast(cmd.count - prelude.count).contains("tmux"))
        }
    }

    // Mirrors pane_scope.rs `a_remote_program_runs_in_a_scope_when_the_host_can_make_one`.
    func testRemoteProgramRunsInAScopeWhenTheHostCanMakeOne() {
        let program = "\"${SHELL:-/bin/sh}\" -ilc 'exec claude'"
        XCTAssertEqual(
            TmuxCommands.remoteExec(program),
            "if command -v systemd-run >/dev/null 2>&1 && "
                + "systemd-run --user --scope --quiet --collect -p OOMPolicy=continue true </dev/null >/dev/null 2>&1; "
                + "then exec systemd-run --user --scope --quiet --collect -p OOMPolicy=continue "
                + "'--description=muxel pane' -- \(program); fi; exec \(program)")
        let cmd = TmuxCommands.launchAgent(session: "s", cwd: "/w", program: "claude", args: [])
        XCTAssertTrue(cmd.contains("OOMPolicy=continue"))
    }

    // iOS must launch the agent through a login+interactive shell so it's on the
    // user's PATH (a no-PTY SSH exec has a bare PATH → the agent exits instantly).
    func testLaunchAgentWrapsInLoginShell() {
        let cmd = TmuxCommands.launchAgent(
            session: "muxel_h_abcdef12", cwd: "/work", program: "claude", args: ["--model", "opus"])
        XCTAssertTrue(cmd.hasPrefix(
            "\(TmuxCommands.pathPrelude); \(TmuxCommands.utf8ColorPrelude); \(TmuxCommands.selectSocket("muxel_h_abcdef12")); "
            + "mkdir -p -- '/work'; "
            + "tmux -u \"$@\" \(TmuxCommands.clearProfilerEnvironment()) set-environment -g -u MUXEL_INSTANCE_ID ';' "
            + "new-session -d -s 'muxel_h_abcdef12' -c '/work' "
            + "-- sh -c "))
        XCTAssertTrue(cmd.contains("@muxel_control_pane"))
        XCTAssertTrue(cmd.contains("${SHELL:-/bin/sh}"))
        XCTAssertTrue(cmd.contains("-ilc"))
        XCTAssertTrue(cmd.contains("exec"))
        XCTAssertTrue(cmd.contains("claude"))
        XCTAssertTrue(cmd.contains("opus"))
        // No program → tmux's own default login shell, no wrapping.
        XCTAssertEqual(
            TmuxCommands.launchAgent(session: "s", cwd: "/w", program: nil, args: []),
            "\(TmuxCommands.pathPrelude); \(TmuxCommands.utf8ColorPrelude); \(TmuxCommands.selectSocket("s")); mkdir -p -- '/w'; "
                + "tmux -u \"$@\" \(TmuxCommands.clearProfilerEnvironment()) set-environment -g -u MUXEL_INSTANCE_ID ';' "
                + "new-session -d -s 's' -c '/w'")
    }

    // Port of the `unset` half of `launch_session_args_with_env` (tmux.rs), which
    // desktop's SSH panes run before `new-session`. A new session's program starts from
    // the tmux *server's* environment; one first started inside a desktop muxel pane on
    // the host holds that pane's id, and an agent launched here would take it as its own.
    func testNewSessionsClearAStalePaneId() {
        XCTAssertEqual(TmuxCommands.instanceIdEnv, "MUXEL_INSTANCE_ID")
        XCTAssertEqual(
            TmuxCommands.newAttachedPTYCommand(session: "s", cwd: "/w", program: nil, args: []),
            "\(TmuxCommands.pathPrelude); \(TmuxCommands.utf8ColorPrelude); \(TmuxCommands.selectSocket("s")); "
                + "\(TmuxCommands.enableTrueColorCommand); mkdir -p -- '/w'; clear; "
                + "exec tmux -u \"$@\" \(TmuxCommands.clearProfilerEnvironment()) set-environment -g -u MUXEL_INSTANCE_ID ';' "
                + "new-session -A -s 's' -c '/w'")
        // Cleared in the same tmux command list, ahead of the session it would leak into.
        let clearThenCreate = "set-environment -g -u MUXEL_INSTANCE_ID ';' new-session "
        for cmd in [
            TmuxCommands.launchAgent(session: "s", cwd: "/w", program: "claude", args: []),
            TmuxCommands.newAttachedPTYCommand(session: "s", cwd: "/w", program: "claude", args: []),
        ] {
            XCTAssertTrue(cmd.contains(clearThenCreate), "no clear before new-session: \(cmd)")
        }
        // Attaching to a session leaves the environment it started with alone.
        XCTAssertFalse(TmuxCommands.attachPTYCommand(session: "s").contains("set-environment"))
    }

    func testPTYCommandsEnableUTF8AndTrueColorGradients() {
        for cmd in [
            TmuxCommands.attachPTYCommand(session: "s"),
            TmuxCommands.newAttachedPTYCommand(session: "s", cwd: "/w", program: "claude", args: []),
        ] {
            XCTAssertTrue(cmd.contains("export TERM=xterm-256color COLORTERM=truecolor"), "missing TERM/COLORTERM: \(cmd)")
            XCTAssertTrue(cmd.contains("C.UTF-8"), "missing Linux UTF-8 fallback: \(cmd)")
            XCTAssertTrue(cmd.contains("en_US.UTF-8"), "missing macOS UTF-8 fallback: \(cmd)")
            XCTAssertTrue(cmd.contains("default-terminal tmux-256color"), "missing 256-color default-terminal: \(cmd)")
            XCTAssertTrue(cmd.contains("update-environment COLORTERM"), "missing COLORTERM update-environment: \(cmd)")
            XCTAssertTrue(cmd.contains("xterm-256color:Tc"), "missing tmux Tc override: \(cmd)")
            XCTAssertTrue(cmd.contains("xterm-256color:RGB"), "missing tmux RGB feature: \(cmd)")
            XCTAssertTrue(cmd.contains("exec tmux -u "), "missing tmux -u UTF-8 flag: \(cmd)")
        }
        let launched = TmuxCommands.newAttachedPTYCommand(session: "s", cwd: "/w", program: "claude", args: [])
        XCTAssertTrue(launched.contains("export TERM=tmux-256color"), "inner pane should upgrade 8-color TERM: \(launched)")
        let ro = TmuxCommands.attachReadOnlyPTYCommand(session: "s", socket: "mxl")
        XCTAssertTrue(ro.contains("exec tmux -u \"$@\" attach-session -r -t '=s'"))
        XCTAssertFalse(ro.contains("capture-pane"))
    }

    func testCommandLineShellQuotes() {
        let line = TmuxCommands.commandLine(TmuxCommands.capturePane(session: "muxel_h_abcdef12"))
        XCTAssertEqual(
            line,
            "\(TmuxCommands.pathPrelude); 'tmux' '-L' 'mxl' 'capture-pane' '-p' '-t' '=muxel_h_abcdef12:'")
    }

    func testProfilerCleanupCoversFutureKeysWithoutRemovingPaneContext() {
        let clear = TmuxCommands.clearProfilerEnvironment(environmentNames: [
            "MUXEL_PROFILE_FUTURE", "MUXEL_INSTANCE_ID", "MUXEL_USER_DATA_DIR", "MUXEL_CONTROL",
        ])
        for name in ["MUXEL_PROFILE", "MUXEL_PROFILE_FUTURE", "MUXEL_PROFILE_KEEP_DAYS", "MUXEL_PROFILE_MAX_BYTES"] {
            XCTAssertTrue(clear.contains("set-environment -g -u \(Shell.quote(name)) ';'"))
        }
        for name in ["MUXEL_INSTANCE_ID", "MUXEL_USER_DATA_DIR", "MUXEL_CONTROL"] {
            XCTAssertFalse(clear.contains(name))
        }
    }

    // Pane/window-target commands must use `=session:` (active pane of the session).
    // The bare `=session` form fails on real tmux ("can't find pane" / "no such
    // window") and makes display-message return empty fields; session-target commands
    // (kill/attach) keep the bare `=session`.
    func testPaneTargetsUseTrailingColon() {
        let s = "muxel_h_abcdef12"
        XCTAssertEqual(TmuxCommands.capturePane(session: s).suffix(1), ["=\(s):"])
        XCTAssertEqual(TmuxCommands.paneStatus(session: s)[3], "=\(s):")
        XCTAssertTrue(TmuxCommands.sendKey(session: s, key: "Enter").contains("=\(s):"))
        XCTAssertTrue(TmuxCommands.sendLiteral(session: s, text: "x").contains("=\(s):"))
        XCTAssertTrue(TmuxCommands.clearBell(session: s).contains("=\(s):"))
        // Session-target commands stay on the bare `=session` (no colon).
        XCTAssertEqual(TmuxCommands.killSession(s), ["kill-session", "-t", "=\(s)"])
    }

    // MARK: classify (port of view.rs classify_priority)

    func testClassifyPriority() {
        let working = ["esc to interrupt"]
        let blocked = ["Do you want to proceed"]
        // exit wins over a stale working marker.
        XCTAssertEqual(
            classify(exited: true, screen: "esc to interrupt", working: working, blocked: blocked, bell: true, idle: 0),
            .done
        )
        // working marker beats a stale bell.
        XCTAssertEqual(
            classify(exited: false, screen: "… esc to interrupt", working: working, blocked: blocked, bell: true, idle: 10),
            .working
        )
        // blocked marker (no working marker on screen) beats the bell.
        XCTAssertEqual(
            classify(exited: false, screen: "Do you want to proceed?", working: working, blocked: blocked, bell: true, idle: 10),
            .blocked
        )
        // bell with markers configured but none on screen → done.
        XCTAssertEqual(
            classify(exited: false, screen: "all done", working: working, blocked: blocked, bell: true, idle: 10),
            .done
        )
        // marker agent, quiet, no bell → idle (heuristic disabled).
        XCTAssertEqual(
            classify(exited: false, screen: "", working: working, blocked: blocked, bell: false, idle: 10),
            .idle
        )
    }

    func testClassifyMarkerlessHeuristic() {
        // No markers: recent output → working; quiet → idle.
        XCTAssertEqual(classify(exited: false, screen: "", working: [], blocked: [], bell: false, idle: 0.1), .working)
        XCTAssertEqual(classify(exited: false, screen: "", working: [], blocked: [], bell: false, idle: 10), .idle)
        // Bell on a marker-less terminal → done.
        XCTAssertEqual(classify(exited: false, screen: "", working: [], blocked: [], bell: true, idle: 10), .done)
    }

    // MARK: latch (port of view.rs done_latch tests)

    func testDoneLatch() {
        // working → idle (no bell) latches done, and holds.
        XCTAssertEqual(latchDone(prevRaw: .working, raw: .idle, latched: false, canLatch: true).status, .done)
        XCTAssertEqual(latchDone(prevRaw: .idle, raw: .idle, latched: true, canLatch: true).status, .done)
        // working again clears it.
        XCTAssertEqual(latchDone(prevRaw: .idle, raw: .working, latched: true, canLatch: true).status, .working)
        // bell/exit done passes through.
        XCTAssertEqual(latchDone(prevRaw: .working, raw: .done, latched: false, canLatch: true).status, .done)
        // marker-less terminals never latch.
        XCTAssertEqual(latchDone(prevRaw: .working, raw: .idle, latched: false, canLatch: false).status, .idle)
    }

    func testPaneStatusTrackerLatchesAndAttends() {
        var t = PaneStatusTracker()
        let working = ["esc to interrupt"]
        // Working tick.
        XCTAssertEqual(t.update(exited: false, screen: "esc to interrupt", working: working, blocked: [], bell: false, idle: 0), .working)
        // Turn ends (marker gone, no bell) → latched done.
        XCTAssertEqual(t.update(exited: false, screen: "", working: working, blocked: [], bell: false, idle: 10), .done)
        // Holds done across quiet ticks.
        XCTAssertEqual(t.update(exited: false, screen: "", working: working, blocked: [], bell: false, idle: 20), .done)
        // Attend → drops back to idle.
        t.attend()
        XCTAssertEqual(t.update(exited: false, screen: "", working: working, blocked: [], bell: false, idle: 30), .idle)
    }

    // MARK: markers (port of default_markers)

    func testDefaultMarkers() {
        XCTAssertEqual(defaultMarkers(program: "claude").working, ["esc to interrupt"])
        XCTAssertEqual(defaultMarkers(program: "/usr/bin/claude").working, ["esc to interrupt"])
        XCTAssertEqual(defaultMarkers(program: "opencode").blocked, ["Permission required"])
        XCTAssertTrue(defaultMarkers(program: "bash").working.isEmpty)
        XCTAssertTrue(defaultMarkers(program: nil).working.isEmpty)
    }


    // MARK: Grok question card (port of view.rs is_grok_question)

    /// Grok's unanswered `ask_user_question` card, from desktop's
    /// `provider-state-screens.json` (`providers/grok/blocked`).
    private let grokQuestionCard = [
        "     ♦ Thought for 6.6s",
        "     ♦ Ask Which option do you want?",
        "   ♦ Waiting on answers for Which option do you want?                  9.2s ↓13.7k [stop]",
        "  │",
        "  │  Which option do you want?",
        "  │",
        "  │  1 (○) Alpha (Recommended)  Choose Alpha.                                     █",
        "  │  2 (○) Beta                 Choose Beta.                                      █",
        "  │  z (○) Type your answer here",
        "  │",
        "  │  ↑/↓ navigate · y copy                                          Enter:submit",
        "  │",
        "  Tab:next answer  │  Esc:scrollback  │  Shift+x:dismiss",
    ].joined(separator: "\n")

    /// Grok's idle composer, which follows any card quoted in the transcript.
    private let grokComposer = [
        "  ╭──────────────────────────────────────────────────────────────╮",
        "  │ >                                                            │",
        "  ╰──────────────────────────── Grok 4.7 (xhigh) · always-approve ─╯",
        "  Shift+Tab:mode  │  Ctrl+x:shortcuts",
    ].joined(separator: "\n")

    /// Antigravity 1.5.14's screens under tmux, at a phone's width.
    private func agyScreen(_ body: [String], footer: String) -> String {
        let rule = String(repeating: "─", count: 50)
        let model = "auto · Gemini 4 Neon · high"
        let padded = footer + String(repeating: " ", count: max(1, 50 - model.count - footer.count)) + model
        return (body + [rule, ">", rule, padded]).joined(separator: "\n")
    }

    func testAntigravityFooterReadsItsTurn() {
        let (working, blocked) = defaultMarkers(program: "/home/me/.local/bin/agy")
        func status(_ screen: String) -> AgentStatus {
            classify(exited: false, screen: screen, working: working, blocked: blocked, bell: false, idle: 0.5)
        }
        let turn = agyScreen(["> say pong", "⣟  Generating..."], footer: "esc to cancel")
        XCTAssertEqual(status(turn), .working)
        XCTAssertEqual(status(turn + "\n[0] 0:agy*                \"host\" 14:32"), .working, "under a tmux status bar")
        // Idle repaints every couple of seconds; fresh output alone isn't a turn.
        XCTAssertEqual(status(agyScreen(["  pong"], footer: "? for shortcuts")), .idle)
        XCTAssertEqual(status(agyScreen(["  pong"], footer: "")), .idle, "a draft shows only the model")
        let slashMenu = [String(repeating: "─", count: 50), "> /", String(repeating: "─", count: 50),
                         "  /changelog    Show release notes", "  ↑/↓ Navigate · enter Select · tab Complete",
                         "esc to cancel        auto · Gemini 4 Neon · high"].joined(separator: "\n")
        XCTAssertEqual(status(slashMenu), .idle)
    }

    func testAntigravityCardsAreBlocked() {
        let (working, blocked) = defaultMarkers(program: "agy")
        let footer = "esc to cancel        auto · Gemini 4 Neon · high"
        let cards = [
            "Do you trust the contents of this project?\n\n> Yes, I trust this folder\n  No, exit\n\n  ↑/↓ Navigate · enter Confirm",
            "Requesting permission for:\n   rm x\n\nRun this command?\n> 1. Yes, run command\n  2. No, cancel\n\n"
                + "  ↑/↓ Navigate · tab Amend · ctrl+g edit/expand command\n" + footer,
            "Accept this file edit?\n> 1. Yes, accept this change\n  2. No, reject this change\n\n"
                + "  ↑/↓ Navigate · f full diff\n" + footer,
            "Question 1/1: Please choose a color:\n\n> 1. Red\n  2. Blue\n  3. Write-in...\n\n"
                + "  ↑/↓ Navigate · enter Select · esc Skip\n" + footer,
        ]
        for card in cards {
            XCTAssertEqual(classify(exited: false, screen: card, working: working, blocked: blocked,
                                    bell: false, idle: 0.5), .blocked, card)
        }
    }

    func testAntigravityFinishedTurnLatchesDone() {
        let (working, blocked) = defaultMarkers(program: "agy")
        var tracker = PaneStatusTracker()
        func update(_ screen: String) -> AgentStatus {
            tracker.update(exited: false, screen: screen, working: working, blocked: blocked, bell: false, idle: 10)
        }
        XCTAssertEqual(update(agyScreen(["⣟  Generating..."], footer: "esc to cancel")), .working)
        XCTAssertEqual(update(agyScreen(["  pong"], footer: "? for shortcuts")), .done)
    }

    func testGrokQuestionCardIsBlockedDespiteItsStopMarker() {
        let (working, blocked) = defaultMarkers(program: "grok")
        XCTAssertTrue(grokQuestionCard.contains("[stop]"), "the card carries the working marker")
        XCTAssertTrue(isGrokQuestion(grokQuestionCard))
        XCTAssertEqual(
            classify(exited: false, screen: grokQuestionCard + "\n\n\n",
                     working: working, blocked: blocked, bell: false, idle: 10),
            .blocked)
        // iOS reads the screen without Grok's title, so a tmux status bar under the
        // card doesn't hide it.
        let underStatusBar = grokQuestionCard + "\n[0] 0:grok*                    \"host\" 14:32 29-Oct-26"
        XCTAssertTrue(isGrokQuestion(underStatusBar))
    }

    func testGrokQuestionCardQuotedAboveTheComposerIsNotBlocked() {
        let (working, blocked) = defaultMarkers(program: "grok")
        let quoted = grokQuestionCard + "\n\n" + grokComposer
        XCTAssertFalse(isGrokQuestion(quoted))
        // A turn still running (`[stop]`) reads as working.
        XCTAssertEqual(
            classify(exited: false, screen: quoted, working: working, blocked: blocked, bell: false, idle: 10),
            .working)
        // A working turn with no card is unchanged.
        let turn = "   - Waiting for response… 1.4s                          1.9s ↓1.77k [stop]\n"
            + grokComposer.replacingOccurrences(of: "Shift+Tab:mode  │  Ctrl+x:shortcuts",
                                                with: "Shift+Tab:mode  │  Ctrl+c:cancel  │  Ctrl+x:shortcuts")
        XCTAssertFalse(isGrokQuestion(turn))
        XCTAssertEqual(
            classify(exited: false, screen: turn, working: working, blocked: blocked, bell: false, idle: 10),
            .working)
        // A card needs at least two numbered options.
        let oneOption = grokQuestionCard.replacingOccurrences(of: "  │  2 (○) Beta", with: "  │  Beta")
        XCTAssertFalse(isGrokQuestion(oneOption))
    }

    func testTrueColorOnlyRaisesTheBasicDefaultTerminal() {
        let cmd = TmuxCommands.enableTrueColorCommand
        // The user's own `default-terminal` (tmux-direct, alacritty, …) is left alone;
        // only RHEL's 8-color `screen` (or an unset/`tmux` value) is raised.
        XCTAssertTrue(cmd.hasPrefix(
            "case \"$(tmux \"$@\" show-options -gv default-terminal 2>/dev/null)\" in ''|screen|tmux) "),
            cmd)
        XCTAssertTrue(cmd.contains("set-option -g default-terminal tmux-256color 2>/dev/null; else "), cmd)
        XCTAssertTrue(cmd.contains("fi ;; esac; "), cmd)
    }
}
