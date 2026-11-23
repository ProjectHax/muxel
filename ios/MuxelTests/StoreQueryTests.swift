import XCTest
@testable import muxel

/// Covers the `StoreDocument` reference queries behind the delete-confirmation copy.
final class StoreQueryTests: XCTestCase {

    func testHostsUsingIdentity() {
        let identity = Identity(name: "deploy")
        let other = Identity(name: "admin")
        var a = Host(name: "a", hostname: "a.example.com")
        a.identityId = identity.id
        var b = Host(name: "b", hostname: "b.example.com")
        b.identityId = other.id
        let c = Host(name: "c", hostname: "c.example.com") // inline credentials

        var doc = StoreDocument()
        doc.identities = [identity, other]
        doc.hosts = [a, b, c]

        XCTAssertEqual(doc.hosts(using: identity).map(\.name), ["a"])
        XCTAssertEqual(doc.hosts(using: other).map(\.name), ["b"])
        XCTAssertTrue(doc.hosts(using: Identity(name: "unused")).isEmpty)
    }

    func testProjectsUnderHost() {
        let host = Host(name: "web", hostname: "example.com")
        let stranger = Host(name: "db", hostname: "db.example.com")
        var doc = StoreDocument()
        doc.hosts = [host, stranger]
        doc.projects = [
            RemoteProject(name: "api", hostId: host.id, remoteRoot: "/srv/api"),
            RemoteProject(name: "site", hostId: host.id, remoteRoot: "/srv/site"),
            RemoteProject(name: "etl", hostId: stranger.id, remoteRoot: "/srv/etl"),
        ]

        XCTAssertEqual(doc.projects(under: host).map(\.name), ["api", "site"])
        XCTAssertEqual(doc.projects(under: stranger).map(\.name), ["etl"])
    }

    @MainActor
    func testCollapsedHostsPersistAcrossRelaunchAndPruneDeleted() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxel-collapsed-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let suiteName = "muxel.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let store = LocalStore(directory: dir)
        let hostA = Host(name: "alpha", hostname: "a.example.com")
        let hostB = Host(name: "beta", hostname: "b.example.com")
        var doc = StoreDocument()
        doc.hosts = [hostA, hostB]
        try store.save(doc)

        let first = AppState(store: store, defaults: defaults)
        XCTAssertFalse(first.isHostCollapsed(hostA.id))
        XCTAssertFalse(first.isHostCollapsed(hostB.id))

        first.toggleHostCollapsed(hostA.id)
        XCTAssertTrue(first.isHostCollapsed(hostA.id))
        XCTAssertFalse(first.isHostCollapsed(hostB.id))

        // Relaunching with the same store + defaults restores the collapsed host.
        let relaunched = AppState(store: store, defaults: defaults)
        relaunched.connectionFactory = { _, _ in MockSSHConnection() }
        XCTAssertTrue(relaunched.isHostCollapsed(hostA.id))
        XCTAssertFalse(relaunched.isHostCollapsed(hostB.id))

        // Importing projects under a collapsed host expands it automatically.
        relaunched.importDiscovered(
            [ProjectDiscovery.Found(remoteRoot: "/srv/repo"),
             ProjectDiscovery.Found(remoteRoot: "/srv/docs")],
            on: hostA)
        XCTAssertFalse(relaunched.isHostCollapsed(hostA.id))

        // Deleting a collapsed host removes its persisted id.
        relaunched.toggleHostCollapsed(hostB.id)
        XCTAssertTrue(relaunched.isHostCollapsed(hostB.id))
        relaunched.deleteHost(hostB)
        XCTAssertFalse(relaunched.isHostCollapsed(hostB.id))

        let afterDelete = AppState(store: store, defaults: defaults)
        XCTAssertFalse(afterDelete.isHostCollapsed(hostB.id))
    }

    @MainActor
    func testOpenedProjectsMoveToTopWithoutReshufflingOnReopen() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxel-opened-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let suiteName = "muxel.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let store = LocalStore(directory: dir)
        let host = Host(name: "dev", hostname: "dev.example.com")
        let pA = RemoteProject(name: "a", hostId: host.id, remoteRoot: "/srv/a")
        let pB = RemoteProject(name: "b", hostId: host.id, remoteRoot: "/srv/b")
        let pC = RemoteProject(name: "c", hostId: host.id, remoteRoot: "/srv/c")
        let pD = RemoteProject(name: "d", hostId: host.id, remoteRoot: "/srv/d")
        var doc = StoreDocument()
        doc.hosts = [host]
        doc.projects = [pA, pB, pC, pD]
        try store.save(doc)

        let state = AppState(store: store, defaults: defaults)
        XCTAssertEqual(state.projects(for: host).map(\.name), ["a", "b", "c", "d"])

        // Opening D promotes it to the top.
        state.markProjectOpened(pD.id)
        XCTAssertEqual(state.projects(for: host).map(\.name), ["d", "a", "b", "c"])

        // Opening B promotes B above D.
        state.markProjectOpened(pB.id)
        XCTAssertEqual(state.projects(for: host).map(\.name), ["b", "d", "a", "c"])

        // Reopening D must NOT reshuffle [b, d].
        state.markProjectOpened(pD.id)
        XCTAssertEqual(state.projects(for: host).map(\.name), ["b", "d", "a", "c"])

        // Reopening B must NOT reshuffle [b, d].
        state.markProjectOpened(pB.id)
        XCTAssertEqual(state.projects(for: host).map(\.name), ["b", "d", "a", "c"])

        // Order persists across relaunch.
        let relaunched = AppState(store: store, defaults: defaults)
        XCTAssertEqual(relaunched.projects(for: host).map(\.name), ["b", "d", "a", "c"])

        // Deleting an opened project removes it cleanly.
        relaunched.deleteProject(pB)
        XCTAssertEqual(relaunched.projects(for: host).map(\.name), ["d", "a", "c"])
    }

    @MainActor
    func testSidebarSweepListsAgentsAndPreservesThemAcrossDeselect() async throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxel-sidebar-agents-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let suiteName = "muxel.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let store = LocalStore(directory: dir)
        let host = Host(name: "rhel", hostname: "rhel.example.com")
        let project = RemoteProject(name: "phNexus", hostId: host.id, remoteRoot: "/home/ryan/Projects/phNexus")
        var doc = StoreDocument()
        doc.hosts = [host]
        doc.projects = [project]
        try store.save(doc)

        let idA = "11111111-1111-1111-1111-111111111111"
        let idB = "22222222-2222-2222-2222-222222222222"
        var instA = Instance(id: idA, projectId: "p", title: "claude", program: "claude", args: [])
        instA.customName = "Auth fix"
        instA.tmuxSession = "muxel_rhel_11111111"
        instA.tmuxSocket = "mxl"
        var instB = Instance(id: idB, projectId: "p", title: "codex", program: "codex", args: [])
        instB.tmuxSession = "muxel_rhel_22222222"
        instB.tmuxSocket = "mxl"
        var remoteLayout = RemoteLayout(remoteRoot: project.remoteRoot)
        remoteLayout.addInstanceAsTab(instA, now: 1)
        remoteLayout.addInstanceAsTab(instB, now: 2)
        let layoutJSON = String(decoding: try MuxelJSON.encoder.encode(remoteLayout), as: UTF8.self)
        let now = Int(Date().timeIntervalSince1970)

        let mock = MockSSHConnection { cmd in
            if cmd.contains("workspace.json") { return layoutJSON }
            if cmd.contains("list-panes") { return "muxel_rhel_11111111\t0\t1\t\(now)\tmxl\n" }
            return ""
        }

        let state = AppState(store: store, defaults: defaults)
        state.connectionFactory = { _, _ in mock }

        await state.refreshSidebarStatuses(connectIfNeeded: true)

        let activity = try XCTUnwrap(state.activity(for: project))
        XCTAssertEqual(activity.running, 1)
        XCTAssertEqual(activity.blocked, 1)
        // Both the running blocked agent and the stopped agent appear on the main screen.
        XCTAssertEqual(activity.agents.map(\.id), [idA, idB])
        XCTAssertEqual(activity.agents.map(\.name), ["Auth fix", "codex"])
        XCTAssertEqual(activity.agents.map(\.running), [true, false])
        XCTAssertEqual(activity.agents[0].status, .blocked)

        // Tapping a specific agent focuses it and backing out preserves the agents list.
        // Because the sidebar sweep cached the layout, `select` populates `layout`
        // synchronously (0ms) and exposes a fast-attach target for the live agent.
        state.select(project, focusingInstance: idA)
        XCTAssertEqual(state.lastLaunched, idA)
        XCTAssertEqual(state.layoutLoad, .loaded)
        XCTAssertEqual(state.layout?.instances.count, 2)
        let fast = state.fastAttachTarget(for: instA, in: project, hostId: host.id)
        XCTAssertEqual(fast, TmuxCommands.SessionTarget(name: "muxel_rhel_11111111", socket: "mxl"))
        // Stopped agent B has no live row, so it goes through full create/resolve.
        XCTAssertNil(state.fastAttachTarget(for: instB, in: project, hostId: host.id))

        state.deselect()
        let afterDeselect = try XCTUnwrap(state.activity(for: project))
        XCTAssertEqual(afterDeselect.agents.map(\.id), [idA, idB])
    }

    @MainActor
    func testAutoWatchProjectsPersistAndScanWatchesOnlyActiveProjects() async throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxel-watch-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let suiteName = "muxel.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let store = LocalStore(directory: dir)
        let host = Host(name: "rhel", hostname: "rhel.example.com")
        let pPinned = RemoteProject(name: "pinned", hostId: host.id, remoteRoot: "/srv/pinned")
        let pWorking = RemoteProject(name: "working", hostId: host.id, remoteRoot: "/srv/working")
        let pQuiet = RemoteProject(name: "quiet", hostId: host.id, remoteRoot: "/srv/quiet")
        var doc = StoreDocument()
        doc.hosts = [host]
        doc.projects = [pPinned, pWorking, pQuiet]
        try store.save(doc)

        let idWork = "11111111-1111-1111-1111-111111111111"
        let idQuiet = "22222222-2222-2222-2222-222222222222"
        var instWork = Instance(id: idWork, projectId: "p", title: "claude", program: "claude", args: [])
        instWork.tmuxSession = "muxel_rhel_11111111"
        instWork.tmuxSocket = "mxl"
        var instQuiet = Instance(id: idQuiet, projectId: "p", title: "shell", program: nil, args: [])
        instQuiet.tmuxSession = "muxel_rhel_22222222"
        instQuiet.tmuxSocket = "mxl"

        var lWork = RemoteLayout(remoteRoot: pWorking.remoteRoot)
        lWork.addInstanceAsTab(instWork, now: 1)
        var lQuiet = RemoteLayout(remoteRoot: pQuiet.remoteRoot)
        lQuiet.addInstanceAsTab(instQuiet, now: 1)
        let jWork = String(decoding: try MuxelJSON.encoder.encode(lWork), as: UTF8.self)
        let jQuiet = String(decoding: try MuxelJSON.encoder.encode(lQuiet), as: UTF8.self)
        let now = Int(Date().timeIntervalSince1970)
        let old = now - 3600

        let mock = MockSSHConnection { cmd in
            if cmd.hasPrefix("f="), cmd.contains("/srv/working") { return jWork }
            if cmd.hasPrefix("f="), cmd.contains("/srv/quiet") { return jQuiet }
            if cmd.contains("list-panes") {
                return "muxel_rhel_11111111\t0\t0\t\(now - 3)\tmxl\n"
                    + "muxel_rhel_22222222\t0\t0\t\(old)\tmxl\n"
            }
            return ""
        }

        let first = AppState(store: store, defaults: defaults)
        first.connectionFactory = { _, _ in mock }
        first.setProjectAutoWatched(pPinned.id, true)
        first.setAutoWatchActiveOnLaunch(true)
        XCTAssertTrue(first.isProjectAutoWatched(pPinned.id))
        XCTAssertTrue(first.isProjectWatched(pPinned.id))
        XCTAssertTrue(first.autoWatchActiveOnLaunch)

        // Relaunch with a fresh empty UserDefaults suite — proves settings persisted
        // atomically inside `store.json` (not only UserDefaults).
        let freshSuite = "muxel.tests.fresh.\(UUID().uuidString)"
        let freshDefaults = try XCTUnwrap(UserDefaults(suiteName: freshSuite))
        defer { freshDefaults.removePersistentDomain(forName: freshSuite) }

        let relaunched = AppState(store: store, defaults: freshDefaults)
        relaunched.connectionFactory = { _, _ in mock }
        XCTAssertTrue(relaunched.isProjectAutoWatched(pPinned.id))
        XCTAssertTrue(relaunched.isProjectWatched(pPinned.id))
        XCTAssertTrue(relaunched.autoWatchActiveOnLaunch)
        XCTAssertFalse(relaunched.isProjectWatched(pWorking.id))
        XCTAssertFalse(relaunched.isProjectWatched(pQuiet.id))

        // Scanning watches `working` (active 3s ago) and skips `quiet` (idle 1h ago).
        let activeCount = await relaunched.scanAndWatchWorkingProjects(announce: false)
        XCTAssertEqual(activeCount, 1)
        XCTAssertTrue(relaunched.isProjectWatched(pWorking.id))
        XCTAssertFalse(relaunched.isProjectWatched(pQuiet.id))

        // Manually unwatching `working` is respected by subsequent automatic sidebar sweeps.
        relaunched.setProjectWatched(pWorking.id, false)
        XCTAssertFalse(relaunched.isProjectWatched(pWorking.id))
        await relaunched.refreshSidebarStatuses(connectIfNeeded: true)
        XCTAssertFalse(relaunched.isProjectWatched(pWorking.id))

        // Deleting a pinned project prunes its persisted auto-watch id.
        relaunched.deleteProject(pPinned)
        XCTAssertFalse(relaunched.isProjectAutoWatched(pPinned.id))
        XCTAssertFalse(relaunched.isProjectWatched(pPinned.id))
    }

    func testNotificationRouteRoundTripAndValidation() {
        let projectId = UUID()
        let instanceId = "11111111-1111-1111-1111-111111111111"
        let info = NotificationManager.userInfo(projectId: projectId, instanceId: instanceId)
        XCTAssertEqual(
            NotificationManager.route(from: info),
            NotificationRoute(projectId: projectId, instanceId: instanceId)
        )
        XCTAssertNil(NotificationManager.route(from: [:]))
        XCTAssertNil(NotificationManager.route(from: [
            NotificationManager.projectIdKey: "not-a-uuid",
            NotificationManager.instanceIdKey: instanceId,
        ]))
        XCTAssertNil(NotificationManager.route(from: [
            NotificationManager.projectIdKey: projectId.uuidString,
            NotificationManager.instanceIdKey: "",
        ]))
    }

    @MainActor
    func testOpenNotificationRouteFocusesProjectAndAgent() throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxel-notif-route-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let suiteName = "muxel.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let store = LocalStore(directory: dir)
        let host = Host(name: "rhel", hostname: "rhel.example.com")
        let projectA = RemoteProject(name: "alpha", hostId: host.id, remoteRoot: "/srv/alpha")
        let projectB = RemoteProject(name: "beta", hostId: host.id, remoteRoot: "/srv/beta")
        var doc = StoreDocument()
        doc.hosts = [host]
        doc.projects = [projectA, projectB]
        try store.save(doc)

        let state = AppState(store: store, defaults: defaults)
        state.connectionFactory = { _, _ in MockSSHConnection { _ in "" } }
        defer { state.stopPolling() }
        state.toggleHostCollapsed(host.id)
        XCTAssertTrue(state.isHostCollapsed(host.id))

        let agentA1 = "11111111-1111-1111-1111-111111111111"
        let agentA2 = "22222222-2222-2222-2222-222222222222"
        let agentB1 = "33333333-3333-3333-3333-333333333333"

        // Tapping a notification from home expands the collapsed host, selects the
        // project, and targets the exact agent tab.
        state.openNotificationRoute(NotificationRoute(projectId: projectA.id, instanceId: agentA1))
        XCTAssertFalse(state.isHostCollapsed(host.id))
        XCTAssertEqual(state.selectedProject?.id, projectA.id)
        XCTAssertEqual(state.lastLaunched, agentA1)

        // Tapping another notification inside the same project updates the focused tab.
        state.openNotificationRoute(NotificationRoute(projectId: projectA.id, instanceId: agentA2))
        XCTAssertEqual(state.selectedProject?.id, projectA.id)
        XCTAssertEqual(state.lastLaunched, agentA2)

        // Tapping a notification for a different project switches project + tab.
        state.openNotificationRoute(NotificationRoute(projectId: projectB.id, instanceId: agentB1))
        XCTAssertEqual(state.selectedProject?.id, projectB.id)
        XCTAssertEqual(state.lastLaunched, agentB1)

        // Unknown project IDs are ignored without disturbing the active selection.
        state.openNotificationRoute(NotificationRoute(projectId: UUID(), instanceId: agentA1))
        XCTAssertEqual(state.selectedProject?.id, projectB.id)
        XCTAssertEqual(state.lastLaunched, agentB1)
    }

    @MainActor
    func testDesktopStartedPromptIsDetectedWithoutOpeningAgent() async throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxel-desktop-prompt-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }

        let suiteName = "muxel.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let store = LocalStore(directory: dir)
        let host = Host(name: "rhel", hostname: "rhel.example.com")
        let project = RemoteProject(name: "muxel", hostId: host.id, remoteRoot: "/srv/muxel")
        var doc = StoreDocument()
        doc.hosts = [host]
        doc.projects = [project]
        try store.save(doc)

        let agentId = "aaaaaaaa-1111-1111-1111-111111111111"
        var claude = Instance(id: agentId, projectId: "p", title: "Claude", program: "claude", args: [])
        claude.tmuxSession = "muxel_rhel_aaaaaaaa"
        claude.tmuxSocket = "mxl"
        var layout = RemoteLayout(remoteRoot: project.remoteRoot)
        layout.addInstanceAsTab(claude, now: 1)
        let layoutJSON = String(decoding: try MuxelJSON.encoder.encode(layout), as: UTF8.self)

        final class MockRemote: @unchecked Sendable {
            var activityOffset: Int = -3600
            var screenText: String = "❯ "
            var probeCount: Int = 0
        }
        let remote = MockRemote()
        let mock = MockSSHConnection { cmd in
            if cmd.hasPrefix("f=") { return layoutJSON }
            if cmd.contains("list-panes") {
                let ts = Int(Date().timeIntervalSince1970) + remote.activityOffset
                return "muxel_rhel_aaaaaaaa\t0\t0\t\(ts)\tmxl\tclaude\t0\n"
            }
            return ""
        }

        let state = AppState(store: store, defaults: defaults)
        state.connectionFactory = { _, _ in mock }
        state.screenProber = { targets, _ in
            remote.probeCount += 1
            var out: [String: LiveScreen] = [:]
            for t in targets {
                out[t.id] = LiveScreen(text: remote.screenText, idle: t.idle, bell: t.bell)
            }
            return out
        }
        defer { state.stopPolling() }

        // 1. Agent has been idle for 1 hour → zero screen probes, status is `.idle`.
        await state.refreshSidebarStatuses(connectIfNeeded: true)
        XCTAssertEqual(remote.probeCount, 0)
        XCTAssertEqual(state.activity(for: project)?.agents.first?.status, .idle)

        // 2. User starts a prompt on muxel desktop (desktop does NOT rewrite
        // `.muxel/workspace.json`, only tmux `window_activity` jumps to now and screen
        // shows Claude's working marker). Next sidebar sweep probes it and picks up `.working`.
        remote.activityOffset = 0
        remote.screenText = "Thinking…\nesc to interrupt"
        await state.refreshSidebarStatuses(connectIfNeeded: true)
        XCTAssertEqual(remote.probeCount, 1)
        XCTAssertEqual(state.activity(for: project)?.agents.first?.status, .working)

        // 3. Prompt finishes on desktop (marker leaves, pane goes quiet). Because iOS
        // knows it was `.working`, it probes again and latches `.done` immediately.
        remote.activityOffset = -30
        remote.screenText = "Done!\n❯ "
        await state.refreshSidebarStatuses(connectIfNeeded: true)
        XCTAssertEqual(remote.probeCount, 2)
        XCTAssertEqual(state.activity(for: project)?.agents.first?.status, .done)
    }

    func testUnmatchedLiveMuxelSessionAndProbeCandidateRules() {
        let knownId = "11111111-1111-1111-1111-111111111111"
        var claude = Instance(id: knownId, projectId: "p", title: "Claude", program: "claude", args: [])
        claude.tmuxSession = "muxel_rhel_11111111"
        claude.tmuxSocket = "mxl"

        let knownRow = PaneRow(session: "muxel_rhel_11111111", exited: false, bell: false, idle: 120,
                               socket: "mxl", activityTimestamp: 1000)
        let newDesktopRow = PaneRow(session: "muxel_rhel_22222222", exited: false, bell: false, idle: 1,
                                    socket: "mxl", activityTimestamp: 1119)
        XCTAssertFalse(AppState.hasUnmatchedLiveMuxelSession(rows: [knownRow], knownInstances: [claude]))
        XCTAssertTrue(AppState.hasUnmatchedLiveMuxelSession(rows: [knownRow, newDesktopRow], knownInstances: [claude]))

        // Quiet idle agent (>20s) is not probed; active agent (idle <= 20s) IS probed.
        XCTAssertTrue(AppState.activeProbeCandidates(
            instances: [claude], rows: [knownRow], attachedIds: [],
            lastStatuses: [knownId: .idle], idleProbeState: [:], now: 1120
        ).isEmpty)

        let activeRow = PaneRow(session: "muxel_rhel_11111111", exited: false, bell: false, idle: 2,
                                socket: "mxl", activityTimestamp: 1118)
        XCTAssertEqual(AppState.activeProbeCandidates(
            instances: [claude], rows: [activeRow], attachedIds: [],
            lastStatuses: [knownId: .idle], idleProbeState: [:], now: 1120
        ).count, 1)

        // Same unchanged activityTimestamp after an idle probe is skipped; advancing
        // activityTimestamp after cooldown triggers a fresh probe.
        XCTAssertTrue(AppState.activeProbeCandidates(
            instances: [claude], rows: [activeRow], attachedIds: [],
            lastStatuses: [knownId: .idle], idleProbeState: [knownId: (1100, 1118)], now: 1120
        ).isEmpty)
        XCTAssertEqual(AppState.activeProbeCandidates(
            instances: [claude], rows: [activeRow], attachedIds: [],
            lastStatuses: [knownId: .idle], idleProbeState: [knownId: (1100, 1110)], now: 1120
        ).count, 1)

        // Already-working agent is always probed even when idle > 20s (catches finish).
        XCTAssertEqual(AppState.activeProbeCandidates(
            instances: [claude], rows: [knownRow], attachedIds: [],
            lastStatuses: [knownId: .working], idleProbeState: [:], now: 1120
        ).count, 1)

        // Ollama Code (`ollama launch opencode`) resolves opencode markers.
        let (w, b) = defaultMarkers(program: "ollama", args: ["launch", "opencode", "--model", "x"])
        XCTAssertEqual(w, ["esc interrupt"])
        XCTAssertEqual(b, ["Permission required"])
    }


    @MainActor
    func testLaunchScanWithNoProjectsClearsTheScanningFlag() async throws {
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxel-scan-\(UUID().uuidString)")
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        defer { try? FileManager.default.removeItem(at: dir) }
        let suiteName = "muxel.tests.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suiteName))
        defer { defaults.removePersistentDomain(forName: suiteName) }

        let state = AppState(store: LocalStore(directory: dir), defaults: defaults)
        state.connectionFactory = { _, _ in MockSSHConnection() }
        state.setAutoWatchActiveOnLaunch(true)
        XCTAssertTrue(state.startMonitoringOnLaunch())
        XCTAssertTrue(state.isScanningActiveProjects)
        for _ in 0..<100 where state.isScanningActiveProjects {
            try await Task.sleep(nanoseconds: 20_000_000)
        }
        XCTAssertFalse(state.isScanningActiveProjects)
        XCTAssertEqual(state.scanStatusText, "No projects to scan yet.")
    }
}
