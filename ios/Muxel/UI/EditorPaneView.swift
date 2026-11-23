import SwiftUI

/// Read-only viewer for a desktop-created `.editor` pane: fetches the remote file over
/// SSH and shows it in a mono scroll view. No editing/saving in v1.
struct EditorPaneView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    let host: Host
    let project: RemoteProject
    let instance: Instance
    @State private var lines: [String] = []
    @State private var rawText = ""
    @State private var status: LoadStatus = .loading
    @State private var wrapLines = false

    enum LoadStatus: Equatable { case loading, loaded, failed(String) }

    var body: some View {
        VStack(spacing: 0) {
            RemoteFileHeader(
                title: instance.editorPath.map { ($0 as NSString).lastPathComponent } ?? instance.displayName,
                subtitle: instance.editorPath,
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
            CenteredState(spinner: true, title: "reading…")
        case let .failed(message):
            CenteredState(icon: "doc.questionmark", iconColor: theme.blockedColor,
                          title: "couldn't read the file", message: message) {
                Button("Try again") { Task { await load() } }.buttonStyle(.bordered)
            }
        case .loaded:
            // Use terminalForeground (always legible on the dark terminalBackground,
            // including under light chrome themes like Catppuccin Latte).
            MonoTextScroll(lines: lines, wrapLines: wrapLines,
                           color: { _ in theme.terminalForeground })
        }
    }

    private func load() async {
        guard let path = instance.editorPath, !path.isEmpty else {
            status = .failed("untitled buffer — open it on desktop")
            return
        }
        status = .loading
        do {
            let conn = state.connection(for: host)
            try await conn.connect()
            let text = try await RemoteFiles.read(conn, path: path)
            rawText = text
            lines = text.isEmpty ? ["(empty or too large to show)"] : text.components(separatedBy: "\n")
            status = .loaded
        } catch {
            status = .failed(error.localizedDescription)
        }
    }
}
