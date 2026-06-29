import SwiftUI

/// v1 terminal: a polled `capture-pane` viewer plus a `send-keys` input bar — it
/// needs only the SSH `run` capability (no PTY, no SwiftTerm), so it works the
/// moment Citadel's exec channel does. Output refreshes on a short timer rather
/// than streaming live.
///
/// Post-MVP enhancement: a true live terminal via SwiftTerm fed by a Citadel PTY
/// channel (smooth real-time rendering + scrollback). That's the spike-gated path
/// the plan deferred; this view is its functional stand-in.
struct TerminalPaneView: View {
    @EnvironmentObject var state: AppState
    let host: Host
    let project: RemoteProject
    let instance: Instance

    @State private var screen = ""
    @State private var input = ""
    @State private var refresh: Task<Void, Never>?

    private var session: String { TmuxSession.name(hostName: host.name, instanceId: instance.id) }

    var body: some View {
        VStack(spacing: 0) {
            ScrollView {
                Text(screen.isEmpty ? "Connecting…" : screen)
                    .font(.system(.footnote, design: .monospaced))
                    .frame(maxWidth: .infinity, alignment: .leading)
                    .textSelection(.enabled)
                    .padding(8)
            }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .background(Color(.systemBackground))

            Divider()
            keyAccessory
            Divider()
            inputBar
        }
        .onAppear { start() }
        .onDisappear { refresh?.cancel() }
    }

    // MARK: Polling

    private func start() {
        refresh?.cancel()
        refresh = Task {
            while !Task.isCancelled {
                await capture()
                try? await Task.sleep(nanoseconds: 1_500_000_000)
            }
        }
    }

    private func capture() async {
        let conn = state.connection(for: host)
        if let text = try? await conn.tmux(TmuxCommands.capturePane(session: session)) {
            screen = text
        }
    }

    private func send(_ args: [String]) {
        Task {
            _ = try? await state.connection(for: host).tmux(args)
            await capture()
        }
    }

    // MARK: Input

    private var inputBar: some View {
        HStack(spacing: 8) {
            TextField("Type, then send…", text: $input)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .submitLabel(.send)
                .onSubmit(submit)
                .textFieldStyle(.roundedBorder)
            Button(action: submit) {
                Image(systemName: "paperplane.fill")
            }
            .disabled(input.isEmpty)
        }
        .padding(8)
    }

    private func submit() {
        guard !input.isEmpty else { return }
        send(TmuxCommands.sendLiteral(session: session, text: input))
        send(TmuxCommands.sendKey(session: session, key: "Enter"))
        input = ""
    }

    private var keyAccessory: some View {
        ScrollView(.horizontal, showsIndicators: false) {
            HStack(spacing: 8) {
                key("esc", "Escape")
                key("tab", "Tab")
                key("⏎", "Enter")
                key("^C", "C-c")
                key("↑", "Up")
                key("↓", "Down")
                key("←", "Left")
                key("→", "Right")
            }
            .padding(.horizontal, 8)
            .padding(.vertical, 6)
        }
    }

    private func key(_ label: String, _ name: String) -> some View {
        Button(label) { send(TmuxCommands.sendKey(session: session, key: name)) }
            .font(.system(.footnote, design: .monospaced))
            .buttonStyle(.bordered)
    }
}
