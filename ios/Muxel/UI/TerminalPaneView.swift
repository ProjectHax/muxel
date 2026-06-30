import SwiftUI

/// A pane's live terminal. Resolves the instance's tmux session and opens a real SSH
/// **PTY** to it via `LiveTerminalView` — `tmux attach` if the session already exists
/// (desktop- or previously-created), otherwise `tmux new-session -A` *attached* with
/// the instance's program. Attached-at-init is what lets interactive TUI agents
/// (claude) run; the old detached `capture-pane` poll crashed them.
struct TerminalPaneView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.scenePhase) private var scenePhase
    let host: Host
    let project: RemoteProject
    let instance: Instance

    @State private var session: TerminalSession?
    @State private var error: String?

    var body: some View {
        Group {
            if let session {
                LiveTerminalView(session: session)
                    .background(Color.black)
                    .ignoresSafeArea(.container, edges: .bottom)
            } else if let error {
                errorState(error)
            } else {
                centered {
                    ProgressView()
                    Text("Attaching to \(host.name)…").foregroundStyle(.secondary)
                }
            }
        }
        .task { await resolve() }
        .onChange(of: scenePhase) { phase in
            // iOS suspends in the background and the SSH transport can drop; when we
            // return, re-attach if this pane's session died (resolve() recycles a dead
            // session and reconnects). A still-connected session is left untouched.
            if phase == .active, let s = session, !s.isConnected {
                Task { await resolve() }
            }
        }
    }

    /// Reuse the instance's already-live terminal if the store still holds one
    /// (navigation keeps it connected); otherwise decide the PTY command — attach to a
    /// live tmux session (matched by uuid8 suffix), or create-and-attach one running
    /// the instance's program — and start a session in the store.
    private func resolve() async {
        error = nil
        if let live = state.terminals.existing(instance.id) {
            session = live
            return
        }
        do {
            let conn = state.connection(for: host)
            try await conn.connect()
            let names = try await listSessionNames(conn)
            let command: String
            if let live = names.first(where: { TmuxSession.session($0, matchesInstanceId: instance.id) }) {
                // A live session for this instance already exists (created by desktop or
                // a prior attach) — attach to its real name, whatever slug it carries.
                // The attach detaches any other client (see `attachPTYCommand`), so the
                // phone is the sole client and the window fits it.
                command = TmuxCommands.attachPTYCommand(session: live)
            } else {
                // Creating it: prefer the authoritative name recorded in the shared
                // layout (`tmux_session`) so we converge with the name desktop uses for
                // this instance; fall back to deriving one from the host name.
                let name: String
                if let recorded = instance.tmuxSession, !recorded.isEmpty {
                    name = recorded
                } else {
                    name = TmuxSession.name(hostName: host.name, instanceId: instance.id)
                }
                command = TmuxCommands.newAttachedPTYCommand(
                    session: name, cwd: project.remoteRoot,
                    program: instance.program, args: instance.args)
            }
            session = state.terminals.session(
                for: instance.id, hostId: host.id, connection: conn, command: command)
        } catch {
            self.error = error.localizedDescription
        }
    }

    /// `tmux list-sessions` exits non-zero with "no server running" before any session
    /// has been created — that's an empty list, not a connection error. Real SSH /
    /// auth / transport errors still propagate so the pane can surface them.
    private func listSessionNames(_ conn: SSHConnection) async throws -> [String] {
        do {
            let out = try await conn.run(TmuxCommands.commandLine(TmuxCommands.listSessions()))
            return out.split(separator: "\n").map(String.init)
        } catch SSHError.command(let msg) where msg.contains("no server running") {
            return []
        }
    }

    private func errorState(_ message: String) -> some View {
        centered {
            Image(systemName: "wifi.exclamationmark").font(.largeTitle).foregroundStyle(.orange)
            Text("Can't reach \(host.name)").font(.headline)
            Text(message).font(.subheadline).foregroundStyle(.secondary).multilineTextAlignment(.center)
            Button("Try again") { Task { await resolve() } }.buttonStyle(.bordered)
        }
    }

    private func centered<C: View>(@ViewBuilder _ content: () -> C) -> some View {
        VStack(spacing: 10) { content() }
            .frame(maxWidth: .infinity, maxHeight: .infinity)
            .padding()
    }
}
