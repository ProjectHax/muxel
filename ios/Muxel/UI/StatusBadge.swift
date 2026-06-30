import SwiftUI

extension AgentStatus {
    var color: Color {
        switch self {
        case .working: return .blue
        case .idle: return .secondary
        case .blocked: return .orange
        case .done: return .green
        }
    }

    var label: String {
        switch self {
        case .working: return "working"
        case .idle: return "idle"
        case .blocked: return "blocked"
        case .done: return "done"
        }
    }
}

/// A small colored status dot. When `running` is supplied (the pane tabs), the color
/// reflects liveness — green for an active session, orange when it's blocked on input,
/// gray when no live tmux session exists — instead of the raw status color.
struct StatusDot: View {
    let status: AgentStatus
    var running: Bool?

    var body: some View {
        Circle()
            .fill(color)
            .frame(width: 9, height: 9)
            .accessibilityLabel(running == false ? "stopped" : status.label)
    }

    private var color: Color {
        guard let running else { return status.color }
        if !running { return .secondary }      // no live session
        return status == .blocked ? .orange : .green   // active (orange flags needs-input)
    }
}

/// A status pill (dot + text) for the tab bar.
struct StatusBadge: View {
    let status: AgentStatus
    var body: some View {
        HStack(spacing: 4) {
            StatusDot(status: status)
            Text(status.label).font(.caption2)
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(status.color.opacity(0.12))
        .clipShape(Capsule())
    }
}
