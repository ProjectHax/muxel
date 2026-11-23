import Foundation

/// Pure aggregation from poll results into the Live Activity payload — the testable
/// core of the status bar. No ActivityKit / UI here.
enum ActivitySummaryBuilder {
    static let nameCap = 20
    static let rowCap = 16 // payload-size guard (ActivityKit ~4 KB)

    /// Coarse per-instance state from its poll signals:
    /// - `.blocked` (rang the bell / a real prompt marker) → **needsInput**
    /// - `.done` (agent process exited) → **finished**
    /// - live and working (recent activity) → **working**
    /// - otherwise (live but quiet, or no live session) → **idle**
    static func state(status: AgentStatus, running: Bool) -> MuxelActivityAttributes.InstanceState {
        switch status {
        case .blocked: return .needsInput
        case .done: return .finished
        case .working: return running ? .working : .idle
        case .idle: return .idle
        }
    }

    static func row(
        id: String, name: String, project: String, projectId: UUID? = nil,
        status: AgentStatus, running: Bool, lastActiveAt: TimeInterval? = nil
    ) -> MuxelActivityAttributes.InstanceRow {
        .init(
            id: id, name: String(name.prefix(nameCap)),
            project: String(project.prefix(nameCap)),
            state: state(status: status, running: running),
            lastActiveAt: lastActiveAt,
            projectKey: projectId.map { MuxelActivityAttributes.InstanceRow.projectKey(for: $0) })
    }

    /// Drop rows whose project no longer exists (deleted, or its host removed), so a
    /// cached summary can't keep them on the Live Activity. Rows cached before rows
    /// carried a `projectKey` are matched by project name instead.
    static func keepingRows(
        _ rows: [MuxelActivityAttributes.InstanceRow], ofProjects projects: [(id: UUID, name: String)]
    ) -> [MuxelActivityAttributes.InstanceRow] {
        let keys = Set(projects.map { MuxelActivityAttributes.InstanceRow.projectKey(for: $0.id) })
        let names = Set(projects.map { String($0.name.prefix(nameCap)) })
        return rows.filter { row in
            if let key = row.projectKey { return keys.contains(key) }
            return names.contains(row.project)
        }
    }

    /// Fold instance rows into a `ContentState`: counts, and an attention-priority sort
    /// (needsInput → finished → working → idle, then most-recently-worked first, then
    /// project, then name) that keeps agents waiting for you and what was working last
    /// at the very top. Caps the list (overflow kept in `instanceCount`).
    static func contentState(
        rows: [MuxelActivityAttributes.InstanceRow], now: Date
    ) -> MuxelActivityAttributes.ContentState {
        func rank(_ s: MuxelActivityAttributes.InstanceState) -> Int {
            switch s {
            case .needsInput: return 0
            case .finished: return 1
            case .working: return 2
            case .idle: return 3
            }
        }
        func count(_ s: MuxelActivityAttributes.InstanceState) -> Int {
            rows.filter { $0.state == s }.count
        }
        let sorted = rows.sorted {
            if rank($0.state) != rank($1.state) { return rank($0.state) < rank($1.state) }
            let a = $0.lastActiveAt ?? 0
            let b = $1.lastActiveAt ?? 0
            if a != b { return a > b }
            if $0.project != $1.project { return $0.project < $1.project }
            return $0.name < $1.name
        }
        return .init(
            instances: Array(sorted.prefix(rowCap)),
            needsInputCount: count(.needsInput),
            finishedCount: count(.finished),
            workingCount: count(.working),
            instanceCount: rows.count, updatedAt: now)
    }
}

/// Persists the most recent Unix timestamp (seconds) each instance was observed
/// working / needing input / finishing, so the Live Activity keeps whatever was
/// working last pinned to the top even after it finishes or goes idle.
enum LastWorkedStore {
    private static let key = "liveactivity.lastWorkedAt"
    private static let maxEntries = 100

    static func record(_ statuses: [InstanceStatus], now: TimeInterval = Date().timeIntervalSince1970,
                       defaults: UserDefaults = .standard) {
        let activeIds = statuses.compactMap { s -> String? in
            guard s.running, s.status == .working || s.status == .blocked || s.status == .done else {
                return nil
            }
            return s.instanceId
        }
        guard !activeIds.isEmpty else { return }
        var map = load(defaults: defaults)
        for id in activeIds {
            map[id] = now
        }
        if map.count > maxEntries {
            let trimmed = map.sorted { $0.value > $1.value }.prefix(maxEntries)
            map = Dictionary(uniqueKeysWithValues: trimmed.map { ($0.key, $0.value) })
        }
        defaults.set(map, forKey: key)
    }

    static func timestamp(for instanceId: String, defaults: UserDefaults = .standard) -> TimeInterval? {
        load(defaults: defaults)[instanceId]
    }

    /// Best recency timestamp for an instance: the latest of our persisted
    /// last-worked timestamp, desktop's persisted `activity` timestamps (ms → s),
    /// and the live tmux pane's last activity (`now - row.idle`).
    static func effectiveLastActiveAt(
        instance: Instance?,
        status: AgentStatus,
        running: Bool,
        paneIdle: TimeInterval?,
        now: TimeInterval = Date().timeIntervalSince1970,
        defaults: UserDefaults = .standard
    ) -> TimeInterval? {
        var best: TimeInterval? = instance.flatMap { timestamp(for: $0.id, defaults: defaults) }
        if running, status == .working || status == .blocked || status == .done {
            best = max(best ?? 0, now)
        }
        if let act = instance?.activity {
            let desktopMs = [act.workStartedAt, act.completedAt, act.blockedAt].compactMap { $0 }.max()
            if let desktopMs {
                let secs = TimeInterval(desktopMs) / 1000.0
                best = max(best ?? 0, secs)
            }
        }
        if let paneIdle, paneIdle.isFinite {
            let tmuxAt = max(0, now - paneIdle)
            if best == nil {
                best = tmuxAt
            }
        }
        return best
    }

    private static func load(defaults: UserDefaults) -> [String: TimeInterval] {
        defaults.dictionary(forKey: key) as? [String: TimeInterval] ?? [:]
    }
}

/// Persists the last full summary so the next `.background` transition can start the
/// activity instantly, without waiting on a network scan.
enum SummaryCache {
    private static let key = "liveactivity.lastsummary"

    static func save(_ s: MuxelActivityAttributes.ContentState) {
        if let d = try? JSONEncoder().encode(s) {
            UserDefaults.standard.set(d, forKey: key)
        }
    }

    static func load() -> MuxelActivityAttributes.ContentState? {
        UserDefaults.standard.data(forKey: key)
            .flatMap { try? JSONDecoder().decode(MuxelActivityAttributes.ContentState.self, from: $0) }
    }
}
