import SwiftUI

/// Dispatches a pane to its viewer by kind. Terminals get the live PTY; editor/diff
/// get read-only viewers; browser/unknown panes are desktop-only placeholders. Used by
/// both the compact single-pane layout and each split leaf.
struct PaneContentView: View {
    @Environment(\.theme) private var theme
    let host: Host
    let project: RemoteProject
    let instance: Instance
    var onFocus: (() -> Void)? = nil

    var body: some View {
        switch instance.kind {
        case .terminal:
            TerminalPaneView(host: host, project: project, instance: instance, onFocus: onFocus)
        case .editor:
            EditorPaneView(host: host, project: project, instance: instance)
        case .diff:
            DiffPaneView(host: host, project: project, instance: instance)
        case .browser, .other:
            CenteredState(icon: "globe",
                          title: instance.displayName,
                          message: "This \(instance.kind.rawValue.lowercased()) pane is shown on desktop only.",
                          showsGrid: true)
        }
    }
}

/// A path + refresh header for the read-only viewers (the nav toolbar belongs to
/// `ProjectDetailView`, so each viewer carries its own).
struct RemoteFileHeader: View {
    @Environment(\.theme) private var theme
    let title: String
    let subtitle: String?
    let onRefresh: () -> Void

    var body: some View {
        HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                    .font(.mono(.footnote, weight: .semibold))
                    .foregroundStyle(theme.textColor)
                if let subtitle {
                    Text(subtitle)
                        .font(.mono(.caption2))
                        .foregroundStyle(theme.mutedColor)
                        .lineLimit(1)
                        .truncationMode(.head)
                }
            }
            Spacer()
            Button(action: onRefresh) { Image(systemName: "arrow.clockwise") }
                .tint(theme.accentColor)
                .accessibilityLabel("Refresh")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

/// A read-only mono text view with per-line coloring (used for files + diffs). Lazy so
/// large files stay smooth; scrolls both axes so long lines aren't wrapped.
struct MonoTextScroll: View {
    let lines: [String]
    let color: (Int) -> Color

    var body: some View {
        ScrollView([.vertical, .horizontal]) {
            LazyVStack(alignment: .leading, spacing: 0) {
                ForEach(Array(lines.enumerated()), id: \.offset) { i, line in
                    Text(line.isEmpty ? " " : line)
                        .font(.mono(.caption))
                        .foregroundStyle(color(i))
                        .textSelection(.enabled)
                        .frame(maxWidth: .infinity, alignment: .leading)
                }
            }
            .padding(8)
        }
    }
}
