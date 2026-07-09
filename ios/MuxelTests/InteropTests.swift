import XCTest
@testable import muxel

final class InteropTests: XCTestCase {

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
