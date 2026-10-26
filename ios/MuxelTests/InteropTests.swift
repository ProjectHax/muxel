import XCTest
@testable import muxel

final class InteropTests: XCTestCase {

    func testRecordedNamedSocketRoundTripsWithoutDroppingOtherPanes() throws {
        let json = """
        {"version":1,"updated_at":1,"remote_root":"/srv/app","layout":null,"worktrees":[],"instances":[
          {"id":"aaaaaaaa-0000-0000-0000-000000000000","project_id":"p","title":"named","tmux_socket":"mxl-profile"},
          {"id":"bbbbbbbb-0000-0000-0000-000000000000","project_id":"p","title":"legacy"}
        ]}
        """
        let layout = try MuxelJSON.decoder.decode(RemoteLayout.self, from: Data(json.utf8))
        XCTAssertEqual(layout.instances.count, 2)
        XCTAssertEqual(layout.instances[0].tmuxSocket, "mxl-profile")
        XCTAssertNil(layout.instances[1].tmuxSocket)
        let restored = try MuxelJSON.decoder.decode(RemoteLayout.self, from: MuxelJSON.encoder.encode(layout))
        XCTAssertEqual(restored, layout)
        XCTAssertEqual(TmuxCommands.selectSocket("s", socket: restored.instances[0].tmuxSocket), "set -- -L 'mxl-profile'")
    }

    func testDefaultRespawnMigratesOnlyWhenBothExactNamesAreAbsent() throws {
        var instance = Instance(id: "aaaaaaaa-0000-0000-0000-000000000000", projectId: "p", title: "pane", program: nil, args: [])
        instance.tmuxSession = "owned"
        instance.tmuxSocket = "default"
        let absent = try TmuxCommands.targetForNewSession(instance, name: "owned", confirmedTargets: [])
        XCTAssertEqual(absent.target.socket, "mxl")
        XCTAssertEqual(absent.migration?.recordedSession, "owned")
        let liveCases: [[TmuxCommands.SessionTarget]] = [
            [.init(name: "owned", socket: "default")],
            [.init(name: "owned", socket: "default"), .init(name: "owned", socket: "mxl")]
        ]
        for targets in liveCases {
            let present = try TmuxCommands.targetForNewSession(instance, name: "owned", confirmedTargets: targets)
            XCTAssertEqual(present.target.socket, "default")
            XCTAssertNil(present.migration)
        }
        for socket in ["mxl", "mxl-profile"] {
            instance.tmuxSocket = socket
            let existing = try TmuxCommands.targetForNewSession(instance, name: "owned", confirmedTargets: [])
            XCTAssertEqual(existing.target.socket, socket)
            XCTAssertNil(existing.migration)
        }
    }

    func testPeerMigrationDoesNotAuthorizeAStaleDefaultClientOrItsRetry() throws {
        var firstPeer = Instance(id: "aaaaaaaa-0000-0000-0000-000000000000", projectId: "p",
                                 title: "pane", program: nil, args: [])
        firstPeer.tmuxSession = "owned|with|pipes "
        firstPeer.tmuxSocket = "default"
        let stalePeer = firstPeer

        // Both peers began with Default. The first completes teardown while
        // neither server holds the name, then saves and starts on Private.
        let firstPlan = try TmuxCommands.targetForNewSession(firstPeer, name: firstPeer.tmuxSession!, confirmedTargets: [])
        firstPeer.tmuxSocket = firstPlan.target.socket
        let inventory = try TmuxCommands.parseConfirmedTargets("owned|with|pipes \n:muxel-listed:0\n:muxel-listed:0\n")
        XCTAssertNil(TmuxCommands.resolveTarget(stalePeer, in: inventory))

        // The second peer cannot distinguish that replacement from an unrelated
        // namesake. Neither an immediate retry nor restoring its stale metadata
        // may recreate Default or adopt Private from inventory alone.
        let restored = try MuxelJSON.decoder.decode(Instance.self, from: MuxelJSON.encoder.encode(stalePeer))
        for snapshot in [stalePeer, stalePeer, restored] {
            XCTAssertThrowsError(try TmuxCommands.targetForNewSession(snapshot, name: snapshot.tmuxSession!, confirmedTargets: inventory)) {
                XCTAssertEqual($0 as? TmuxCommands.ProbeError, .ownershipRefreshRequired)
            }
            XCTAssertEqual(snapshot.tmuxSocket, "default")
            XCTAssertEqual(snapshot.tmuxSession, "owned|with|pipes ")
        }

        // An authoritative refresh of this same pane supplies the binding that
        // inventory could not. It can then attach to the recorded Private owner.
        let refreshed = try MuxelJSON.decoder.decode(Instance.self, from: MuxelJSON.encoder.encode(firstPeer))
        XCTAssertEqual(refreshed.id, stalePeer.id)
        XCTAssertEqual(TmuxCommands.resolveTarget(refreshed, in: inventory), firstPlan.target)
        let refreshedPlan = try TmuxCommands.targetForNewSession(refreshed, name: refreshed.tmuxSession!, confirmedTargets: inventory)
        XCTAssertEqual(refreshedPlan.target, firstPlan.target)
        XCTAssertNil(refreshedPlan.migration)
    }

    func testMigrationProbeRequiresCompleteSuccessfulOrKnownAbsentReplies() throws {
        let empty = "no server running on /tmp/mxl\n:muxel-listed:1\nerror connecting to /tmp/default (No such file or directory)\n:muxel-listed:1\n"
        XCTAssertTrue(try TmuxCommands.parseConfirmedTargets(empty).isEmpty)
        let collision = "owned\n:muxel-listed:0\n:muxel-listed:0\n"
        XCTAssertEqual(try TmuxCommands.parseConfirmedTargets(collision), [.init(name: "owned", socket: "mxl")])
        let spaced = try TmuxCommands.parseConfirmedTargets(":muxel-listed:0\nowned \n:muxel-listed:0\n")
        XCTAssertEqual(spaced, [.init(name: "owned ", socket: "default")])
        var instance = Instance(id: "aaaaaaaa-0000-0000-0000-000000000000", projectId: "p", title: "pane", program: nil, args: [])
        instance.tmuxSession = "owned "
        instance.tmuxSocket = "default"
        XCTAssertEqual(TmuxCommands.resolveTarget(instance, in: spaced)?.name, "owned ")
        XCTAssertNil(try TmuxCommands.targetForNewSession(instance, name: "owned ", confirmedTargets: spaced).migration)
        for bad in [
            ":muxel-listed:0\n",
            ":muxel-listed:0\nerror connecting to /tmp/default (Permission denied)\n:muxel-listed:1\n",
            // An empty line is no session name: it spoils a status-0 answer.
            "\n:muxel-listed:0\n:muxel-listed:0\n",
            ":muxel-listed:0\nowned\n\n:muxel-listed:0\n",
            "owned\n:muxel-listed:0\n:muxel-listed:0\n:muxel-listed:0\n",
            "no server running on /tmp/mxl\n:muxel-listed:unknown\n:muxel-listed:0\n"
        ] {
            XCTAssertThrowsError(try TmuxCommands.parseConfirmedTargets(bad))
        }
        XCTAssertEqual(TmuxCommands.confirmedTargetsCommand().components(separatedBy: ":muxel-listed:").count - 1, 2)
        XCTAssertTrue(TmuxCommands.confirmedTargetsCommand().contains("'#{session_name}'"))
        // CRLF line ends read as desktop's `str::lines` reads them.
        XCTAssertEqual(try TmuxCommands.parseConfirmedTargets(
            "owned\r\n:muxel-listed:0\r\nno server running on /tmp/default\r\n:muxel-listed:1\r\n"),
            [.init(name: "owned", socket: "mxl")])
        // Any other line of a status-0 answer is one exact name, like desktop's parser.
        XCTAssertEqual(try TmuxCommands.parseConfirmedTargets("a\tb|c \n:muxel-listed:0\n:muxel-listed:0\n \n"),
                       [.init(name: "a\tb|c ", socket: "mxl")])
    }

    /// Reattaching a session still running on Default needs only Default's answer.
    /// Whatever muxel's own server replied, the pane attaches as it did before
    /// migration existed; only a move off Default needs both. Mirrors
    /// `a_live_default_session_needs_only_defaults_answer` in `ssh.rs`.
    func testALiveDefaultSessionNeedsOnlyDefaultsAnswer() throws {
        var instance = Instance(id: "aaaaaaaa-0000-0000-0000-000000000000", projectId: "p", title: "pane", program: nil, args: [])
        instance.tmuxSession = "owned "
        instance.tmuxSocket = "default"
        let live = "owned \n:muxel-listed:0\n"
        for own in [
            "Welcome to devbox\nowned \n:muxel-listed:0\n",
            "error connecting to /tmp/mxl (Permission denied)\n:muxel-listed:1\n",
            "\n:muxel-listed:0\n",
            "something unfamiliar\n:muxel-listed:2\n",
            "garbage without a status\n:muxel-listed:\n"
        ] {
            let targets = try TmuxCommands.defaultOwnerTargets(own + live, name: "owned ")
            XCTAssertEqual(TmuxCommands.resolveTarget(instance, in: targets),
                           TmuxCommands.SessionTarget(name: "owned ", socket: "default"), own)
        }
        let denied = "error connecting to /tmp/mxl (Permission denied)\n:muxel-listed:1\n"
        XCTAssertThrowsError(try TmuxCommands.parseConfirmedTargets(denied + live))
        // Default's answer must itself be complete and hold the exact name.
        for onDefault in [
            "owned\n:muxel-listed:0\n",
            "owned \n",
            "owned \n:muxel-listed:1\n",
            "owned \n\n:muxel-listed:0\n",
            "no server running on /tmp/default\n:muxel-listed:1\n"
        ] {
            XCTAssertThrowsError(try TmuxCommands.defaultOwnerTargets(denied + onDefault, name: "owned ")) {
                XCTAssertEqual($0 as? TmuxCommands.ProbeError, .incomplete)
            }
        }
        // Gone from Default with both answers in: the ordinary migration decision.
        let gone = "no server running on /tmp/mxl\n:muxel-listed:1\n:muxel-listed:0\n"
        let targets = try TmuxCommands.defaultOwnerTargets(gone, name: "owned ")
        XCTAssertNil(TmuxCommands.resolveTarget(instance, in: targets))
        XCTAssertEqual(try TmuxCommands.targetForNewSession(instance, name: "owned ", confirmedTargets: targets).target.socket, "mxl")
    }

    /// `tmux -L` joins the name onto its socket directory, so a recorded owner that
    /// isn't a plain file name could reach a server elsewhere. It decodes as no
    /// owner (legacy discovery) and the rest of the layout still loads. Mirrors
    /// `a_recorded_owner_that_is_not_a_socket_name_reads_as_none` in `tmux.rs`.
    func testARecordedOwnerThatIsNotASocketNameDecodesAsNone() throws {
        let invalid = ["", ".", "..", "/tmp/elsewhere", "../mxl", "a/b", "nul\u{0}x"]
        for name in invalid { XCTAssertFalse(TmuxCommands.isSocketName(name), name) }
        for name in ["mxl", "default", "mxl-profile", "   ", "...", ".hidden"] {
            XCTAssertTrue(TmuxCommands.isSocketName(name), name)
        }
        var rows: [[String: Any]] = []
        for (index, name) in (invalid + ["mxl-profile"]).enumerated() {
            rows.append(["id": "aaaaaaaa-0000-0000-0000-00000000000\(index)", "project_id": "p", "title": "pane",
                         "tmux_session": "work", "tmux_socket": name])
        }
        let document: [String: Any] = ["version": 1, "updated_at": 1, "remote_root": "/srv/app",
                                       "worktrees": [Any](), "instances": rows]
        let data = try JSONSerialization.data(withJSONObject: document)
        let layout = try MuxelJSON.decoder.decode(RemoteLayout.self, from: data)
        XCTAssertEqual(layout.instances.count, invalid.count + 1)
        for instance in layout.instances.dropLast() {
            XCTAssertNil(instance.tmuxSocket)
            XCTAssertEqual(instance.tmuxSession, "work")
        }
        XCTAssertEqual(layout.instances.last?.tmuxSocket, "mxl-profile")
    }

    func testRemoteLayoutDecodesDesktopShape() throws {
        let json = """
        {
          "version": 1,
          "updated_at": 1719500000,
          "remote_root": "/srv/app",
          "layout": {
            "kind": "split",
            "direction": "horizontal",
            "sizes": [600.0, 400.0],
            "children": [
              { "kind": "leaf", "tabs": ["aaaaaaaa-0000-0000-0000-000000000000"], "active": 0 },
              { "kind": "leaf", "instance": "bbbbbbbb-0000-0000-0000-000000000000" }
            ]
          },
          "instances": [
            { "id": "aaaaaaaa-0000-0000-0000-000000000000", "project_id": "11110000-0000-0000-0000-000000000000", "title": "Claude", "program": "claude" }
          ],
          "worktrees": []
        }
        """
        let layout = try MuxelJSON.decoder.decode(RemoteLayout.self, from: Data(json.utf8))
        XCTAssertTrue(layout.isValid(forRoot: "/srv/app"))
        XCTAssertEqual(layout.updatedAt, 1719500000)
        // Legacy single-tab leaf decodes to a one-tab leaf.
        XCTAssertEqual(layout.layout?.allTabs,
                       ["aaaaaaaa-0000-0000-0000-000000000000", "bbbbbbbb-0000-0000-0000-000000000000"])
        // Instance defaults applied for missing fields.
        XCTAssertEqual(layout.instances.first?.kind, .terminal)
        XCTAssertEqual(layout.instances.first?.injection, InjectionMode.none)
        XCTAssertNil(layout.instances.first?.autoName)
    }

    func testInstanceDisplayNamePrefersCustomThenAutoThenTitle() {
        var instance = Instance(id: "aaaa", projectId: "p", title: "Claude", program: "claude", args: [])
        XCTAssertEqual(instance.displayName, "Claude")

        instance.autoName = "Generated title"
        XCTAssertEqual(instance.displayName, "Generated title")

        instance.customName = "My title"
        XCTAssertEqual(instance.displayName, "My title")

        instance.customName = " \n"
        XCTAssertEqual(instance.displayName, "Generated title")

        instance.autoName = "\t"
        XCTAssertEqual(instance.displayName, "Claude")

        instance.autoName = "019f957d-b731-7db0-904d-9e08330e0000"
        XCTAssertEqual(instance.displayName, "Claude")
    }

    func testInstanceAutoNameRoundTrips() throws {
        var instance = Instance(id: "aaaa", projectId: "p", title: "Claude", program: "claude", args: [])
        instance.autoName = "Generated title"

        let data = try MuxelJSON.encoder.encode(instance)
        let decoded = try MuxelJSON.decoder.decode(Instance.self, from: data)

        XCTAssertEqual(decoded.autoName, "Generated title")
        XCTAssertEqual(decoded.displayName, "Generated title")
    }

    func testInstanceSessionPathRoundTrips() throws {
        // The desktop's saved Codex rollout path must survive an iOS write-back,
        // or the desktop's next restore has to look the conversation up again.
        let json = """
        { "id": "aaaa", "project_id": "p", "title": "Codex", "program": "codex",
          "session_id": "01a1543c-9700-7000-8000-000000000001", "session_started": true,
          "session_path": "/home/me/.codex/sessions/2026/10/19/rollout-01a1543c.jsonl" }
        """
        let decoded = try MuxelJSON.decoder.decode(Instance.self, from: Data(json.utf8))
        XCTAssertEqual(decoded.sessionPath, "/home/me/.codex/sessions/2026/10/19/rollout-01a1543c.jsonl")

        let data = try MuxelJSON.encoder.encode(decoded)
        let object = try XCTUnwrap(JSONSerialization.jsonObject(with: data) as? [String: Any])
        XCTAssertEqual(object["session_path"] as? String, decoded.sessionPath)
        let back = try MuxelJSON.decoder.decode(Instance.self, from: data)
        XCTAssertEqual(back.sessionPath, decoded.sessionPath)

        // Documents from desktops that predate the field still decode.
        let older = try MuxelJSON.decoder.decode(
            Instance.self, from: Data(#"{ "id": "bbbb", "project_id": "p", "title": "Codex" }"#.utf8))
        XCTAssertNil(older.sessionPath)
    }

    func testDuplicateResetClearsConversationStateAndAutoName() {
        var instance = Instance(id: "aaaa", projectId: "p", title: "Claude", program: "claude", args: [])
        instance.sessionId = "session"
        instance.sessionPath = "/home/me/.claude/projects/p/session.jsonl"
        instance.sessionStarted = true
        instance.autoName = "Generated title"
        instance.customName = "Manual title"
        instance.controlCreated = true

        instance.resetConversationForDuplicate()

        XCTAssertNil(instance.sessionId)
        XCTAssertNil(instance.sessionPath)
        XCTAssertFalse(instance.sessionStarted)
        XCTAssertNil(instance.autoName)
        XCTAssertEqual(instance.customName, "Manual title")
        XCTAssertFalse(instance.controlCreated)
    }

    func testDuplicateNeverCarriesAnUnconsumedCtlTask() {
        var task = Instance(id: "aaaa", projectId: "p", title: "Claude", program: "claude", args: [])
        task.controlPrompt = true
        task.systemPrompt = "deploy the branch"
        task.resetConversationForDuplicate()
        XCTAssertNil(task.systemPrompt)
        var runner = Instance(id: "bbbb", projectId: "p", title: "Claude", program: "claude", args: [])
        runner.isRunner = true
        runner.systemPrompt = "Review the diff"
        runner.resetConversationForDuplicate()
        XCTAssertEqual(runner.systemPrompt, "Review the diff")
    }

    func testOutsideCreationOriginRoundTripsAndDefaultsForOlderDocuments() throws {
        var instance = Instance(id: "aaaa", projectId: "p", title: "Claude", program: "claude", args: [])
        instance.controlCreated = true
        let encoded = try MuxelJSON.encoder.encode(instance)
        let decoded = try MuxelJSON.decoder.decode(Instance.self, from: encoded)
        XCTAssertTrue(decoded.controlCreated)
        let legacy = Data(#"{"id":"aaaa","project_id":"p","title":"Claude"}"#.utf8)
        XCTAssertFalse(try MuxelJSON.decoder.decode(Instance.self, from: legacy).controlCreated)
    }

    func testPaneNodeRoundTrips() throws {
        let node = PaneNode.split(
            direction: .vertical, sizes: [1, 2],
            children: [.leaf(tabs: ["x"], active: 0), .leaf(tabs: ["y", "z"], active: 1)]
        )
        let data = try MuxelJSON.encoder.encode(node)
        let back = try MuxelJSON.decoder.decode(PaneNode.self, from: data)
        XCTAssertEqual(node, back)
    }

    func testInjectionModeRoundTrips() throws {
        for mode in [InjectionMode.none, .typeIn, .cliFlag(flag: "--append-system-prompt")] {
            let data = try MuxelJSON.encoder.encode(mode)
            XCTAssertEqual(try MuxelJSON.decoder.decode(InjectionMode.self, from: data), mode)
        }
    }

    func testPaneIdsRemovedReportsOnlyPeerClosedPanes() {
        var old = RemoteLayout(remoteRoot: "/srv/app")
        for id in ["aaaa", "bbbb", "cccc"] {
            old.addInstanceAsTab(Instance(id: id, projectId: "p", title: id, program: nil, args: []),
                                 now: 1)
        }
        var fresh = old
        fresh.removeInstance(id: "bbbb", now: 2) // closed on desktop
        fresh.removeInstance(id: "cccc", now: 2) // launched here, write not landed yet
        XCTAssertEqual(fresh.paneIdsRemoved(since: old, pending: ["cccc"]), ["bbbb"])
        // Nothing dropped → nothing reported.
        XCTAssertEqual(old.paneIdsRemoved(since: old), [])
        // A different project's layout never disconnects this one's terminals.
        XCTAssertEqual(RemoteLayout(remoteRoot: "/srv/other").paneIdsRemoved(since: old), [])
    }

    func testAddInstanceAsTabSeedsAndAppends() {
        var layout = RemoteLayout(remoteRoot: "/srv/app")
        let a = Instance(id: "aaaa", projectId: "p", title: "A", program: "claude", args: [])
        layout.addInstanceAsTab(a, now: 100)
        XCTAssertEqual(layout.layout?.allTabs, ["aaaa"])
        XCTAssertEqual(layout.updatedAt, 100)

        let b = Instance(id: "bbbb", projectId: "p", title: "B", program: nil, args: [])
        layout.addInstanceAsTab(b, now: 200)
        XCTAssertEqual(layout.layout?.allTabs, ["aaaa", "bbbb"])
        if case let .leaf(_, active) = layout.layout { XCTAssertEqual(active, 1) } else { XCTFail() }

        layout.removeInstance(id: "aaaa", now: 300)
        XCTAssertEqual(layout.layout?.allTabs, ["bbbb"])
        XCTAssertEqual(layout.instances.map(\.id), ["bbbb"])
    }

    func testUnknownInstanceKindDecodesAndRoundTrips() throws {
        // A desktop that adds a new pane kind (here Browser + a hypothetical future
        // one) must not break the whole layout decode on an older iOS build, and an
        // iOS write-back must preserve the raw kind + browser_url verbatim.
        let json = """
        {
          "version": 1,
          "updated_at": 1719500000,
          "remote_root": "/srv/app",
          "layout": { "kind": "leaf", "tabs": ["cccccccc-0000-0000-0000-000000000000"], "active": 0 },
          "instances": [
            { "id": "cccccccc-0000-0000-0000-000000000000", "project_id": "11110000-0000-0000-0000-000000000000",
              "title": "Docs", "kind": "Browser", "browser_url": "https://example.com" },
            { "id": "dddddddd-0000-0000-0000-000000000000", "project_id": "11110000-0000-0000-0000-000000000000",
              "title": "Mystery", "kind": "SomethingNew" }
          ],
          "worktrees": []
        }
        """
        let layout = try MuxelJSON.decoder.decode(RemoteLayout.self, from: Data(json.utf8))
        XCTAssertEqual(layout.instances.count, 2)
        XCTAssertEqual(layout.instances[0].kind, .browser)
        XCTAssertEqual(layout.instances[0].browserUrl, "https://example.com")
        XCTAssertEqual(layout.instances[1].kind, .other("SomethingNew"))

        // Round-trip: unknown kind + browser_url survive an encode (an iOS rewrite).
        let data = try MuxelJSON.encoder.encode(layout.instances[1])
        let back = try MuxelJSON.decoder.decode(Instance.self, from: data)
        XCTAssertEqual(back.kind, .other("SomethingNew"))
        let bdata = try MuxelJSON.encoder.encode(layout.instances[0])
        let bback = try MuxelJSON.decoder.decode(Instance.self, from: bdata)
        XCTAssertEqual(bback.kind, .browser)
        XCTAssertEqual(bback.browserUrl, "https://example.com")
    }

    func testOrderedPaneInstancesIncludesEditorAndDiff() throws {
        let json = """
        {
          "version": 1, "updated_at": 1, "remote_root": "/srv/app",
          "layout": { "kind": "leaf", "tabs": [
            "aaaaaaaa-0000-0000-0000-000000000000",
            "bbbbbbbb-0000-0000-0000-000000000000",
            "cccccccc-0000-0000-0000-000000000000" ], "active": 0 },
          "instances": [
            { "id": "aaaaaaaa-0000-0000-0000-000000000000", "project_id": "p", "title": "Claude", "program": "claude" },
            { "id": "bbbbbbbb-0000-0000-0000-000000000000", "project_id": "p", "title": "file.swift", "kind": "Editor", "editor_path": "/srv/app/file.swift" },
            { "id": "cccccccc-0000-0000-0000-000000000000", "project_id": "p", "title": "diff", "kind": "Diff" }
          ],
          "worktrees": []
        }
        """
        let layout = try MuxelJSON.decoder.decode(RemoteLayout.self, from: Data(json.utf8))
        // Pane list keeps all three; the terminal-only list (poll input) keeps one.
        XCTAssertEqual(layout.orderedPaneInstances.map(\.kind), [.terminal, .editor, .diff])
        XCTAssertEqual(layout.orderedTerminalInstances.map(\.kind), [.terminal])
    }

    func testParseAllPanes() {
        let now = Int(Date().timeIntervalSince1970)
        let s = """
        muxel_host_aaaaaaaa\t0\t0\t\(now - 1)
        muxel_host_bbbbbbbb\t1\t0\t\(now - 100)
        muxel_host_cccccccc\t0\t1\t\(now - 3)
        """
        let rows = PollService.parseAllPanes(s)
        XCTAssertEqual(rows.count, 3)
        XCTAssertEqual(rows[0].session, "muxel_host_aaaaaaaa")
        XCTAssertFalse(rows[0].exited)
        XCTAssertLessThan(rows[0].idle, 5)
        XCTAssertTrue(rows[1].exited)
        XCTAssertTrue(rows[2].bell)
        // Empty output (no running server) → no rows.
        XCTAssertEqual(PollService.parseAllPanes("").count, 0)
        // A session reporting two panes keeps the first row.
        let dup = PollService.parseAllPanes("s\t0\t0\t\(now)\ns\t1\t0\t\(now)")
        XCTAssertEqual(dup.count, 1)
        XCTAssertFalse(dup[0].exited)
    }

    func testAggregateProjectActivity() {
        let results = [
            InstanceStatus(instanceId: "a", status: .working, running: true),
            InstanceStatus(instanceId: "b", status: .blocked, running: true),
            InstanceStatus(instanceId: "c", status: .done, running: true),
            InstanceStatus(instanceId: "d", status: .idle, running: false),
        ]
        let a = AppState.aggregate(results)
        XCTAssertEqual(a.running, 3)  // "d" has no live session
        XCTAssertEqual(a.blocked, 1)
        XCTAssertEqual(a.done, 1)
        XCTAssertEqual(AppState.aggregate([]).running, 0)
    }

    func testParseMeta() {
        XCTAssertEqual(PollService.parseMeta("0\t0\t0").exited, false)
        XCTAssertEqual(PollService.parseMeta("1\t0\t0").exited, true)
        XCTAssertEqual(PollService.parseMeta("0\t1\t0").bell, true)
        let recent = Int(Date().timeIntervalSince1970) - 1
        XCTAssertLessThan(PollService.parseMeta("0\t0\t\(recent)").idle, 5)
    }
}
