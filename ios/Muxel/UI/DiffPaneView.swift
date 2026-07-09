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
    @State private var status: LoadStatus = .loading

    enum LoadStatus: Equatable { case loading, loaded, failed(String) }

    private var dir: String { instance.editorPath ?? instance.worktreePath ?? project.remoteRoot }

    var body: some View {
        VStack(spacing: 0) {
            RemoteFileHeader(title: "diff", subtitle: dir, onRefresh: { Task { await load() } })
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
        case let .failed(message):
            CenteredState(icon: "exclamationmark.triangle", iconColor: theme.blockedColor,
                          title: "couldn't read the diff", message: message) {
                Button("Try again") { Task { await load() } }.buttonStyle(.bordered)
            }
        case .loaded:
            MonoTextScroll(lines: lines, color: colorForLine)
        }
    }

    private func colorForLine(_ i: Int) -> Color {
        switch diffLineKind(lines[i]) {
        case .add: return theme.runningColor
        case .remove: return theme.blockedColor
        case .hunk: return theme.accentColor
        case .meta: return theme.mutedColor
        case .context: return theme.textColor
        }
    }

    private func load() async {
        status = .loading
        do {
            let conn = state.connection(for: host)
            try await conn.connect()
            let text = try await RemoteFiles.diff(conn, dir: dir)
            lines = text.components(separatedBy: "\n")
            status = .loaded
        } catch {
            status = .failed(error.localizedDescription)
        }
    }
}
