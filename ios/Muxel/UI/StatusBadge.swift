import SwiftUI

extension AgentStatus {
    /// Themed status color — matches the brand mark's pane dots (green running,
    /// yellow working, red blocked, muted idle).
    func color(_ theme: MuxelTheme) -> Color {
        switch self {
        case .working: return theme.workingColor
        case .idle: return theme.mutedColor
        case .blocked: return theme.blockedColor
        case .done: return theme.runningColor
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
/// reflects liveness — green for an active session, red when it's blocked on input,
/// muted when no live tmux session exists — instead of the raw status color.
struct StatusDot: View {
    @Environment(\.theme) private var theme
    let status: AgentStatus
    var running: Bool?

    var body: some View {
        Circle()
            .fill(color)
            .frame(width: 9, height: 9)
            .shadow(color: color.opacity(alive ? 0.7 : 0), radius: alive ? 3 : 0)
            .accessibilityLabel(running == false ? "stopped" : status.label)
    }

    private var alive: Bool {
        if let running { return running }
        return status == .working || status == .done
    }

    private var color: Color {
        guard let running else { return status.color(theme) }
        if !running { return theme.mutedColor }             // no live session
        return status == .blocked ? theme.blockedColor : theme.runningColor
    }
}

/// A status pill (dot + text) for the tab bar.
struct StatusBadge: View {
    @Environment(\.theme) private var theme
    let status: AgentStatus
    var body: some View {
        HStack(spacing: 4) {
            StatusDot(status: status)
            Text(status.label).font(.mono(.caption2))
        }
        .padding(.horizontal, 6)
        .padding(.vertical, 2)
        .background(status.color(theme).opacity(0.14))
        .clipShape(Capsule())
    }
}
