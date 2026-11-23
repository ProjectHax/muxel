import Foundation
import OSLog

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
    var socket: String? = nil
    /// Foreground process name reported by tmux (`#{pane_current_command}`), e.g.
    /// `zsh` when idle at a shell prompt or `cargo`/`sleep` while a command runs.
    var currentCommand: String? = nil
    /// Kernel job-control check on the pane's leader PID (`pgrp != tpgid && tpgid > 0`):
    /// `true` while ANY foreground command (binary, script, subshell, pipeline) owns the
    /// terminal, `false` when the shell itself owns the terminal at its prompt, `nil` if
    /// unavailable (e.g. unit-test mocks or `ls` fallback).
    var foregroundBusy: Bool? = nil
    /// Raw Unix epoch seconds of tmux's `window_activity` (used to detect fresh output
    /// bursts started on desktop without re-probing quiet idle panes).
    var activityTimestamp: TimeInterval? = nil
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

/// When a host's login-shell discovery fallback (`PollService.fetchPaneRows`) may
/// run again. A run that finds none of the host's agents backs off for
/// `retryAfter`, since that's the normal state when they're all stopped and the
/// fallback starts a full login shell. A run that finds them keeps the host on
/// it every poll: its sessions can only be seen that way.
struct DiscoveryFallbackSchedule {
    static let retryAfter: TimeInterval = 60
    private var nextAllowedAt: [UUID: TimeInterval] = [:]

    func mayRun(_ hostId: UUID, now: TimeInterval) -> Bool {
        guard let next = nextAllowedAt[hostId] else { return true }
        return now >= next
    }

    mutating func record(_ hostId: UUID, foundAgents: Bool, now: TimeInterval) {
        nextAllowedAt[hostId] = foundAgents ? nil : now + Self.retryAfter
    }
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
    private static let log = Logger(subsystem: "dev.muxel.ios", category: "PollService")
    /// Shared by every instance: background refreshes build a fresh `PollService`.
    private static var fallbackSchedule = DiscoveryFallbackSchedule()
    private static let fallbackScheduleLock = NSLock()
    private var trackers: [String: PaneStatusTracker] = [:]
    /// Last SSH error encountered during `fetchPaneRows`, cleared on a successful read.
    private(set) var lastError: String?

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
        let rows = await fetchPaneRows(conn, instances: instances)
        return classify(rows: rows, instances: instances, liveScreens: liveScreens)
    }

    /// Fetch one host-wide sweep from the legacy servers and every server recorded
    /// by its projects (including named servers used by isolated desktop profiles).
    /// Each socket is queried once, retaining its owner tag beside each pane row.
    /// A server that isn't running contributes nothing. `hostId` rate-limits the
    /// login-shell fallback (`DiscoveryFallbackSchedule`); nil runs it every time.
    func fetchPaneRows(_ conn: SSHConnection, instances: [Instance],
                       hostId: UUID? = nil) async -> [PaneRow] {
        var seen = Set<String>()
        let sockets = ([TmuxCommands.socketName, "default"] + instances.compactMap(\.tmuxSocket))
            .filter { seen.insert($0).inserted }
        let commands = sockets.map { socket in
            TmuxCommands.commandLine(TmuxCommands.allPaneStatuses(socket: socket), session: "", socket: socket)
                + " 2>/dev/null"
        }
        let batchCmd = "{ " + commands.joined(separator: "; ") + "; true; } | " + Self.foregroundBusyFilter
        var raw = ""
        do {
            raw = try await conn.run(batchCmd)
            lastError = nil
        } catch {
            Self.log.warning("fetchPaneRows primary failed: \(error.localizedDescription, privacy: .public)")
            lastError = error.localizedDescription
            // Retry once after ensuring the SSH transport is up (covers transient
            // channel drops on cold launch / foreground return).
            if (try? await conn.connect()) != nil {
                if let retried = try? await conn.run(batchCmd) {
                    raw = retried
                    lastError = nil
                }
            }
        }
        var parsed = Self.parseAllPanes(raw)
        let terminalInstances = instances.filter { $0.kind == .terminal }
        let anyMatched = terminalInstances.contains { Self.resolveRow(for: $0, in: parsed) != nil }
        if terminalInstances.isEmpty || anyMatched {
            return parsed
        }
        let now = Date().timeIntervalSince1970
        if let hostId {
            let mayRun = Self.fallbackScheduleLock.withLock {
                Self.fallbackSchedule.mayRun(hostId, now: now)
            }
            if !mayRun { return parsed }
        }
        // Deep discovery fallback when bare non-login SSH exec matched 0 configured
        // agents (even if another unrelated socket had rows): reconnect if needed and
        // query via the user's login shell (`$SHELL -lc`) plus scan standard tmux
        // socket directories (`$TMUX_TMPDIR`, `$XDG_RUNTIME_DIR`, `/run/user/$uid`,
        // `/tmp`) with `lsp` and `ls` fallback.
        _ = try? await conn.connect()
        do {
            let fallbackRaw = try await conn.run(Self.loginShellPaneQuery(sockets: sockets))
            lastError = nil
            let extra = Self.parseAllPanes(fallbackRaw)
            if !extra.isEmpty {
                var keys = Set(parsed.map { "\($0.socket ?? "")\t\($0.session)" })
                for row in extra where keys.insert("\(row.socket ?? "")\t\(row.session)").inserted {
                    parsed.append(row)
                }
            }
        } catch {
            Self.log.warning("fetchPaneRows fallback failed: \(error.localizedDescription, privacy: .public)")
            lastError = error.localizedDescription
        }
        if let hostId {
            let found = terminalInstances.contains { Self.resolveRow(for: $0, in: parsed) != nil }
            Self.fallbackScheduleLock.withLock {
                Self.fallbackSchedule.record(hostId, foundAgents: found, now: now)
            }
        }
        return parsed
    }

    /// POSIX filter piped after `tmux list-panes` (0 extra SSH round trips) that
    /// converts the trailing `#{pane_pid}` column into a `1`/`0` kernel foreground-job
    /// flag (`pgrp != tpgid && tpgid > 0`) via Linux `/proc/$p/stat` or BSD/macOS `ps`.
    /// That accurately detects running shell scripts (`./build.sh`, `bash -c …`),
    /// pipelines, and subshells even when `#{pane_current_command}` still says `bash`.
    static let foregroundBusyFilter: String = "sh -c " + Shell.quote(
        "tab=$(printf '\\t'); "
        + "while IFS=\"$tab\" read -r s d b a k c p; do "
        + "[ -n \"$s\" ] || continue; "
        + "busy=\"\"; "
        + "if [ -n \"$p\" ]; then "
        + "if [ -r \"/proc/$p/stat\" ]; then "
        + "st=$(cat \"/proc/$p/stat\" 2>/dev/null); st=${st##*) }; set -- $st; "
        + "if [ -n \"$3\" ] && [ -n \"$6\" ] && [ \"$6\" -gt 0 ] 2>/dev/null && [ \"$3\" != \"$6\" ]; then busy=1; else busy=0; fi; "
        + "elif out=$(ps -o pgid=,tpgid= -p \"$p\" 2>/dev/null); then "
        + "set -- $out; "
        + "if [ -n \"$1\" ] && [ -n \"$2\" ] && [ \"$2\" -gt 0 ] 2>/dev/null && [ \"$1\" != \"$2\" ]; then busy=1; else busy=0; fi; "
        + "fi; fi; "
        + "printf '%s\\t%s\\t%s\\t%s\\t%s\\t%s\\t%s\\n' \"$s\" \"$d\" \"$b\" \"$a\" \"$k\" \"$c\" \"$busy\"; "
        + "done; true"
    )

    /// Login-shell + socket-directory discovery query using `tmux lsp` (`list-panes`
    /// alias) with `ls` (`list-sessions`) fallback, so hosts whose `TMUX_TMPDIR`,
    /// `XDG_RUNTIME_DIR`, custom socket name, or `tmux` binary lives in the user's
    /// login profile still report live sessions without colliding with bare-exec tests.
    static func loginShellPaneQuery(sockets: [String]) -> String {
        let lspCommands = sockets.map { socket -> String in
            let fmt = "#{session_name}\t#{pane_dead}\t#{window_bell_flag}\t#{window_activity}\t\(socket)\t#{pane_current_command}\t#{pane_pid}"
            let lsFmt = "#{session_name}\t0\t0\t#{session_activity}\t\(socket)"
            let flag = socket == "default" ? "" : "-L \(Shell.quote(socket)) "
            return "out=$(tmux \(flag)lsp -a -F \(Shell.quote(fmt)) 2>/dev/null); "
                + "if [ -n \"$out\" ]; then printf '%s\\n' \"$out\"; "
                + "else tmux \(flag)ls -F \(Shell.quote(lsFmt)) 2>/dev/null; fi"
        }.joined(separator: "; ")
        // Run the socket-dir scan under POSIX `sh` so unmatched globs never abort zsh.
        let dirScan = "sh -c " + Shell.quote(
            "uid=$(id -u); "
            + "for d in \"${TMUX_TMPDIR:-}\" \"${XDG_RUNTIME_DIR:-}\" \"/run/user/$uid\" /tmp; do "
            + "[ -n \"$d\" ] && [ -d \"$d/tmux-$uid\" ] || continue; "
            + "for s in \"$d/tmux-$uid\"/*; do "
            + "[ -S \"$s\" ] || continue; "
            + "b=$(basename \"$s\"); "
            + "out=$(tmux -S \"$s\" lsp -a -F \"#{session_name}\t#{pane_dead}\t#{window_bell_flag}\t#{window_activity}\t$b\t#{pane_current_command}\t#{pane_pid}\" 2>/dev/null); "
            + "if [ -n \"$out\" ]; then printf '%s\\n' \"$out\"; "
            + "else tmux -S \"$s\" ls -F \"#{session_name}\t0\t0\t#{session_activity}\t$b\" 2>/dev/null; fi; "
            + "done; done; true"
        )
        return "\"${SHELL:-/bin/sh}\" -lc " + Shell.quote("\(TmuxCommands.pathPrelude); { \(lspCommands); \(dirScan); true; } | \(foregroundBusyFilter)")
    }

    /// Resolve a read-only status `PaneRow` for `instance`: prefers strict socket-aware
    /// `TmuxCommands.resolveTarget`, and if none matched (e.g. `.muxel/workspace.json`
    /// recorded a stale socket name while the session is running on another socket),
    /// falls back ONLY to a unique `muxel_*_<uuid8>` session matching `instance.id`.
    static func resolveRow(for instance: Instance, in rows: [PaneRow]) -> PaneRow? {
        if let exact = TmuxCommands.resolveTarget(instance, in: rows,
                                                  name: { $0.session }, socket: { $0.socket }) {
            return exact
        }
        return rows.first { TmuxSession.session($0.session, matchesInstanceId: instance.id) }
    }

    /// Seed per-instance trackers for agents previously persisted as `.working` so a
    /// fresh `PollService` (e.g. background `StatusPoller.run()` or cold relaunch)
    /// latches `.working → .done` when the agent finishes instead of dropping to `.idle`.
    func seedPreviouslyWorking(_ instanceIds: Set<String>) {
        for id in instanceIds {
            var tracker = trackers[id] ?? PaneStatusTracker()
            tracker.seedWorking()
            trackers[id] = tracker
        }
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
            let (working, blocked) = defaultMarkers(program: inst.program, args: inst.args)
            guard let row = Self.resolveRow(for: inst, in: rows) else {
                // If this pane is currently attached via a live terminal in the app,
                // classify it from its live screen even if the tmux listing missed it.
                if let screen = liveScreens[inst.id] {
                    var tracker = trackers[inst.id] ?? PaneStatusTracker()
                    let status = tracker.update(
                        exited: false, screen: screen.text,
                        working: working, blocked: blocked,
                        bell: screen.bell, idle: screen.idle
                    )
                    trackers[inst.id] = tracker
                    let effective: AgentStatus = (working.isEmpty && blocked.isEmpty && status == .working) ? .idle : status
                    out.append(InstanceStatus(instanceId: inst.id, status: effective, running: true))
                } else {
                    out.append(InstanceStatus(instanceId: inst.id,
                                              status: inst.activity?.currentState ?? .idle,
                                              running: false))
                }
                continue
            }

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

            // Shell panes: kernel job control (`foregroundBusy`: `pgrp != tpgid`) and
            // tmux's `#{pane_current_command}` tell us deterministically whether ANY
            // foreground command (binary, script, subshell, pipeline) is running or
            // control has returned to the shell prompt (`zsh`/`bash`/`fish`). While a
            // command is running the shell stays `.working`, and returning to the prompt
            // latches `.done` until attended.
            if inst.isShell, row.foregroundBusy != nil || (row.currentCommand?.isEmpty == false) {
                var tracker = trackers[inst.id] ?? PaneStatusTracker()
                let raw: AgentStatus
                if row.exited {
                    raw = .done
                } else if row.foregroundBusy == true {
                    raw = .working
                } else if let cmd = row.currentCommand, !cmd.isEmpty, !Instance.isShellCommand(cmd) {
                    raw = .working
                } else if row.bell {
                    raw = .done
                } else {
                    raw = .idle
                }
                let status = tracker.updateRaw(raw, canLatch: true)
                trackers[inst.id] = tracker
                out.append(InstanceStatus(instanceId: inst.id, status: status, running: true))
                continue
            }

            // No screen scrape (see the type doc) — pass an empty screen + no markers.
            // classify then falls back to exit/bell + activity, and the tracker
            // can't latch (canLatch == !working.isEmpty), which is the intended
            // marker-less behaviour.
            var tracker = trackers[inst.id] ?? PaneStatusTracker()
            let wasWorking = tracker.wasWorking
            if wasWorking && !row.exited && !row.bell {
                let hasMarkers = !(working.isEmpty && blocked.isEmpty)
                if hasMarkers {
                    // Marker-bearing agents (Claude, opencode, Grok, Codex) routinely
                    // pause output for 10–30s while thinking or waiting on APIs/tools.
                    // Never infer `.done` without a real `LiveScreen` confirming the
                    // working marker is gone; hold `.working` across a missed probe
                    // (capped at 10 min so a permanently unreachable probe won't stick).
                    if row.idle < 600 {
                        let status = tracker.updateRaw(.working, canLatch: true)
                        trackers[inst.id] = tracker
                        out.append(InstanceStatus(instanceId: inst.id, status: status, running: true))
                        continue
                    }
                } else if row.idle < 5 {
                    let status = tracker.updateRaw(.working, canLatch: true)
                    trackers[inst.id] = tracker
                    out.append(InstanceStatus(instanceId: inst.id, status: status, running: true))
                    continue
                }
            }
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
            // finished). When neither exit nor bell fired and desktop recorded a live
            // activity state in `.muxel/workspace.json`, surface that state so working /
            // blocked / done agents show accurate dots without screen scraping.
            if row.bell && !row.exited {
                effective = .blocked
            } else if !row.exited,
                      let desktopState = inst.activity?.effectiveLiveState(paneIdle: row.idle) {
                // Don't keep showing a stale desktop "done" badge for an agent that has
                // been completely idle for >5 minutes; treat it as idle instead.
                if desktopState == .done && row.idle > 300 {
                    effective = .idle
                } else {
                    effective = desktopState
                }
            }
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
    /// first row wins per socket and session. Blank/short lines are skipped.
    static func parseAllPanes(_ s: String) -> [PaneRow] {
        var rows: [PaneRow] = []
        var seen = Set<String>()
        let now = Date().timeIntervalSince1970
        for rawLine in s.split(separator: "\n", omittingEmptySubsequences: true) {
            let line = rawLine.last == "\r" ? rawLine.dropLast() : rawLine
            let parts = line.split(separator: "\t", omittingEmptySubsequences: false)
            guard let first = parts.first else { continue }
            let session = String(first)
            let socket = parts.count > 4 ? String(parts[4]) : nil
            let currentCmd: String? = parts.count > 5 && !parts[5].isEmpty ? String(parts[5]) : nil
            let fgBusy: Bool? = {
                guard parts.count > 6 else { return nil }
                if parts[6] == "1" { return true }
                if parts[6] == "0" { return false }
                return nil
            }()
            guard !session.isEmpty, seen.insert("\(socket ?? "")\t\(session)").inserted else { continue }
            let dead = parts.count > 1 && parts[1] == "1"
            let bell = parts.count > 2 && parts[2] == "1"
            var idle: TimeInterval = 0
            var activityTimestamp: TimeInterval? = nil
            if parts.count > 3, let activity = TimeInterval(parts[3]) {
                activityTimestamp = activity
                idle = max(0, now - activity)
            }
            rows.append(PaneRow(session: session, exited: dead, bell: bell, idle: idle,
                                socket: socket, currentCommand: currentCmd, foregroundBusy: fgBusy,
                                activityTimestamp: activityTimestamp))
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
