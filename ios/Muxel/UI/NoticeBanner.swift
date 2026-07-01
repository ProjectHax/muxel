import SwiftUI

/// A transient, self-dismissing notice shown as a themed banner at the top of the
/// window — the replacement for the old blocking "Something went wrong" alert.
struct AppNotice: Identifiable, Equatable {
    enum Style: Equatable {
        case error, success, info
    }

    let id = UUID()
    let style: Style
    let text: String
    var duration: TimeInterval = 4
}

/// Themed toast for `AppState.notice`: a style-tinted prompt caret, mono text, on a
/// pane-card surface. Tap to dismiss; RootView auto-dismisses after `duration`.
struct NoticeBanner: View {
    @Environment(\.theme) private var theme
    let notice: AppNotice
    let dismiss: () -> Void

    var body: some View {
        HStack(alignment: .firstTextBaseline, spacing: 8) {
            Text("❯")
                .font(.mono(.footnote, weight: .bold))
                .foregroundStyle(caretColor)
            Text(notice.text)
                .font(.mono(.footnote))
                .foregroundStyle(theme.textColor)
                .multilineTextAlignment(.leading)
                .frame(maxWidth: .infinity, alignment: .leading)
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
        .paneCard(theme, radius: 10)
        .contentShape(Rectangle())
        .onTapGesture(perform: dismiss)
        .accessibilityAddTraits(.isButton)
        .accessibilityLabel(notice.text)
        .accessibilityHint("Dismisses the notice")
    }

    private var caretColor: Color {
        switch notice.style {
        case .error: return theme.blockedColor
        case .success: return theme.runningColor
        case .info: return theme.accentColor
        }
    }
}
