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
        id: String, name: String, project: String, status: AgentStatus, running: Bool
    ) -> MuxelActivityAttributes.InstanceRow {
        .init(
            id: id, name: String(name.prefix(nameCap)),
            project: String(project.prefix(nameCap)),
            state: state(status: status, running: running))
    }

    /// Fold instance rows into a `ContentState`: counts, and an attention-priority sort
    /// (needsInput → finished → working → idle, then project, then name) that puts
    /// agents waiting for you at the very top. Caps the list (overflow kept in
    /// `instanceCount`).
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
