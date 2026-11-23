import SwiftUI

/// Read-only viewer for a desktop-created `.diff` pane: runs `git diff` over SSH and
/// renders it with +/- coloring. The dir is the instance's worktree (or the project
/// root). No staging in v1.
struct DiffPaneView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    let host: Host
    let project: RemoteProject
    let instance: Instance
    @State private var lines: [String] = []
    @State private var rawText = ""
    @State private var status: LoadStatus = .loading
    @State private var wrapLines = false

    enum LoadStatus: Equatable { case loading, loaded, empty, failed(String) }

    private var dir: String { instance.editorPath ?? instance.worktreePath ?? project.remoteRoot }

    var body: some View {
        VStack(spacing: 0) {
            RemoteFileHeader(
                title: "diff", subtitle: dir,
                wrapLines: $wrapLines,
                onCopy: rawText.isEmpty ? nil : { UIPasteboard.general.string = rawText },
                onRefresh: { Task { await load() } })
            Divider()
            content
        }
        .background(theme.terminalBackground)
        .task { await load() }
    }

    @ViewBuilder private var content: some View {
        switch status {
        case .loading:
            CenteredState(spinner: true, title: "reading changes…")
        case .empty:
            CenteredState(icon: "checkmark.circle", iconColor: theme.runningColor,
                          title: "no uncommitted changes", prompt: true)
        case let .failed(message):
            CenteredState(icon: "exclamationmark.triangle", iconColor: theme.blockedColor,
                          title: "couldn't read the diff", message: message) {
                Button("Try again") { Task { await load() } }.buttonStyle(.bordered)
            }
        case .loaded:
            MonoTextScroll(lines: lines, wrapLines: wrapLines,
                           showLineNumbers: false, color: colorForLine)
        }
    }

    private func colorForLine(_ i: Int) -> Color {
        switch diffLineKind(lines[i]) {
        case .add: return theme.runningColor
        case .remove: return theme.blockedColor
        case .hunk: return theme.accentColor
        case .meta: return theme.terminalMuted
        case .context: return theme.terminalForeground
        }
    }

    private func load() async {
        status = .loading
        do {
            let conn = state.connection(for: host)
            try await conn.connect()
            let text = try await RemoteFiles.diff(conn, dir: dir)
            rawText = text
            if text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                lines = []
                status = .empty
            } else {
                lines = text.components(separatedBy: "\n")
                status = .loaded
            }
        } catch {
            status = .failed(error.localizedDescription)
        }
    }
}
