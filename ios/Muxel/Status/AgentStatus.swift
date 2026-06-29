import Foundation

/// Lifecycle state of an agent pane. Port of `AgentStatus`
/// (`crates/muxel-terminal/src/view.rs`).
enum AgentStatus: String, Codable, Equatable, CaseIterable {
    case working
    case idle
    case blocked
    case done
}

/// Decide an agent's state from its signals. Faithful port of `classify`
/// (view.rs). Priority, highest first:
/// 1. exited → done
/// 2. a working marker on screen → working
/// 3. a blocked marker on screen → blocked
/// 4. bell rang → done
/// 5. (marker-less only) output within the last 2s → working
/// 6. otherwise → idle
func classify(
    exited: Bool,
    screen: String,
    working: [String],
    blocked: [String],
    bell: Bool,
    idle: TimeInterval
) -> AgentStatus {
    if exited { return .done }
    if working.contains(where: { screen.contains($0) }) { return .working }
    if blocked.contains(where: { screen.contains($0) }) { return .blocked }
    if bell { return .done }
    // Output-activity fallback ONLY for agents without a working marker.
    if working.isEmpty && idle < 2 { return .working }
    return .idle
}

/// Promote a working→idle transition to a latched `done` for marker-based agents
/// that finish without ringing the bell. Port of `latch_done` (view.rs).
/// Returns the displayed status + the new latch state.
func latchDone(
    prevRaw: AgentStatus?,
    raw: AgentStatus,
    latched: Bool,
    canLatch: Bool
) -> (status: AgentStatus, latched: Bool) {
    switch raw {
    case .working, .blocked, .done:
        return (raw, false)
    case .idle:
        if canLatch && (latched || prevRaw == .working) {
            return (.done, true)
        }
        return (.idle, false)
    }
}

/// Per-pane status state machine mirroring `TerminalView::status()`: it holds the
/// previous raw classification + the done-latch across poll ticks and yields the
/// displayed status. `attend()` clears the latch when the user views the pane.
///
/// `canLatch` is `!working.isEmpty` — marker-less terminals never latch (incidental
/// output would otherwise fake a finished turn).
struct PaneStatusTracker {
    private var prevRaw: AgentStatus?
    private var latched = false

    mutating func update(
        exited: Bool,
        screen: String,
        working: [String],
        blocked: [String],
        bell: Bool,
        idle: TimeInterval
    ) -> AgentStatus {
        let raw = classify(
            exited: exited, screen: screen,
            working: working, blocked: blocked,
            bell: bell, idle: idle
        )
        let result = latchDone(
            prevRaw: prevRaw, raw: raw,
            latched: latched, canLatch: !working.isEmpty
        )
        prevRaw = raw
        latched = result.latched
        return result.status
    }

    /// The pane was attended (viewed) — drop the done latch.
    mutating func attend() {
        latched = false
    }
}
