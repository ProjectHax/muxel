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

    func testParseMeta() {
        XCTAssertEqual(PollService.parseMeta("0\t0\t0").exited, false)
        XCTAssertEqual(PollService.parseMeta("1\t0\t0").exited, true)
        XCTAssertEqual(PollService.parseMeta("0\t1\t0").bell, true)
        let recent = Int(Date().timeIntervalSince1970) - 1
        XCTAssertLessThan(PollService.parseMeta("0\t0\t\(recent)").idle, 5)
    }
}
