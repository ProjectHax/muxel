import Foundation
import OSLog
import SwiftUI

/// A migration carries the exact binding observed while its probes ran. Ordinary
/// attachment has no permission to replace a durable owner.
struct TmuxAttachmentPlan<Attachment> {
    let target: TmuxCommands.SessionTarget
    let attachment: Attachment
    var defaultMigration: TmuxCommands.DefaultMigration? = nil
}

/// The app's observable state: the device-local host/project store, the active
/// project's live `RemoteLayout` read from the remote, per-instance status, and one
/// pooled SSH connection per host. Owns the foreground poll loop.
@MainActor
final class AppState: ObservableObject {
    @Published var doc: StoreDocument
    @Published var selectedProject: RemoteProject?
    @Published var layout: RemoteLayout? {
        didSet {
            // A refresh that drops a pane (a peer closed it) drops its live terminal
            // too, rather than leaving a dead entry behind. Same project only —
            // `select`/`deselect` pass through nil, so switching projects never
            // disconnects the terminals the store keeps across navigation.
            guard let oldValue, let layout else { return }
            for id in layout.paneIdsRemoved(since: oldValue, pending: pendingLaunches) {
                terminals.disconnect(id)
                deadPanes.remove(id)
            }
        }
    }
    /// Load state of the selected project's shared layout — lets the detail view
    /// distinguish "can't reach the host" from a genuinely empty project.
    @Published var layoutLoad: LayoutLoadState = .idle
    /// True while a manual or initial `refreshLayout()` round-trip is in flight,
    /// driving the toolbar refresh spinner.
    @Published var isRefreshingLayout = false
    @Published var statuses: [String: AgentStatus] = [:]
    /// Instance ids with a live tmux session right now (from the latest poll of the
    /// selected project).
    @Published var running: Set<String> = []
    /// Per-project activity counts for **non-selected** projects, refreshed by the
    /// cross-project sweep (every ~4th poll tick) over hosts that already have a
    /// pooled connection. Keyed by `RemoteProject.id`; absent = unknown (host not
    /// connected or no sessions) → no badge. The selected project uses live `running`.
    @Published var projectActivity: [UUID: ProjectActivity] = [:]
    /// Instance ids whose live terminal dropped unexpectedly and hasn't reconnected —
    /// each such pane shows a reconnect overlay + backoff retry.
    @Published var deadPanes: Set<String> = []
    /// The current transient notice (banner). Set via `report`; RootView shows and
    /// auto-dismisses it. Failures that need a decision (connection tests) have
    /// their own bespoke paths instead.
    @Published var notice: AppNotice?
    /// Id of the most recently launched instance — the detail view selects it so its
    /// live terminal opens (and creates the session attached).
    @Published var lastLaunched: String?
    /// Instance id currently visible on screen in `ProjectDetailView` (nil on the home
    /// screen). Suppresses redundant notifications for the pane the user is already viewing.
    var activeViewedInstanceId: String?

    /// Outcome of testing a host's saved credential. Returned inline by the in-form
    /// draft test (`testConnection(draft:…)`); the saved-host test reports a banner.
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

    /// One terminal agent inside a project, listed under its project on the home
    /// screen with its status dot so the user can see at a glance what needs attention.
    struct AgentSummary: Equatable, Identifiable {
        let id: String
        var name: String
        var status: AgentStatus
        var running: Bool

        var needsAttention: Bool {
            running && (status == .blocked || status == .done)
        }
    }

    /// Live agent counts + per-agent status summaries for one project (sidebar rows).
    /// `agentStatuses` has one entry per live tmux session; `agents` lists every
    /// terminal agent in the project (running and stopped), sorted by attention
    /// priority (`blocked` → `working` → `done` → running `idle` → stopped).
    struct ProjectActivity: Equatable {
        var running: Int
        var blocked: Int
        var done: Int
        var agentStatuses: [AgentStatus] = []
        var agents: [AgentSummary] = []
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
    /// Host sections currently collapsed in the sidebar; persisted in `UserDefaults`
    /// so relaunching restores the user's expanded/collapsed layout.
    @Published private(set) var collapsedHostIds: Set<UUID> = []
    /// Projects the user has opened, promoted near the top of their host's list.
    /// Newly opened projects join at index 0; reopening an already-opened project
    /// never reshuffles this list. Persisted in `UserDefaults`.
    @Published private(set) var openedProjectIds: [UUID] = []
    /// Projects actively being watched/monitored right now (highlighted folder icon
    /// + faster 5s home-screen polling + prioritized sweeps). Populated from
    /// `autoWatchProjectIds` on launch, manual watch toggles, and active-work scans.
    @Published private(set) var watchedProjectIds: Set<UUID> = []
    /// Projects pinned to automatically start watching whenever the app launches.
    /// Persisted in `UserDefaults`.
    @Published private(set) var autoWatchProjectIds: Set<UUID> = []
    /// When enabled, launching the app automatically scans all projects and starts
    /// watching any whose agents are actively working or need attention.
    @Published private(set) var autoWatchActiveOnLaunch: Bool = false
    /// True while a scan for actively working projects is running.
    @Published private(set) var isScanningActiveProjects = false
    /// Human-readable live progress / outcome of the active-project scan, surfaced
    /// inline inside Settings (which sits above the root banner) and on the home screen.
    @Published private(set) var scanStatusText: String? = nil

    /// Injectable so previews/tests use `MockSSHConnection`. The second argument
    /// carries the host's resolved credentials — target (nil = the host's inline
    /// fields) and bastion — so a connection authenticates with the right logins
    /// + secret owners.
    var connectionFactory: (Host, ConnectionCredentials) -> SSHConnection = {
        CitadelSSHConnection(host: $0, credentials: $1)
    }

    private let store: LocalStore
    private let defaults: UserDefaults
    private static let sidebarLog = Logger(subsystem: "dev.muxel.ios", category: "SidebarPoll")
    private static let collapsedHostsKey = "muxel.collapsedHostIds"
    private static let openedProjectsKey = "muxel.openedProjectIds"
    private static let autoWatchProjectsKey = "muxel.autoWatchProjectIds"
    private static let autoWatchActiveOnLaunchKey = "muxel.autoWatchActiveOnLaunch"
    private var didStartLaunchMonitoring = false
    /// True while launch-time monitoring (`scanAndWatchWorkingProjects` /
    /// `probeWatchedProjects`) is running, preventing `SidebarView.onAppear` or
    /// `scenePhase == .active` from cancelling or racing the launch sweep.
    private var isLaunchMonitoringActive = false
    /// Projects the user explicitly unwatched in this session so ongoing auto-scans
    /// respect their choice instead of immediately re-watching them.
    private var manuallyUnwatchedProjectIds: Set<UUID> = []
    private var connections: [UUID: SSHConnection] = [:]
    /// Live PTY terminals, owned here (not by the SwiftUI views) so a pane stays
    /// connected across navigation until the instance is closed or the app quits.
    let terminals = TerminalStore()
    private let poll = PollService()
    private var pollLoop: Task<Void, Never>?
    /// Gentle foreground poll loop while viewing the home screen (`selectedProject == nil`)
    /// so per-project status dots stay live across all reachable hosts.
    private var sidebarPollLoop: Task<Void, Never>?
    /// Latest Live Activity rows per project from foreground sweeps (so the Lock Screen
    /// widget reflects every project, not only the currently selected one).
    private var summaryRowsByProject: [UUID: [MuxelActivityAttributes.InstanceRow]] = [:]
    /// Serializes the phone's own layout write-backs so two rapid mutations can't
    /// interleave their read-modify-write pairs (the SSH command mutex serializes
    /// individual commands, not whole RMW sequences).
    private var layoutWriteChain: Task<Void, Never>?
    /// Panes launched here whose layout write-back hasn't landed yet. They're shown
    /// before the shared file has them, so a refresh must not read them as closed.
    private var pendingLaunches: Set<String> = []
    /// Foreground poll counter — used to re-read the shared layout only every Nth poll.
    private var pollTick = 0
    /// True once `runPollOnce()` has populated `running`/`statuses` for the currently
    /// selected project (prevents `deselect()` from overwriting cached sidebar dots
    /// with an empty snapshot if the user backs out before the first poll returns).
    private var hasPolledSelectedProject = false
    /// Cache of each non-selected project's terminal instances (`project.id` →
    /// instances + fetch time) so the cross-project sweep rarely re-reads layouts.
    private var instanceCache: [UUID: (instances: [Instance], at: TimeInterval)] = [:]
    /// Cache of each project's full `RemoteLayout` from recent sweeps/opens so
    /// switching into a project renders its tabs and opens its terminal immediately
    /// while a background refresh verifies any peer edits.
    private var layoutCache: [UUID: RemoteLayout] = [:]
    private let instanceCacheTTL: TimeInterval = 15
    /// Latest tmux pane rows per host (plus capture timestamp) so background monitors
    /// reclassify immediately and opening an already-confirmed live agent can attach
    /// without waiting for redundant pre-flight round trips.
    private var lastPaneRowsByHost: [UUID: [PaneRow]] = [:]
    private var lastPaneRowsAtByHost: [UUID: TimeInterval] = [:]
    /// Short-lived cache of screens captured via `batchProbeScreens` so intermediate
    /// reclassifications (e.g. a sibling monitor painting) never clobber a freshly
    /// detected desktop-started `.working` agent back to `.idle`.
    private var probedScreensCache: [String: (screen: LiveScreen, at: TimeInterval)] = [:]
    /// Per-instance last `.idle` probe timestamp + tmux `window_activity` epoch so
    /// quiet idle agents incur zero probes and noisy animating idle TUIs are rate-limited.
    private var idleProbeState: [String: (at: TimeInterval, activityTimestamp: TimeInterval?)] = [:]
    /// Last observed live status per instance, used to keep probing/monitoring agents
    /// that are `.working` or `.blocked` until they finish.
    private var lastObservedStatusByInstance: [String: AgentStatus] = [:]
    /// Throttle for immediate layout cache invalidation when tmux reports an unknown
    /// `muxel_*` session (e.g. a brand-new pane just opened on desktop).
    private var lastUnmatchedRefreshByHost: [UUID: TimeInterval] = [:]
    /// Optional test override for single-PTY screen probing (`terminals.batchProbeScreens`).
    var screenProber: (([(id: String, session: String, socket: String?, idle: TimeInterval, bell: Bool)], SSHConnection) async -> [String: LiveScreen])?

    init(store: LocalStore = LocalStore(), defaults: UserDefaults = .standard) {
        self.store = store
        self.defaults = defaults
        let loaded = (try? store.load()) ?? StoreDocument()
        self.doc = loaded
        let savedCollapsed = Set(
            (defaults.stringArray(forKey: Self.collapsedHostsKey) ?? []).compactMap(UUID.init(uuidString:))
        )
        self.collapsedHostIds = savedCollapsed.intersection(Set(loaded.hosts.map(\.id)))
        let validProjectIds = Set(loaded.projects.map(\.id))
        var seenOpened = Set<UUID>()
        self.openedProjectIds = (defaults.stringArray(forKey: Self.openedProjectsKey) ?? [])
            .compactMap(UUID.init(uuidString:))
            .filter { validProjectIds.contains($0) && seenOpened.insert($0).inserted }
        // Load auto-watch preferences from atomic `store.json` first, migrating from
        // `UserDefaults` if `store.json` predates these fields.
        let legacyAutoWatch = Set(
            (defaults.stringArray(forKey: Self.autoWatchProjectsKey) ?? []).compactMap(UUID.init(uuidString:))
        )
        let savedAutoWatch = loaded.autoWatchProjectIds.isEmpty
            ? legacyAutoWatch
            : Set(loaded.autoWatchProjectIds)
        self.autoWatchProjectIds = savedAutoWatch.intersection(validProjectIds)
        self.watchedProjectIds = self.autoWatchProjectIds
        self.autoWatchActiveOnLaunch = loaded.autoWatchActiveOnLaunch
            || defaults.bool(forKey: Self.autoWatchActiveOnLaunchKey)
        self.doc.autoWatchProjectIds = self.autoWatchProjectIds.sorted { $0.uuidString < $1.uuidString }
        self.doc.autoWatchActiveOnLaunch = self.autoWatchActiveOnLaunch
        // A failed load preserved the damaged file and recorded a pending notice
        // (a background task may also have recorded one before the app launched) —
        // take it once and tell the user instead of silently starting empty.
        if let corrupt = LocalStore.takeCorruptNotice() {
            report(corrupt, duration: 12)
        }
        // An in-form connection test killed mid-flight can leave a staged scratch
        // secret behind; it's meaningless outside that test, so clear it.
        Keychain.deleteAll(for: Self.scratchSecretOwner)
        // A live terminal dropping unexpectedly marks its pane dead so it shows a
        // reconnect overlay (the pane clears it on a successful re-attach).
        terminals.onSessionDied = { [weak self] id in self?.deadPanes.insert(id) }
    }

    /// The pane reconnected (or gave up) — stop signalling it as dead.
    func clearDead(_ instanceId: String) { deadPanes.remove(instanceId) }

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
    /// Projects under `host`, with user-opened projects promoted near the top in
    /// stable order (reopening an already-opened project does not reshuffle them).
    func projects(for host: Host) -> [RemoteProject] {
        Self.orderedProjects(doc.projects.filter { $0.hostId == host.id }, openedIds: openedProjectIds)
    }
    func isProjectOpened(_ projectId: UUID) -> Bool { openedProjectIds.contains(projectId) }
    func status(_ instanceId: String) -> AgentStatus { statuses[instanceId] ?? .idle }
    func isRunning(_ instanceId: String) -> Bool { running.contains(instanceId) }

    /// Pure ordering: opened projects first (matching `openedIds` order), then the
    /// remaining unopened projects in their original order.
    nonisolated static func orderedProjects(_ projects: [RemoteProject], openedIds: [UUID]) -> [RemoteProject] {
        guard !openedIds.isEmpty else { return projects }
        let rank = Dictionary(uniqueKeysWithValues: openedIds.enumerated().map { ($0.element, $0.offset) })
        var opened: [(Int, RemoteProject)] = []
        var rest: [RemoteProject] = []
        for p in projects {
            if let r = rank[p.id] { opened.append((r, p)) } else { rest.append(p) }
        }
        opened.sort { $0.0 < $1.0 }
        return opened.map(\.1) + rest
    }

    /// Record that the user opened `projectId`. Only newly opened projects move to the
    /// top; reopening a project already in `openedProjectIds` leaves the order unchanged.
    func markProjectOpened(_ projectId: UUID) {
        guard !openedProjectIds.contains(projectId) else { return }
        openedProjectIds.insert(projectId, at: 0)
        persistOpenedProjects()
    }

    private func pruneOpenedProjects(removing ids: Set<UUID>) {
        let before = openedProjectIds.count
        openedProjectIds.removeAll { ids.contains($0) }
        if openedProjectIds.count != before { persistOpenedProjects() }
        watchedProjectIds.subtract(ids)
        let beforeAuto = autoWatchProjectIds.count
        autoWatchProjectIds.subtract(ids)
        if autoWatchProjectIds.count != beforeAuto { persistAutoWatchProjects() }
    }

    private func persistOpenedProjects() {
        defaults.set(openedProjectIds.map(\.uuidString), forKey: Self.openedProjectsKey)
    }

    // MARK: Project watching & auto-monitoring

    /// True whenever `projectId` is actively watched or monitored — either explicitly
    /// watched/pinned/auto-detected, or holding at least one live connected terminal
    /// session whose screen is actively monitored. Drives the folder icon color so
    /// every monitored project is styled consistently.
    func isProjectWatched(_ projectId: UUID) -> Bool {
        if watchedProjectIds.contains(projectId) { return true }
        let ids: [String]
        if selectedProject?.id == projectId, let layout {
            ids = layout.orderedTerminalInstances.map(\.id)
        } else if let cached = instanceCache[projectId]?.instances {
            ids = cached.map(\.id)
        } else if let agents = projectActivity[projectId]?.agents {
            ids = agents.map(\.id)
        } else {
            return false
        }
        return terminals.hasLiveSession(for: ids)
    }

    func isProjectAutoWatched(_ projectId: UUID) -> Bool {
        autoWatchProjectIds.contains(projectId)
    }

    func setProjectWatched(_ projectId: UUID, _ watched: Bool) {
        if watched {
            manuallyUnwatchedProjectIds.remove(projectId)
            watchedProjectIds.insert(projectId)
            if selectedProject == nil {
                startSidebarPolling(force: true)
            } else {
                Task { await refreshSidebarStatuses(connectIfNeeded: true) }
            }
        } else {
            manuallyUnwatchedProjectIds.insert(projectId)
            watchedProjectIds.remove(projectId)
            // If the user explicitly stops watching a backgrounded project, also tear
            // down its idle live PTY attachments so active monitoring genuinely stops.
            if selectedProject?.id != projectId {
                let ids = instanceCache[projectId]?.instances.map(\.id)
                    ?? projectActivity[projectId]?.agents.map(\.id)
                    ?? []
                if terminals.hasLiveSession(for: ids) {
                    objectWillChange.send()
                    for id in ids { terminals.disconnect(id) }
                }
            }
        }
    }

    func toggleProjectWatched(_ projectId: UUID) {
        setProjectWatched(projectId, !isProjectWatched(projectId))
    }

    /// Pin or unpin `projectId` to automatically start monitoring on every launch.
    /// Enabling it also starts watching immediately; disabling it stops watching if
    /// the project isn't currently open.
    func setProjectAutoWatched(_ projectId: UUID, _ enabled: Bool) {
        let changed = enabled
            ? autoWatchProjectIds.insert(projectId).inserted
            : (autoWatchProjectIds.remove(projectId) != nil)
        if changed { persistAutoWatchProjects() }
        if enabled {
            setProjectWatched(projectId, true)
        } else if selectedProject?.id != projectId {
            setProjectWatched(projectId, false)
        }
    }

    func toggleProjectAutoWatched(_ projectId: UUID) {
        setProjectAutoWatched(projectId, !autoWatchProjectIds.contains(projectId))
    }

    /// Toggle whether launching the app automatically scans all projects and starts
    /// watching any that are actively working on something. Persists atomically to
    /// `store.json` (plus UserDefaults) and kicks off a live scan immediately when enabled.
    func setAutoWatchActiveOnLaunch(_ enabled: Bool) {
        guard autoWatchActiveOnLaunch != enabled else { return }
        autoWatchActiveOnLaunch = enabled
        doc.autoWatchActiveOnLaunch = enabled
        defaults.set(enabled, forKey: Self.autoWatchActiveOnLaunchKey)
        persist()
        if enabled {
            Task { _ = await scanAndWatchWorkingProjects(announce: false) }
        }
    }

    private func persistAutoWatchProjects() {
        let sorted = autoWatchProjectIds.sorted { $0.uuidString < $1.uuidString }
        doc.autoWatchProjectIds = sorted
        defaults.set(sorted.map(\.uuidString), forKey: Self.autoWatchProjectsKey)
        persist()
    }

    /// Kick off launch-time monitoring once per app run: activate all pinned
    /// auto-watch projects and, if enabled, connect to each project's live agents to
    /// inspect whether they're idle or working and watch active ones.
    @discardableResult
    func startMonitoringOnLaunch() -> Bool {
        guard !didStartLaunchMonitoring else { return false }
        didStartLaunchMonitoring = true
        watchedProjectIds.formUnion(autoWatchProjectIds)
        Self.sidebarLog.info("launchMonitoring autoWatchActive=\(self.autoWatchActiveOnLaunch) pinned=\(self.autoWatchProjectIds.count)")
        if autoWatchActiveOnLaunch {
            isLaunchMonitoringActive = true
            isScanningActiveProjects = true
            scanStatusText = "Scanning active projects…"
            sidebarPollLoop?.cancel()
            sidebarPollLoop = nil
            Task {
                defer { isLaunchMonitoringActive = false }
                _ = await scanAndWatchWorkingProjects(announce: false)
                if selectedProject == nil {
                    startSidebarPolling(force: false, initialDelay: true)
                }
            }
            return true
        }
        if !watchedProjectIds.isEmpty {
            isLaunchMonitoringActive = true
            sidebarPollLoop?.cancel()
            sidebarPollLoop = nil
            Task {
                defer { isLaunchMonitoringActive = false }
                await probeWatchedProjects()
                if selectedProject == nil {
                    startSidebarPolling(force: false, initialDelay: true)
                }
            }
            return selectedProject == nil
        }
        return false
    }

    /// Connect to each pinned/watched project's live agents over a single batched PTY
    /// probe so their real screen status (working / needs input / done / idle) appears
    /// right when the app opens without requiring the user to tap into each project.
    func probeWatchedProjects() async {
        let targets = watchedProjectIds
        guard !targets.isEmpty else { return }
        for host in doc.hosts {
            let allHostProjects = projects(for: host)
            let watchedOnHost = allHostProjects.filter { targets.contains($0.id) }
            guard !watchedOnHost.isEmpty else { continue }
            let conn = connection(for: host)
            guard (try? await conn.connect()) != nil else { continue }
            // Sweep all projects on this host in one round trip (watched projects are
            // prioritized first by `sweepHostProjects`) so the whole home screen populates.
            _ = await sweepHostProjects(host: host, projects: allHostProjects,
                                        connectIfNeeded: true, attachMonitors: false)
            let rows = lastPaneRowsByHost[host.id] ?? []
            var instancesByProject: [UUID: [Instance]] = [:]
            for p in watchedOnHost {
                if let cached = instanceCache[p.id]?.instances { instancesByProject[p.id] = cached }
            }
            var probeTargets: [(id: String, session: String, socket: String?, idle: TimeInterval, bell: Bool)] = []
            for p in watchedOnHost {
                for inst in instancesByProject[p.id] ?? [] where inst.kind == .terminal {
                    guard let row = PollService.resolveRow(for: inst, in: rows),
                          !row.exited, inst.program != nil else { continue }
                    probeTargets.append((inst.id, row.session, row.socket ?? inst.tmuxSocket, row.idle, row.bell))
                }
            }
            let probedScreens = await terminals.batchProbeScreens(targets: probeTargets, connection: conn)
            applyProjectSweep(projects: watchedOnHost, instancesByProject: instancesByProject,
                              rows: rows, extraScreens: probedScreens)
            attachMonitorsForWatchedProjects(on: host, projects: watchedOnHost,
                                             instancesByProject: instancesByProject,
                                             rows: rows, conn: conn)
        }
        syncLiveActivity()
    }

    /// Pure heuristic: whether a project currently has at least one live agent
    /// actively working or needing attention (explicit working/blocked/done status,
    /// or recent tmux pane output within `activeWindowSecs`).
    nonisolated static func isActivelyWorking(
        results: [InstanceStatus],
        rows: [PaneRow],
        instances: [Instance],
        activeWindowSecs: TimeInterval = 120
    ) -> Bool {
        let byId = Dictionary(instances.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        for r in results where r.running {
            if r.status == .working || r.status == .blocked || r.status == .done {
                return true
            }
            if let inst = byId[r.instanceId],
               let row = PollService.resolveRow(for: inst, in: rows),
               !row.exited, row.idle <= activeWindowSecs {
                return true
            }
        }
        return false
    }

    /// Connect to all hosts, read every project's layout + live tmux sessions, and
    /// actually connect read-only (`tmux -u attach-session -r`) to each running agent
    /// to inspect its live screen (working / needs input / done vs idle). Projects
    /// actively working or needing attention stay watched with live monitors; purely
    /// idle unpinned projects release their temporary probe channels cleanly.
    @discardableResult
    func scanAndWatchWorkingProjects(announce: Bool = false) async -> Int {
        guard !doc.projects.isEmpty else {
            let msg = "No projects to scan yet."
            scanStatusText = msg
            if announce { report(msg, duration: 3) }
            return 0
        }
        isScanningActiveProjects = true
        scanStatusText = "Connecting to hosts…"
        defer { isScanningActiveProjects = false }

        var activeIds = Set<UUID>()
        var checkedProjects = 0
        var checkedAgents = 0
        var connectedHosts = 0
        var totalConfiguredAgents = 0
        var totalTmuxRows = 0
        var discoveredSockets = Set<String>()

        for host in doc.hosts {
            let hostProjects = projects(for: host)
            guard !hostProjects.isEmpty else { continue }
            let conn = connection(for: host)
            guard (try? await conn.connect()) != nil else { continue }
            connectedHosts += 1

            // 1. Fetch all layouts + tmux pane rows on this host in one sweep over the
            // single pooled SSH connection (deferring persistent monitor attaches until
            // after we know which projects are actually working).
            let heuristicWorking = await sweepHostProjects(
                host: host,
                projects: hostProjects,
                connectIfNeeded: true,
                detectWorking: true,
                attachMonitors: false)
            let rows = lastPaneRowsByHost[host.id] ?? []
            totalTmuxRows += rows.count
            for r in rows { discoveredSockets.insert(r.socket ?? "default") }

            var instancesByProject: [UUID: [Instance]] = [:]
            for p in hostProjects {
                if selectedProject?.id == p.id, let layout {
                    instancesByProject[p.id] = layout.orderedTerminalInstances
                } else if let cached = instanceCache[p.id]?.instances {
                    instancesByProject[p.id] = cached
                }
                totalConfiguredAgents += (instancesByProject[p.id] ?? []).count
            }

            // 2. Identify every project with at least one live running agent in tmux.
            var projectProbes: [(project: RemoteProject, agents: [(inst: Instance, row: PaneRow)])] = []
            var allProbeTargets: [(id: String, session: String, socket: String?, idle: TimeInterval, bell: Bool)] = []
            for p in hostProjects {
                let liveAgents: [(Instance, PaneRow)] = (instancesByProject[p.id] ?? []).compactMap { inst in
                    guard inst.kind == .terminal,
                          let row = PollService.resolveRow(for: inst, in: rows),
                          !row.exited else { return nil }
                    return (inst, row)
                }
                // Prefer non-shell agent programs for screen probing; include all live panes if needed.
                let preferred = liveAgents.filter { $0.0.program != nil }
                let toProbe = preferred.isEmpty ? liveAgents : preferred
                if !toProbe.isEmpty {
                    projectProbes.append((p, toProbe))
                    for (inst, row) in toProbe {
                        allProbeTargets.append((inst.id, row.session, row.socket ?? inst.tmuxSocket, row.idle, row.bell))
                    }
                }
            }

            // 3. Snapshot EVERY live agent across ALL projects on this host over ONE
            // single SSH PTY channel (~150ms per agent, zero extra connections, zero
            // capture-pane), then classify all projects at once and only attach
            // persistent monitors to projects that are actually active or pinned.
            if !allProbeTargets.isEmpty {
                scanStatusText = "Checking \(allProbeTargets.count) live \(allProbeTargets.count == 1 ? "agent" : "agents") on \(host.name)…"
            }
            let probedScreens = await terminals.batchProbeScreens(targets: allProbeTargets, connection: conn)
            checkedProjects += projectProbes.count
            checkedAgents += allProbeTargets.count

            var liveProbedActive = Set<UUID>()
            for entry in projectProbes {
                let project = entry.project
                let allInsts = instancesByProject[project.id] ?? []
                var screens = liveScreens(for: allInsts)
                var gotScreen = false
                for inst in allInsts {
                    if let s = probedScreens[inst.id] {
                        screens[inst.id] = s
                        gotScreen = true
                    }
                }
                let results = poll.classify(rows: rows, instances: allInsts, liveScreens: screens)
                let now = Date().timeIntervalSince1970
                recordObservedProbeOutcomes(results: results, probedIds: Set(probedScreens.keys), rows: rows, instances: allInsts, now: now)
                LastWorkedStore.record(results, now: now, defaults: defaults)
                StatusPoller.recordObservedStatuses(results, defaults: defaults)
                projectActivity[project.id] = Self.aggregate(results, instances: allInsts)
                let byId = Dictionary(allInsts.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
                summaryRowsByProject[project.id] = results.compactMap { r in
                    guard r.running else { return nil }
                    let inst = byId[r.instanceId]
                    let name = inst?.displayName ?? "agent"
                    let paneIdle = inst.flatMap { PollService.resolveRow(for: $0, in: rows)?.idle }
                    let lastActive = LastWorkedStore.effectiveLastActiveAt(
                        instance: inst, status: r.status, running: r.running,
                        paneIdle: paneIdle, now: now, defaults: defaults)
                    return ActivitySummaryBuilder.row(
                        id: r.instanceId, name: name, project: project.name,
                        status: r.status, running: r.running, lastActiveAt: lastActive)
                }

                let screenSaysWorking = results.contains {
                    $0.running && ($0.status == .working || $0.status == .blocked || $0.status == .done)
                }
                // Trust real live screens when captured; fall back to tmux activity
                // when no PTY is available (e.g. unit-test mocks).
                let isWorking = gotScreen ? screenSaysWorking : heuristicWorking.contains(project.id)
                if isWorking {
                    liveProbedActive.insert(project.id)
                    manuallyUnwatchedProjectIds.remove(project.id)
                    watchedProjectIds.insert(project.id)
                }
            }

            let watchedHere = hostProjects.filter { watchedProjectIds.contains($0.id) }
            if !watchedHere.isEmpty {
                attachMonitorsForWatchedProjects(
                    on: host, projects: watchedHere, instancesByProject: instancesByProject,
                    rows: rows, conn: conn)
            }

            activeIds.formUnion(liveProbedActive)
        }

        if !activeIds.isEmpty, selectedProject == nil {
            startSidebarPolling(force: false, initialDelay: true)
        }
        syncLiveActivity()

        let summary: String
        if checkedAgents == 0 {
            if connectedHosts == 0 {
                summary = "Couldn't connect to host."
            } else if totalConfiguredAgents == 0 {
                summary = "No agents configured in projects."
            } else if totalTmuxRows == 0 {
                let errSuffix = poll.lastError.map { " — \($0)" } ?? ""
                summary = "0 tmux sessions found (\(totalConfiguredAgents) agents configured)\(errSuffix)."
            } else {
                let socks = discoveredSockets.sorted().joined(separator: ", ")
                summary = "Found \(totalTmuxRows) tmux sessions (\(socks)), 0 matched \(totalConfiguredAgents) agents."
            }
        } else if activeIds.isEmpty {
            summary = "Checked \(checkedAgents) \(checkedAgents == 1 ? "agent" : "agents") across \(checkedProjects) \(checkedProjects == 1 ? "project" : "projects") — all idle."
        } else {
            let n = activeIds.count
            summary = "Watching \(n) active \(n == 1 ? "project" : "projects") (\(checkedAgents) \(checkedAgents == 1 ? "agent" : "agents") checked)."
        }
        scanStatusText = summary
        if announce {
            report(summary, style: activeIds.isEmpty ? .error : .success, duration: 4)
        }
        return activeIds.count
    }

    /// Live running-instance count for `project`. The selected project uses the live
    /// `running` set; others use the latest cross-project sweep (`nil` = unknown, e.g.
    /// the host isn't connected yet) so no stale badge is shown.
    func runningCount(for project: RemoteProject) -> Int? {
        if selectedProject?.id == project.id { return running.count }
        return projectActivity[project.id]?.running
    }

    /// Full activity for `project` (running / blocked / done + per-agent summaries
    /// and dots) for the sidebar. The selected project is derived live; others come
    /// from the latest snapshot/sweep. `nil` when unknown so the row shows no badge.
    func activity(for project: RemoteProject) -> ProjectActivity? {
        if selectedProject?.id == project.id {
            return liveSelectedActivity() ?? projectActivity[project.id]
        }
        return projectActivity[project.id]
    }

    private func liveSelectedActivity() -> ProjectActivity? {
        guard let layout else { return nil }
        let instances = layout.orderedTerminalInstances
        if hasPolledSelectedProject {
            let results = instances.map { inst in
                InstanceStatus(instanceId: inst.id,
                               status: statuses[inst.id] ?? .idle,
                               running: running.contains(inst.id))
            }
            return Self.aggregate(results, instances: instances)
        }
        // Before the first poll returns after selecting, preserve any previously polled
        // statuses for matching instances rather than flickering everything to stopped.
        let prior = selectedProject
            .flatMap { projectActivity[$0.id]?.agents }
            .map { Dictionary($0.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a }) } ?? [:]
        let results = instances.map { inst in
            if let prev = prior[inst.id] {
                return InstanceStatus(instanceId: inst.id, status: prev.status, running: prev.running)
            }
            return InstanceStatus(instanceId: inst.id,
                                  status: inst.activity?.currentState ?? .idle,
                                  running: false)
        }
        return Self.aggregate(results, instances: instances)
    }

    /// Attention-priority sort for per-project status dots: blocked (needs input) →
    /// working → done → idle, keeping the row stable across polls.
    nonisolated static func sortAgentStatuses(_ list: [AgentStatus]) -> [AgentStatus] {
        func rank(_ s: AgentStatus) -> Int {
            switch s {
            case .blocked: return 0
            case .working: return 1
            case .done: return 2
            case .idle: return 3
            }
        }
        return list.sorted { rank($0) < rank($1) }
    }

    /// Attention-priority sort for per-project agent rows: running `blocked` →
    /// running `working` → running `done` → running `idle` → stopped, preserving
    /// tab order within the same priority tier.
    nonisolated static func sortAgents(_ list: [AgentSummary]) -> [AgentSummary] {
        func rank(_ a: AgentSummary) -> Int {
            guard a.running else { return 4 }
            switch a.status {
            case .blocked: return 0
            case .working: return 1
            case .done: return 2
            case .idle: return 3
            }
        }
        return list.enumerated()
            .sorted { lhs, rhs in
                let rl = rank(lhs.element), rr = rank(rhs.element)
                return rl != rr ? rl < rr : lhs.offset < rhs.offset
            }
            .map(\.element)
    }

    func isHostCollapsed(_ hostId: UUID) -> Bool {
        collapsedHostIds.contains(hostId)
    }

    func toggleHostCollapsed(_ hostId: UUID) {
        setHostCollapsed(hostId, !collapsedHostIds.contains(hostId))
    }

    func setHostCollapsed(_ hostId: UUID, _ collapsed: Bool) {
        let changed = collapsed
            ? collapsedHostIds.insert(hostId).inserted
            : (collapsedHostIds.remove(hostId) != nil)
        guard changed else { return }
        persistCollapsedHosts()
    }

    private func persistCollapsedHosts() {
        let sorted = collapsedHostIds.map(\.uuidString).sorted()
        defaults.set(sorted, forKey: Self.collapsedHostsKey)
    }

    /// Rolled-up live activity counts + sorted per-agent statuses across every project
    /// under `host` — shown on a collapsed host header so agent dots stay visible.
    func hostActivity(for host: Host) -> ProjectActivity? {
        var total = ProjectActivity(running: 0, blocked: 0, done: 0)
        var statuses: [AgentStatus] = []
        var agents: [AgentSummary] = []
        var anyKnown = false
        for project in projects(for: host) {
            if let a = activity(for: project) {
                anyKnown = true
                total.running += a.running
                total.blocked += a.blocked
                total.done += a.done
                statuses.append(contentsOf: a.agentStatuses)
                agents.append(contentsOf: a.agents)
            }
        }
        total.agentStatuses = Self.sortAgentStatuses(statuses)
        total.agents = Self.sortAgents(agents)
        return anyKnown ? total : nil
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
        Task { await requestNotificationsIfNeeded() }
    }

    /// Connect to `host` with a *fresh* connection (re-reading its Keychain secret) to
    /// verify the saved credential authenticates. Surfaces the result as a transient
    /// banner (a changed key still routes to the trust prompt).
    func testConnection(_ host: Host) async {
        let result = await attemptConnection(
            host, credentials: host.connectionCredentials(in: doc.identities))
        if !result.ok, promptIfHostKeyChanged(result.underlying ?? SSHError.notConnected,
                                              host: host) { return }
        if result.ok {
            report("\(host.name): connected and authenticated.", style: .success, duration: 5)
        } else {
            report("\(host.name): \(result.message)", duration: 8)
        }
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
        let removedProjects = Set(doc.projects.filter { $0.hostId == host.id }.map(\.id))
        doc.projects.removeAll { $0.hostId == host.id }
        doc.hosts.removeAll { $0.id == host.id }
        pruneOpenedProjects(removing: removedProjects)
        setHostCollapsed(host.id, false)
        if let sel = selectedProject, sel.hostId == host.id {
            deselect()
        }
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
        setHostCollapsed(project.hostId, false)
        persist()
        // Onboarding: land on the project you just added (its empty layout shows the
        // "Launch one" CTA, completing the add-host → add-project → launch chain).
        select(project)
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
        var added: [RemoteProject] = []
        for item in found where !existing.contains(item.remoteRoot) {
            let project = RemoteProject(name: item.name, hostId: host.id, remoteRoot: item.remoteRoot)
            doc.projects.append(project)
            added.append(project)
        }
        if !added.isEmpty { setHostCollapsed(host.id, false) }
        persist()
        // Importing exactly one project opens it; multiple stay in the sidebar (which
        // one to open is ambiguous, and the fresh badges are the payoff there).
        if added.count == 1 {
            select(added[0])
        } else if !added.isEmpty, selectedProject == nil {
            startSidebarPolling()
        }
    }

    func deleteProject(_ project: RemoteProject) {
        doc.projects.removeAll { $0.id == project.id }
        pruneOpenedProjects(removing: [project.id])
        projectActivity[project.id] = nil
        layoutCache[project.id] = nil
        instanceCache[project.id] = nil
        if selectedProject?.id == project.id { deselect() }
        else if UserDefaults.standard.string(forKey: Self.lastSelectedProjectKey) == project.id.uuidString {
            UserDefaults.standard.removeObject(forKey: Self.lastSelectedProjectKey)
        }
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

    /// The pooled connection for a host **if one already exists** — never dials. The
    /// cross-project sweep uses this so an unreachable host can't add a connect-timeout
    /// stall to every tick; projects on un-connected hosts just keep no badge.
    func existingConnection(for hostId: UUID) -> SSHConnection? {
        connections[hostId]
    }

    /// Instant, no-network snapshot of every instance: the last cached full summary,
    /// overlaid with fresh live data for the selected project (the only one the
    /// foreground tracks). `nil` only when there are no instances anywhere. The
    /// background `StatusPoller` refines it with a full multi-project scan.
    func currentSummarySnapshot() -> MuxelActivityAttributes.ContentState? {
        var rows: [MuxelActivityAttributes.InstanceRow] = []
        var seenIds = Set<String>()
        if let sel = selectedProject, let layout {
            let hostRows = lastPaneRowsByHost[sel.hostId] ?? []
            let now = Date().timeIntervalSince1970
            for inst in layout.orderedTerminalInstances {
                seenIds.insert(inst.id)
                let st = statuses[inst.id] ?? .idle
                let isRun = running.contains(inst.id)
                let paneIdle = PollService.resolveRow(for: inst, in: hostRows)?.idle
                let lastActive = LastWorkedStore.effectiveLastActiveAt(
                    instance: inst, status: st, running: isRun,
                    paneIdle: paneIdle, now: now, defaults: defaults)
                rows.append(ActivitySummaryBuilder.row(
                    id: inst.id, name: inst.displayName, project: sel.name,
                    status: st, running: isRun, lastActiveAt: lastActive))
            }
        }
        // Overlay fresh rows from foreground sidebar/sibling sweeps across all projects.
        for (projectId, projectRows) in summaryRowsByProject where projectId != selectedProject?.id {
            for r in projectRows where seenIds.insert(r.id).inserted {
                rows.append(r)
            }
        }
        // Fall back to the last cached background scan for any project not yet swept.
        for r in SummaryCache.load()?.instances ?? [] where seenIds.insert(r.id).inserted {
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
        if !state.isEmpty { SummaryCache.save(state) }
        Task { await LiveActivityController.apply(state) }
    }

    // MARK: Selecting a project (connect + read layout + poll)

    private static let lastSelectedProjectKey = "muxel.lastSelectedProjectId"
    private var didRestoreLastProject = false

    /// Restore the project the user last had open (called once from the UI on launch,
    /// so unit tests constructing `AppState` don't auto-dial).
    func restoreLastProjectIfNeeded(defaults: UserDefaults = .standard) {
        guard !didRestoreLastProject else { return }
        didRestoreLastProject = true
        guard selectedProject == nil,
              let raw = defaults.string(forKey: Self.lastSelectedProjectKey),
              let id = UUID(uuidString: raw),
              let project = doc.projects.first(where: { $0.id == id })
        else { return }
        select(project)
    }

    /// Deep-link from a tapped notification: jump straight to the notified project and
    /// focus the exact agent tab, clearing its done latch.
    func openNotificationRoute(_ route: NotificationRoute) {
        guard let project = doc.projects.first(where: { $0.id == route.projectId }) else { return }
        if isHostCollapsed(project.hostId) {
            toggleHostCollapsed(project.hostId)
        }
        attend(route.instanceId)
        select(project, focusingInstance: route.instanceId)
    }

    func select(_ project: RemoteProject, focusingInstance: String? = nil) {
        if let prev = selectedProject, prev.id != project.id,
           let snapshot = liveSelectedActivity() {
            projectActivity[prev.id] = snapshot
        }
        markProjectOpened(project.id)
        if let focusingInstance {
            lastLaunched = focusingInstance
        }
        if selectedProject?.id == project.id {
            if let focusingInstance,
               layout?.orderedPaneInstances.contains(where: { $0.id == focusingInstance }) != true {
                Task { await refreshLayout() }
            }
            return
        }
        selectedProject = project
        UserDefaults.standard.set(project.id.uuidString, forKey: Self.lastSelectedProjectKey)
        if let cached = layoutCache[project.id] {
            layout = cached
            layoutLoad = .loaded
            if let prior = projectActivity[project.id] {
                var nextStatuses: [String: AgentStatus] = [:]
                var nextRunning = Set<String>()
                for agent in prior.agents {
                    nextStatuses[agent.id] = agent.status
                    if agent.running { nextRunning.insert(agent.id) }
                }
                statuses = nextStatuses
                running = nextRunning
            } else {
                statuses = [:]
                running = []
            }
        } else {
            layout = nil
            layoutLoad = .loading
            statuses = [:]
            running = []
        }
        pollTick = 0
        hasPolledSelectedProject = false
        sidebarPollLoop?.cancel()
        sidebarPollLoop = nil
        Task { await refreshLayout() }
        startPolling()
    }

    /// Returns a confirmed live `(session, socket)` target for `instance` from a
    /// recent host sweep (within 20s) when the instance already has a matching durable
    /// binding and no pending local layout write — allowing `TerminalPaneView` to
    /// open the PTY immediately without waiting for extra pre-flight SSH round trips.
    func fastAttachTarget(for instance: Instance, in project: RemoteProject, hostId: UUID) -> TmuxCommands.SessionTarget? {
        guard !instance.literalArgv,
              !pendingLaunches.contains(instance.id),
              layoutWriteChain == nil,
              let recordedName = instance.tmuxSession, !recordedName.isEmpty,
              let recordedSocket = instance.tmuxSocket,
              let fetchedAt = lastPaneRowsAtByHost[hostId],
              Date().timeIntervalSince1970 - fetchedAt <= 20,
              let rows = lastPaneRowsByHost[hostId],
              let row = TmuxCommands.resolveTarget(instance, in: rows,
                                                   name: { $0.session }, socket: { $0.socket }),
              !row.exited,
              row.session == recordedName,
              (row.socket ?? recordedSocket) == recordedSocket
        else { return nil }
        return TmuxCommands.SessionTarget(name: recordedName, socket: recordedSocket)
    }

    /// Leave the active project (the detail was popped on iPhone). Clearing the
    /// selection is what lets the *same* project be re-selected: the sidebar binds
    /// navigation to `selectedProject?.id`, so if it stays set, the row reads as
    /// already-selected and tapping it again does nothing. Switches to gentle
    /// home-screen polling so per-project status dots stay fresh.
    func deselect() {
        if let prev = selectedProject, let snapshot = liveSelectedActivity() {
            projectActivity[prev.id] = snapshot
        }
        selectedProject = nil
        activeViewedInstanceId = nil
        UserDefaults.standard.removeObject(forKey: Self.lastSelectedProjectKey)
        layout = nil
        layoutLoad = .idle
        statuses = [:]
        running = []
        hasPolledSelectedProject = false
        pollLoop?.cancel()
        pollLoop = nil
        startSidebarPolling(force: true)
    }

    func refreshLayout() async {
        guard let project = selectedProject, let host = host(for: project) else { return }
        isRefreshingLayout = true
        defer { isRefreshingLayout = false }
        // Only show the loading state when nothing is loaded yet — a refresh of
        // already-visible panes shouldn't flicker the terminal away.
        if layout == nil { layoutLoad = .loading }
        do {
            let conn = connection(for: host)
            try await conn.connect()
            let fresh = try await RemoteLayoutStore.read(conn, root: project.remoteRoot)
            guard selectedProject?.id == project.id else { return }
            layout = fresh
            layoutLoad = .loaded
            if let fresh {
                layoutCache[project.id] = fresh
                instanceCache[project.id] = (fresh.orderedTerminalInstances, Date().timeIntervalSince1970)
                if let snapshot = liveSelectedActivity() {
                    projectActivity[project.id] = snapshot
                }
            }
            await runPollOnce()
        } catch {
            guard selectedProject?.id == project.id else { return }
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

    func stopPolling() {
        pollLoop?.cancel(); pollLoop = nil
        sidebarPollLoop?.cancel(); sidebarPollLoop = nil
    }

    /// Ensure the home-screen poll loop is active whenever the sidebar appears with
    /// projects and no project currently selected. Skips while launch monitoring or an
    /// active-project scan is already sweeping hosts so cold-launch sweeps never race.
    func ensureSidebarPolling() {
        guard selectedProject == nil,
              sidebarPollLoop == nil,
              !isScanningActiveProjects,
              !isLaunchMonitoringActive,
              !doc.projects.isEmpty else { return }
        startSidebarPolling(force: false)
    }

    /// Foreground home-screen poll loop while no project is open: polls every 5s when
    /// any project is actively watched, or gently every 12s otherwise. When `force` is
    /// false and a loop is already active, leaves the in-flight sweep running so
    /// redundant lifecycle callbacks (`scenePhase == .active`) never cancel an SSH read.
    private func startSidebarPolling(force: Bool = false, initialDelay: Bool = false) {
        if !force, sidebarPollLoop != nil { return }
        sidebarPollLoop?.cancel()
        guard selectedProject == nil, !doc.projects.isEmpty else {
            sidebarPollLoop = nil
            return
        }
        sidebarPollLoop = Task { [weak self] in
            if !initialDelay {
                await self?.refreshSidebarStatuses(connectIfNeeded: true)
            }
            while !Task.isCancelled {
                let intervalNs: UInt64 = (self?.watchedProjectIds.isEmpty ?? true)
                    ? 12_000_000_000
                    : 5_000_000_000
                try? await Task.sleep(nanoseconds: intervalNs)
                guard !Task.isCancelled else { break }
                await self?.refreshSidebarStatuses(connectIfNeeded: true)
            }
        }
    }

    /// Foreground return: restart the active project poll loop, or kick off a
    /// home-screen status sweep across reachable hosts when no project is open.
    func resumeForeground() {
        if selectedProject != nil {
            startPolling()
            Task { await refreshLayout() }
        } else if !isScanningActiveProjects, !isLaunchMonitoringActive {
            startSidebarPolling(force: false)
        }
    }

    /// Pull-to-refresh from the sidebar: refresh notification permission, re-read the
    /// selected layout, and force an immediate cross-project status sweep across all
    /// hosts (connecting if needed so agents and status dots populate right away).
    func refreshAll() async {
        instanceCache.removeAll()
        await refreshNotificationStatus()
        if selectedProject != nil { await refreshLayout() }
        await refreshSidebarStatuses(connectIfNeeded: true)
    }

    /// Sweep every host with projects in parallel (connecting silently if needed) so
    /// the home screen displays per-project agents + status dots for all reachable hosts.
    /// Projects are swept in watched/opened-first order in small chunks so the top of
    /// each host populates immediately even when dozens of projects exist. When
    /// `autoWatchActiveOnLaunch` is enabled, newly-working projects are also watched.
    func refreshSidebarStatuses(connectIfNeeded: Bool) async {
        let selectedProjectId = selectedProject?.id
        let hostsWithProjects = doc.hosts.compactMap { host -> (Host, [RemoteProject])? in
            let list = projects(for: host).filter { $0.id != selectedProjectId }
            return list.isEmpty ? nil : (host, list)
        }
        guard !hostsWithProjects.isEmpty else { return }
        let autoDetect = autoWatchActiveOnLaunch
        var newlyWorking = Set<UUID>()
        await withTaskGroup(of: Set<UUID>.self) { group in
            for (host, projects) in hostsWithProjects {
                group.addTask { @MainActor [weak self] in
                    guard let self else { return [] }
                    return await self.sweepHostProjects(
                        host: host,
                        projects: projects,
                        connectIfNeeded: connectIfNeeded,
                        detectWorking: autoDetect)
                }
            }
            for await found in group {
                newlyWorking.formUnion(found)
            }
        }
        if autoDetect {
            let eligible = newlyWorking.subtracting(manuallyUnwatchedProjectIds)
            if !eligible.isEmpty {
                watchedProjectIds.formUnion(eligible)
            }
        }
        syncLiveActivity()
    }

    /// Incrementally read layouts + poll tmux status for `projects` on `host`.
    /// Watched and cached projects update first; uncached projects update progressively
    /// in small chunks. When `detectWorking` is true, returns the ids of projects with
    /// an agent currently working or needing attention.
    @discardableResult
    private func sweepHostProjects(
        host: Host,
        projects: [RemoteProject],
        connectIfNeeded: Bool,
        preFetchedRows: [PaneRow]? = nil,
        detectWorking: Bool = false,
        attachMonitors: Bool = true
    ) async -> Set<UUID> {
        let conn: SSHConnection
        if let existing = existingConnection(for: host.id) {
            conn = existing
        } else if connectIfNeeded {
            conn = connection(for: host)
        } else {
            return []
        }
        if connectIfNeeded {
            // Silently skip unreachable hosts so one offline box never
            // blocks dots on other hosts or spams error banners.
            guard (try? await conn.connect()) != nil else { return [] }
        }

        // Prioritize watched projects ahead of non-watched ones so monitored projects
        // refresh first on hosts with many repos.
        let ordered = projects.filter { watchedProjectIds.contains($0.id) }
            + projects.filter { !watchedProjectIds.contains($0.id) }

        let now = Date().timeIntervalSince1970
        var cachedProjects: [RemoteProject] = []
        var cachedInstancesByProject: [UUID: [Instance]] = [:]
        var stale: [RemoteProject] = []
        for project in ordered {
            if selectedProject?.id == project.id, let layout {
                cachedProjects.append(project)
                cachedInstancesByProject[project.id] = layout.orderedTerminalInstances
            } else if let hit = instanceCache[project.id], now - hit.at < instanceCacheTTL {
                cachedProjects.append(project)
                cachedInstancesByProject[project.id] = hit.instances
            } else {
                stale.append(project)
            }
        }

        let chunkSize = 10
        var totalLayouts = cachedInstancesByProject.values.filter { !$0.isEmpty }.count
        var rows: [PaneRow]? = preFetchedRows
        var queriedSockets = Set([TmuxCommands.socketName, "default"]
            + cachedInstancesByProject.values.flatMap { $0 }.compactMap(\.tmuxSocket))
        var knownInstances: [Instance] = cachedInstancesByProject.values.flatMap { $0 }
        var workingProjectIds = Set<UUID>()

        // If we already have cached projects, refresh them immediately in one round trip.
        if !cachedProjects.isEmpty {
            if rows == nil {
                rows = await poll.fetchPaneRows(conn, instances: knownInstances)
            }
            // If tmux reports a live `muxel_*` session not matched by any cached
            // instance (e.g. desktop just opened a new pane/prompt), re-read cached
            // layouts immediately instead of waiting for `instanceCacheTTL`.
            if let currentRows = rows,
               Self.hasUnmatchedLiveMuxelSession(rows: currentRows, knownInstances: knownInstances),
               now - (lastUnmatchedRefreshByHost[host.id] ?? 0) > 5 {
                lastUnmatchedRefreshByHost[host.id] = now
                let refreshable = cachedProjects.filter { $0.id != selectedProject?.id }
                for p in refreshable { instanceCache.removeValue(forKey: p.id) }
                stale.insert(contentsOf: refreshable, at: 0)
            }
            let cachedProbed = attachMonitors
                ? await probeActiveUnattachedAgents(
                    instances: cachedInstancesByProject.values.flatMap { $0 },
                    rows: rows ?? [],
                    conn: conn)
                : [:]
            let found = applyProjectSweep(projects: cachedProjects,
                                          instancesByProject: cachedInstancesByProject,
                                          rows: rows ?? [],
                                          detectWorking: detectWorking,
                                          extraScreens: cachedProbed,
                                          probedIds: Set(cachedProbed.keys))
            workingProjectIds.formUnion(found)
        }

        // Read uncached layouts in small chunks (watched/opened projects first) BEFORE
        // fetching pane rows if we haven't fetched yet, so custom recorded sockets on
        // this host are included in the very first `list-panes` query.
        var offset = 0
        while offset < stale.count {
            guard !Task.isCancelled else { break }
            let end = min(offset + chunkSize, stale.count)
            let chunk = Array(stale[offset..<end])
            offset = end

            let chunkInstances = await readProjectChunk(chunk, conn: conn, now: Date().timeIntervalSince1970)
            let chunkFlat = chunkInstances.values.flatMap { $0 }
            knownInstances.append(contentsOf: chunkFlat)
            totalLayouts += chunkInstances.values.filter { !$0.isEmpty }.count

            let chunkSockets = Set(chunkFlat.compactMap(\.tmuxSocket))
            if rows == nil || (rows?.isEmpty == true && !chunkFlat.isEmpty) || !chunkSockets.subtracting(queriedSockets).isEmpty {
                queriedSockets.formUnion(chunkSockets)
                rows = await poll.fetchPaneRows(conn, instances: knownInstances)
            }
            let chunkProbed = attachMonitors
                ? await probeActiveUnattachedAgents(instances: chunkFlat, rows: rows ?? [], conn: conn)
                : [:]
            let found = applyProjectSweep(projects: chunk,
                                          instancesByProject: chunkInstances,
                                          rows: rows ?? [],
                                          detectWorking: detectWorking,
                                          extraScreens: chunkProbed,
                                          probedIds: Set(chunkProbed.keys))
            workingProjectIds.formUnion(found)
        }

        let finalRows = rows ?? []
        lastPaneRowsByHost[host.id] = finalRows
        lastPaneRowsAtByHost[host.id] = Date().timeIntervalSince1970
        var allInstancesByProject = cachedInstancesByProject
        for p in ordered {
            if let cached = instanceCache[p.id]?.instances {
                allInstancesByProject[p.id] = cached
            }
        }
        let watchedHere = ordered.filter { watchedProjectIds.contains($0.id) || workingProjectIds.contains($0.id) }
        if attachMonitors, !watchedHere.isEmpty {
            attachMonitorsForWatchedProjects(
                on: host, projects: watchedHere, instancesByProject: allInstancesByProject,
                rows: finalRows, conn: conn)
        }

        let activeCount = ordered.filter { (projectActivity[$0.id]?.running ?? 0) > 0 }.count
        let withAgents = ordered.filter { !(projectActivity[$0.id]?.agents.isEmpty ?? true) }.count
        Self.sidebarLog.info("host=\(host.name, privacy: .public) projects=\(ordered.count) layouts=\(totalLayouts) tmuxRows=\(finalRows.count) withAgents=\(withAgents) activeProjects=\(activeCount)")
        return workingProjectIds
    }

    /// Attach lightweight read-only (`tmux attach-session -r`, i.e. `read-only,ignore-size`)
    /// background monitors for live running agents inside watched projects so the home
    /// screen shows real screen-marker `working` / `needs input` / `done` status without
    /// opening the project and without resizing desktop's tmux window.
    private func attachMonitorsForWatchedProjects(
        on host: Host,
        projects: [RemoteProject],
        instancesByProject: [UUID: [Instance]],
        rows: [PaneRow],
        conn: SSHConnection
    ) {
        guard !rows.isEmpty else { return }
        // Prioritize actively working / blocked / recently-active marker-bearing agents
        // ahead of long-idle agents so monitor slots always go where work is happening.
        var candidates: [(activeRank: Int, markerRank: Int, inst: Instance, row: PaneRow)] = []
        for project in projects where project.id != selectedProject?.id {
            guard let instances = instancesByProject[project.id] else { continue }
            for inst in instances where inst.kind == .terminal {
                guard let row = PollService.resolveRow(for: inst, in: rows),
                      !row.exited else { continue }
                let (w, b) = defaultMarkers(program: inst.program, args: inst.args)
                let markerRank = !(w.isEmpty && b.isEmpty) ? 0 : (inst.program != nil ? 1 : 2)
                let observed = lastObservedStatusByInstance[inst.id]
                let activeRank: Int
                if observed == .working || observed == .blocked {
                    activeRank = 0
                } else if row.idle <= 20 {
                    activeRank = 1
                } else {
                    activeRank = 2
                }
                candidates.append((activeRank, markerRank, inst, row))
            }
        }
        candidates.sort {
            if $0.activeRank != $1.activeRank { return $0.activeRank < $1.activeRank }
            return $0.markerRank < $1.markerRank
        }
        for item in candidates where item.markerRank < 2 {
            let cmd = TmuxCommands.attachReadOnlyPTYCommand(
                session: item.row.session,
                socket: item.row.socket ?? item.inst.tmuxSocket)
            terminals.startBackgroundMonitor(
                for: item.inst.id, hostId: host.id, connection: conn, command: cmd,
                evictIdleIfNeeded: item.activeRank < 2
            ) { [weak self] in
                self?.reclassifyMonitoredProjects(on: host.id)
            }
        }
    }

    /// Re-run local classification on cached instances + rows for `hostId` using the
    /// latest live screens (0 extra SSH round trips) right after a background monitor
    /// finishes its initial screen paint.
    private func reclassifyMonitoredProjects(on hostId: UUID) {
        guard let host = host(id: hostId) else { return }
        let rows = lastPaneRowsByHost[hostId] ?? []
        let hostProjects = projects(for: host).filter { $0.id != selectedProject?.id }
        var map: [UUID: [Instance]] = [:]
        for p in hostProjects {
            if let cached = instanceCache[p.id]?.instances { map[p.id] = cached }
        }
        guard !map.isEmpty else { return }
        applyProjectSweep(projects: hostProjects, instancesByProject: map, rows: rows)
        syncLiveActivity()
    }

    private func readProjectChunk(
        _ chunk: [RemoteProject],
        conn: SSHConnection,
        now: TimeInterval
    ) async -> [UUID: [Instance]] {
        var result: [UUID: [Instance]] = [:]
        if chunk.count > 1 {
            let roots = chunk.map(\.remoteRoot)
            var batched = await RemoteLayoutStore.readMany(conn, roots: roots)
            if batched == nil, (try? await conn.connect()) != nil {
                batched = await RemoteLayoutStore.readMany(conn, roots: roots)
            }
            if let batched {
                for project in chunk {
                    if let layout = batched[project.remoteRoot] {
                        layoutCache[project.id] = layout
                        let list = layout.orderedTerminalInstances
                        instanceCache[project.id] = (list, now)
                        result[project.id] = list
                    } else if let prev = instanceCache[project.id], !prev.instances.isEmpty {
                        // A project that previously had agents didn't show up in the batch —
                        // verify with a single read before dropping its cached agents.
                        if let verified = try? await RemoteLayoutStore.read(conn, root: project.remoteRoot) {
                            layoutCache[project.id] = verified
                            let list = verified.orderedTerminalInstances
                            instanceCache[project.id] = (list, now)
                            result[project.id] = list
                        } else {
                            result[project.id] = prev.instances
                        }
                    } else {
                        layoutCache[project.id] = nil
                        instanceCache[project.id] = ([], now)
                        result[project.id] = []
                    }
                }
                return result
            }
        }
        for project in chunk {
            result[project.id] = await cachedInstances(for: project, conn: conn)
        }
        return result
    }

    private func runPollOnce() async {
        guard let project = selectedProject, let host = host(for: project) else { return }
        let conn = connection(for: host)
        // Every 2nd poll (~6s — desktop's live-sync cadence is ~5s), re-read the shared
        // layout so a peer's new panes, closed panes and renames show up live while this
        // project is open. Lightweight — a single file read; the same instance ids keep
        // the selected tab + live terminals. (Selecting a project / returning to the app
        // still refresh immediately.)
        pollTick &+= 1
        if pollTick % 2 == 0,
           let fresh = try? await RemoteLayoutStore.read(conn, root: project.remoteRoot) {
            guard selectedProject?.id == project.id else { return }
            layout = fresh
        }
        guard selectedProject?.id == project.id, let layout else { return }
        let now = Date().timeIntervalSince1970
        let terminalInstances = layout.orderedTerminalInstances
        instanceCache[project.id] = (terminalInstances, now)

        // Include sibling projects' recorded sockets on the selected host so one
        // `fetchPaneRows` covers the entire host (required for isolated-socket peers).
        let siblingsOnHost = projects(for: host).filter { $0.id != project.id }
        var siblingInstances: [Instance] = []
        let shouldSweepSiblings = pollTick == 1 || pollTick % 4 == 0
        if shouldSweepSiblings {
            var staleSiblings: [RemoteProject] = []
            for sib in siblingsOnHost {
                if let hit = instanceCache[sib.id], now - hit.at < instanceCacheTTL {
                    siblingInstances.append(contentsOf: hit.instances)
                } else {
                    staleSiblings.append(sib)
                }
            }
            // Read the first batch (up to 10 opened-first siblings) before `fetchPaneRows`
            // so their recorded sockets are queried in the same single sweep.
            if !staleSiblings.isEmpty {
                let firstBatch = Array(staleSiblings.prefix(10))
                let read = await readProjectChunk(firstBatch, conn: conn, now: Date().timeIntervalSince1970)
                siblingInstances.append(contentsOf: read.values.flatMap { $0 })
            }
        } else {
            for sib in siblingsOnHost {
                if let hit = instanceCache[sib.id] {
                    siblingInstances.append(contentsOf: hit.instances)
                }
            }
        }

        guard selectedProject?.id == project.id else { return }
        var terminalInstancesNow = terminalInstances
        var selectedRows = await poll.fetchPaneRows(conn, instances: terminalInstancesNow + siblingInstances)
        lastPaneRowsByHost[host.id] = selectedRows
        lastPaneRowsAtByHost[host.id] = Date().timeIntervalSince1970
        guard selectedProject?.id == project.id else { return }

        // If desktop just opened a brand-new `muxel_*` pane on this host, refresh
        // layouts immediately instead of waiting for the next periodic layout read.
        var forceSiblingSweep = false
        let pollNow = Date().timeIntervalSince1970
        if Self.hasUnmatchedLiveMuxelSession(rows: selectedRows, knownInstances: terminalInstancesNow + siblingInstances),
           pollNow - (lastUnmatchedRefreshByHost[host.id] ?? 0) > 5 {
            lastUnmatchedRefreshByHost[host.id] = pollNow
            if pollTick % 2 != 0,
               let fresh = try? await RemoteLayoutStore.read(conn, root: project.remoteRoot),
               selectedProject?.id == project.id {
                self.layout = fresh
                layoutCache[project.id] = fresh
                terminalInstancesNow = fresh.orderedTerminalInstances
                instanceCache[project.id] = (terminalInstancesNow, pollNow)
            }
            for sib in siblingsOnHost { instanceCache.removeValue(forKey: sib.id) }
            forceSiblingSweep = !siblingsOnHost.isEmpty
            // Re-fetch rows if the newly discovered layout uses a new socket.
            let freshSockets = Set(terminalInstancesNow.compactMap(\.tmuxSocket))
            let knownSockets = Set([TmuxCommands.socketName, "default"] + (terminalInstances + siblingInstances).compactMap(\.tmuxSocket))
            if !freshSockets.subtracting(knownSockets).isEmpty {
                selectedRows = await poll.fetchPaneRows(conn, instances: terminalInstancesNow + siblingInstances)
                lastPaneRowsByHost[host.id] = selectedRows
                lastPaneRowsAtByHost[host.id] = Date().timeIntervalSince1970
            }
        }

        // Probe any unattached marker-bearing agent (in this open project's other tabs
        // OR in sibling projects on this host) that shows fresh tmux output (e.g. a
        // prompt just started on desktop) or was already working/blocked.
        let probedScreens = await probeActiveUnattachedAgents(
            instances: terminalInstancesNow + siblingInstances,
            rows: selectedRows,
            conn: conn)
        guard selectedProject?.id == project.id else { return }

        var screens = liveScreens(for: terminalInstancesNow)
        for inst in terminalInstancesNow {
            if screens[inst.id] == nil, let probed = probedScreens[inst.id] {
                screens[inst.id] = probed
            }
        }
        poll.seedPreviouslyWorking(StatusPoller.persistedWorkingIds(for: terminalInstancesNow, defaults: defaults))
        let results = poll.classify(rows: selectedRows,
                                    instances: terminalInstancesNow,
                                    liveScreens: screens)
        recordObservedProbeOutcomes(results: results,
                                    probedIds: Set(probedScreens.keys),
                                    rows: selectedRows,
                                    instances: terminalInstancesNow,
                                    now: Date().timeIntervalSince1970)
        LastWorkedStore.record(results, defaults: defaults)
        let suppressed: Set<String> = activeViewedInstanceId.map { [$0] } ?? []
        StatusPoller.processTransitions(
            results: results,
            instances: terminalInstancesNow,
            projectId: project.id,
            projectName: project.name,
            rows: selectedRows,
            suppressIds: suppressed,
            defaults: defaults)
        for r in results { statuses[r.instanceId] = r.status }
        running = Set(results.filter(\.running).map(\.instanceId))
        hasPolledSelectedProject = true
        projectActivity[project.id] = Self.aggregate(results, instances: terminalInstancesNow)
        // Foreground start/refresh — keeps the Live Activity alive so it's on the
        // Lock Screen when the app is minimized (starting must happen here).
        syncLiveActivity()

        // Sweep sibling projects (including re-reading stale layouts) on the first poll,
        // every ~4th tick (~12s), or immediately when an unknown `muxel_*` session appeared.
        // On intermediate 3s ticks, re-classify already-cached sibling projects on THIS host
        // using `selectedRows` + any freshly probed screens so prompts and short shell
        // commands in other projects update and notify promptly.
        if shouldSweepSiblings || forceSiblingSweep {
            await sweepOtherProjects(selectedHostId: host.id, selectedRows: selectedRows)
        } else if !siblingsOnHost.isEmpty {
            var cachedMap: [UUID: [Instance]] = [:]
            for sib in siblingsOnHost {
                if let hit = instanceCache[sib.id], !hit.instances.isEmpty {
                    cachedMap[sib.id] = hit.instances
                }
            }
            if !cachedMap.isEmpty {
                applyProjectSweep(projects: siblingsOnHost,
                                  instancesByProject: cachedMap,
                                  rows: selectedRows,
                                  extraScreens: probedScreens,
                                  probedIds: Set(probedScreens.keys))
                syncLiveActivity()
            }
        }
    }

    /// True if `rows` contains a live, non-exited `muxel_*` tmux session that doesn't
    /// match any currently cached `Instance` on that host (e.g. a new pane/prompt just
    /// created on muxel desktop).
    nonisolated static func hasUnmatchedLiveMuxelSession(
        rows: [PaneRow],
        knownInstances: [Instance]
    ) -> Bool {
        for row in rows where !row.exited && row.session.hasPrefix("muxel_") {
            let matched = knownInstances.contains { inst in
                PollService.resolveRow(for: inst, in: [row]) != nil
            }
            if !matched { return true }
        }
        return false
    }

    /// Pure selection of unattached marker-bearing agents that should be snapshotted
    /// via a single read-only PTY batch on this poll tick:
    /// - live, non-exited, non-shell agent with working/blocked markers and no live PTY,
    /// - AND either already observed `.working`/`.blocked` (so completion is caught
    ///   promptly) OR showing recent tmux output (`idle <= recentActivityWindow`) since
    ///   the last idle probe (with a cooldown floor for continuously animating TUIs).
    nonisolated static func activeProbeCandidates(
        instances: [Instance],
        rows: [PaneRow],
        attachedIds: Set<String>,
        lastStatuses: [String: AgentStatus],
        idleProbeState: [String: (at: TimeInterval, activityTimestamp: TimeInterval?)],
        now: TimeInterval,
        recentActivityWindow: TimeInterval = 20,
        idleReprobeCooldown: TimeInterval = 12
    ) -> [(id: String, session: String, socket: String?, idle: TimeInterval, bell: Bool)] {
        var targets: [(id: String, session: String, socket: String?, idle: TimeInterval, bell: Bool)] = []
        var seenIds = Set<String>()
        for inst in instances where inst.kind == .terminal && !inst.isShell {
            guard seenIds.insert(inst.id).inserted,
                  !attachedIds.contains(inst.id) else { continue }
            let (w, b) = defaultMarkers(program: inst.program, args: inst.args)
            guard !(w.isEmpty && b.isEmpty) else { continue }
            guard let row = PollService.resolveRow(for: inst, in: rows),
                  !row.exited else { continue }
            let prevStatus = lastStatuses[inst.id]
            let isOngoing = prevStatus == .working || prevStatus == .blocked
            if !isOngoing {
                guard row.idle <= recentActivityWindow else { continue }
                if let prevIdle = idleProbeState[inst.id] {
                    if now - prevIdle.at < idleReprobeCooldown { continue }
                    if let prevAct = prevIdle.activityTimestamp,
                       let curAct = row.activityTimestamp,
                       curAct <= prevAct {
                        continue
                    }
                }
            }
            targets.append((inst.id, row.session, row.socket ?? inst.tmuxSocket, row.idle, row.bell))
        }
        return targets
    }

    private func probeActiveUnattachedAgents(
        instances: [Instance],
        rows: [PaneRow],
        conn: SSHConnection
    ) async -> [String: LiveScreen] {
        guard !rows.isEmpty, !instances.isEmpty else { return [:] }
        var attachedIds = Set<String>()
        for inst in instances where terminals.liveScreen(for: inst.id) != nil {
            attachedIds.insert(inst.id)
        }
        var effectiveLastStatuses = lastObservedStatusByInstance
        for id in StatusPoller.persistedWorkingIds(for: instances, defaults: defaults) {
            if effectiveLastStatuses[id] == nil {
                effectiveLastStatuses[id] = .working
            }
        }
        let now = Date().timeIntervalSince1970
        let targets = Self.activeProbeCandidates(
            instances: instances,
            rows: rows,
            attachedIds: attachedIds,
            lastStatuses: effectiveLastStatuses,
            idleProbeState: idleProbeState,
            now: now)
        guard !targets.isEmpty else { return [:] }
        let probed: [String: LiveScreen]
        if let screenProber {
            probed = await screenProber(targets, conn)
        } else {
            probed = await terminals.batchProbeScreens(targets: targets, connection: conn)
        }
        let stamp = Date().timeIntervalSince1970
        for (id, screen) in probed {
            probedScreensCache[id] = (screen, stamp)
        }
        return probed
    }

    private func recordObservedProbeOutcomes(
        results: [InstanceStatus],
        probedIds: Set<String>,
        rows: [PaneRow],
        instances: [Instance],
        now: TimeInterval
    ) {
        let byId = Dictionary(instances.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
        for r in results {
            if r.running {
                lastObservedStatusByInstance[r.instanceId] = r.status
                if r.status == .working || r.status == .blocked {
                    idleProbeState.removeValue(forKey: r.instanceId)
                } else if probedIds.contains(r.instanceId) {
                    let act = byId[r.instanceId].flatMap { PollService.resolveRow(for: $0, in: rows)?.activityTimestamp }
                    idleProbeState[r.instanceId] = (now, act)
                }
            } else {
                lastObservedStatusByInstance.removeValue(forKey: r.instanceId)
                probedScreensCache.removeValue(forKey: r.instanceId)
                idleProbeState.removeValue(forKey: r.instanceId)
            }
        }
    }

    /// Live-screen snapshots for any of `instances` that are currently attached in
    /// `TerminalStore` (or recently snapshotted via `batchProbeScreens`), so both
    /// attached and freshly probed panes get real marker status.
    private func liveScreens(for instances: [Instance]) -> [String: LiveScreen] {
        var screens: [String: LiveScreen] = [:]
        let now = Date().timeIntervalSince1970
        for inst in instances {
            if let screen = terminals.liveScreen(for: inst.id) {
                screens[inst.id] = screen
            } else if let cached = probedScreensCache[inst.id], now - cached.at <= 15 {
                screens[inst.id] = cached.screen
            }
        }
        return screens
    }

    /// Aggregate per-instance statuses into a project's badge counts, sorted
    /// per-running-agent status dots, and full per-agent summary list.
    nonisolated static func aggregate(
        _ results: [InstanceStatus],
        instances: [Instance] = []
    ) -> ProjectActivity {
        var a = ProjectActivity(running: 0, blocked: 0, done: 0)
        var list: [AgentStatus] = []
        let byId = Dictionary(instances.map { ($0.id, $0) }, uniquingKeysWith: { first, _ in first })
        var agents: [AgentSummary] = []
        agents.reserveCapacity(results.count)
        for r in results {
            let name = byId[r.instanceId]?.displayName ?? "agent"
            agents.append(AgentSummary(id: r.instanceId, name: name, status: r.status, running: r.running))
            if r.running {
                a.running += 1
                list.append(r.status)
                switch r.status {
                case .blocked: a.blocked += 1
                case .done: a.done += 1
                default: break
                }
            }
        }
        a.agentStatuses = sortAgentStatuses(list)
        a.agents = sortAgents(agents)
        return a
    }

    /// Refresh `projectActivity` for every non-selected project on a host we already
    /// hold a connection to (or all hosts when `connectIfNeeded` is true).
    private func sweepOtherProjects(selectedHostId: UUID, selectedRows: [PaneRow],
                                    connectIfNeeded: Bool = false) async {
        let selectedProjectId = selectedProject?.id
        for host in doc.hosts {
            let hostProjects = projects(for: host).filter { $0.id != selectedProjectId }
            guard !hostProjects.isEmpty else { continue }
            await sweepHostProjects(
                host: host,
                projects: hostProjects,
                connectIfNeeded: connectIfNeeded,
                preFetchedRows: host.id == selectedHostId ? selectedRows : nil)
        }
    }

    @discardableResult
    private func applyProjectSweep(
        projects: [RemoteProject],
        instancesByProject: [UUID: [Instance]],
        rows: [PaneRow],
        detectWorking: Bool = false,
        extraScreens: [String: LiveScreen] = [:],
        probedIds: Set<String> = []
    ) -> Set<UUID> {
        var working = Set<UUID>()
        for project in projects {
            guard let instances = instancesByProject[project.id],
                  !instances.isEmpty else {
                projectActivity[project.id] = nil
                summaryRowsByProject[project.id] = nil
                continue
            }
            // Attached panes of a backgrounded project (plus any freshly batch-probed
            // snapshots) still get full marker status. Fresh `extraScreens` take
            // precedence over the short-lived probe cache so transitions land immediately.
            var screens = liveScreens(for: instances)
            for inst in instances {
                if let extra = extraScreens[inst.id], terminals.liveScreen(for: inst.id) == nil {
                    screens[inst.id] = extra
                }
            }
            poll.seedPreviouslyWorking(StatusPoller.persistedWorkingIds(for: instances, defaults: defaults))
            let results = poll.classify(rows: rows, instances: instances, liveScreens: screens)
            let now = Date().timeIntervalSince1970
            recordObservedProbeOutcomes(results: results, probedIds: probedIds, rows: rows, instances: instances, now: now)
            LastWorkedStore.record(results, now: now, defaults: defaults)
            let suppressed: Set<String> = (selectedProject?.id == project.id)
                ? (activeViewedInstanceId.map { [$0] } ?? [])
                : []
            StatusPoller.processTransitions(
                results: results,
                instances: instances,
                projectId: project.id,
                projectName: project.name,
                rows: rows,
                suppressIds: suppressed,
                defaults: defaults)
            projectActivity[project.id] = Self.aggregate(results, instances: instances)
            if detectWorking, Self.isActivelyWorking(results: results, rows: rows, instances: instances) {
                working.insert(project.id)
            }
            let byId = Dictionary(instances.map { ($0.id, $0) }, uniquingKeysWith: { a, _ in a })
            summaryRowsByProject[project.id] = results.compactMap { r in
                guard r.running else { return nil }
                let inst = byId[r.instanceId]
                let name = inst?.displayName ?? "agent"
                let paneIdle = inst.flatMap { PollService.resolveRow(for: $0, in: rows)?.idle }
                let lastActive = LastWorkedStore.effectiveLastActiveAt(
                    instance: inst, status: r.status, running: r.running,
                    paneIdle: paneIdle, now: now, defaults: defaults)
                return ActivitySummaryBuilder.row(
                    id: r.instanceId, name: name, project: project.name,
                    status: r.status, running: r.running, lastActiveAt: lastActive)
            }
        }
        return working
    }

    /// A non-selected project's terminal instances, cached with a short TTL so a sweep
    /// is usually one `list-panes` per host plus occasional layout reads. A failed
    /// read falls back to the stale entry rather than dropping the badge.
    private func cachedInstances(for project: RemoteProject, conn: SSHConnection) async -> [Instance]? {
        let now = Date().timeIntervalSince1970
        if let hit = instanceCache[project.id], now - hit.at < instanceCacheTTL {
            return hit.instances
        }
        guard let fresh = try? await RemoteLayoutStore.read(conn, root: project.remoteRoot) else {
            return instanceCache[project.id]?.instances
        }
        layoutCache[project.id] = fresh
        let instances = fresh.orderedTerminalInstances
        instanceCache[project.id] = (instances, now)
        return instances
    }

    /// The user viewed a pane — clear its done latch so it stops showing done.
    func attend(_ instanceId: String) { poll.attend(instanceId) }

    /// Refresh `notificationsDenied` (called on foreground, so granting permission
    /// in Settings clears the sidebar notice on return).
    func refreshNotificationStatus() async {
        notificationsDenied = await NotificationManager.authorizationStatus() == .denied
    }

    /// Request notification permission once the user has started setting up a host
    /// (instead of prompting cold on first launch before they see the app).
    func requestNotificationsIfNeeded() async {
        let status = await NotificationManager.requestAuthorizationIfNotDetermined()
        notificationsDenied = status == .denied
    }

    // MARK: Launch a new instance

    func launch(preset: Preset, customCommand: String?, into project: RemoteProject,
                targetLeafAnchor: String? = nil,
                newWorktree: Bool = false, worktreeBranch: String? = nil) async {
        guard let host = host(for: project) else { return }

        let instanceId = UUID().uuidString.lowercased()
        // Reuse the project's existing instance project_id so iOS-launched panes
        // group with the rest; seed a fresh one for an empty project.
        let projectId = layout?.instances.first?.projectId ?? UUID().uuidString.lowercased()

        var instance: Instance
        if let custom = customCommand, !custom.trimmingCharacters(in: .whitespaces).isEmpty {
            // Quote-aware split, so `claude --append-system-prompt "be terse"` works.
            // The launch sheet live-validates, so nil here is a stale-form edge case.
            guard let parts = Shell.splitWords(custom), let first = parts.first else {
                report("Couldn't parse the command — check for an unbalanced quote.")
                return
            }
            instance = Instance(id: instanceId, projectId: projectId,
                                title: (first as NSString).lastPathComponent,
                                program: first, args: Array(parts.dropFirst()))
        } else {
            let title = preset.program.map { ($0 as NSString).lastPathComponent } ?? "shell"
            // model/effort/extra go into persisted args; the system prompt does NOT —
            // it's stored + applied at PTY-build time (CliFlag) or typed in (TypeIn), so
            // a desktop respawn doesn't double-append it.
            instance = Instance(id: instanceId, projectId: projectId, title: title,
                                program: preset.program, args: AgentLaunch.composeArgs(preset))
            instance.preset = preset.name
            instance.systemPrompt = preset.systemPrompt
            instance.injection = preset.injection
            // Resume-capable presets get a fresh session id, not yet started, so the
            // first launch uses `--session-id` and later re-attaches use `--resume`.
            if preset.sessionIdFlag != nil, preset.resumeFlag != nil {
                instance.sessionId = UUID().uuidString.lowercased()
                instance.sessionStarted = false
            }
        }
        // Record the authoritative tmux session name in the shared layout so a peer
        // (desktop muxel) adopts this instance into tmux under the same name rather
        // than spawning a second session for it.
        instance.tmuxSession = TmuxSession.name(hostName: host.name, instanceId: instanceId)
        instance.tmuxSocket = TmuxCommands.socketName

        // Create the git worktree first (the pane's cwd depends on it). On failure,
        // fall back to the project root with a notice — desktop's `setup_worktree`
        // parity. The Launch button's Task keeps the sheet up during this brief git op.
        var worktreeRecord: Worktree?
        if newWorktree {
            let repoName = (project.remoteRoot as NSString).lastPathComponent
            let branch = (worktreeBranch?.isEmpty == false)
                ? worktreeBranch! : WorktreeNaming.branchName(instanceId: instanceId)
            let dir = WorktreeNaming.dirName(repoName: repoName, instanceId: instanceId)
            do {
                let conn = connection(for: host)
                try await conn.connect()
                let path = try await WorktreeService.create(
                    conn, root: project.remoteRoot, dirName: dir, branch: branch)
                let wt = Worktree(id: UUID().uuidString.lowercased(), projectId: projectId,
                                  name: WorktreeNaming.randomName(), path: path, branch: branch,
                                  color: WorktreeNaming.nextColor(worktrees: layout?.worktrees ?? [],
                                                                  projectId: projectId),
                                  detached: false)
                instance.useWorktree = true
                instance.worktreeId = wt.id
                instance.worktreePath = path
                instance.worktreeBranch = branch
                worktreeRecord = wt
            } catch let err as WorktreeError {
                report("worktree skipped: \(err.message) — launching in the project root", duration: 6)
            } catch {
                surface(error, host: host)
            }
        }

        // Show the pane immediately: add it to the in-memory layout and select it, so
        // the click feels instant. The live terminal creates the tmux session itself
        // (`new-session -A` over a PTY) when the pane opens — it doesn't need the shared
        // layout written first. We deliberately don't create a detached session here (it
        // crashes a TUI agent at init).
        pendingLaunches.insert(instanceId)
        if selectedProject?.id == project.id {
            var next = layout ?? RemoteLayout(remoteRoot: project.remoteRoot)
            next.addInstanceAsTab(instance, now: unixNow(), targetLeafAnchor: targetLeafAnchor)
            if let worktreeRecord { next.worktrees.append(worktreeRecord) }
            layout = next
            lastLaunched = instanceId
        }

        // Persist `.muxel/workspace.json` to the remote in the background (read-modify-
        // write over SSH) rather than blocking the UI on those round-trips; reassign the
        // authoritative layout when it returns (newer-wins also picks up peer changes).
        enqueueLayoutWrite { [self] in
            defer { self.pendingLaunches.remove(instanceId) }
            do {
                let conn = self.connection(for: host)
                try await conn.connect()
                let written = try await RemoteLayoutStore.appendInstance(
                    conn, root: project.remoteRoot, instance: instance,
                    targetLeafAnchor: targetLeafAnchor, worktree: worktreeRecord)
                publishLayout(written, for: project)
                if selectedProject?.id == project.id { await self.runPollOnce() }
            } catch {
                self.surface(error, host: host)
            }
        }
    }

    /// Kill an instance's tmux session and drop it from the layout.
    func close(_ instance: Instance, in project: RemoteProject) async {
        guard let host = host(for: project) else { return }
        await enqueueLayoutWrite { [self] in
            do {
                let conn = connection(for: host)
                try await conn.connect()
                // The caller's tab snapshot (and the selected project's in-memory
                // layout) may predate a queued binding or a peer's close. Only a
                // surviving instance in the shared document authorizes a kill.
                let fresh = try await RemoteLayoutStore.read(conn, root: project.remoteRoot)
                guard let current = fresh?.instances.first(where: { $0.id == instance.id }) else {
                    terminals.disconnect(instance.id)
                    publishLayout(fresh, for: project)
                    return
                }
                // Resolve the *live* session by uuid8 suffix so we kill the real one (which
                // may carry desktop's host slug), not a phone-slug name that doesn't exist.
                // Another pane still showing it keeps it running.
                if current.kind == .terminal, !current.literalArgv,
                   let target = await liveSessionTarget(current, on: host),
                   !TmuxCommands.sessionStillShown(target.name, closing: [current.id],
                                                   in: fresh?.instances ?? []) {
                    _ = try? await conn.tmux(TmuxCommands.killSession(target.name),
                                            session: target.name, socket: target.socket)
                }
                terminals.disconnect(instance.id)
                let written = try await RemoteLayoutStore.removeInstance(conn, root: project.remoteRoot, instanceId: instance.id)
                publishLayout(written, for: project)
            } catch {
                surface(error, host: host)
            }
        }.value
    }

    /// Close every pane except `keepId`: kill each other terminal's tmux session, then
    /// drop them all from the layout in ONE write (not N round-trips).
    func closeOthers(keeping keepId: String, in project: RemoteProject) async {
        guard let host = host(for: project) else { return }
        let others = (layout?.orderedPaneInstances ?? []).filter { $0.id != keepId }
        guard !others.isEmpty else { return }
        let ids = Set(others.map(\.id))
        await enqueueLayoutWrite { [self] in
            do {
                let conn = connection(for: host)
                try await conn.connect()
                let fresh = try await RemoteLayoutStore.read(conn, root: project.remoteRoot)
                // Keep the action's intended IDs, while taking each surviving
                // target's current owner from the document inside this queue slot.
                let current = fresh?.instances.filter { ids.contains($0.id) } ?? []
                for inst in current where inst.kind == .terminal && !inst.literalArgv {
                    // A session a kept pane still shows keeps running for it.
                    if let target = await liveSessionTarget(inst, on: host),
                       !TmuxCommands.sessionStillShown(target.name, closing: ids,
                                                       in: fresh?.instances ?? []) {
                        _ = try? await conn.tmux(TmuxCommands.killSession(target.name),
                                                session: target.name, socket: target.socket)
                    }
                }
                for id in ids { terminals.disconnect(id) }
                guard !current.isEmpty else {
                    publishLayout(fresh, for: project)
                    return
                }
                let written = try await RemoteLayoutStore.mutate(conn, root: project.remoteRoot, seedIfMissing: false) { layout in
                    layout.instances.removeAll { ids.contains($0.id) }
                    for id in ids { _ = PaneTree.remove(&layout.layout, target: id) }
                    return true
                }
                publishLayout(written, for: project)
            } catch {
                surface(error, host: host)
            }
        }.value
    }

    /// Rename an instance (its tab label) in the shared layout. A blank name clears
    /// the custom name, reverting to the program-derived title.
    func rename(_ instance: Instance, to name: String, in project: RemoteProject) async {
        guard let host = host(for: project) else { return }
        await enqueueLayoutWrite { [self] in
            do {
                let conn = connection(for: host)
                try await conn.connect()
                if let updated = try await RemoteLayoutStore.renameInstance(
                    conn, root: project.remoteRoot, instanceId: instance.id, name: name)
                {
                    publishLayout(updated, for: project)
                }
            } catch {
                surface(error, host: host)
            }
        }.value
    }

    /// Duplicate an instance: a new pane running the same program/args, with a fresh
    /// id + tmux session (and no inherited worktree binding). Like `launch`, it only
    /// records the instance — the live terminal creates the session attached on open.
    func duplicate(_ instance: Instance, in project: RemoteProject) async {
        guard let host = host(for: project) else { return }
        let newId = UUID().uuidString.lowercased()
        var copy = instance
        copy.id = newId
        copy.tmuxSession = copy.literalArgv ? nil : TmuxSession.name(hostName: host.name, instanceId: newId)
        copy.tmuxSocket = copy.literalArgv ? nil : TmuxCommands.socketName
        if copy.literalArgv { copy.useTmux = false }
        // Desktop "Inherit" semantics: the duplicate shares the source's worktree
        // (a worktree can back several panes) but gets a fresh, not-yet-started
        // session so it doesn't collide on `--resume`.
        copy.resetConversationForDuplicate()
        await enqueueLayoutWrite { [self] in
            do {
                let conn = connection(for: host)
                try await conn.connect()
                let written = try await RemoteLayoutStore.appendInstance(
                    conn, root: project.remoteRoot, instance: copy)
                publishLayout(written, for: project)
                if selectedProject?.id == project.id {
                    lastLaunched = newId
                    await runPollOnce()
                }
            } catch {
                surface(error, host: host)
            }
        }.value
    }

    /// Run `op` after any in-flight layout write completes, so the phone's own
    /// mutations serialize into one RMW-at-a-time queue.
    @discardableResult
    func enqueueLayoutWrite(_ op: @escaping @MainActor () async -> Void) -> Task<Void, Never> {
        let prev = layoutWriteChain
        let task = Task { @MainActor in
            await prev?.value
            await op()
        }
        layoutWriteChain = task
        return task
    }

    /// A queued write may finish after navigation; keep its remote result out of
    /// the newly selected project's UI and invalidate the sidebar's cached layout.
    private func publishLayout(_ written: RemoteLayout?, for project: RemoteProject) {
        instanceCache[project.id] = nil
        layoutCache[project.id] = written
        guard selectedProject?.id == project.id else { return }
        layout = written
    }

    /// iPad basic split editing: pull `instance`'s tab out of its group into a new pane
    /// split beside the rest. Optimistic local apply, then a serialized write-back so
    /// desktop adopts it. No-op if `instance` is already the sole tab of its pane.
    func openInSplit(_ instance: Instance, direction: SplitDirection, before: Bool = false,
                     in project: RemoteProject) {
        guard let host = host(for: project),
              let current = layout, let root = current.layout,
              let path = root.findPath(instance.id),
              case let .leaf(tabs, _)? = root.node(atPath: path),
              let sibling = tabs.first(where: { $0 != instance.id }) else { return }
        // Optimistic local apply so the split appears instantly.
        var next = current
        guard PaneTree.moveIntoSplit(&next.layout, dragged: instance.id, target: sibling,
                                     direction: direction, before: before) else { return }
        guard withinPaneCap(next) else { return }
        next.updatedAt = max(unixNow(), current.updatedAt + 1)
        layout = next
        enqueueLayoutWrite { [self] in
            do {
                let conn = connection(for: host)
                try await conn.connect()
                if let written = try await RemoteLayoutStore.moveIntoSplit(
                    conn, root: project.remoteRoot, dragged: instance.id, target: sibling,
                    direction: direction, before: before) {
                    publishLayout(written, for: project)
                }
            } catch {
                surface(error, host: host)
                await refreshLayout()
            }
        }
    }

    /// The maximum number of side-by-side panes (leaves) allowed.
    static let maxPanes = 4

    /// True if `layout` is within the pane cap; otherwise reports a notice and returns
    /// false so the caller can abort a split that would create too many panes.
    private func withinPaneCap(_ layout: RemoteLayout) -> Bool {
        guard (layout.layout?.leafCount ?? 0) <= Self.maxPanes else {
            report("Up to \(Self.maxPanes) split panes. Close one first.", duration: 4)
            return false
        }
        return true
    }

    /// Persist that a resume-capable instance's session has started (and backfill a
    /// missing session id), so a later re-attach resumes with `--resume` instead of
    /// creating a duplicate. Best-effort + serialized with other layout writes.
    func markSessionStarted(_ instanceId: String, sessionId: String?, in project: RemoteProject) {
        guard let host = host(for: project) else { return }
        enqueueLayoutWrite { [self] in
            do {
                let conn = connection(for: host)
                try await conn.connect()
                if let written = try await RemoteLayoutStore.mutate(
                    conn, root: project.remoteRoot, seedIfMissing: false, transform: { layout in
                        guard let idx = layout.instances.firstIndex(where: { $0.id == instanceId }) else { return false }
                        layout.instances[idx].sessionStarted = true
                        if let sid = sessionId, layout.instances[idx].sessionId == nil {
                            layout.instances[idx].sessionId = sid
                        }
                        return true
                    }) {
                    publishLayout(written, for: project)
                }
            } catch {
                // Non-fatal: resume just won't engage on the next attach.
            }
        }
    }

    /// TypeIn injection: once the freshly-launched agent has drawn its first output,
    /// type its system prompt into the pane (and optionally submit). Mirrors desktop's
    /// timing (`view.rs`): wait for first output, then the preset's startup delay, then
    /// type via `tmux send-keys`. Owned here (not the view) so it survives navigation.
    func scheduleStartupInjection(instanceId: String, sessionName: String, socket: String, host: Host,
                                  text: String, delayMs: Int, submit: Bool) {
        guard !text.isEmpty else { return }
        Task { [weak self] in
            guard let self else { return }
            // Wait for the agent's first output (readiness), polling 100ms, cap 30s.
            let deadline = Date().addingTimeInterval(30)
            while Date() < deadline, !self.terminals.hasOutput(for: instanceId) {
                try? await Task.sleep(nanoseconds: 100_000_000)
            }
            // Startup delay (preset's, or a 2s default), then a 300ms settle.
            let delay = delayMs > 0 ? delayMs : 2000
            try? await Task.sleep(nanoseconds: UInt64(delay) * 1_000_000 + 300_000_000)
            let conn = self.connection(for: host)
            _ = try? await conn.tmux(TmuxCommands.sendLiteral(session: sessionName, text: text), session: sessionName, socket: socket)
            if submit {
                try? await Task.sleep(nanoseconds: 400_000_000)
                _ = try? await conn.tmux(TmuxCommands.sendKey(session: sessionName, key: "Enter"), session: sessionName, socket: socket)
            }
        }
    }

    /// iPad drag-to-split: pull `dragged` out of its pane into a new split beside the
    /// leaf anchored at `targetAnchor`. Optimistic local apply + serialized write-back.
    /// No-op if the move is meaningless (same pane, sole tab dropped on itself).
    func moveTabIntoSplit(dragged: String, targetAnchor: String,
                          direction: SplitDirection = .horizontal, before: Bool = false,
                          in project: RemoteProject) {
        guard let host = host(for: project), let current = layout, current.layout != nil else { return }
        var next = current
        guard PaneTree.moveIntoSplit(&next.layout, dragged: dragged, target: targetAnchor,
                                     direction: direction, before: before) else { return }
        guard withinPaneCap(next) else { return }
        next.updatedAt = max(unixNow(), current.updatedAt + 1)
        layout = next
        enqueueLayoutWrite { [self] in
            do {
                let conn = connection(for: host)
                try await conn.connect()
                if let written = try await RemoteLayoutStore.moveIntoSplit(
                    conn, root: project.remoteRoot, dragged: dragged, target: targetAnchor,
                    direction: direction, before: before) {
                    publishLayout(written, for: project)
                }
            } catch {
                surface(error, host: host)
                await refreshLayout()
            }
        }
    }

    /// Persist new sizes for the split identified by `key` (its `stableKey`) after an
    /// interactive divider drag. Optimistic local apply + serialized write-back.
    func setSplitSizes(key: String, sizes: [Double], in project: RemoteProject) {
        guard let host = host(for: project), let current = layout, current.layout != nil else { return }
        var next = current
        guard PaneTree.setSplitSizes(&next.layout, key: key, sizes: sizes) else { return }
        next.updatedAt = max(unixNow(), current.updatedAt + 1)
        layout = next
        enqueueLayoutWrite { [self] in
            do {
                let conn = connection(for: host)
                try await conn.connect()
                if let written = try await RemoteLayoutStore.setSplitSizes(
                    conn, root: project.remoteRoot, key: key, sizes: sizes) {
                    publishLayout(written, for: project)
                }
            } catch {
                surface(error, host: host)
                await refreshLayout()
            }
        }
    }

    /// Drag-to-reorder: move `dragged` to the slot of `targetChip` — reordering within
    /// a group, or inserting at that position when dropped on another group's tab.
    func moveTab(dragged: String, toPositionOf targetChip: String, in project: RemoteProject) {
        guard let host = host(for: project), let current = layout, let root = current.layout else { return }
        guard let pt = root.findPath(targetChip) else { return }
        guard case let .leaf(tabs, _)? = root.node(atPath: pt),
              let index = tabs.firstIndex(of: targetChip) else { return }
        var next = current
        guard PaneTree.moveTabTo(&next.layout, dragged: dragged, targetAnchor: targetChip, index: index) else { return }
        next.updatedAt = max(unixNow(), current.updatedAt + 1)
        layout = next
        enqueueLayoutWrite { [self] in
            do {
                let conn = connection(for: host)
                try await conn.connect()
                if let written = try await RemoteLayoutStore.moveTabTo(
                    conn, root: project.remoteRoot, dragged: dragged, targetAnchor: targetChip, index: index) {
                    publishLayout(written, for: project)
                }
            } catch {
                surface(error, host: host)
                await refreshLayout()
            }
        }
    }

    /// iPad drag-to-move: move `dragged`'s tab into `targetAnchor`'s pane (join its tab
    /// group). Optimistic local apply + serialized write-back. No-op on a same-pane move.
    func moveTabIntoPane(dragged: String, targetAnchor: String, in project: RemoteProject) {
        guard let host = host(for: project), let current = layout, current.layout != nil else { return }
        var next = current
        guard PaneTree.moveIntoTabs(&next.layout, dragged: dragged, target: targetAnchor) else { return }
        next.updatedAt = max(unixNow(), current.updatedAt + 1)
        layout = next
        enqueueLayoutWrite { [self] in
            do {
                let conn = connection(for: host)
                try await conn.connect()
                if let written = try await RemoteLayoutStore.moveIntoTabs(
                    conn, root: project.remoteRoot, dragged: dragged, target: targetAnchor) {
                    publishLayout(written, for: project)
                }
            } catch {
                surface(error, host: host)
                await refreshLayout()
            }
        }
    }

    /// Rebuild the whole attachment plan once if binding finds a newer saved owner.
    /// Never reuse the view's captured instance after a conflict or removed pane.
    func prepareTmuxAttachment<Attachment>(
        instanceId: String, in project: RemoteProject,
        prepare: @MainActor (Instance) async throws -> (target: TmuxCommands.SessionTarget, attachment: Attachment)
    ) async throws -> Attachment? {
        try await prepareTmuxAttachmentPlan(instanceId: instanceId, in: project) { current in
            let prepared = try await prepare(current)
            return TmuxAttachmentPlan(target: prepared.target, attachment: prepared.attachment)
        }
    }

    func prepareTmuxAttachmentPlan<Attachment>(
        instanceId: String, in project: RemoteProject,
        prepare: @MainActor (Instance) async throws -> TmuxAttachmentPlan<Attachment>
    ) async throws -> Attachment? {
        for _ in 0..<2 {
            guard !Task.isCancelled, selectedProject?.id == project.id,
                  let current = layout?.instances.first(where: { $0.id == instanceId }),
                  !current.literalArgv else { return nil }
            let prepared = try await prepare(current)
            guard !Task.isCancelled, selectedProject?.id == project.id else { return nil }
            if try await bindTmuxTarget(prepared.target, instance: current, in: project,
                                        defaultMigration: prepared.defaultMigration) {
                guard !Task.isCancelled, selectedProject?.id == project.id,
                      let bound = layout?.instances.first(where: { $0.id == instanceId }),
                      !bound.literalArgv, bound.tmuxSocket == prepared.target.socket,
                      bound.tmuxSession == prepared.target.name else { return nil }
                return prepared.attachment
            }
        }
        return nil
    }

    /// Confirm saved membership and the resolved owner before attachment so close
    /// and restart use that server even if another gains the same name.
    func bindTmuxTarget(_ target: TmuxCommands.SessionTarget, instance: Instance,
                        in project: RemoteProject,
                        defaultMigration: TmuxCommands.DefaultMigration? = nil) async throws -> Bool {
        guard selectedProject?.id == project.id, let host = host(for: project) else { return false }
        return try await withCheckedThrowingContinuation { (continuation: CheckedContinuation<Bool, Error>) in
            enqueueLayoutWrite { [self] in
                do {
                    guard selectedProject?.id == project.id else {
                        continuation.resume(returning: false); return
                    }
                    let conn = connection(for: host)
                    try await conn.connect()
                    guard selectedProject?.id == project.id else {
                        continuation.resume(returning: false); return
                    }
                    // A new pane already has its intended owner in the optimistic
                    // UI. Await its append and verify durable membership before
                    // allowing a PTY, even when that owner appears unchanged.
                    var observed: RemoteLayout?
                    var permitted = false
                    let written = try await RemoteLayoutStore.mutate(
                        conn, root: project.remoteRoot, seedIfMissing: false, transform: { layout in
                            observed = layout
                            guard let idx = layout.instances.firstIndex(where: { $0.id == instance.id }) else { return false }
                            let saved = layout.instances[idx]
                            guard !saved.literalArgv else { return false }
                            // A resolved server is authoritative, including when
                            // its session name has not been backfilled yet. A
                            // legacy name without a socket still needs discovery.
                            if let socket = saved.tmuxSocket {
                                let migrating = socket == "default" && target.socket == TmuxCommands.socketName
                                    && defaultMigration?.recordedSession == saved.tmuxSession
                                    && defaultMigration?.sessionName == target.name
                                    && defaultMigration != nil
                                guard socket == target.socket || migrating else { return false }
                                if let name = saved.tmuxSession, !name.isEmpty,
                                   name != target.name { return false }
                            }
                            permitted = true
                            if layout.instances[idx].tmuxSession == target.name,
                               layout.instances[idx].tmuxSocket == target.socket { return false }
                            layout.instances[idx].tmuxSession = target.name
                            layout.instances[idx].tmuxSocket = target.socket
                            return true
                        })
                    guard selectedProject?.id == project.id else {
                        continuation.resume(returning: false); return
                    }
                    // A declined transform does not write, but its fresh durable
                    // state must replace the snapshot used for the rejected plan.
                    publishLayout(written ?? observed, for: project)
                    continuation.resume(returning: permitted)
                } catch {
                    continuation.resume(throwing: error)
                }
            }
        }
    }

    /// Callers supply a surviving instance read inside their queued transaction.
    private func liveSessionTarget(_ instance: Instance, on host: Host) async -> TmuxCommands.SessionTarget? {
        if let socket = instance.tmuxSocket, let name = instance.tmuxSession {
            return TmuxCommands.SessionTarget(name: name, socket: socket)
        }
        let conn = connection(for: host)
        let out = (try? await conn.run(TmuxCommands.listTargetsCommand(socket: instance.tmuxSocket))) ?? ""
        return TmuxCommands.resolveTarget(instance, in: TmuxCommands.parseTargets(out))
    }
}
