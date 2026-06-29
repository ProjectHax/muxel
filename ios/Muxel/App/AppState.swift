import Foundation
import SwiftUI

/// The app's observable state: the device-local host/project store, the active
/// project's live `RemoteLayout` read from the remote, per-instance status, and one
/// pooled SSH connection per host. Owns the foreground poll loop.
@MainActor
final class AppState: ObservableObject {
    @Published var doc: StoreDocument
    @Published var selectedProject: RemoteProject?
    @Published var layout: RemoteLayout?
    @Published var statuses: [String: AgentStatus] = [:]
    @Published var errorMessage: String?
    @Published var isBusy = false

    /// Injectable so previews/tests use `MockSSHConnection`.
    var connectionFactory: (Host) -> SSHConnection = { CitadelSSHConnection(host: $0) }

    private let store: LocalStore
    private var connections: [UUID: SSHConnection] = [:]
    private let poll = PollService()
    private var pollLoop: Task<Void, Never>?

    init(store: LocalStore = LocalStore()) {
        self.store = store
        self.doc = store.load()
    }

    // MARK: Lookups

    func host(for project: RemoteProject) -> Host? { doc.hosts.first { $0.id == project.hostId } }
    func host(id: UUID) -> Host? { doc.hosts.first { $0.id == id } }
    func projects(for host: Host) -> [RemoteProject] { doc.projects.filter { $0.hostId == host.id } }
    func status(_ instanceId: String) -> AgentStatus { statuses[instanceId] ?? .idle }

    // MARK: Host / project CRUD

    func addHost(_ host: Host, password: String?, keyData: Data?, passphrase: String?) {
        if let password { Keychain.setPassword(password, for: host.id) }
        if let keyData { Keychain.setPrivateKey(keyData, for: host.id) }
        if let passphrase, !passphrase.isEmpty { Keychain.setKeyPassphrase(passphrase, for: host.id) }
        doc.hosts.append(host)
        persist()
    }

    func deleteHost(_ host: Host) {
        Keychain.deleteAll(for: host.id)
        connections[host.id] = nil
        doc.projects.removeAll { $0.hostId == host.id }
        doc.hosts.removeAll { $0.id == host.id }
        persist()
    }

    func addProject(_ project: RemoteProject) {
        doc.projects.append(project)
        persist()
    }

    func deleteProject(_ project: RemoteProject) {
        doc.projects.removeAll { $0.id == project.id }
        if selectedProject?.id == project.id { selectedProject = nil; layout = nil }
        persist()
    }

    private func persist() { store.save(doc) }

    // MARK: Connections

    func connection(for host: Host) -> SSHConnection {
        if let c = connections[host.id] { return c }
        let c = connectionFactory(host)
        connections[host.id] = c
        return c
    }

    // MARK: Selecting a project (connect + read layout + poll)

    func select(_ project: RemoteProject) {
        selectedProject = project
        layout = nil
        statuses = [:]
        Task { await refreshLayout() }
        startPolling()
    }

    func refreshLayout() async {
        guard let project = selectedProject, let host = host(for: project) else { return }
        isBusy = true
        defer { isBusy = false }
        do {
            let conn = connection(for: host)
            try await conn.connect()
            layout = try await RemoteLayoutStore.read(conn, root: project.remoteRoot)
            await runPollOnce()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    private func startPolling() {
        pollLoop?.cancel()
        pollLoop = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(nanoseconds: 3_000_000_000)
                await self?.runPollOnce()
            }
        }
    }

    func stopPolling() { pollLoop?.cancel(); pollLoop = nil }

    private func runPollOnce() async {
        guard let project = selectedProject, let host = host(for: project), let layout else { return }
        let conn = connection(for: host)
        let results = await poll.poll(conn, instances: layout.orderedTerminalInstances)
        for r in results { statuses[r.instanceId] = r.status }
    }

    /// The user viewed a pane — clear its done latch so it stops showing done.
    func attend(_ instanceId: String) { poll.attend(instanceId) }

    // MARK: Launch a new instance

    func launch(preset: Preset, customCommand: String?, into project: RemoteProject) async {
        guard let host = host(for: project) else { return }
        isBusy = true
        defer { isBusy = false }

        let program: String?
        let args: [String]
        if let custom = customCommand, !custom.trimmingCharacters(in: .whitespaces).isEmpty {
            // Split a custom command line crudely into program + args.
            let parts = custom.split(separator: " ").map(String.init)
            program = parts.first
            args = Array(parts.dropFirst())
        } else {
            program = preset.program
            args = preset.args
        }

        let instanceId = UUID().uuidString.lowercased()
        let title = preset.program.map { ($0 as NSString).lastPathComponent } ?? "shell"
        // Reuse the project's existing instance project_id so iOS-launched panes
        // group with the rest; seed a fresh one for an empty project.
        let projectId = layout?.instances.first?.projectId ?? UUID().uuidString.lowercased()
        let instance = Instance(id: instanceId, projectId: projectId,
                                title: title, program: program, args: args)
        let session = TmuxSession.name(hostName: host.name, instanceId: instanceId)

        do {
            let conn = connection(for: host)
            try await conn.connect()
            // Create the session detached so it persists; then record it in the
            // shared layout so desktop sees the new pane.
            _ = try await conn.tmux(TmuxCommands.newSession(
                session: session, cwd: project.remoteRoot, program: program, args: args))
            layout = try await RemoteLayoutStore.appendInstance(conn, root: project.remoteRoot, instance: instance)
            await runPollOnce()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Kill an instance's tmux session and drop it from the layout.
    func close(_ instance: Instance, in project: RemoteProject) async {
        guard let host = host(for: project) else { return }
        let session = TmuxSession.name(hostName: host.name, instanceId: instance.id)
        do {
            let conn = connection(for: host)
            try await conn.connect()
            _ = try? await conn.tmux(TmuxCommands.killSession(session))
            layout = try await RemoteLayoutStore.removeInstance(conn, root: project.remoteRoot, instanceId: instance.id)
        } catch {
            errorMessage = error.localizedDescription
        }
    }
}
