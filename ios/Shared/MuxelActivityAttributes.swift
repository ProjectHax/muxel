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

    /// A pane/agent's coarse state, background-detectable from tmux signals.
    enum InstanceState: String, Codable, Hashable {
        case working    // recent activity
        case attention  // exited or rang the bell — "needs you"
        case idle       // live but quiet, or no live session
    }

    /// One row per agent instance (pane).
    struct InstanceRow: Codable, Hashable, Identifiable {
        var id: String       // instance id — stable across updates
        var name: String     // agent/pane display name (truncated)
        var project: String  // owning project name (truncated), for context
        var state: InstanceState

        var needsAttention: Bool { state == .attention }
    }

    /// The dynamic content, refreshed on every poll.
    struct ContentState: Codable, Hashable {
        var instances: [InstanceRow]
        var attentionCount: Int
        var workingCount: Int
        /// True total across all projects — may exceed `instances.count` when capped.
        var instanceCount: Int
        var updatedAt: Date

        /// Nothing to show at all → the activity ends. (Idle instances still show, so
        /// the status bar stays present while the app is minimized.)
        var isEmpty: Bool { instanceCount == 0 }
    }
}
