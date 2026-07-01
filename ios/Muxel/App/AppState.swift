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
    /// Instance ids with a live tmux session right now (from the latest poll of the
    /// selected project).
    @Published var running: Set<String> = []
    @Published var errorMessage: String?
    @Published var isBusy = false
    /// Id of the most recently launched instance — the detail view selects it so its
    /// live terminal opens (and creates the session attached).
    @Published var lastLaunched: String?
    /// Result of a one-off "Test connection" — drives a confirmation alert.
    @Published var testResult: ConnectionTest?

    /// Outcome of testing a host's saved credential.
    struct ConnectionTest: Identifiable {
        let id = UUID()
        let hostName: String
        let ok: Bool
        let message: String
    }

    /// Injectable so previews/tests use `MockSSHConnection`. The second argument is
    /// the host's resolved shared-identity credential (nil = use the host's inline
    /// fields), so a connection authenticates with the right login + secret owner.
    var connectionFactory: (Host, ResolvedCredential?) -> SSHConnection = {
        CitadelSSHConnection(host: $0, credential: $1)
    }

    private let store: LocalStore
    private var connections: [UUID: SSHConnection] = [:]
    /// Live PTY terminals, owned here (not by the SwiftUI views) so a pane stays
    /// connected across navigation until the instance is closed or the app quits.
    let terminals = TerminalStore()
    private let poll = PollService()
    private var pollLoop: Task<Void, Never>?
    /// Foreground poll counter — used to re-read the shared layout only every Nth poll.
    private var pollTick = 0

    init(store: LocalStore = LocalStore()) {
        self.store = store
        self.doc = store.load()
    }

    // MARK: Lookups

    func host(for project: RemoteProject) -> Host? { doc.hosts.first { $0.id == project.hostId } }
    func host(id: UUID) -> Host? { doc.hosts.first { $0.id == id } }
    func projects(for host: Host) -> [RemoteProject] { doc.projects.filter { $0.hostId == host.id } }
    func status(_ instanceId: String) -> AgentStatus { statuses[instanceId] ?? .idle }
    func isRunning(_ instanceId: String) -> Bool { running.contains(instanceId) }

    /// Live instance count for `project` — known only for the selected project (the
    /// one the foreground poller is watching); `nil` for others.
    func runningCount(for project: RemoteProject) -> Int? {
        selectedProject?.id == project.id ? running.count : nil
    }

    // MARK: Host / project CRUD

    func addHost(_ host: Host, password: String?, keyData: Data?, passphrase: String?) {
        var saved = true
        if let password { saved = Keychain.setPassword(password, for: host.id) && saved }
        if let keyData { saved = Keychain.setPrivateKey(keyData, for: host.id) && saved }
        if let passphrase, !passphrase.isEmpty {
            saved = Keychain.setKeyPassphrase(passphrase, for: host.id) && saved
        }
        doc.hosts.append(host)
        persist()
        if !saved {
            errorMessage = "Couldn't save the credential to the Keychain. The host was added, but " +
                "you may need to re-add it or check the device's Keychain access."
        }
    }

    /// Connect to `host` with a *fresh* connection (re-reading its Keychain secret) to
    /// verify the saved credential authenticates. Surfaces the result via `testResult`.
    func testConnection(_ host: Host) async {
        isBusy = true
        defer { isBusy = false }
        let conn = connectionFactory(host, resolvedCredential(for: host))
        do {
            try await conn.connect()
            _ = try await conn.run("true")
            await conn.close()
            testResult = ConnectionTest(hostName: host.name, ok: true,
                                        message: "Connected and authenticated successfully.")
        } catch {
            await conn.close()
            testResult = ConnectionTest(hostName: host.name, ok: false,
                                        message: error.localizedDescription)
        }
    }

    func deleteHost(_ host: Host) {
        Keychain.deleteAll(for: host.id)
        terminals.disconnect(forHost: host.id)
        connections[host.id] = nil
        doc.projects.removeAll { $0.hostId == host.id }
        doc.hosts.removeAll { $0.id == host.id }
        persist()
    }

    // MARK: Identity CRUD (shared logins, secrets keyed by identity id)

    func addIdentity(_ identity: Identity, password: String?, keyData: Data?, passphrase: String?) {
        var saved = true
        if let password { saved = Keychain.setPassword(password, for: identity.id) && saved }
        if let keyData { saved = Keychain.setPrivateKey(keyData, for: identity.id) && saved }
        if let passphrase, !passphrase.isEmpty {
            saved = Keychain.setKeyPassphrase(passphrase, for: identity.id) && saved
        }
        doc.identities.append(identity)
        persist()
        if !saved {
            errorMessage = "Couldn't save the identity's credential to the Keychain."
        }
    }

    /// Update an identity's fields, optionally replacing its stored secret. Hosts
    /// referencing it get the new credentials on their next connect.
    func updateIdentity(_ identity: Identity, password: String?, keyData: Data?, passphrase: String?) {
        guard let idx = doc.identities.firstIndex(where: { $0.id == identity.id }) else { return }
        doc.identities[idx] = identity
        if let password, !password.isEmpty { _ = Keychain.setPassword(password, for: identity.id) }
        if let keyData { _ = Keychain.setPrivateKey(keyData, for: identity.id) }
        if let passphrase, !passphrase.isEmpty {
            _ = Keychain.setKeyPassphrase(passphrase, for: identity.id)
        }
        // Referencing hosts' pooled connections must re-auth with the new credential.
        dropConnectionsUsing(identity.id)
        persist()
    }

    func deleteIdentity(_ identity: Identity) {
        Keychain.deleteAll(for: identity.id)
        // Detach hosts that referenced it — they fall back to their inline fields.
        for i in doc.hosts.indices where doc.hosts[i].identityId == identity.id {
            doc.hosts[i].identityId = nil
        }
        dropConnectionsUsing(identity.id)
        doc.identities.removeAll { $0.id == identity.id }
        persist()
    }

    /// Drop pooled SSH connections for every host that references `identityId`, so
    /// they reconnect with the updated/removed credential.
    private func dropConnectionsUsing(_ identityId: UUID) {
        for host in doc.hosts where host.identityId == identityId {
            terminals.disconnect(forHost: host.id)
            connections[host.id] = nil
        }
    }

    func addProject(_ project: RemoteProject) {
        doc.projects.append(project)
        persist()
    }

    // MARK: Project discovery (scan the host for `.muxel/` markers)

    /// Connect to `host` and scan its filesystem for muxel projects, excluding ones
    /// already added under this host. Throws so the scan sheet can show connection
    /// errors inline (it's presented over the shared error alert).
    func scanProjects(on host: Host) async throws -> [ProjectDiscovery.Found] {
        isBusy = true
        defer { isBusy = false }
        let conn = connection(for: host)
        try await conn.connect()
        let existing = Set(projects(for: host).map(\.remoteRoot))
        return try await ProjectDiscovery.scan(conn).filter { !existing.contains($0.remoteRoot) }
    }

    /// Add the chosen discovered roots as projects under `host` (skips duplicates).
    func importDiscovered(_ found: [ProjectDiscovery.Found], on host: Host) {
        let existing = Set(projects(for: host).map(\.remoteRoot))
        for item in found where !existing.contains(item.remoteRoot) {
            doc.projects.append(RemoteProject(name: item.name, hostId: host.id, remoteRoot: item.remoteRoot))
        }
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
        let c = connectionFactory(host, resolvedCredential(for: host))
        connections[host.id] = c
        return c
    }

    /// The effective credential for a host: nil when it uses its own inline fields,
    /// else the referenced shared identity's user/auth + Keychain secret owner.
    func resolvedCredential(for host: Host) -> ResolvedCredential? {
        host.resolvedCredential(in: doc.identities)
    }

    /// Instant, no-network snapshot of every instance: the last cached full summary,
    /// overlaid with fresh live data for the selected project (the only one the
    /// foreground tracks). `nil` only when there are no instances anywhere. The
    /// background `StatusPoller` refines it with a full multi-project scan.
    func currentSummarySnapshot() -> MuxelActivityAttributes.ContentState? {
        var rows: [MuxelActivityAttributes.InstanceRow] = []
        var selectedIds = Set<String>()
        if let sel = selectedProject, let layout {
            for inst in layout.orderedTerminalInstances {
                selectedIds.insert(inst.id)
                rows.append(ActivitySummaryBuilder.row(
                    id: inst.id, name: inst.displayName, project: sel.name,
                    status: statuses[inst.id] ?? .idle, running: running.contains(inst.id)))
            }
        }
        // Carry other projects' instances from the last full background scan.
        for r in SummaryCache.load()?.instances ?? [] where !selectedIds.contains(r.id) {
            rows.append(r)
        }
        let state = ActivitySummaryBuilder.contentState(rows: rows, now: Date())
        return state.isEmpty ? nil : state
    }

    /// Reconcile the Live Activity to the current snapshot. Must be driven from the
    /// FOREGROUND (poll ticks, selecting a project, becoming active): ActivityKit only
    /// lets an activity be *started* while the app is foreground. Once started it
    /// persists onto the Lock Screen when the app is minimized; background polls then
    /// just update it. Applying an empty state ends the activity (no instances left).
    func syncLiveActivity() {
        let state = currentSummarySnapshot()
            ?? ActivitySummaryBuilder.contentState(rows: [], now: Date())
        Task { await LiveActivityController.apply(state) }
    }

    // MARK: Selecting a project (connect + read layout + poll)

    func select(_ project: RemoteProject) {
        selectedProject = project
        layout = nil
        statuses = [:]
        running = []
        Task { await refreshLayout() }
        startPolling()
    }

    /// Leave the active project (the detail was popped on iPhone). Clearing the
    /// selection is what lets the *same* project be re-selected: the sidebar binds
    /// navigation to `selectedProject?.id`, so if it stays set, the row reads as
    /// already-selected and tapping it again does nothing. Stops the foreground poll;
    /// background notifications stay on the `StatusPoller`.
    func deselect() {
        selectedProject = nil
        layout = nil
        statuses = [:]
        running = []
        stopPolling()
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
        guard let project = selectedProject, let host = host(for: project) else { return }
        let conn = connection(for: host)
        // Every ~5th poll (~15s), re-read the shared layout so a peer's rename (and
        // other edits) show up live while this project is open. Lightweight — a single
        // file read; the same instance ids keep the selected tab + live terminals.
        // (Selecting a project / returning to the app still refresh immediately.)
        pollTick &+= 1
        if pollTick % 5 == 0,
           let fresh = try? await RemoteLayoutStore.read(conn, root: project.remoteRoot) {
            layout = fresh
        }
        guard let layout else { return }
        let results = await poll.poll(conn, instances: layout.orderedTerminalInstances)
        for r in results { statuses[r.instanceId] = r.status }
        running = Set(results.filter(\.running).map(\.instanceId))
        // Foreground start/refresh — keeps the Live Activity alive so it's on the
        // Lock Screen when the app is minimized (starting must happen here).
        syncLiveActivity()
    }

    /// The user viewed a pane — clear its done latch so it stops showing done.
    func attend(_ instanceId: String) { poll.attend(instanceId) }

    // MARK: Launch a new instance

    func launch(preset: Preset, customCommand: String?, into project: RemoteProject) async {
        guard let host = host(for: project) else { return }

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
        var instance = Instance(id: instanceId, projectId: projectId,
                                title: title, program: program, args: args)
        // Record the authoritative tmux session name in the shared layout so a peer
        // (desktop muxel) adopts this instance into tmux under the same name rather
        // than spawning a second session for it.
        instance.tmuxSession = TmuxSession.name(hostName: host.name, instanceId: instanceId)

        // Show the pane immediately: add it to the in-memory layout and select it, so
        // the click feels instant. The live terminal creates the tmux session itself
        // (`new-session -A` over a PTY) when the pane opens — it doesn't need the shared
        // layout written first. We deliberately don't create a detached session here (it
        // crashes a TUI agent at init).
        var next = layout ?? RemoteLayout(remoteRoot: project.remoteRoot)
        next.addInstanceAsTab(instance, now: unixNow())
        layout = next
        lastLaunched = instanceId

        // Persist `.muxel/workspace.json` to the remote in the background (read-modify-
        // write over SSH) rather than blocking the UI on those round-trips; reassign the
        // authoritative layout when it returns (newer-wins also picks up peer changes).
        Task {
            do {
                let conn = self.connection(for: host)
                try await conn.connect()
                self.layout = try await RemoteLayoutStore.appendInstance(
                    conn, root: project.remoteRoot, instance: instance)
                await self.runPollOnce()
            } catch {
                self.errorMessage = error.localizedDescription
            }
        }
    }

    /// Kill an instance's tmux session and drop it from the layout.
    func close(_ instance: Instance, in project: RemoteProject) async {
        guard let host = host(for: project) else { return }
        do {
            let conn = connection(for: host)
            try await conn.connect()
            // Resolve the *live* session by uuid8 suffix so we kill the real one (which
            // may carry desktop's host slug), not a phone-slug name that doesn't exist.
            if let session = await liveSessionName(instance.id, on: host) {
                _ = try? await conn.tmux(TmuxCommands.killSession(session))
            }
            terminals.disconnect(instance.id)
            layout = try await RemoteLayoutStore.removeInstance(conn, root: project.remoteRoot, instanceId: instance.id)
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Rename an instance (its tab label) in the shared layout. A blank name clears
    /// the custom name, reverting to the program-derived title.
    func rename(_ instance: Instance, to name: String, in project: RemoteProject) async {
        guard let host = host(for: project) else { return }
        do {
            let conn = connection(for: host)
            try await conn.connect()
            if let updated = try await RemoteLayoutStore.renameInstance(
                conn, root: project.remoteRoot, instanceId: instance.id, name: name)
            {
                layout = updated
            }
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// Duplicate an instance: a new pane running the same program/args, with a fresh
    /// id + tmux session (and no inherited worktree binding). Like `launch`, it only
    /// records the instance — the live terminal creates the session attached on open.
    func duplicate(_ instance: Instance, in project: RemoteProject) async {
        guard let host = host(for: project) else { return }
        let newId = UUID().uuidString.lowercased()
        var copy = instance
        copy.id = newId
        copy.tmuxSession = TmuxSession.name(hostName: host.name, instanceId: newId)
        copy.sessionStarted = true
        // A fresh pane shouldn't share the original's worktree/session bindings.
        copy.worktreeId = nil
        copy.worktreePath = nil
        copy.worktreeBranch = nil
        copy.sessionId = nil
        do {
            let conn = connection(for: host)
            try await conn.connect()
            layout = try await RemoteLayoutStore.appendInstance(
                conn, root: project.remoteRoot, instance: copy)
            lastLaunched = newId
            await runPollOnce()
        } catch {
            errorMessage = error.localizedDescription
        }
    }

    /// The live tmux session name for `instanceId` on `host`, matched by uuid8 suffix
    /// (so it finds sessions created by any peer — desktop or phone). nil if none.
    private func liveSessionName(_ instanceId: String, on host: Host) async -> String? {
        let conn = connection(for: host)
        let out = (try? await conn.run(TmuxCommands.commandLine(TmuxCommands.listSessions()))) ?? ""
        return out.split(separator: "\n").map(String.init)
            .first { TmuxSession.session($0, matchesInstanceId: instanceId) }
    }
}
