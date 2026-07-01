import ActivityKit
import Foundation

/// Starts / updates / ends the single muxel Live Activity. Stateless — safe to call
/// from `@MainActor` (AppState / MuxelApp) or the background `StatusPoller`. iOS 17
/// floor, so ActivityKit APIs are unconditionally available; only the runtime
/// user-enablement toggle is checked.
enum LiveActivityController {
    /// ~20 min — just past the ≥15-min background-poll cadence, so the system marks the
    /// activity visibly stale once polls stop (honest surfacing of the no-push limit).
    static let staleAfter: TimeInterval = 20 * 60

    static var areActivitiesEnabled: Bool {
        ActivityAuthorizationInfo().areActivitiesEnabled
    }

    /// Reconcile the live activity to `state`, guaranteeing a single instance:
    /// - nothing running → end any existing activity
    /// - an activity exists → update it (and end any accidental extras)
    /// - none exists + something running → request one (only succeeds while the app
    ///   has runtime — the `.background` transition or under a background-task
    ///   assertion; a suspended-woken poll may no-op, which is fine)
    @discardableResult
    static func apply(_ state: MuxelActivityAttributes.ContentState) async -> Bool {
        guard areActivitiesEnabled else { return false }
        let existing = Activity<MuxelActivityAttributes>.activities

        // End only when there's nothing at all to show; idle instances stay present
        // so the status bar remains while the app is minimized.
        if state.isEmpty {
            for a in existing { await a.end(nil, dismissalPolicy: .immediate) }
            return false
        }

        let content = ActivityContent(
            state: state, staleDate: Date().addingTimeInterval(staleAfter))
        if let live = existing.first {
            await live.update(content)
            for extra in existing.dropFirst() {
                await extra.end(nil, dismissalPolicy: .immediate)
            }
            return true
        }
        do {
            _ = try Activity.request(
                attributes: MuxelActivityAttributes(startedAt: Date()),
                content: content,
                pushType: nil) // local updates only; no APNs
            return true
        } catch {
            return false
        }
    }

    /// End every muxel activity — called on foreground return and to clear a stale one
    /// left by a force-quit (the next cold launch's `.active`).
    static func endAll() async {
        for a in Activity<MuxelActivityAttributes>.activities {
            await a.end(nil, dismissalPolicy: .immediate)
        }
    }
}
