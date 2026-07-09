import SwiftUI

/// The shared "no live terminal" state for a pane, built on `CenteredState`. One
/// component so every pane — the single detail terminal today and each split leaf
/// later — shows the same treatment: either the host is unreachable (with a retry),
/// or a live session dropped and we're reconnecting with backoff.
struct HostReachabilityState: View {
    enum Mode: Equatable {
        /// Couldn't reach the host (initial attach failed, or reconnect exhausted).
        case unreachable(message: String)
        /// A live session dropped; a backoff retry is in flight (`attempt` ≥ 1).
        case reconnecting(attempt: Int)
    }

    @Environment(\.theme) private var theme
    let hostName: String
    let mode: Mode
    /// "Try again" / "Try now" — a manual, immediate reconnect.
    let onRetry: () -> Void

    var body: some View {
        switch mode {
        case let .unreachable(message):
            CenteredState(icon: "wifi.exclamationmark",
                          iconColor: theme.blockedColor,
                          title: "can't reach \(hostName)",
                          message: message,
                          showsGrid: true) {
                Button(action: onRetry) {
                    Label("Try again", systemImage: "arrow.clockwise")
                        .font(.mono(.footnote, weight: .semibold))
                }
                .buttonStyle(.borderedProminent)
            }
        case let .reconnecting(attempt):
            CenteredState(spinner: true,
                          title: "reconnecting to \(hostName)…",
                          message: attempt > 1 ? "attempt \(attempt)" : nil,
                          showsGrid: true) {
                Button("Try now", action: onRetry)
                    .buttonStyle(.bordered)
            }
        }
    }
}
