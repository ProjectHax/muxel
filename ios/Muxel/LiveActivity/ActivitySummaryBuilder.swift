import Foundation

/// Pure aggregation from poll results into the Live Activity payload — the testable
/// core of the status bar. No ActivityKit / UI here.
enum ActivitySummaryBuilder {
    static let nameCap = 20
    static let rowCap = 16 // payload-size guard (ActivityKit ~4 KB)

    /// Coarse per-instance state from its poll signals:
    /// - `.done` (agent exited or rang the bell) → **attention**
    /// - live and working (recent activity) → **working**
    /// - otherwise (live but quiet, or no live session) → **idle**
    static func state(status: AgentStatus, running: Bool) -> MuxelActivityAttributes.InstanceState {
        if status == .done { return .attention }
        if running && status == .working { return .working }
        return .idle
    }

    static func row(
        id: String, name: String, project: String, status: AgentStatus, running: Bool
    ) -> MuxelActivityAttributes.InstanceRow {
        .init(
            id: id, name: String(name.prefix(nameCap)),
            project: String(project.prefix(nameCap)),
            state: state(status: status, running: running))
    }

    /// Fold instance rows into a `ContentState`: counts, attention→working→idle sort
    /// (then project, then name), and cap the list (overflow kept in `instanceCount`).
    static func contentState(
        rows: [MuxelActivityAttributes.InstanceRow], now: Date
    ) -> MuxelActivityAttributes.ContentState {
        func rank(_ s: MuxelActivityAttributes.InstanceState) -> Int {
            switch s {
            case .attention: return 0
            case .working: return 1
            case .idle: return 2
            }
        }
        let attention = rows.filter { $0.state == .attention }.count
        let working = rows.filter { $0.state == .working }.count
        let sorted = rows.sorted {
            if rank($0.state) != rank($1.state) { return rank($0.state) < rank($1.state) }
            if $0.project != $1.project { return $0.project < $1.project }
            return $0.name < $1.name
        }
        return .init(
            instances: Array(sorted.prefix(rowCap)),
            attentionCount: attention, workingCount: working,
            instanceCount: rows.count, updatedAt: now)
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
