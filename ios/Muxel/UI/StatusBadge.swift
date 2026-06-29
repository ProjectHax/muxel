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

/// A small colored status dot.
struct StatusDot: View {
    let status: AgentStatus
    var body: some View {
        Circle()
            .fill(status.color)
            .frame(width: 9, height: 9)
            .accessibilityLabel(status.label)
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
