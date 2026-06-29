import Foundation

/// The polled status of one instance.
struct InstanceStatus: Equatable {
    let instanceId: String
    let status: AgentStatus
    /// Whether a live tmux session for this instance exists right now.
    let running: Bool
}

/// Computes per-instance `AgentStatus` over an SSH connection without an attached
/// PTY — the engine behind status badges and background notifications.
///
/// Per muxel session it reads four tmux signals (capture-pane text, `pane_dead`,
/// `window_bell_flag`, `window_activity`) and feeds them into the ported
/// `classify` + a per-instance `PaneStatusTracker` (so the done-latch persists
/// across polls). Markers come from `defaultMarkers(instance.program)`.
final class PollService {
    private var trackers: [String: PaneStatusTracker] = [:]

    /// Poll every instance once; returns its current status. Instances with no live
    /// session are reported `running: false` (status carried from the last poll, or
    /// `.idle`). Tracker state is retained between calls.
    func poll(_ conn: SSHConnection, instances: [Instance]) async -> [InstanceStatus] {
        let sessions = (try? await conn.run(TmuxCommands.commandLine(TmuxCommands.listSessions())))?
            .split(separator: "\n").map(String.init) ?? []

        var out: [InstanceStatus] = []
        for inst in instances where inst.kind == .terminal {
            guard let session = sessions.first(where: {
                TmuxSession.session($0, matchesInstanceId: inst.id)
            }) else {
                out.append(InstanceStatus(instanceId: inst.id, status: .idle, running: false))
                continue
            }

            async let screenTask = conn.run(TmuxCommands.commandLine(TmuxCommands.capturePane(session: session)))
            async let metaTask = conn.run(TmuxCommands.commandLine(TmuxCommands.paneStatus(session: session)))
            let screen = (try? await screenTask) ?? ""
            let meta = (try? await metaTask) ?? ""

            let (exited, bell, idle) = Self.parseMeta(meta)
            let markers = defaultMarkers(program: inst.program)
            var tracker = trackers[inst.id] ?? PaneStatusTracker()
            let status = tracker.update(
                exited: exited, screen: screen,
                working: markers.working, blocked: markers.blocked,
                bell: bell, idle: idle
            )
            trackers[inst.id] = tracker
            out.append(InstanceStatus(instanceId: inst.id, status: status, running: true))
        }
        return out
    }

    /// Mark an instance attended (the user viewed it): drop its done latch and clear
    /// the tmux bell flag so it doesn't re-fire.
    func attend(_ instanceId: String) {
        trackers[instanceId]?.attend()
    }

    /// Parse `display-message` output: `pane_dead<TAB>window_bell_flag<TAB>window_activity`.
    static func parseMeta(_ s: String) -> (exited: Bool, bell: Bool, idle: TimeInterval) {
        let parts = s.trimmingCharacters(in: .whitespacesAndNewlines).split(separator: "\t", omittingEmptySubsequences: false)
        let dead = parts.count > 0 && parts[0] == "1"
        let bell = parts.count > 1 && parts[1] == "1"
        var idle: TimeInterval = 0
        if parts.count > 2, let activity = TimeInterval(parts[2]) {
            idle = max(0, Date().timeIntervalSince1970 - activity)
        }
        return (dead, bell, idle)
    }
}
