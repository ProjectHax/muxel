import Foundation

/// Polls every host/project and posts a local notification when an instance newly
/// becomes **blocked** or **done**. Shared by the background task and any
/// foreground refresh. Diffs against a persisted last-status (UserDefaults) so each
/// transition notifies once.
///
/// Note: the done-*latch* (working→idle without a bell) is in-memory and resets
/// between background launches, so background notifications lean on the direct
/// signals — process exit (`pane_dead`), the bell (`window_bell_flag`), and the
/// blocked marker — which classify reports without any latch. This is the
/// best-effort tradeoff of on-device polling.
struct StatusPoller {
    let store: LocalStore
    private let defaults: UserDefaults

    init(store: LocalStore = LocalStore(), defaults: UserDefaults = .standard) {
        self.store = store
        self.defaults = defaults
    }

    private static func lastStatusKey(_ id: String) -> String { "laststatus:\(id)" }
    func lastStatus(_ id: String) -> AgentStatus? {
        defaults.string(forKey: Self.lastStatusKey(id)).flatMap(AgentStatus.init(rawValue:))
    }
    func setLastStatus(_ s: AgentStatus, _ id: String) {
        defaults.set(s.rawValue, forKey: Self.lastStatusKey(id))
    }

    /// Sync statuses already observed in the foreground into the poller's baseline,
    /// so agents that were already `done` or `idle` while the app was open never fire
    /// stale "finished" notifications once the app backgrounds — while agents seen as
    /// `working` in the foreground DO notify if they finish in the background.
    static func recordObservedStatuses(_ results: [InstanceStatus], defaults: UserDefaults = .standard) {
        for r in results where r.running {
            defaults.set(r.status.rawValue, forKey: lastStatusKey(r.instanceId))
        }
    }

    /// Instance ids whose persisted baseline in `defaults` is currently `.working`.
    /// Used to seed fresh `PollService` trackers across background refreshes and cold
    /// relaunches so finishing agents latch `.done` instead of dropping to `.idle`.
    static func persistedWorkingIds(
        for instances: [Instance],
        defaults: UserDefaults = .standard
    ) -> Set<String> {
        var ids = Set<String>()
        for inst in instances where inst.kind == .terminal {
            if defaults.string(forKey: lastStatusKey(inst.id)) == AgentStatus.working.rawValue {
                ids.insert(inst.id)
            }
        }
        return ids
    }

    /// Pure transition filter: whether observing `current` (with tmux pane inactivity
    /// `idle` seconds and process-exited flag `exited`) after `previous` should post a
    /// local notification. Never alerts on first sight (`previous == nil`), and for
    /// "finished" requires either a real observed `.working` → `.done`/`.idle`
    /// transition or a fresh process exit (`exited && idle <= 120`) — so resizing or
    /// attaching to an hours-old idle/finished agent can never trigger a fake alert.
    static func notificationLabel(
        previous: AgentStatus?,
        current: AgentStatus,
        idle: TimeInterval,
        isShell: Bool = false,
        exited: Bool = false
    ) -> String? {
        guard let previous, previous != current else { return nil }
        if current == .blocked {
            return (previous == .working || idle <= 300) ? "needs input" : nil
        }
        if current == .done {
            return (previous == .working || (exited && idle <= 120)) ? "finished" : nil
        }
        if isShell && previous == .working && current == .idle {
            return "finished"
        }
        return nil
    }

    static func shouldNotify(
        previous: AgentStatus?,
        current: AgentStatus,
        idle: TimeInterval,
        isShell: Bool = false,
        exited: Bool = false
    ) -> Bool {
        notificationLabel(previous: previous, current: current, idle: idle,
                          isShell: isShell, exited: exited) != nil
    }

    /// Evaluate status transitions for `results`, post local notifications for any
    /// non-suppressed instance that newly finished or needs input, and persist the new
    /// baseline into `defaults`. Shared by foreground sweeps and background refreshes.
    @discardableResult
    static func processTransitions(
        results: [InstanceStatus],
        instances: [Instance],
        projectId: UUID,
        projectName: String,
        rows: [PaneRow],
        suppressIds: Set<String> = [],
        defaults: UserDefaults = .standard
    ) -> Int {
        let byId = Dictionary(instances.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        var posted = 0
        for s in results where s.running {
            let inst = byId[s.instanceId]
            let prev = defaults.string(forKey: lastStatusKey(s.instanceId)).flatMap(AgentStatus.init(rawValue:))
            if !suppressIds.contains(s.instanceId) {
                let row = inst.flatMap { PollService.resolveRow(for: $0, in: rows) }
                let idle = row?.idle ?? .greatestFiniteMagnitude
                let exited = row?.exited ?? false
                if let label = notificationLabel(previous: prev, current: s.status, idle: idle,
                                                 isShell: inst?.isShell ?? false, exited: exited) {
                    let name = inst?.displayName ?? "agent"
                    NotificationManager.notify(
                        title: "\(name) \(label)",
                        body: projectName,
                        projectId: projectId,
                        instanceId: s.instanceId)
                    posted += 1
                }
            }
            defaults.set(s.status.rawValue, forKey: lastStatusKey(s.instanceId))
        }
        return posted
    }

    /// Run one poll-and-notify pass. `makeConnection` is injectable for tests.
    /// Returns the number of notifications posted.
    @discardableResult
    func run(
        makeConnection: (Host, ConnectionCredentials) -> SSHConnection = {
            CitadelSSHConnection(host: $0, credentials: $1)
        }
    ) async -> Int {
        await runDetailed(makeConnection: makeConnection).posted
    }

    /// Run one poll-and-notify pass, returning both notification count and whether any
    /// agent/shell is still actively `.working`. When `sharedConnections` is provided,
    /// callers (like the ~25s background-transition loop) can reuse one SSH connection
    /// per host across ticks instead of paying a fresh handshake every few seconds.
    func runDetailed(
        sharedConnections: [UUID: SSHConnection]? = nil,
        makeConnection: (Host, ConnectionCredentials) -> SSHConnection = {
            CitadelSSHConnection(host: $0, credentials: $1)
        }
    ) async -> (posted: Int, hasWorking: Bool, connections: [UUID: SSHConnection]) {
        // Tolerant of a damaged store here (nothing to notify about) — load()
        // still preserved the file and recorded the notice the app shows next launch.
        let doc = (try? store.load()) ?? StoreDocument()
        var posted = 0
        var rows: [MuxelActivityAttributes.InstanceRow] = []
        var pool = sharedConnections ?? [:]
        let ownsConnections = sharedConnections == nil
        for host in doc.hosts {
            let projects = doc.projects.filter { $0.hostId == host.id }
            guard !projects.isEmpty else { continue }

            let conn: SSHConnection
            if let existing = pool[host.id] {
                conn = existing
            } else {
                let created = makeConnection(host, host.connectionCredentials(in: doc.identities))
                pool[host.id] = created
                conn = created
            }
            do { try await conn.connect() } catch { continue }
            defer {
                if ownsConnections {
                    Task { await conn.close() }
                }
            }

            let poll = PollService()
            var layouts: [(RemoteProject, RemoteLayout)] = []
            if projects.count > 1,
               let batched = await RemoteLayoutStore.readMany(conn, roots: projects.map(\.remoteRoot)) {
                for project in projects {
                    if let layout = batched[project.remoteRoot] {
                        layouts.append((project, layout))
                    }
                }
            } else {
                for project in projects {
                    guard let layout = try? await RemoteLayoutStore.read(conn, root: project.remoteRoot) else { continue }
                    layouts.append((project, layout))
                }
            }
            // Read all layouts first so one host sweep includes every recorded
            // named socket, rather than polling only the two legacy servers.
            let allHostInstances = layouts.flatMap { $0.1.orderedTerminalInstances }
            // Seed this fresh PollService with any instances previously observed as
            // `.working` so finishing in the background latches `.done` (instead of
            // dropping straight to `.idle`) in both notifications and the Live Activity.
            poll.seedPreviouslyWorking(Self.persistedWorkingIds(for: allHostInstances, defaults: defaults))
            let paneRows = await poll.fetchPaneRows(conn, instances: allHostInstances)
            // Probe any marker-bearing agent that shows recent tmux output (e.g. a
            // prompt just started on desktop) or was previously working/blocked so
            // background refreshes & Live Activity pick up live state immediately.
            var probeTargets: [(id: String, session: String, socket: String?, idle: TimeInterval, bell: Bool)] = []
            var seenProbeIds = Set<String>()
            for inst in allHostInstances where inst.kind == .terminal && !inst.isShell {
                guard seenProbeIds.insert(inst.id).inserted else { continue }
                let (w, b) = defaultMarkers(program: inst.program, args: inst.args)
                guard !(w.isEmpty && b.isEmpty) else { continue }
                guard let row = PollService.resolveRow(for: inst, in: paneRows), !row.exited else { continue }
                let prev = lastStatus(inst.id)
                if prev == .working || prev == .blocked || row.idle <= 20 {
                    probeTargets.append((inst.id, row.session, row.socket ?? inst.tmuxSocket, row.idle, row.bell))
                }
            }
            let probedScreens = probeTargets.isEmpty
                ? [:]
                : await TerminalStore.batchProbeScreensOffscreen(targets: probeTargets, connection: conn)
            for (project, layout) in layouts {
                let instances = layout.orderedTerminalInstances
                let byId = Dictionary(instances.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
                let statuses = poll.classify(rows: paneRows, instances: instances, liveScreens: probedScreens)
                let now = Date().timeIntervalSince1970
                LastWorkedStore.record(statuses, now: now, defaults: defaults)
                posted += Self.processTransitions(
                    results: statuses,
                    instances: instances,
                    projectId: project.id,
                    projectName: project.name,
                    rows: paneRows,
                    defaults: defaults)
                for s in statuses {
                    let inst = byId[s.instanceId]
                    let name = inst?.displayName ?? "agent"
                    let paneIdle = inst.flatMap { PollService.resolveRow(for: $0, in: paneRows)?.idle }
                    let lastActive = LastWorkedStore.effectiveLastActiveAt(
                        instance: inst, status: s.status, running: s.running,
                        paneIdle: paneIdle, now: now, defaults: defaults)
                    rows.append(ActivitySummaryBuilder.row(
                        id: s.instanceId, name: name, project: project.name,
                        status: s.status, running: s.running, lastActiveAt: lastActive))
                }
            }
        }
        // Refresh the Live Activity from the full multi-project scan (updates an
        // existing one, or ends it when nothing is running). Cache it so the next
        // background transition can start the activity instantly.
        let summary = ActivitySummaryBuilder.contentState(rows: rows, now: Date())
        SummaryCache.save(summary)
        await LiveActivityController.apply(summary)
        return (posted, summary.workingCount > 0, pool)
    }
}
