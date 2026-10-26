import XCTest
import Citadel
@testable import muxel

private actor SocketPollingConnection: SSHConnection {
    private let layouts: [RemoteLayout]
    private let emitRows: Bool
    private(set) var sweeps: [String] = []

    init(layouts: [RemoteLayout], emitRows: Bool = true) {
        self.layouts = layouts
        self.emitRows = emitRows
    }

    func connect() async throws {}
    func sshClient() async throws -> SSHClient? { nil }
    func close() async {}

    func run(_ command: String) async throws -> String {
        if command.hasPrefix("f="), let layout = layouts.first(where: {
            command.contains(Shell.quote(RemoteLayoutStore.path($0.remoteRoot)))
        }) {
            return String(decoding: try MuxelJSON.encoder.encode(layout), as: UTF8.self)
        }
        guard command.contains("list-panes") else { return "" }
        sweeps.append(command)
        guard emitRows else { return "" }
        // Simulate only the servers actually requested, not a canned row that
        // would let a caller omitting the recorded socket pass accidentally.
        return layouts.flatMap(\.orderedTerminalInstances).compactMap { instance -> String? in
            let socket = instance.tmuxSocket ?? TmuxCommands.socketName
            guard command.contains(TmuxCommands.selectSocket("", socket: socket)),
                  let name = instance.tmuxSession else { return nil }
            return "\(name)\t0\t0\t0\t\(socket)"
        }.joined(separator: "\n")
    }
}

@MainActor
final class RecordedSocketPollingTests: XCTestCase {
    private func layout(root: String, socket: String, id: String = UUID().uuidString.lowercased()) -> RemoteLayout {
        var instance = Instance(id: id, projectId: "p", title: "shell", program: nil, args: [])
        instance.tmuxSocket = socket
        instance.tmuxSession = TmuxSession.name(hostName: "host", instanceId: id)
        var layout = RemoteLayout(remoteRoot: root)
        layout.addInstanceAsTab(instance, now: 1)
        return layout
    }

    private func assertSockets(_ commands: [String], named: String, file: StaticString = #filePath, line: UInt = #line) {
        XCTAssertFalse(commands.isEmpty, file: file, line: line)
        for command in commands {
            XCTAssertEqual(command.components(separatedBy: "'list-panes'").count - 1, 3,
                           "one query per mxl/default/recorded socket", file: file, line: line)
            XCTAssertTrue(command.contains(TmuxCommands.selectSocket("", socket: named)), file: file, line: line)
            XCTAssertTrue(command.contains("\t\(named)"), "rows must retain their server owner", file: file, line: line)
        }
    }

    func testStandalonePollIncludesRecordedSocketAndDeduplicatesServers() async {
        let named = layout(root: "/srv/named", socket: "mxl-profile")
        let legacy = layout(root: "/srv/legacy", socket: "mxl")
        let defaultServer = layout(root: "/srv/default", socket: "default")
        let conn = SocketPollingConnection(layouts: [named, legacy, defaultServer])
        let instances = named.instances + legacy.instances + defaultServer.instances + named.instances
        let statuses = await PollService().poll(conn, instances: instances)
        XCTAssertTrue(statuses.allSatisfy(\.running))
        let commands = await conn.sweeps
        XCTAssertEqual(commands.count, 1)
        assertSockets(commands, named: "mxl-profile")
    }

    func testNamedOwnerDoesNotTakeStatusFromDefaultNamesake() async {
        let named = layout(root: "/srv/named", socket: "mxl-profile")
        let instance = named.instances[0]
        let session = instance.tmuxSession!
        let conn = MockSSHConnection { _ in
            "\(session)\t1\t0\t0\tdefault\n\(session)\t0\t1\t0\tmxl-profile\n"
        }
        let results = await PollService().poll(conn, instances: [instance])
        XCTAssertEqual(results.first?.running, true)
        XCTAssertEqual(results.first?.status, .blocked)
    }

    func testForegroundSelectedHostSweepIncludesSiblingProjectSocket() async {
        let state = TestFixtures.makeState()
        let (host, selected) = TestFixtures.seedProject(state)
        let sibling = RemoteProject(name: "sibling", hostId: host.id, remoteRoot: "/srv/sibling")
        state.doc.projects.append(sibling)
        let conn = SocketPollingConnection(layouts: [
            layout(root: selected.remoteRoot, socket: "mxl"),
            layout(root: sibling.remoteRoot, socket: "mxl-profile"),
        ])
        state.connectionFactory = { _, _ in conn }
        state.selectedProject = selected
        await state.refreshAll()
        XCTAssertEqual(state.running.count, 1)
        XCTAssertEqual(state.projectActivity[sibling.id]?.running, 1)
        let commands = await conn.sweeps
        assertSockets(commands, named: "mxl-profile")
    }

    func testSidebarSweepWithoutSelectionIncludesRecordedSocket() async {
        let state = TestFixtures.makeState()
        let (host, project) = TestFixtures.seedProject(state)
        let conn = SocketPollingConnection(layouts: [layout(root: project.remoteRoot, socket: "mxl-profile")])
        state.connectionFactory = { _, _ in conn }
        _ = state.connection(for: host) // A sidebar sweep only uses pooled hosts.
        await state.refreshAll()
        XCTAssertEqual(state.projectActivity[project.id]?.running, 1)
        let commands = await conn.sweeps
        assertSockets(commands, named: "mxl-profile")
    }

    func testBackgroundReadsAllLayoutsBeforeItsSingleHostSweep() async throws {
        let directory = FileManager.default.temporaryDirectory.appendingPathComponent(UUID().uuidString)
        defer { try? FileManager.default.removeItem(at: directory) }
        let store = LocalStore(directory: directory)
        let host = Host(name: "host", hostname: "example.com")
        let a = RemoteProject(name: "a", hostId: host.id, remoteRoot: "/srv/a")
        let b = RemoteProject(name: "b", hostId: host.id, remoteRoot: "/srv/b")
        var doc = StoreDocument()
        doc.hosts = [host]
        doc.projects = [a, b]
        try store.save(doc)
        // Empty servers exercise the real background caller without posting test
        // notifications or starting a Live Activity for these simulated sessions.
        let conn = SocketPollingConnection(layouts: [
            layout(root: a.remoteRoot, socket: "mxl"),
            layout(root: b.remoteRoot, socket: "mxl-profile"),
        ], emitRows: false)
        let summaryKey = "liveactivity.lastsummary"
        let previousSummary = UserDefaults.standard.object(forKey: summaryKey)
        defer { UserDefaults.standard.set(previousSummary, forKey: summaryKey) }
        let posted = await StatusPoller(store: store).run(makeConnection: { _, _ in conn })
        XCTAssertEqual(posted, 0)
        let commands = await conn.sweeps
        XCTAssertEqual(commands.count, 1)
        assertSockets(commands, named: "mxl-profile")
    }
}
