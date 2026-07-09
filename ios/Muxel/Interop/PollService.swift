import Foundation

/// The polled status of one instance.
struct InstanceStatus: Equatable {
    let instanceId: String
    let status: AgentStatus
    /// Whether a live tmux session for this instance exists right now.
    let running: Bool
}

/// One pane's raw tmux status signals, keyed by its session name. Parsed from a
/// batched `list-panes -a` sweep (all sessions on a host in one round trip).
struct PaneRow: Equatable {
    let session: String
    let exited: Bool
    let bell: Bool
    let idle: TimeInterval
}

/// A snapshot of an attached pane's live screen, so `classify` can scan it for the
/// agent's working/blocked markers — the on-device analogue of desktop reading its
/// live PTY grid. Only available while a pane is attached (a `TerminalSession` is
/// live in the store); background polling never has one.
struct LiveScreen: Equatable {
    let text: String
    let idle: TimeInterval
    let bell: Bool
}

/// Computes per-instance `AgentStatus` over an SSH connection without an attached
/// PTY — the engine behind status badges and background notifications.
///
/// Per muxel session it reads three tmux signals via one `display-message`
/// (`pane_dead`, `window_bell_flag`, `window_activity`) and feeds them into the
/// ported `classify` + a per-instance `PaneStatusTracker` (so the latch persists
/// across polls).
///
/// It intentionally does **not** scrape the screen with `capture-pane -p`: some
/// tmux builds crash the entire server on that command (a stock AlmaLinux/RHEL 10
/// `tmux 3.3a-…el10` build does, killing every session), and desktop muxel never
/// uses it — desktop reads markers from its live PTY grid. The cost here is
/// marker-based `working`/`blocked` detection in the background: with no screen
/// text, status degrades to `done` (exit/bell) and `working`/`idle` (recent
/// activity), and marker-less panes never latch. The attached `LiveTerminalView`
/// still shows full agent state when a pane is open.
final class PollService {
    private var trackers: [String: PaneStatusTracker] = [:]

    /// Poll every instance once; returns its current status. Instances with no live
    /// session are reported `running: false` (status carried from the last poll, or
    /// `.idle`). Tracker state is retained between calls.
    ///
    /// One batched `list-panes -a` fetches every session's signals on the host in a
    /// single round trip (was `list-sessions` + N per-session reads). Because the
    /// sweep is host-global, the same fetched `rows` can classify any project on the
    /// host — the cross-project sidebar sweep reuses `classify(rows:instances:)`.
    func poll(_ conn: SSHConnection, instances: [Instance],
              liveScreens: [String: LiveScreen] = [:]) async -> [InstanceStatus] {
        let rows = await fetchPaneRows(conn)
        return classify(rows: rows, instances: instances, liveScreens: liveScreens)
    }

    /// Fetch every pane's status signals on the host in one round trip. A host with
    /// no running tmux server exits non-zero → empty (no sessions, all idle).
    func fetchPaneRows(_ conn: SSHConnection) async -> [PaneRow] {
        let raw = (try? await conn.run(TmuxCommands.commandLine(TmuxCommands.allPaneStatuses()))) ?? ""
        return Self.parseAllPanes(raw)
    }

    /// Pure classification: map already-fetched `rows` onto `instances`. Retains
    /// per-instance tracker state between calls, so the done-latch persists. No I/O —
    /// unit-testable by passing `PaneRow`s (and optional `liveScreens`) directly.
    ///
    /// When a `LiveScreen` is supplied for an instance whose program has markers, we
    /// run the **desktop-faithful** `classify` over the real grid text (working/blocked
    /// become trustworthy, and the done-latch is enabled). Otherwise we fall back to
    /// the tmux-vars-only path: no screen, so marker-based state is unavailable and the
    /// bell doubles as a "needs input" signal.
    func classify(rows: [PaneRow], instances: [Instance],
                  liveScreens: [String: LiveScreen] = [:]) -> [InstanceStatus] {
        var out: [InstanceStatus] = []
        for inst in instances where inst.kind == .terminal {
            guard let row = rows.first(where: {
                TmuxSession.session($0.session, matchesInstanceId: inst.id)
            }) else {
                out.append(InstanceStatus(instanceId: inst.id, status: .idle, running: false))
                continue
            }

            let (working, blocked) = defaultMarkers(program: inst.program)
            if let screen = liveScreens[inst.id], !(working.isEmpty && blocked.isEmpty) {
                // Attached pane WITH markers → faithful classify over the live grid.
                // Markers make `.working` real and enable the latch, so no post-hoc
                // downgrade/bell override (unlike the marker-less path below).
                var tracker = trackers[inst.id] ?? PaneStatusTracker()
                let status = tracker.update(
                    exited: row.exited, screen: screen.text,
                    working: working, blocked: blocked,
                    bell: row.bell || screen.bell,
                    idle: min(row.idle, screen.idle)
                )
                trackers[inst.id] = tracker
                out.append(InstanceStatus(instanceId: inst.id, status: status, running: true))
                continue
            }

            // No screen scrape (see the type doc) — pass an empty screen + no markers.
            // classify then falls back to exit/bell + activity, and the tracker
            // can't latch (canLatch == !working.isEmpty), which is the intended
            // marker-less behaviour.
            var tracker = trackers[inst.id] ?? PaneStatusTracker()
            let status = tracker.update(
                exited: row.exited, screen: "",
                working: [], blocked: [],
                bell: row.bell, idle: row.idle
            )
            trackers[inst.id] = tracker
            // With no screen markers on iOS, classify's only route to `.working` is its
            // activity fallback (output within ~2s). TUI agents (e.g. Claude) constantly
            // redraw their input line even while idle at the prompt, so that fallback
            // fires forever — a false "working". We can't distinguish an idle redraw
            // from real output without scraping the screen, so don't infer working;
            // report idle instead. (Desktop uses live-grid markers for this.)
            var effective: AgentStatus = status == .working ? .idle : status
            // A bell on a still-running pane is our best "needs input" signal (waiting
            // for you) — surfaced as `.blocked`, distinct from a clean exit (`.done` =
            // finished). `classify` stays a faithful port; this is an iOS poll-layer read.
            if row.bell && !row.exited { effective = .blocked }
            out.append(InstanceStatus(instanceId: inst.id, status: effective, running: true))
        }
        return out
    }

    /// Mark an instance attended (the user viewed it): drop its done latch and clear
    /// the tmux bell flag so it doesn't re-fire.
    func attend(_ instanceId: String) {
        trackers[instanceId]?.attend()
    }

    /// Parse a batched `list-panes -a` sweep: one line per pane, each
    /// `session_name<TAB>pane_dead<TAB>window_bell_flag<TAB>window_activity`. muxel
    /// sessions are single-pane, but if a session ever reports multiple panes the
    /// first row wins (dedupe by session name). Blank/short lines are skipped.
    static func parseAllPanes(_ s: String) -> [PaneRow] {
        var rows: [PaneRow] = []
        var seen = Set<String>()
        let now = Date().timeIntervalSince1970
        for line in s.split(separator: "\n", omittingEmptySubsequences: true) {
            let parts = line.split(separator: "\t", omittingEmptySubsequences: false)
            guard let first = parts.first else { continue }
            let session = String(first)
            guard !session.isEmpty, seen.insert(session).inserted else { continue }
            let dead = parts.count > 1 && parts[1] == "1"
            let bell = parts.count > 2 && parts[2] == "1"
            var idle: TimeInterval = 0
            if parts.count > 3, let activity = TimeInterval(parts[3]) {
                idle = max(0, now - activity)
            }
            rows.append(PaneRow(session: session, exited: dead, bell: bell, idle: idle))
        }
        return rows
    }

    /// Parse `display-message` output: `pane_dead<TAB>window_bell_flag<TAB>window_activity`.
    static func parseMeta(_ s: String) -> (exited: Bool, bell: Bool, idle: TimeInterval) {
        let parts = s.trimmingCharacters(in: .whitespacesAndNewlines).split(separator: "\t", omittingEmptySubsequences: false)
        let dead = parts.count > 0 && parts[0] == "1"
        let bell = parts.count > 1 && parts[1] == "1"
        var idle: TimeInterval = 0
        if parts.count > 2, let activity = TimeInterval(parts[2]) {
            idle = max(0, Date().timeIntervalSince1970 - activity)
        }
        return (dead, bell, idle)
    }
}
