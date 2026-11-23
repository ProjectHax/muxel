import ActivityKit
import Foundation

/// The Live Activity payload, shared by the app (which starts/updates it) and the
/// `MuxelWidgets` extension (which renders it). This one file is compiled into both
/// targets; ActivityKit matches the app's `Activity<MuxelActivityAttributes>` to the
/// widget's `ActivityConfiguration(for: MuxelActivityAttributes.self)` by type name.
///
/// Keep it small — ActivityKit caps the encoded attributes + state at ~4 KB, so the
/// builder truncates names and caps the instance list (overflow is shown as a count).
struct MuxelActivityAttributes: ActivityAttributes {
    /// Static; set once when the activity starts.
    var startedAt: Date

    /// A pane/agent's coarse state, background-detectable from tmux signals. Ordered
    /// by attention priority — `needsInput` is surfaced first in the list.
    enum InstanceState: String, Codable, Hashable {
        case needsInput  // rang the bell / blocked — waiting for you
        case finished    // the agent's process exited
        case working     // recent activity
        case idle        // live but quiet, or no live session
    }

    /// One row per agent instance (pane).
    struct InstanceRow: Codable, Hashable, Identifiable {
        var id: String       // instance id — stable across updates
        var name: String     // agent/pane display name (truncated)
        var project: String  // owning project name (truncated), for context
        var state: InstanceState
        /// Unix timestamp (seconds) when this agent was last actively working /
        /// needing attention / producing activity, so recently-active agents and
        /// projects stay pinned to the top of the Live Activity.
        var lastActiveAt: TimeInterval? = nil

        var needsInput: Bool { state == .needsInput }
    }

    /// The dynamic content, refreshed on every poll.
    struct ContentState: Codable, Hashable {
        var instances: [InstanceRow]
        var needsInputCount: Int
        var finishedCount: Int
        var workingCount: Int
        /// True total across all projects — may exceed `instances.count` when capped.
        var instanceCount: Int
        var updatedAt: Date

        /// Nothing to show at all → the activity ends. (Idle instances still show, so
        /// the status bar stays present while the app is minimized.)
        var isEmpty: Bool { instanceCount == 0 }
    }

    /// One summary line per project for the Lock Screen / Dynamic Island Live Activity:
    /// multiple status dots (one per agent) plus the highest-priority attention/last
    /// status text (e.g. "claude: needs input", "codex working", "claude finished", "idle").
    struct ProjectGroup: Equatable, Identifiable {
        var id: String { project }
        var project: String
        var states: [InstanceState]
        var highlightState: InstanceState
        var summaryText: String
        var lastActiveAt: TimeInterval? = nil
    }

    /// Roll flat `InstanceRow`s into one line per project, ordered by urgency
    /// (`needsInput` → `working` → `finished` → `idle`), then most-recently-worked
    /// first (`lastActiveAt` descending), then project name.
    static func groupByProject(_ instances: [InstanceRow]) -> [ProjectGroup] {
        func rank(_ s: InstanceState) -> Int {
            switch s {
            case .needsInput: return 0
            case .working: return 1
            case .finished: return 2
            case .idle: return 3
            }
        }
        let grouped = Dictionary(grouping: instances, by: \.project)
        var out: [ProjectGroup] = []
        out.reserveCapacity(grouped.count)
        for (project, rawRows) in grouped {
            let rows = rawRows.sorted {
                if rank($0.state) != rank($1.state) { return rank($0.state) < rank($1.state) }
                let a = $0.lastActiveAt ?? 0
                let b = $1.lastActiveAt ?? 0
                if a != b { return a > b }
                return $0.name < $1.name
            }
            let states = rows.map(\.state)
            let needsInput = rows.filter { $0.state == .needsInput }
            let working = rows.filter { $0.state == .working }
            let finished = rows.filter { $0.state == .finished }
            let highlight: InstanceState
            let summary: String
            if !needsInput.isEmpty {
                highlight = .needsInput
                summary = needsInput.count == 1
                    ? "\(needsInput[0].name): needs input"
                    : "\(needsInput.count) need input"
            } else if !working.isEmpty {
                highlight = .working
                summary = working.count == 1
                    ? "\(working[0].name) working"
                    : "\(working.count) working"
            } else if !finished.isEmpty {
                highlight = .finished
                summary = finished.count == 1
                    ? "\(finished[0].name) finished"
                    : "\(finished.count) finished"
            } else {
                highlight = .idle
                if let top = rows.first, rows.count == 1 || top.lastActiveAt != nil {
                    summary = "\(top.name) idle"
                } else {
                    summary = "idle"
                }
            }
            out.append(ProjectGroup(
                project: project,
                states: states,
                highlightState: highlight,
                summaryText: summary,
                lastActiveAt: rows.compactMap(\.lastActiveAt).max()))
        }
        return out.sorted {
            if rank($0.highlightState) != rank($1.highlightState) {
                return rank($0.highlightState) < rank($1.highlightState)
            }
            let a = $0.lastActiveAt ?? 0
            let b = $1.lastActiveAt ?? 0
            if a != b { return a > b }
            return $0.project < $1.project
        }
    }
}
