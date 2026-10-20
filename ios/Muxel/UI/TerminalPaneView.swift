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
        case stopped
        case attaching
        case live
        case reconnecting(attempt: Int)
        case unreachable(String)
    }

    private struct AttachmentPlan {
        let command: String
        let sessionName: String
        let socket: String
        let creating: Bool
        let startupInput: String?
        let startupDelayMs: Int
        let startupSubmit: Bool
        let resumeSessionId: String?
    }

    private var currentInstance: Instance? {
        guard state.selectedProject?.id == project.id else { return nil }
        return state.layout?.instances.first { $0.id == instance.id }
    }

    /// A peer may replace the captured pane metadata while attachment is awaiting
    /// SSH. Literal commands stay stopped on the phone, including an old grid.
    @discardableResult
    private func stopIfLiteral() -> Bool {
        guard currentInstance?.literalArgv == true else { return false }
        retryTask?.cancel()
        retryTask = nil
        state.terminals.disconnect(instance.id)
        session = nil
        state.clearDead(instance.id)
        status = .stopped
        return true
    }

    /// The terminal is dimmed (and non-interactive) whenever an overlay covers it but a
    /// prior session's frozen grid is still worth showing underneath.
    private var dimmed: Bool {
        switch status {
        case .reconnecting, .unreachable: return session != nil
        case .stopped, .attaching, .live: return false
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
        .onChange(of: currentInstance?.literalArgv) { literal in
            if literal == true { stopIfLiteral() }
        }
        .onDisappear { retryTask?.cancel(); retryTask = nil }
    }

    @ViewBuilder private var overlay: some View {
        switch status {
        case .stopped:
            CenteredState(spinner: false, title: "Command stopped. Run again on desktop.")
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
        guard !stopIfLiteral() else { return }
        status = .attaching
        if await resolve(announce: true) {
            status = .live
        } else if status != .stopped, session == nil {
            status = .unreachable(lastError ?? "Couldn't reach \(host.name).")
        }
    }

    /// Start (or continue) the backoff reconnect loop for a dropped live session.
    private func beginReconnect() {
        guard !stopIfLiteral(), retryTask == nil else { return }
        status = .reconnecting(attempt: 1)
        retryTask = Task { await reconnectLoop() }
    }

    private func reconnectLoop() async {
        let delaysSecs: [UInt64] = [1, 2, 4, 8, 15]
        for (i, secs) in delaysSecs.enumerated() {
            if Task.isCancelled { retryTask = nil; return }
            if stopIfLiteral() { return }
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
            if status == .stopped { retryTask = nil; return }
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
        guard !stopIfLiteral() else { return }
        Task {
            guard !stopIfLiteral() else { return }
            status = .reconnecting(attempt: 1)
            if await resolve(announce: true) {
                state.clearDead(instance.id)
                status = .live
            } else if status != .stopped {
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
        guard !Task.isCancelled, state.selectedProject?.id == project.id,
              state.layout?.instances.contains(where: { $0.id == instance.id }) == true else { return false }
        guard !stopIfLiteral() else { return false }
        if let live = state.terminals.existing(instance.id) {
            live.onFocusRequested = onFocus
            session = live
            return true
        }
        do {
            let conn = state.connection(for: host)
            try await conn.connect()
            guard let plan = try await state.prepareTmuxAttachment(instanceId: instance.id, in: project, prepare: { current in
                let targets = TmuxCommands.parseTargets(try await conn.run(
                    TmuxCommands.listTargetsCommand(socket: current.tmuxSocket)))
                let command: String
                let sessionName: String
                let socket: String
                var creating = false
                var startupInput: String?
                var startupDelayMs = 0
                var startupSubmit = false
                var resumeSessionId: String?
                if let live = TmuxCommands.resolveTarget(current, in: targets) {
                    // A live session for this instance already exists (created by desktop or
                    // a prior attach) — attach to its real name, whatever slug it carries.
                    sessionName = live.name
                    socket = live.socket
                    command = TmuxCommands.attachPTYCommand(session: live.name, socket: socket)
                } else {
                    // Creating it: prefer the authoritative name recorded in the shared
                    // layout (`tmux_session`) so we converge with the name desktop uses for
                    // this instance; fall back to deriving one from the host name.
                    creating = true
                    socket = current.tmuxSocket ?? TmuxCommands.socketName
                    let name = (current.tmuxSession?.isEmpty == false)
                        ? current.tmuxSession!
                        : TmuxSession.name(hostName: host.name, instanceId: current.id)
                    sessionName = name

                    // Resolve program/args like desktop: model/effort/extra are already in
                    // `args`; apply the system-prompt injection + prepend session-resume args.
                    var inst = current
                    let preset = AgentLaunch.builtinPreset(for: inst)
                    let resumeCapable = preset?.sessionIdFlag != nil && preset?.resumeFlag != nil
                    if resumeCapable, inst.sessionId == nil {
                        inst.sessionId = UUID().uuidString.lowercased()  // backfill (e.g. a duplicate)
                    }
                    let resolved = AgentLaunch.resolveLaunch(inst)
                    let resumeArgs = preset.flatMap { AgentLaunch.sessionResumeArgs(preset: $0, instance: inst) } ?? []
                    command = TmuxCommands.newAttachedPTYCommand(
                        session: name, cwd: inst.launchDir ?? inst.worktreePath ?? project.remoteRoot,
                        program: resolved.program, args: resumeArgs + resolved.args, socket: socket)
                    startupInput = resolved.startupInput
                    startupDelayMs = preset?.startupDelayMs ?? 0
                    startupSubmit = resolved.submit
                    if resumeCapable { resumeSessionId = inst.sessionId }
                }
                return (target: .init(name: sessionName, socket: socket),
                        attachment: AttachmentPlan(command: command, sessionName: sessionName, socket: socket,
                                                   creating: creating, startupInput: startupInput,
                                                   startupDelayMs: startupDelayMs, startupSubmit: startupSubmit,
                                                   resumeSessionId: resumeSessionId))
            }) else { stopIfLiteral(); return false }
            guard !Task.isCancelled, state.selectedProject?.id == project.id,
                  let accepted = state.layout?.instances.first(where: { $0.id == instance.id }),
                  !accepted.literalArgv, accepted.tmuxSocket == plan.socket,
                  accepted.tmuxSession == plan.sessionName else { stopIfLiteral(); return false }
            // Prewarm only the accepted owner, from a project-less command line,
            // so the server does not retain a project's argv. Best-effort: another
            // client may already have started it.
            if plan.creating, plan.socket != "default" {
                _ = try? await conn.tmux(TmuxCommands.startServer(), session: "", socket: plan.socket)
            }
            guard !Task.isCancelled, state.selectedProject?.id == project.id,
                  let current = state.layout?.instances.first(where: { $0.id == instance.id }),
                  !current.literalArgv, current.tmuxSocket == plan.socket,
                  current.tmuxSession == plan.sessionName else { stopIfLiteral(); return false }
            let s = state.terminals.session(
                for: instance.id, hostId: host.id, connection: conn, command: plan.command)
            s.onFocusRequested = onFocus
            session = s

            if plan.creating {
                // Persist that a resume-capable session has started (+ backfill its id).
                if let sid = plan.resumeSessionId {
                    state.markSessionStarted(instance.id, sessionId: sid, in: project)
                }
                // TypeIn agents: type the system prompt once the agent is ready.
                if let input = plan.startupInput {
                    state.scheduleStartupInjection(instanceId: instance.id, sessionName: plan.sessionName, socket: plan.socket,
                                                   host: host, text: input,
                                                   delayMs: plan.startupDelayMs, submit: plan.startupSubmit)
                }
            }
            // Enable tmux mouse mode so a touch drag scrolls the pane's scrollback
            // (copy mode), and OSC-52 forwarding so remote copies land on the phone
            // clipboard. Best-effort + separate from the attach so they can't break
            // it; a freshly-created session may not exist yet when this runs, but it
            // has no history to scroll anyway and picks it up on the next open.
            Task {
                _ = try? await conn.tmux(TmuxCommands.setMouseOn(session: plan.sessionName), session: plan.sessionName, socket: plan.socket)
                _ = try? await conn.tmux(TmuxCommands.setClipboardOn(), session: plan.sessionName, socket: plan.socket)
            }
            return true
        } catch {
            guard !Task.isCancelled, state.selectedProject?.id == project.id else { return false }
            guard !stopIfLiteral() else { return false }
            lastError = error.localizedDescription
            // A changed host key needs the trust-prompt sheet, not just inline text.
            if announce { state.surface(error, host: host) }
            return false
        }
    }

}
