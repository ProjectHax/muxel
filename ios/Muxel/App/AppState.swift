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
    /// Load state of the selected project's shared layout — lets the detail view
    /// distinguish "can't reach the host" from a genuinely empty project.
    @Published var layoutLoad: LayoutLoadState = .idle
    @Published var statuses: [String: AgentStatus] = [:]
    /// Instance ids with a live tmux session right now (from the latest poll of the
    /// selected project).
    @Published var running: Set<String> = []
    /// The current transient notice (banner). Set via `report`; RootView shows and
    /// auto-dismisses it. Failures that need a decision (connection tests) have
    /// their own bespoke paths instead.
    @Published var notice: AppNotice?
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

    enum LayoutLoadState: Equatable {
        case idle
        case loading
        case loaded
        case failed(String)
    }

    /// A changed host key awaiting the user's decision — drives the trust-prompt
    /// sheet (stored vs presented fingerprint). `scope` says whether the target
    /// host's key changed or its jump host's (bastion's).
    struct HostKeyPrompt: Identifiable {
        let id = UUID()
        let host: Host
        let expected: String
        let presented: String
        let scope: HostKeyStore.Scope
    }

    @Published var hostKeyPrompt: HostKeyPrompt?
    /// True when the user has explicitly denied notification permission — the
    /// sidebar shows a quiet Settings pointer (`.notDetermined` must not nag).
    @Published var notificationsDenied = false

    /// Injectable so previews/tests use `MockSSHConnection`. The second argument
    /// carries the host's resolved credentials — target (nil = the host's inline
    /// fields) and bastion — so a connection authenticates with the right logins
    /// + secret owners.
    var connectionFactory: (Host, ConnectionCredentials) -> SSHConnection = {
        CitadelSSHConnection(host: $0, credentials: $1)
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
        self.doc = (try? store.load()) ?? StoreDocument()
        // A failed load preserved the damaged file and recorded a pending notice
        // (a background task may also have recorded one before the app launched) —
        // take it once and tell the user instead of silently starting empty.
        if let corrupt = LocalStore.takeCorruptNotice() {
            report(corrupt, duration: 12)
        }
        // An in-form connection test killed mid-flight can leave a staged scratch
        // secret behind; it's meaningless outside that test, so clear it.
        Keychain.deleteAll(for: Self.scratchSecretOwner)
    }

    /// Surface a transient notice as a self-dismissing banner.
    func report(_ text: String, style: AppNotice.Style = .error, duration: TimeInterval = 4) {
        notice = AppNotice(style: style, text: text, duration: duration)
    }

    /// Central error router for host-scoped failures: a changed host key becomes
    /// the trust-prompt sheet (a decision, not a toast); everything else a
    /// transient banner.
    func surface(_ error: Error, host: Host?) {
        if let host, promptIfHostKeyChanged(error, host: host) { return }
        report(error.localizedDescription)
    }

    /// If `error` is a refused (changed) host key, show the trust prompt for it.
    @discardableResult
    private func promptIfHostKeyChanged(_ error: Error, host: Host) -> Bool {
        guard case let SSHError.hostKeyChanged(expected, got, scope) = error else { return false }
        hostKeyPrompt = HostKeyPrompt(host: host, expected: expected,
                                      presented: got, scope: scope)
        return true
    }

    /// The trust-prompt accept path: pin the presented fingerprint, drop the
    /// host's pooled connection and live terminals (they were refused), and retry
    /// the layout if this host backs the current selection.
    func acceptNewHostKey(_ prompt: HostKeyPrompt) {
        HostKeyStore().setFingerprint(prompt.presented, for: prompt.host.id,
                                      scope: prompt.scope)
        terminals.disconnect(forHost: prompt.host.id)
        connections[prompt.host.id] = nil
        hostKeyPrompt = nil
        if let project = selectedProject, project.hostId == prompt.host.id {
            Task { await refreshLayout() }
        }
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
        let saved = Keychain.saveSecrets(for: host.id, password: password,
                                         keyData: keyData, passphrase: passphrase)
        doc.hosts.append(host)
        persist()
        if !saved {
            report("Couldn't save the credential to the Keychain. The host was added, but "
                + "you may need to re-add it or check the device's Keychain access.",
                duration: 8)
        }
    }

    /// Connect to `host` with a *fresh* connection (re-reading its Keychain secret) to
    /// verify the saved credential authenticates. Surfaces the result via `testResult`.
    func testConnection(_ host: Host) async {
        let result = await attemptConnection(
            host, credentials: host.connectionCredentials(in: doc.identities))
        if !result.ok, promptIfHostKeyChanged(result.underlying ?? SSHError.notConnected,
                                              host: host) { return }
        testResult = ConnectionTest(hostName: host.name, ok: result.ok, message: result.message)
    }

    /// Well-known scratch Keychain owner for in-form connection tests: inline form
    /// secrets are staged under it for the duration of one attempt and deleted
    /// right after (and once at launch, in case a test was killed mid-flight) — so
    /// canceling the form can never leave secrets behind under a phantom host id.
    static let scratchSecretOwner = UUID(uuidString: "00000000-0000-0000-0000-4d5558454c01")!

    /// Test a not-yet-saved host built from the editor's current form state,
    /// without persisting anything. Returns the outcome for inline display in the
    /// form (unlike `testConnection(_:)`, which publishes the blocking alert).
    func testConnection(draft host: Host, identityId: UUID?, password: String?,
                        keyData: Data?, passphrase: String?) async -> ConnectionTest {
        var draft = host
        draft.identityId = identityId
        var credentials = draft.connectionCredentials(in: doc.identities)
        if credentials.target == nil, password != nil || keyData != nil || passphrase != nil {
            Keychain.saveSecrets(for: Self.scratchSecretOwner, password: password,
                                 keyData: keyData, passphrase: passphrase)
            credentials.target = ResolvedCredential(user: draft.user, auth: draft.auth,
                                                    keyHasPassphrase: draft.keyHasPassphrase,
                                                    secretOwner: Self.scratchSecretOwner)
            // A "same as host" bastion credential points at the (unsaved) host's
            // slot — redirect it to the staged scratch secret too.
            if credentials.jump?.secretOwner == draft.id {
                credentials.jump?.secretOwner = Self.scratchSecretOwner
            }
        }
        // With no identity and no new inline secret (edit mode, keeping the stored
        // one) the credential stays nil — the connection reads the host's own slot.
        defer { Keychain.deleteAll(for: Self.scratchSecretOwner) }
        let result = await attemptConnection(draft, credentials: credentials)
        return ConnectionTest(hostName: draft.name, ok: result.ok, message: result.message)
    }

    /// Shared connect-and-verify core for both test paths: a fresh (non-pooled)
    /// connection, one round-trip, then close.
    private func attemptConnection(
        _ host: Host, credentials: ConnectionCredentials
    ) async -> (ok: Bool, message: String, underlying: Error?) {
        let conn = connectionFactory(host, credentials)
        do {
            try await conn.connect()
            _ = try await conn.run("true")
            await conn.close()
            return (true, "Connected and authenticated successfully.", nil)
        } catch {
            await conn.close()
            return (false, error.localizedDescription, error)
        }
    }

    /// Update a host's fields, optionally replacing its stored secrets (nil or
    /// blank = keep what's stored). Drops its pooled connection + live terminals so
    /// the next connect re-authenticates with the new settings.
    func updateHost(_ host: Host, password: String?, keyData: Data?, passphrase: String?) {
        guard let idx = doc.hosts.firstIndex(where: { $0.id == host.id }) else { return }
        doc.hosts[idx] = host
        Keychain.saveSecrets(for: host.id, password: password,
                             keyData: keyData, passphrase: passphrase)
        terminals.disconnect(forHost: host.id)
        connections[host.id] = nil
        persist()
        if let project = selectedProject, project.hostId == host.id {
            Task { await refreshLayout() }
        }
    }

    func deleteHost(_ host: Host) {
        Keychain.deleteAll(for: host.id)
        HostKeyStore().clear(for: host.id)
        terminals.disconnect(forHost: host.id)
        connections[host.id] = nil
        doc.projects.removeAll { $0.hostId == host.id }
        doc.hosts.removeAll { $0.id == host.id }
        persist()
    }

    // MARK: Identity CRUD (shared logins, secrets keyed by identity id)

    func addIdentity(_ identity: Identity, password: String?, keyData: Data?, passphrase: String?) {
        let saved = Keychain.saveSecrets(for: identity.id, password: password,
                                         keyData: keyData, passphrase: passphrase)
        doc.identities.append(identity)
        persist()
        if !saved {
            report("Couldn't save the identity's credential to the Keychain.", duration: 8)
        }
    }

    /// Update an identity's fields, optionally replacing its stored secret. Hosts
    /// referencing it get the new credentials on their next connect.
    func updateIdentity(_ identity: Identity, password: String?, keyData: Data?, passphrase: String?) {
        guard let idx = doc.identities.firstIndex(where: { $0.id == identity.id }) else { return }
        doc.identities[idx] = identity
        Keychain.saveSecrets(for: identity.id, password: password,
                             keyData: keyData, passphrase: passphrase)
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
        do {
            let conn = connection(for: host)
            try await conn.connect()
            let existing = Set(projects(for: host).map(\.remoteRoot))
            return try await ProjectDiscovery.scan(conn).filter { !existing.contains($0.remoteRoot) }
        } catch {
            // A changed key needs the trust prompt (it appears once the scan sheet
            // closes); the sheet still shows the error inline either way.
            promptIfHostKeyChanged(error, host: host)
            throw error
        }
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

    private func persist() {
        do {
            try store.save(doc)
        } catch {
            report(error.localizedDescription, duration: 8)
        }
    }

    // MARK: Connections

    func connection(for host: Host) -> SSHConnection {
        if let c = connections[host.id] { return c }
        let c = connectionFactory(host, host.connectionCredentials(in: doc.identities))
        connections[host.id] = c
        return c
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
        layoutLoad = .loading
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
        layoutLoad = .idle
        statuses = [:]
        running = []
        stopPolling()
    }

    func refreshLayout() async {
        guard let project = selectedProject, let host = host(for: project) else { return }
        // Only show the loading state when nothing is loaded yet — a refresh of
        // already-visible panes shouldn't flicker the terminal away.
        if layout == nil { layoutLoad = .loading }
        do {
            let conn = connection(for: host)
            try await conn.connect()
            layout = try await RemoteLayoutStore.read(conn, root: project.remoteRoot)
            layoutLoad = .loaded
            await runPollOnce()
        } catch {
            if layout == nil {
                layoutLoad = .failed(error.localizedDescription)
                // A changed key additionally needs the decision prompt.
                promptIfHostKeyChanged(error, host: host)
            } else {
                surface(error, host: host)
            }
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

    /// Refresh `notificationsDenied` (called on foreground, so granting permission
    /// in Settings clears the sidebar notice on return).
    func refreshNotificationStatus() async {
        notificationsDenied = await NotificationManager.authorizationStatus() == .denied
    }

    // MARK: Launch a new instance

    func launch(preset: Preset, customCommand: String?, into project: RemoteProject) async {
        guard let host = host(for: project) else { return }

        let program: String?
        let args: [String]
        if let custom = customCommand, !custom.trimmingCharacters(in: .whitespaces).isEmpty {
            // Quote-aware split, so `claude --append-system-prompt "be terse"` works.
            // The launch sheet live-validates, so nil here is a stale-form edge case.
            guard let parts = Shell.splitWords(custom), let first = parts.first else {
                report("Couldn't parse the command — check for an unbalanced quote.")
                return
            }
            program = first
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
                self.surface(error, host: host)
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
            surface(error, host: host)
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
            surface(error, host: host)
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
            surface(error, host: host)
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
