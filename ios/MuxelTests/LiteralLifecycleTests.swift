import Foundation
import XCTest
@testable import muxel

/// Exercise AppState's real layout writes and SSH command decisions with a
/// disposable in-memory remote, including a matching session that must survive.
@MainActor
final class LiteralLifecycleTests: XCTestCase {
    private final class Remote {
        var layout: RemoteLayout
        var sessions: [String]
        var commands: [String] = []
        lazy var connection = MockSSHConnection { [unowned self] command in
            commands.append(command)
            if command.hasPrefix("f=") {
                return String(data: try! MuxelJSON.encoder.encode(layout), encoding: .utf8)!
            }
            if command.hasPrefix("cat > "),
               let start = command.firstIndex(of: "\n"),
               let end = command.range(of: "\nMUXEL_EOF_", options: .backwards) {
                let json = command[command.index(after: start)..<end.lowerBound]
                layout = try! MuxelJSON.decoder.decode(RemoteLayout.self, from: Data(json.utf8))
            }
            if command.contains("list-sessions") { return sessions.map { "mxl\t\($0)" }.joined(separator: "\n") }
            return ""
        }

        init(_ layout: RemoteLayout, sessions: [String]) {
            self.layout = layout
            self.sessions = sessions
        }
    }

    private func pane(_ project: RemoteProject, literal: Bool) -> Instance {
        var instance = Instance(id: UUID().uuidString.lowercased(),
                                projectId: project.id.uuidString.lowercased(),
                                title: "fixture", program: "native", args: ["brief with spaces", ""])
        instance.literalArgv = literal
        instance.useTmux = true
        instance.tmuxSession = "stale-peer-owner"
        instance.launchDir = "/srv/external"
        instance.worktreeId = UUID().uuidString.lowercased()
        instance.worktreePath = "/srv/owned-worktree"
        return instance
    }

    private func install(_ instances: [Instance], state: AppState, project: RemoteProject) -> Remote {
        let layout = RemoteLayout(remoteRoot: project.remoteRoot,
                                  layout: .leaf(tabs: instances.map(\.id), active: 0),
                                  instances: instances)
        let remote = Remote(layout, sessions: instances.map {
            TmuxSession.name(hostName: "different-desktop", instanceId: $0.id)
        })
        state.selectedProject = project
        state.layout = layout
        state.connectionFactory = { _, _ in remote.connection }
        return remote
    }

    func testDuplicateLiteralDropsTransportButPreservesLaunchAndWorktree() async throws {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        let original = pane(project, literal: true)
        let remote = install([original], state: state, project: project)

        await state.duplicate(original, in: project)

        let copy = try XCTUnwrap(remote.layout.instances.first { $0.id != original.id })
        XCTAssertEqual(state.lastLaunched, copy.id)
        XCTAssertTrue(copy.literalArgv)
        XCTAssertEqual(copy.program, original.program)
        XCTAssertEqual(copy.args, original.args)
        XCTAssertEqual(copy.launchDir, original.launchDir)
        XCTAssertEqual(copy.worktreeId, original.worktreeId)
        XCTAssertEqual(copy.worktreePath, original.worktreePath)
        XCTAssertNil(copy.tmuxSession)
        XCTAssertNil(copy.tmuxSocket)
        XCTAssertFalse(copy.useTmux)
        XCTAssertFalse(remote.commands.contains { $0.contains("kill-session") || $0.contains("new-session") })
        XCTAssertEqual(remote.layout.instances.first { $0.id == original.id }, original)
    }

    func testDuplicateNonliteralStillAssignsItsOwnSession() async throws {
        let state = TestFixtures.makeState()
        let (host, project) = TestFixtures.seedProject(state)
        let original = pane(project, literal: false)
        let remote = install([original], state: state, project: project)

        await state.duplicate(original, in: project)

        let copy = try XCTUnwrap(remote.layout.instances.first { $0.id != original.id })
        XCTAssertFalse(copy.literalArgv)
        XCTAssertTrue(copy.useTmux)
        XCTAssertEqual(copy.tmuxSession, TmuxSession.name(hostName: host.name, instanceId: copy.id))
        XCTAssertEqual(copy.tmuxSocket, TmuxCommands.socketName)
    }

    func testCloseLiteralRemovesLayoutWithoutProbingOrKillingMatchingSession() async {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        let original = pane(project, literal: true)
        let remote = install([original], state: state, project: project)

        await state.close(original, in: project)

        XCTAssertTrue(remote.layout.instances.isEmpty)
        XCTAssertNil(remote.layout.layout)
        XCTAssertTrue(state.layout?.instances.isEmpty == true)
        XCTAssertFalse(remote.commands.contains { $0.contains("list-sessions") || $0.contains("kill-session") })
    }

    func testCloseOthersKillsOnlyNonliteralAndKeepsSelectedPane() async {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        let keep = pane(project, literal: false)
        let literal = pane(project, literal: true)
        let normal = pane(project, literal: false)
        let remote = install([keep, literal, normal], state: state, project: project)

        await state.closeOthers(keeping: keep.id, in: project)

        XCTAssertEqual(remote.layout.instances.map(\.id), [keep.id])
        XCTAssertEqual(remote.layout.layout?.allTabs, [keep.id])
        let kills = remote.commands.filter { $0.contains("kill-session") }
        XCTAssertEqual(kills.count, 1)
        XCTAssertTrue(kills.first?.contains(TmuxSession.name(hostName: "different-desktop", instanceId: normal.id)) == true)
        XCTAssertFalse(kills.contains { $0.contains(String(literal.id.prefix(8))) })
        XCTAssertEqual(remote.commands.filter { $0.contains("list-sessions") }.count, 1)
    }

    func testCloseNonliteralStillDiscoversAndKillsRenamedLiveSession() async {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        let original = pane(project, literal: false)
        let remote = install([original], state: state, project: project)

        await state.close(original, in: project)

        XCTAssertTrue(remote.layout.instances.isEmpty)
        XCTAssertEqual(remote.commands.filter { $0.contains("list-sessions") }.count, 1)
        let kills = remote.commands.filter { $0.contains("kill-session") }
        XCTAssertEqual(kills.count, 1)
        XCTAssertTrue(kills.first?.contains(remote.sessions[0]) == true)
    }
}
