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

/// A path + action header for the read-only viewers (the nav toolbar belongs to
/// `ProjectDetailView`, so each viewer carries its own). Sits over `terminalBackground`,
/// so labels use `terminalForeground`/`terminalMuted` (stays legible on light themes).
struct RemoteFileHeader: View {
    @Environment(\.theme) private var theme
    let title: String
    let subtitle: String?
    @Binding var wrapLines: Bool
    var onCopy: (() -> Void)? = nil
    let onRefresh: () -> Void
    @State private var copied = false

    var body: some View {
        HStack(spacing: 12) {
            VStack(alignment: .leading, spacing: 1) {
                Text(title)
                    .font(.mono(.footnote, weight: .semibold))
                    .foregroundStyle(theme.terminalForeground)
                    .lineLimit(1)
                if let subtitle {
                    Text(subtitle)
                        .font(.mono(.caption2))
                        .foregroundStyle(theme.terminalMuted)
                        .lineLimit(1)
                        .truncationMode(.head)
                }
            }
            Spacer()
            Button {
                wrapLines.toggle()
            } label: {
                Image(systemName: wrapLines ? "text.alignleft" : "arrow.left.and.right.text.vertical")
                    .foregroundStyle(wrapLines ? theme.accentColor : theme.terminalMuted)
            }
            .accessibilityLabel(wrapLines ? "Disable word wrap" : "Enable word wrap")
            if let onCopy {
                Button {
                    onCopy()
                    UISelectionFeedbackGenerator().selectionChanged()
                    copied = true
                    Task {
                        try? await Task.sleep(for: .seconds(1.5))
                        copied = false
                    }
                } label: {
                    Image(systemName: copied ? "checkmark" : "doc.on.doc")
                        .foregroundStyle(copied ? theme.runningColor : theme.terminalForeground)
                }
                .accessibilityLabel("Copy text")
            }
            Button(action: onRefresh) {
                Image(systemName: "arrow.clockwise")
                    .foregroundStyle(theme.accentColor)
            }
            .accessibilityLabel("Refresh")
        }
        .padding(.horizontal, 12)
        .padding(.vertical, 8)
    }
}

/// A read-only mono text view with line numbers, optional word wrap, and per-line
/// coloring (used for files + diffs). Lazy so large files stay smooth.
struct MonoTextScroll: View {
    @Environment(\.theme) private var theme
    let lines: [String]
    var wrapLines: Bool = false
    var showLineNumbers: Bool = true
    let color: (Int) -> Color

    private var gutterDigits: Int { max(2, String(lines.count).count) }

    var body: some View {
        ScrollView(wrapLines ? [.vertical] : [.vertical, .horizontal]) {
            LazyVStack(alignment: .leading, spacing: 1) {
                ForEach(Array(lines.enumerated()), id: \.offset) { i, line in
                    HStack(alignment: .top, spacing: 8) {
                        if showLineNumbers {
                            Text(String(format: "%\(gutterDigits)d", i + 1))
                                .font(.mono(.caption2))
                                .foregroundStyle(theme.terminalMuted.opacity(0.65))
                                .monospacedDigit()
                                .frame(minWidth: CGFloat(gutterDigits) * 8, alignment: .trailing)
                        }
                        Text(line.isEmpty ? " " : line)
                            .font(.mono(.caption))
                            .foregroundStyle(color(i))
                            .textSelection(.enabled)
                            .fixedSize(horizontal: !wrapLines, vertical: true)
                            .frame(maxWidth: .infinity, alignment: .leading)
                    }
                }
            }
            .padding(8)
        }
    }
}
