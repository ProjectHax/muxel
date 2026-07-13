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
        XCTAssertEqual(
            TmuxCommands.commandLine(TmuxCommands.startServer()),
            "\(TmuxCommands.pathPrelude); 'tmux' 'start-server' ';' 'set' '-s' 'exit-empty' 'off'")
        XCTAssertFalse(TmuxCommands.commandLine(TmuxCommands.startServer()).contains("muxel"))
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

    // iOS must launch the agent through a login+interactive shell so it's on the
    // user's PATH (a no-PTY SSH exec has a bare PATH → the agent exits instantly).
    func testLaunchAgentWrapsInLoginShell() {
        let cmd = TmuxCommands.launchAgent(
            session: "muxel_h_abcdef12", cwd: "/work", program: "claude", args: ["--model", "opus"])
        XCTAssertTrue(cmd.hasPrefix(
            "\(TmuxCommands.pathPrelude); tmux new-session -d -s 'muxel_h_abcdef12' -c '/work' "
            + "-- \"${SHELL:-/bin/sh}\" -ilc "))
        XCTAssertTrue(cmd.contains("exec"))
        XCTAssertTrue(cmd.contains("claude"))
        XCTAssertTrue(cmd.contains("opus"))
        // No program → tmux's own default login shell, no wrapping.
        XCTAssertEqual(
            TmuxCommands.launchAgent(session: "s", cwd: "/w", program: nil, args: []),
            "\(TmuxCommands.pathPrelude); tmux new-session -d -s 's' -c '/w'")
    }

    func testCommandLineShellQuotes() {
        let line = TmuxCommands.commandLine(TmuxCommands.capturePane(session: "muxel_h_abcdef12"))
        XCTAssertEqual(
            line,
            "\(TmuxCommands.pathPrelude); 'tmux' 'capture-pane' '-p' '-t' '=muxel_h_abcdef12:'")
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
}
