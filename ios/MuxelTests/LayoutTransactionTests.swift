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
    private let listedTargets: String
    private var releaseRead: CheckedContinuation<Void, Never>?
    private let onPaused: () -> Void
    private let onOverlappingRead: () -> Void
    private(set) var commands: [String] = []

    init(_ document: RemoteLayout, pauseFirstRead: Bool = true, failWrites: Int = 0,
         listedTargets: String = "mxl\tmuxel_host_aaaaaaaa\n",
         onPaused: @escaping () -> Void = {}, onOverlappingRead: @escaping () -> Void = {}) {
        self.document = document
        self.pauseNextRead = pauseFirstRead
        self.writeFailuresRemaining = failWrites
        self.listedTargets = listedTargets
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
        if command.contains("list-sessions") { return listedTargets }
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

    func testStaleBindingDeclinesSavedOwnerAndRefreshesWithoutWriting() async throws {
        let (state, project, a, _, layout) = fixture()
        var persisted = layout
        persisted.instances[0].tmuxSession = "muxel_host_aaaaaaaa"
        persisted.instances[0].tmuxSocket = "default"
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }

        let permitted = try await state.bindTmuxTarget(.init(name: "muxel_host_aaaaaaaa", socket: "mxl"),
                                                      instance: a, in: project)

        XCTAssertFalse(permitted)
        XCTAssertEqual(state.layout, persisted, "the next resolution must see the durable owner")
        let saved = await conn.snapshot()
        XCTAssertEqual(saved, persisted)
        let commands = await conn.commands
        XCTAssertEqual(commands.filter { $0.hasPrefix("f=") }.count, 1)
        XCTAssertFalse(commands.contains { $0.hasPrefix("cat > ") })
    }

    func testAttachmentDiscardsStalePlanResolvesSavedDefaultAndClosesOnlyDefault() async throws {
        let (state, project, a, _, layout) = fixture()
        var persisted = layout
        persisted.instances[0].tmuxSession = "muxel_host_aaaaaaaa"
        persisted.instances[0].tmuxSocket = "default"
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false,
            listedTargets: "mxl\tmuxel_host_aaaaaaaa\ndefault\tmuxel_host_aaaaaaaa\n")
        state.connectionFactory = { _, _ in conn }
        var plannedSockets: [String?] = []
        var startedCommands: [String] = []

        let command = try await state.prepareTmuxAttachment(instanceId: a.id, in: project) { current in
            plannedSockets.append(current.tmuxSocket)
            let targets = TmuxCommands.parseTargets(try await conn.run(
                TmuxCommands.listTargetsCommand(socket: current.tmuxSocket)))
            let target = try XCTUnwrap(TmuxCommands.resolveTarget(current, in: targets))
            return (target: target, attachment: TmuxCommands.attachPTYCommand(session: target.name, socket: target.socket))
        }
        if let command { startedCommands.append(command) }

        XCTAssertEqual(plannedSockets, [nil, "default"])
        XCTAssertEqual(startedCommands, [TmuxCommands.attachPTYCommand(session: "muxel_host_aaaaaaaa", socket: "default")],
                       "only the re-resolved plan may cross the PTY start guard")
        var commands = await conn.commands
        XCTAssertEqual(commands.filter { $0.hasPrefix("f=") }.count, 2)
        XCTAssertEqual(commands.filter { $0.contains("list-sessions") }.count, 2)
        XCTAssertFalse(commands.contains { $0.hasPrefix("cat > ") })
        let saved = await conn.snapshot()
        XCTAssertEqual(saved, persisted)

        await state.close(a, in: project) // the mounted view may still hold its legacy instance
        commands = await conn.commands
        let kills = commands.filter { $0.contains("kill-session") }
        XCTAssertEqual(kills.count, 1)
        XCTAssertTrue(kills.first?.contains("; set --; tmux") ?? false)
        XCTAssertFalse(kills.first?.contains("-L") ?? true)
    }

    func testQueuedStaleBindingCannotReplaceOwnerSavedByEarlierBinding() async throws {
        let (state, project, a, _, layout) = fixture()
        let paused = expectation(description: "first binding captured unresolved ownership")
        let overlap = expectation(description: "stale binding read before first binding saved")
        overlap.isInverted = true
        let conn = LayoutTransactionConnection(layout, onPaused: { paused.fulfill() },
                                               onOverlappingRead: { overlap.fulfill() })
        state.connectionFactory = { _, _ in conn }
        let first = Task { try await state.bindTmuxTarget(.init(name: "muxel_host_aaaaaaaa", socket: "default"),
                                                        instance: a, in: project) }
        await fulfillment(of: [paused], timeout: 1)
        let submitted = expectation(description: "stale binding submitted while earlier binding is paused")
        let stale = Task { @MainActor in
            submitted.fulfill()
            return try await state.bindTmuxTarget(.init(name: "muxel_host_aaaaaaaa", socket: "mxl"),
                                                 instance: a, in: project)
        }
        await fulfillment(of: [submitted], timeout: 1)
        await fulfillment(of: [overlap], timeout: 0.1)
        await conn.resumeRead()
        let firstPermitted = try await first.value
        let stalePermitted = try await stale.value

        XCTAssertTrue(firstPermitted)
        XCTAssertFalse(stalePermitted)
        let saved = await conn.snapshot()
        XCTAssertEqual(saved.instances[0].tmuxSocket, "default")
        XCTAssertEqual(state.layout, saved)
        let commands = await conn.commands
        XCTAssertEqual(commands.filter { $0.hasPrefix("f=") }.count, 2)
        XCTAssertEqual(commands.filter { $0.hasPrefix("cat > ") }.count, 1)
    }

    func testSavedSocketWithoutNameOnlyAcceptsNameOnThatSocket() async throws {
        let (state, project, a, _, layout) = fixture()
        var persisted = layout
        persisted.instances[0].tmuxSocket = "default"
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }

        let wrong = try await state.bindTmuxTarget(.init(name: "muxel_host_aaaaaaaa", socket: "mxl"),
                                                  instance: a, in: project)
        XCTAssertFalse(wrong)
        XCTAssertEqual(state.layout, persisted)
        let right = try await state.bindTmuxTarget(.init(name: "muxel_host_aaaaaaaa", socket: "default"),
                                                  instance: a, in: project)
        XCTAssertTrue(right)
        let saved = await conn.snapshot()
        XCTAssertEqual(saved.instances[0].tmuxSocket, "default")
        XCTAssertEqual(saved.instances[0].tmuxSession, "muxel_host_aaaaaaaa")
        let commands = await conn.commands
        XCTAssertEqual(commands.filter { $0.hasPrefix("cat > ") }.count, 1)
    }

    func testSavedConcreteOwnerCannotChangeNameOnSameSocket() async throws {
        let (state, project, a, _, layout) = fixture()
        var persisted = layout
        persisted.instances[0].tmuxSession = "muxel_original_aaaaaaaa"
        persisted.instances[0].tmuxSocket = "default"
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }

        let permitted = try await state.bindTmuxTarget(.init(name: "muxel_renamed_aaaaaaaa", socket: "default"),
                                                      instance: a, in: project)
        XCTAssertFalse(permitted)
        XCTAssertEqual(state.layout, persisted)
        let commands = await conn.commands
        XCTAssertFalse(commands.contains { $0.hasPrefix("cat > ") })
    }

    func testLegacyNameWithoutSocketCanStillResolveRenamedSession() async throws {
        let (state, project, a, _, layout) = fixture()
        var persisted = layout
        persisted.instances[0].tmuxSession = "muxel_original_aaaaaaaa"
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }
        let target = try XCTUnwrap(TmuxCommands.resolveTarget(persisted.instances[0], in: [
            .init(name: "muxel_renamed_aaaaaaaa", socket: "default")
        ]))

        let permitted = try await state.bindTmuxTarget(target, instance: a, in: project)

        XCTAssertTrue(permitted)
        let saved = await conn.snapshot()
        XCTAssertEqual(saved.instances[0].tmuxSession, target.name)
        XCTAssertEqual(saved.instances[0].tmuxSocket, target.socket)
    }

    func testAttachmentStopsAfterOneConflictRetry() async throws {
        let (state, project, a, _, layout) = fixture()
        var persisted = layout
        persisted.instances[0].tmuxSession = "muxel_host_aaaaaaaa"
        persisted.instances[0].tmuxSocket = "default"
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }
        var attempts = 0
        let command = try await state.prepareTmuxAttachment(instanceId: a.id, in: project) { _ in
            attempts += 1
            return (target: TmuxCommands.SessionTarget(name: "muxel_host_aaaaaaaa", socket: "mxl"), attachment: "stale")
        }

        XCTAssertNil(command)
        XCTAssertEqual(attempts, 2)
        let saved = await conn.snapshot()
        XCTAssertEqual(saved, persisted)
        let commands = await conn.commands
        XCTAssertFalse(commands.contains { $0.hasPrefix("cat > ") })
    }

    func testAttachmentDoesNotRetryRemovedPaneOrStartAfterNavigation() async throws {
        let (state, project, a, _, layout) = fixture()
        var persisted = layout
        persisted.removeInstance(id: a.id, now: 2)
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }
        var attempts = 0
        let removed = try await state.prepareTmuxAttachment(instanceId: a.id, in: project) { _ in
            attempts += 1
            return (target: TmuxCommands.SessionTarget(name: "muxel_host_aaaaaaaa", socket: "mxl"), attachment: "removed")
        }
        XCTAssertNil(removed)
        XCTAssertEqual(attempts, 1)
        XCTAssertEqual(state.layout, persisted)

        state.layout = layout
        let navigated = try await state.prepareTmuxAttachment(instanceId: a.id, in: project) { _ in
            state.selectedProject = nil
            return (target: TmuxCommands.SessionTarget(name: "muxel_host_aaaaaaaa", socket: "mxl"), attachment: "navigated")
        }
        XCTAssertNil(navigated)
        let commands = await conn.commands
        XCTAssertEqual(commands.filter { $0.hasPrefix("f=") }.count, 1,
                       "navigation after planning cannot enter the binding queue")
        XCTAssertFalse(commands.contains { $0.hasPrefix("cat > ") })
    }

    func testAttachmentDoesNotReturnPlanOrRetryAfterBindingWriteFailure() async {
        let (state, project, a, _, layout) = fixture()
        let conn = LayoutTransactionConnection(layout, pauseFirstRead: false, failWrites: 1)
        state.connectionFactory = { _, _ in conn }
        var attempts = 0
        do {
            _ = try await state.prepareTmuxAttachment(instanceId: a.id, in: project) { _ in
                attempts += 1
                return (target: TmuxCommands.SessionTarget(name: "muxel_host_aaaaaaaa", socket: "mxl"), attachment: "unsaved")
            }
            XCTFail("a plan whose owner could not be saved must not reach PTY startup")
        } catch {
            XCTAssertTrue(error.localizedDescription.contains("layout write failed"))
        }
        XCTAssertEqual(attempts, 1)
        let saved = await conn.snapshot()
        XCTAssertEqual(saved, layout)
    }

    func testAttachmentDoesNotBindAfterCancellationDuringPlanning() async throws {
        let (state, project, a, _, layout) = fixture()
        let conn = LayoutTransactionConnection(layout, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }
        let preparing = Task { @MainActor in
            try await state.prepareTmuxAttachment(instanceId: a.id, in: project) { _ in
                withUnsafeCurrentTask { $0?.cancel() }
                return (target: TmuxCommands.SessionTarget(name: "muxel_host_aaaaaaaa", socket: "mxl"), attachment: "cancelled")
            }
        }
        let command = try await preparing.value
        XCTAssertNil(command)
        let commands = await conn.commands
        XCTAssertTrue(commands.isEmpty)
    }

    func testFreshLiteralPaneDoesNotPrepareOrBindTmuxAttachment() async throws {
        let (state, project, a, _, layout) = fixture()
        var literal = layout
        literal.instances[0].literalArgv = true
        state.layout = literal
        let conn = LayoutTransactionConnection(literal, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }
        var preparations = 0

        let command = try await state.prepareTmuxAttachment(instanceId: a.id, in: project) { _ in
            preparations += 1
            return (target: TmuxCommands.SessionTarget(name: "muxel_host_aaaaaaaa", socket: "mxl"), attachment: "literal")
        }

        XCTAssertNil(command)
        XCTAssertEqual(preparations, 0)
        let commands = await conn.commands
        XCTAssertTrue(commands.isEmpty)
    }

    func testDurableLiteralConversionStopsAttachmentBeforeRetryOrOwnerWrite() async throws {
        let (state, project, a, _, layout) = fixture()
        var persisted = layout
        persisted.instances[0].literalArgv = true
        let conn = LayoutTransactionConnection(persisted, pauseFirstRead: false)
        state.connectionFactory = { _, _ in conn }
        var preparations = 0

        let command = try await state.prepareTmuxAttachment(instanceId: a.id, in: project) { _ in
            preparations += 1
            return (target: TmuxCommands.SessionTarget(name: "muxel_host_aaaaaaaa", socket: "default"), attachment: "stale")
        }

        XCTAssertNil(command, "a literal conversion must decline the stale plan before saving its owner")
        XCTAssertEqual(preparations, 1, "the refreshed literal member must stop before another preparation")
        XCTAssertEqual(state.layout, persisted)
        let saved = await conn.snapshot()
        XCTAssertEqual(saved, persisted)
        let commands = await conn.commands
        XCTAssertEqual(commands.filter { $0.hasPrefix("f=") }.count, 1)
        XCTAssertFalse(commands.contains { $0.hasPrefix("cat > ") })
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
