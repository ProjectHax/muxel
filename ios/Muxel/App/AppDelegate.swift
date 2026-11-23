import UIKit
import BackgroundTasks
import UserNotifications

/// Registers + drives the background poll task and notification authorization.
/// Wired into the SwiftUI app via `@UIApplicationDelegateAdaptor`.
///
/// iOS schedules `BGAppRefreshTask` opportunistically (throttled by usage, often
/// 15+ minutes apart and not at all under low-power/long-suspension), so background
/// notifications are best-effort. While foregrounded (e.g. viewing another project,
/// tab, or the home screen), `UNUserNotificationCenterDelegate` presents banners +
/// sound immediately when any watched/polled shell or agent finishes or needs input.
final class AppDelegate: NSObject, UIApplicationDelegate, UNUserNotificationCenterDelegate {
    static let pollTaskIdentifier = "dev.muxel.ios.poll"

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        UNUserNotificationCenter.current().delegate = self
        BGTaskScheduler.shared.register(
            forTaskWithIdentifier: Self.pollTaskIdentifier, using: nil
        ) { task in
            self.handlePoll(task)
        }
        return true
    }

    /// Show local notifications as banners + sound even when muxel is in the foreground
    /// (for agents/shells finishing in another tab or project, or while on the home screen).
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        willPresent notification: UNNotification
    ) async -> UNNotificationPresentationOptions {
        [.banner, .sound, .list]
    }

    /// Tapping a local notification jumps straight to the project and agent tab that
    /// triggered it (both when already running and on cold launch).
    func userNotificationCenter(
        _ center: UNUserNotificationCenter,
        didReceive response: UNNotificationResponse
    ) async {
        let userInfo = response.notification.request.content.userInfo
        guard let route = NotificationManager.route(from: userInfo) else { return }
        await MainActor.run {
            NotificationRouter.shared.enqueue(route)
        }
    }

    /// Fully closing muxel (swiping it away in the app switcher) should clear its
    /// Live Activity from the Dynamic Island and Lock Screen — a Live Activity
    /// otherwise outlives its app until it hits its stale/dismissal timeout.
    /// `Activity.end` is async, so block briefly here: the system gives a
    /// terminating app a short window before it's killed, enough for the end to
    /// land. (For a long-suspended app iOS may skip this callback; the activity's
    /// ~20-min stale date is the backstop for that case.)
    func applicationWillTerminate(_ application: UIApplication) {
        let done = DispatchSemaphore(value: 0)
        Task {
            await LiveActivityController.endAll()
            done.signal()
        }
        _ = done.wait(timeout: .now() + 3)
    }

    /// Earliest-begin delay for the next `BGAppRefreshTask`: 1 minute while any agent
    /// is actively working (asking iOS to wake as soon as its budget allows) vs 15
    /// minutes when everything is idle.
    static func nextBackgroundRefreshDelay(hasWorking: Bool) -> TimeInterval {
        hasWorking ? 60 : 15 * 60
    }

    /// Ask iOS to run the next background poll. Call when entering the background and
    /// at the end of each background refresh.
    func scheduleNextPoll(hasWorking: Bool = false) {
        let request = BGAppRefreshTaskRequest(identifier: Self.pollTaskIdentifier)
        request.earliestBeginDate = Date(timeIntervalSinceNow: Self.nextBackgroundRefreshDelay(hasWorking: hasWorking))
        try? BGTaskScheduler.shared.submit(request)
    }

    private func handlePoll(_ task: BGTask) {
        let work = Task {
            let result = await StatusPoller().runDetailed()
            self.scheduleNextPoll(hasWorking: result.hasWorking)
            task.setTaskCompleted(success: true)
        }
        task.expirationHandler = {
            work.cancel()
            self.scheduleNextPoll(hasWorking: (SummaryCache.load()?.workingCount ?? 0) > 0)
        }
    }

    /// Active background-transition refresh task (cancelled on a new backgrounding or foreground return).
    private var liveActivityRefreshTask: Task<Void, Never>?

    func cancelLiveActivityRefresh() {
        liveActivityRefreshTask?.cancel()
        liveActivityRefreshTask = nil
    }

    /// Called at the background transition: push the latest snapshot to the (already
    /// foreground-started) Live Activity, then poll to refine it before iOS suspends
    /// us. Reuses a single SSH connection per host and polls every ~4s while any agent
    /// or shell is actively `.working` for the full background-task window iOS grants
    /// (~25–28s), then queues `BGAppRefreshTask` with a 1-minute earliest-begin date if
    /// work is still in flight.
    func refreshLiveActivity(with snapshot: MuxelActivityAttributes.ContentState?) {
        liveActivityRefreshTask?.cancel()
        var bgId: UIBackgroundTaskIdentifier = .invalid
        let finishBackgroundTask = {
            if bgId != .invalid {
                UIApplication.shared.endBackgroundTask(bgId)
                bgId = .invalid
            }
        }
        bgId = UIApplication.shared.beginBackgroundTask(withName: "muxel.liveactivity.refresh") { [weak self] in
            self?.liveActivityRefreshTask?.cancel()
            finishBackgroundTask()
        }
        liveActivityRefreshTask = Task {
            var pool: [UUID: SSHConnection] = [:]
            var hasWorking = (snapshot?.workingCount ?? 0) > 0
            defer {
                let toClose = Array(pool.values)
                if !toClose.isEmpty {
                    Task {
                        for conn in toClose { await conn.close() }
                    }
                }
                self.scheduleNextPoll(hasWorking: hasWorking)
                finishBackgroundTask()
            }
            if let snapshot { await LiveActivityController.apply(snapshot) }
            let poller = StatusPoller()
            var result = await poller.runDetailed(sharedConnections: pool)
            pool = result.connections
            hasWorking = result.hasWorking
            // While an agent/shell is actively working and iOS still gives us
            // background headroom (~25–30s), keep polling over the reused SSH
            // connection every 4s until work finishes or headroom drops to <= 6s.
            for _ in 0..<8 {
                guard hasWorking && !Task.isCancelled else { break }
                let remaining = await MainActor.run { UIApplication.shared.backgroundTimeRemaining }
                guard remaining > 8 else { break }
                try? await Task.sleep(nanoseconds: 4_000_000_000)
                guard !Task.isCancelled else { break }
                let afterSleep = await MainActor.run { UIApplication.shared.backgroundTimeRemaining }
                guard afterSleep > 4 else { break }
                result = await poller.runDetailed(sharedConnections: pool)
                pool = result.connections
                hasWorking = result.hasWorking
            }
        }
    }
}
