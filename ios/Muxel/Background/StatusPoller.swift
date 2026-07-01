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
    private let defaults = UserDefaults.standard

    init(store: LocalStore = LocalStore()) { self.store = store }

    private func lastStatusKey(_ id: String) -> String { "laststatus:\(id)" }
    func lastStatus(_ id: String) -> AgentStatus? {
        defaults.string(forKey: lastStatusKey(id)).flatMap(AgentStatus.init(rawValue:))
    }
    func setLastStatus(_ s: AgentStatus, _ id: String) {
        defaults.set(s.rawValue, forKey: lastStatusKey(id))
    }

    /// Run one poll-and-notify pass. `makeConnection` is injectable for tests.
    /// Returns the number of notifications posted.
    @discardableResult
    func run(
        makeConnection: (Host, ResolvedCredential?) -> SSHConnection = {
            CitadelSSHConnection(host: $0, credential: $1)
        }
    ) async -> Int {
        let doc = store.load()
        var posted = 0
        var rows: [MuxelActivityAttributes.InstanceRow] = []
        for host in doc.hosts {
            let projects = doc.projects.filter { $0.hostId == host.id }
            guard !projects.isEmpty else { continue }

            let conn = makeConnection(host, host.resolvedCredential(in: doc.identities))
            do { try await conn.connect() } catch { continue }
            defer { Task { await conn.close() } }

            let poll = PollService()
            for project in projects {
                guard let layout = try? await RemoteLayoutStore.read(conn, root: project.remoteRoot) else { continue }
                let statuses = await poll.poll(conn, instances: layout.orderedTerminalInstances)
                for s in statuses {
                    let name = layout.instances.first { $0.id == s.instanceId }?.displayName ?? "agent"
                    if s.running {
                        let prev = lastStatus(s.instanceId)
                        if (s.status == .blocked || s.status == .done), s.status != prev {
                            let label = s.status == .blocked ? "needs input" : "finished"
                            NotificationManager.notify(title: "\(name) \(label)", body: project.name)
                            posted += 1
                        }
                        setLastStatus(s.status, s.instanceId)
                    }
                    rows.append(ActivitySummaryBuilder.row(
                        id: s.instanceId, name: name, project: project.name,
                        status: s.status, running: s.running))
                }
            }
        }
        // Refresh the Live Activity from the full multi-project scan (updates an
        // existing one, or ends it when nothing is running). Cache it so the next
        // background transition can start the activity instantly.
        let summary = ActivitySummaryBuilder.contentState(rows: rows, now: Date())
        SummaryCache.save(summary)
        await LiveActivityController.apply(summary)
        return posted
    }
}
