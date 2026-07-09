import SwiftUI

/// A pane's live terminal. Resolves the instance's tmux session and opens a real SSH
/// **PTY** to it via `LiveTerminalView` — `tmux attach` if the session already exists
/// (desktop- or previously-created), otherwise `tmux new-session -A` *attached* with
/// the instance's program. Attached-at-init is what lets interactive TUI agents
/// (claude) run; the old detached `capture-pane` poll crashed them.
///
/// A live session that drops (transport suspend, server idle-close) doesn't clear the
/// grid: the last output stays visible, dimmed, under a "reconnecting…" overlay while
/// a backoff retry runs. The drop is signalled by `AppState.deadPanes` (fed by the
/// session's `onConnectionLost`), so a transport-wide drop lights every mounted pane.
struct TerminalPaneView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.scenePhase) private var scenePhase
    @Environment(\.theme) private var theme
    let host: Host
    let project: RemoteProject
    let instance: Instance
    /// Called when the user taps into this terminal (split-view focus). nil in the
    /// single-pane compact layout, where there's nothing to focus between.
    var onFocus: (() -> Void)? = nil

    @State private var session: TerminalSession?
    @State private var status: Status = .attaching
    @State private var lastError: String?
    @State private var retryTask: Task<Void, Never>?

    enum Status: Equatable {
        case attaching
        case live
        case reconnecting(attempt: Int)
        case unreachable(String)
    }

    /// The terminal is dimmed (and non-interactive) whenever an overlay covers it but a
    /// prior session's frozen grid is still worth showing underneath.
    private var dimmed: Bool {
        switch status {
        case .reconnecting, .unreachable: return session != nil
        case .attaching, .live: return false
        }
    }

    var body: some View {
        ZStack {
            if let session {
                LiveTerminalView(session: session, theme: theme)
                    // Match the (always-dark) terminal grid so an attach/resize
                    // gap doesn't flash the light chrome background.
                    .background(theme.terminalBackground)
                    .opacity(dimmed ? 0.35 : 1)
                    .allowsHitTesting(!dimmed)
            }
            overlay
        }
        .task { await initialAttach() }
        .onChange(of: scenePhase) { phase in
            // iOS suspends in the background and the SSH transport can drop; when we
            // return, reconnect if this pane's session died.
            if phase == .active, let s = session, !s.isConnected { beginReconnect() }
        }
        .onChange(of: state.deadPanes.contains(instance.id)) { dead in
            if dead { beginReconnect() }
        }
        .onDisappear { retryTask?.cancel(); retryTask = nil }
    }

    @ViewBuilder private var overlay: some View {
        switch status {
        case .attaching:
            if session == nil {
                CenteredState(spinner: true, title: "attaching to \(host.name)…")
            }
        case .live:
            EmptyView()
        case let .reconnecting(attempt):
            HostReachabilityState(hostName: host.name,
                                  mode: .reconnecting(attempt: attempt),
                                  onRetry: retryNow)
        case let .unreachable(message):
            HostReachabilityState(hostName: host.name,
                                  mode: .unreachable(message: message),
                                  onRetry: retryNow)
        }
    }

    private func initialAttach() async {
        status = .attaching
        if await resolve(announce: true) {
            status = .live
        } else if session == nil {
            status = .unreachable(lastError ?? "Couldn't reach \(host.name).")
        }
    }

    /// Start (or continue) the backoff reconnect loop for a dropped live session.
    private func beginReconnect() {
        guard retryTask == nil else { return }
        status = .reconnecting(attempt: 1)
        retryTask = Task { await reconnectLoop() }
    }

    private func reconnectLoop() async {
        let delaysSecs: [UInt64] = [1, 2, 4, 8, 15]
        for (i, secs) in delaysSecs.enumerated() {
            if Task.isCancelled { retryTask = nil; return }
            status = .reconnecting(attempt: i + 1)
            try? await Task.sleep(nanoseconds: secs * 1_000_000_000)
            if Task.isCancelled { retryTask = nil; return }
            // Don't burn attempts while backgrounded; scenePhase→active re-triggers.
            if scenePhase != .active { retryTask = nil; return }
            if await resolve(announce: false) {
                state.clearDead(instance.id)
                status = .live
                retryTask = nil
                return
            }
        }
        // Exhausted — settle into unreachable; clearing the dead flag stops it from
        // immediately re-triggering the loop. The user can Try again from the overlay.
        state.clearDead(instance.id)
        status = .unreachable(lastError ?? "Couldn't reconnect to \(host.name).")
        retryTask = nil
    }

    private func retryNow() {
        retryTask?.cancel()
        retryTask = nil
        Task {
            status = .reconnecting(attempt: 1)
            if await resolve(announce: true) {
                state.clearDead(instance.id)
                status = .live
            } else {
                beginReconnect()
            }
        }
    }

    /// Reuse the instance's already-live terminal if the store still holds one
    /// (navigation keeps it connected); otherwise decide the PTY command — attach to a
    /// live tmux session (matched by uuid8 suffix), or create-and-attach one running
    /// the instance's program — and start a session in the store. Returns whether a
    /// live session is now in hand. `announce` surfaces the error (banner / host-key
    /// prompt) on the first/manual attempt but stays quiet during silent backoff.
    @discardableResult
    private func resolve(announce: Bool) async -> Bool {
        if let live = state.terminals.existing(instance.id) {
            live.onFocusRequested = onFocus
            session = live
            return true
        }
        do {
            let conn = state.connection(for: host)
            try await conn.connect()
            let names = try await listSessionNames(conn)
            let command: String
            let sessionName: String
            var creating = false
            var startupInput: String?
            var startupDelayMs = 0
            var startupSubmit = false
            var resumeSessionId: String?
            if let live = names.first(where: { TmuxSession.session($0, matchesInstanceId: instance.id) }) {
                // A live session for this instance already exists (created by desktop or
                // a prior attach) — attach to its real name, whatever slug it carries.
                sessionName = live
                command = TmuxCommands.attachPTYCommand(session: live)
            } else {
                // Creating it: prefer the authoritative name recorded in the shared
                // layout (`tmux_session`) so we converge with the name desktop uses for
                // this instance; fall back to deriving one from the host name.
                creating = true
                // Fork the server off a project-less command line first, or the
                // `new-session` below (the first client on a host with no server) hands
                // its own argv to the long-lived server — and a `pkill -f <project>` on
                // that host then kills every session on it. Idempotent and cheap; a
                // failure here is not fatal (tmux may already be running).
                _ = try? await conn.tmux(TmuxCommands.startServer())
                let name = (instance.tmuxSession?.isEmpty == false)
                    ? instance.tmuxSession!
                    : TmuxSession.name(hostName: host.name, instanceId: instance.id)
                sessionName = name

                // Resolve program/args like desktop: model/effort/extra are already in
                // `args`; apply the system-prompt injection + prepend session-resume args.
                var inst = instance
                let preset = AgentLaunch.builtinPreset(for: inst)
                let resumeCapable = preset?.sessionIdFlag != nil && preset?.resumeFlag != nil
                if resumeCapable, inst.sessionId == nil {
                    inst.sessionId = UUID().uuidString.lowercased()  // backfill (e.g. a duplicate)
                }
                let resolved = AgentLaunch.resolveLaunch(inst)
                let resumeArgs = preset.flatMap { AgentLaunch.sessionResumeArgs(preset: $0, instance: inst) } ?? []
                command = TmuxCommands.newAttachedPTYCommand(
                    session: name, cwd: inst.worktreePath ?? project.remoteRoot,
                    program: resolved.program, args: resumeArgs + resolved.args)
                startupInput = resolved.startupInput
                startupDelayMs = preset?.startupDelayMs ?? 0
                startupSubmit = resolved.submit
                if resumeCapable { resumeSessionId = inst.sessionId }
            }
            let s = state.terminals.session(
                for: instance.id, hostId: host.id, connection: conn, command: command)
            s.onFocusRequested = onFocus
            session = s

            if creating {
                // Persist that a resume-capable session has started (+ backfill its id).
                if let sid = resumeSessionId {
                    state.markSessionStarted(instance.id, sessionId: sid, in: project)
                }
                // TypeIn agents: type the system prompt once the agent is ready.
                if let input = startupInput {
                    state.scheduleStartupInjection(instanceId: instance.id, sessionName: sessionName,
                                                   host: host, text: input,
                                                   delayMs: startupDelayMs, submit: startupSubmit)
                }
            }
            // Enable tmux mouse mode so a touch drag scrolls the pane's scrollback
            // (copy mode), and OSC-52 forwarding so remote copies land on the phone
            // clipboard. Best-effort + separate from the attach so they can't break
            // it; a freshly-created session may not exist yet when this runs, but it
            // has no history to scroll anyway and picks it up on the next open.
            Task {
                _ = try? await conn.tmux(TmuxCommands.setMouseOn(session: sessionName))
                _ = try? await conn.tmux(TmuxCommands.setClipboardOn())
            }
            return true
        } catch {
            lastError = error.localizedDescription
            // A changed host key needs the trust-prompt sheet, not just inline text.
            if announce { state.surface(error, host: host) }
            return false
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
}
