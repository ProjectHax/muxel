import XCTest
import Citadel
@testable import muxel

/// Pause an actual remote read after capturing its document. If a second phone
/// transaction reads before that first one writes, both would modify the same
/// snapshot — the lost-owner / resurrected-close race these tests exercise.
private actor LayoutTransactionConnection: SSHConnection {
    private var document: RemoteLayout
    private var pauseNextRead: Bool
    private var writeFailuresRemaining: Int
    private var releaseRead: CheckedContinuation<Void, Never>?
    private let onPaused: () -> Void
    private let onOverlappingRead: () -> Void
    private(set) var commands: [String] = []

    init(_ document: RemoteLayout, pauseFirstRead: Bool = true, failWrites: Int = 0,
         onPaused: @escaping () -> Void = {}, onOverlappingRead: @escaping () -> Void = {}) {
        self.document = document
        self.pauseNextRead = pauseFirstRead
        self.writeFailuresRemaining = failWrites
        self.onPaused = onPaused
        self.onOverlappingRead = onOverlappingRead
    }

    func connect() async throws {}
    func sshClient() async throws -> SSHClient? { nil }
    func close() async {}

    func run(_ command: String) async throws -> String {
        commands.append(command)
        if command.hasPrefix("f=") {
            let snapshot = String(decoding: try MuxelJSON.encoder.encode(document), as: UTF8.self)
            if releaseRead != nil { onOverlappingRead() }
            if pauseNextRead {
                pauseNextRead = false
                await withCheckedContinuation { continuation in
                    releaseRead = continuation
                    onPaused()
                }
            }
            return snapshot
        }
        if command.hasPrefix("cat > ") {
            if writeFailuresRemaining > 0 {
                writeFailuresRemaining -= 1
                throw SSHError.command("layout write failed")
            }
            // Strip the production writer's heredoc header, delimiter and trailing
            // newline, then retain the actual encoded layout for the next read.
            let json = command.components(separatedBy: "\n").dropFirst().dropLast(2).joined(separator: "\n")
            document = try MuxelJSON.decoder.decode(RemoteLayout.self, from: Data(json.utf8))
        }
        if command.contains("list-sessions") { return "mxl\tmuxel_host_aaaaaaaa\n" }
        return ""
    }

    func resumeRead() {
        let continuation = releaseRead
        releaseRead = nil
        continuation?.resume()
    }

    func snapshot() -> RemoteLayout { document }
}

@MainActor
final class LayoutTransactionTests: XCTestCase {
    private func fixture() -> (AppState, RemoteProject, Instance, Instance, RemoteLayout) {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        let a = Instance(id: "aaaaaaaa-0000-0000-0000-000000000000", projectId: "p",
                         title: "A", program: nil, args: [])
        let b = Instance(id: "bbbbbbbb-0000-0000-0000-000000000000", projectId: "p",
                         title: "B", program: nil, args: [])
        var layout = RemoteLayout(remoteRoot: project.remoteRoot)
        layout.addInstanceAsTab(a, now: 1)
        layout.addInstanceAsTab(b, now: 1)
        state.selectedProject = project
        state.layout = layout
        return (state, project, a, b, layout)
    }

    func testBindingThenRenamePreservesBothOwnerAndName() async throws {
        try await bindingThenMutation(close: false)
    }

    func testBindingThenCloseDoesNotResurrectPaneAndKillsBoundOwner() async throws {
        try await bindingThenMutation(close: true)
    }

    private func bindingThenMutation(close: Bool) async throws {
        let (state, project, a, b, layout) = fixture()
        let paused = expectation(description: "binding captured the original document")
        let overlap = expectation(description: "another transaction read before binding finished")
        overlap.isInverted = true
        let conn = LayoutTransactionConnection(layout, onPaused: { paused.fulfill() },
                                               onOverlappingRead: { overlap.fulfill() })
        state.connectionFactory = { _, _ in conn }
        let target = TmuxCommands.SessionTarget(name: "muxel_host_aaaaaaaa", socket: "mxl-profile")
        let binding = Task { try await state.bindTmuxTarget(target, instance: a, in: project) }
        await fulfillment(of: [paused], timeout: 1)

        let submitted = expectation(description: "tab action submitted while binding is paused")
        let mutation = Task { @MainActor in
            submitted.fulfill()
            if close { await state.close(a, in: project) }
            else { await state.rename(b, to: "Renamed", in: project) }
        }
        await fulfillment(of: [submitted], timeout: 1)
        await fulfillment(of: [overlap], timeout: 0.1)
        await conn.resumeRead()
        let bound = try await binding.value
        await mutation.value
        XCTAssertTrue(bound)
        let saved = await conn.snapshot()
        if close {
            XCTAssertFalse(saved.instances.contains { $0.id == a.id })
            XCTAssertFalse(saved.layout?.allTabs.contains(a.id) ?? false)
            let commands = await conn.commands
            let kills = commands.filter { $0.contains("kill-session") }
            XCTAssertEqual(kills.count, 1)
            XCTAssertTrue(kills.first?.contains("set -- -L 'mxl-profile'") ?? false)
        } else {
            XCTAssertEqual(saved.instances.first { $0.id == a.id }?.tmuxSocket, "mxl-profile")
            XCTAssertEqual(saved.instances.first { $0.id == b.id }?.customName, "Renamed")
        }
    }

    func testCloseThenBindingDeclinesRemovedInstance() async throws {
        let (state, project, a, _, layout) = fixture()
        let paused = expectation(description: "close captured the original document")
        let overlap = expectation(description: "binding read before close finished")
        overlap.isInverted = true
        let conn = LayoutTransactionConnection(layout, onPaused: { paused.fulfill() },
                                               onOverlappingRead: { overlap.fulfill() })
        state.connectionFactory = { _, _ in conn }
        let closing = Task { await state.close(a, in: project) }
        await fulfillment(of: [paused], timeout: 1)
        let submitted = expectation(description: "binding submitted during close")
        let binding = Task { @MainActor in
            submitted.fulfill()
            return try await state.bindTmuxTarget(.init(name: "muxel_host_aaaaaaaa", socket: "mxl"),
                                                 instance: a, in: project)
        }
        await fulfillment(of: [submitted], timeout: 1)
        await fulfillment(of: [overlap], timeout: 0.1)
        await conn.resumeRead()
        await closing.value
        let bound = try await binding.value
        XCTAssertFalse(bound)
        let saved = await conn.snapshot()
        XCTAssertFalse(saved.instances.contains { $0.id == a.id })
    }

    func testQueuedRenameDoesNotPublishIntoAnotherProjectAfterNavigation() async {
        let (state, project, a, _, layout) = fixture()
        let paused = expectation(description: "rename captured original project")
        let conn = LayoutTransactionConnection(layout, onPaused: { paused.fulfill() }, onOverlappingRead: {})
        state.connectionFactory = { _, _ in conn }
        let rename = Task { await state.rename(a, to: "Renamed", in: project) }
        await fulfillment(of: [paused], timeout: 1)
        let other = RemoteProject(name: "other", hostId: project.hostId, remoteRoot: "/srv/other")
        state.selectedProject = other
        state.layout = RemoteLayout(remoteRoot: other.remoteRoot)
        await conn.resumeRead()
        await rename.value
        XCTAssertEqual(state.layout?.remoteRoot, other.remoteRoot)
        let saved = await conn.snapshot()
        XCTAssertEqual(saved.instances.first { $0.id == a.id }?.customName, "Renamed")
    }

    func testCloseUsesPersistedOwnerWhenCallerAndLocalLayoutAreLegacy() async {
        let (state, project, a, _, layout) = fixture()
        var persisted = layout
        persisted.instances[0].tmuxSession = "muxel_host_aaaaaaaa"
        persisted.instances[0].tmuxSocket = "mxl-profile"
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }

        await state.close(a, in: project)

        let commands = await conn.commands
        let kills = commands.filter { $0.contains("kill-session") }
        XCTAssertEqual(kills.count, 1)
        XCTAssertTrue(kills.first?.contains("set -- -L 'mxl-profile'") ?? false)
        XCTAssertFalse(commands.contains { $0.contains("list-sessions") },
                       "a persisted owner must not be resolved again on the legacy servers")
        let saved = await conn.snapshot()
        XCTAssertFalse(saved.instances.contains { $0.id == a.id })
    }

    func testCloseOfAlreadyRemovedInstanceDoesNotKillNamesake() async {
        let (state, project, a, _, layout) = fixture()
        var persisted = layout
        persisted.removeInstance(id: a.id, now: 2)
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }

        await state.close(a, in: project)

        let commands = await conn.commands
        XCTAssertFalse(commands.contains { $0.contains("kill-session") || $0.contains("list-sessions") })
        XCTAssertFalse(commands.contains { $0.hasPrefix("cat > ") })
        XCTAssertEqual(state.layout, persisted)
        let saved = await conn.snapshot()
        XCTAssertEqual(saved, persisted)
    }

    func testCloseOthersRefreshesOwnersSkipsRemovedTargetsAndKeepsNewPanes() async {
        let (state, project, a, b, layout) = fixture()
        let removed = Instance(id: "cccccccc-0000-0000-0000-000000000000", projectId: "p",
                               title: "removed", program: nil, args: [])
        var local = layout
        local.addInstanceAsTab(removed, now: 1)
        state.layout = local
        // The selected UI still has an unresolved A and the peer-closed C. The
        // remote now records A's owner and a newly opened D, outside this action.
        var persisted = layout
        persisted.instances[0].tmuxSession = "muxel_host_aaaaaaaa"
        persisted.instances[0].tmuxSocket = "mxl-profile"
        let added = Instance(id: "dddddddd-0000-0000-0000-000000000000", projectId: "p",
                             title: "new", program: nil, args: [])
        persisted.addInstanceAsTab(added, now: 2)
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }

        await state.closeOthers(keeping: b.id, in: project)

        let commands = await conn.commands
        let kills = commands.filter { $0.contains("kill-session") }
        XCTAssertEqual(kills.count, 1)
        XCTAssertTrue(kills.first?.contains("set -- -L 'mxl-profile'") ?? false)
        XCTAssertFalse(commands.contains { $0.contains("list-sessions") })
        let saved = await conn.snapshot()
        XCTAssertEqual(Set(saved.instances.map(\.id)), Set([b.id, added.id]))
        XCTAssertFalse(saved.layout?.allTabs.contains(a.id) ?? false)
    }

    func testOptimisticLaunchWithFailedAppendCannotAuthorizePTYStart() async throws {
        let (state, project, _, _, layout) = fixture()
        let paused = expectation(description: "launch append captured persisted document")
        let premature = expectation(description: "known-owner binding completed before launch save")
        premature.isInverted = true
        let conn = LayoutTransactionConnection(layout, failWrites: 1, onPaused: { paused.fulfill() })
        state.connectionFactory = { _, _ in conn }

        await state.launch(preset: Preset(name: "Shell", program: nil), customCommand: nil, into: project)
        let launchedId = try XCTUnwrap(state.lastLaunched)
        let optimistic = try XCTUnwrap(state.layout?.instances.first { $0.id == launchedId })
        let target = TmuxCommands.SessionTarget(name: try XCTUnwrap(optimistic.tmuxSession),
                                               socket: try XCTUnwrap(optimistic.tmuxSocket))
        await fulfillment(of: [paused], timeout: 1)
        var released = false
        let submitted = expectation(description: "binding submitted on optimistic known owner")
        let binding = Task { @MainActor in
            submitted.fulfill()
            let permitted = try await state.bindTmuxTarget(target, instance: optimistic, in: project)
            if !released { premature.fulfill() }
            return permitted
        }
        await fulfillment(of: [submitted], timeout: 1)
        await fulfillment(of: [premature], timeout: 0.1)
        released = true
        await conn.resumeRead()
        let permitted = try await binding.value

        XCTAssertFalse(permitted, "TerminalPaneView must not cross its binding guard to create a PTY")
        XCTAssertNil(state.terminals.existing(launchedId))
        XCTAssertNotNil(state.notice, "the append failure remains visible")
        let saved = await conn.snapshot()
        XCTAssertFalse(saved.instances.contains { $0.id == launchedId })
        let commands = await conn.commands
        XCTAssertEqual(commands.filter { $0.hasPrefix("f=") }.count, 2,
                       "append and binding each read once")
        XCTAssertEqual(commands.filter { $0.hasPrefix("cat > ") }.count, 1,
                       "binding cannot recreate the pane after its append failed")
        XCTAssertFalse(commands.contains { $0.contains("new-session") || $0.contains("attach-session") })
    }

    func testUnchangedPersistedOwnerWaitsForEarlierTransactionWithoutWriting() async throws {
        let (state, project, _, b, layout) = fixture()
        var persisted = layout
        persisted.instances[0].tmuxSession = "muxel_host_aaaaaaaa"
        persisted.instances[0].tmuxSocket = "mxl-profile"
        let instance = persisted.instances[0]
        state.layout = persisted
        let paused = expectation(description: "earlier rename captured persisted document")
        let premature = expectation(description: "unchanged-owner binding bypassed earlier transaction")
        premature.isInverted = true
        let conn = LayoutTransactionConnection(persisted, onPaused: { paused.fulfill() })
        state.connectionFactory = { _, _ in conn }
        let rename = Task { await state.rename(b, to: "Renamed", in: project) }
        await fulfillment(of: [paused], timeout: 1)
        var released = false
        let submitted = expectation(description: "unchanged-owner binding submitted")
        let binding = Task { @MainActor in
            submitted.fulfill()
            let permitted = try await state.bindTmuxTarget(.init(name: "muxel_host_aaaaaaaa", socket: "mxl-profile"),
                                                          instance: instance, in: project)
            if !released { premature.fulfill() }
            return permitted
        }
        await fulfillment(of: [submitted], timeout: 1)
        await fulfillment(of: [premature], timeout: 0.1)
        released = true
        await conn.resumeRead()
        await rename.value
        let permitted = try await binding.value

        XCTAssertTrue(permitted)
        let commands = await conn.commands
        XCTAssertEqual(commands.filter { $0.hasPrefix("f=") }.count, 2)
        XCTAssertEqual(commands.filter { $0.hasPrefix("cat > ") }.count, 1,
                       "only rename writes; unchanged binding confirms its single read")
        let saved = await conn.snapshot()
        XCTAssertEqual(saved.instances.first { $0.id == b.id }?.customName, "Renamed")
        XCTAssertEqual(saved.instances.first { $0.id == instance.id }?.tmuxSocket, "mxl-profile")
    }

    func testUnchangedOwnerCannotAuthorizeRemovedInstance() async throws {
        let (state, project, a, _, layout) = fixture()
        var stale = a
        stale.tmuxSession = "muxel_host_aaaaaaaa"
        stale.tmuxSocket = "mxl-profile"
        var persisted = layout
        persisted.removeInstance(id: a.id, now: 2)
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }

        let permitted = try await state.bindTmuxTarget(.init(name: "muxel_host_aaaaaaaa", socket: "mxl-profile"),
                                                      instance: stale, in: project)

        XCTAssertFalse(permitted)
        let commands = await conn.commands
        XCTAssertEqual(commands.filter { $0.hasPrefix("f=") }.count, 1)
        XCTAssertFalse(commands.contains { $0.hasPrefix("cat > ") })
    }

    func testBindingWriteFailureThrowsInsteadOfAuthorizingFoundMember() async {
        let (state, project, a, _, layout) = fixture()
        let conn = LayoutTransactionConnection(layout, pauseFirstRead: false, failWrites: 1)
        state.connectionFactory = { _, _ in conn }
        do {
            _ = try await state.bindTmuxTarget(.init(name: "muxel_host_aaaaaaaa", socket: "mxl-profile"),
                                              instance: a, in: project)
            XCTFail("failed owner persistence must not authorize attachment")
        } catch {
            XCTAssertTrue(error.localizedDescription.contains("layout write failed"))
        }
        let saved = await conn.snapshot()
        XCTAssertNil(saved.instances.first { $0.id == a.id }?.tmuxSocket)
        XCTAssertNil(state.terminals.existing(a.id))
    }
}
