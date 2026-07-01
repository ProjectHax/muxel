import UIKit
import BackgroundTasks

/// Registers + drives the background poll task and notification authorization.
/// Wired into the SwiftUI app via `@UIApplicationDelegateAdaptor`.
///
/// iOS schedules `BGAppRefreshTask` opportunistically (throttled by usage, often
/// 15+ minutes apart and not at all under low-power/long-suspension), so these
/// notifications are best-effort — the documented tradeoff of on-device polling.
final class AppDelegate: NSObject, UIApplicationDelegate {
    static let pollTaskIdentifier = "dev.muxel.ios.poll"

    func application(
        _ application: UIApplication,
        didFinishLaunchingWithOptions launchOptions: [UIApplication.LaunchOptionsKey: Any]? = nil
    ) -> Bool {
        NotificationManager.requestAuthorization()
        BGTaskScheduler.shared.register(
            forTaskWithIdentifier: Self.pollTaskIdentifier, using: nil
        ) { task in
            self.handlePoll(task)
        }
        return true
    }

    /// Ask iOS to run the next poll no sooner than ~15 min out. Call when entering
    /// the background.
    func scheduleNextPoll() {
        let request = BGAppRefreshTaskRequest(identifier: Self.pollTaskIdentifier)
        request.earliestBeginDate = Date(timeIntervalSinceNow: 15 * 60)
        try? BGTaskScheduler.shared.submit(request)
    }

    private func handlePoll(_ task: BGTask) {
        scheduleNextPoll() // chain the next run

        let work = Task {
            _ = await StatusPoller().run() // also updates/ends the Live Activity
            task.setTaskCompleted(success: true)
        }
        task.expirationHandler = { work.cancel() }
    }

    /// Called at the background transition: push the latest snapshot to the (already
    /// foreground-started) Live Activity, then run one full multi-project poll to
    /// refine it before iOS suspends us. Held open by a background-task assertion.
    /// (Starting a new activity here isn't reliable — ActivityKit starts are
    /// foreground-only — so this only updates; the foreground poll owns the start.)
    func refreshLiveActivity(with snapshot: MuxelActivityAttributes.ContentState?) {
        var bgId: UIBackgroundTaskIdentifier = .invalid
        bgId = UIApplication.shared.beginBackgroundTask(withName: "muxel.liveactivity.refresh") {
            if bgId != .invalid {
                UIApplication.shared.endBackgroundTask(bgId)
                bgId = .invalid
            }
        }
        Task {
            if let snapshot { await LiveActivityController.apply(snapshot) }
            _ = await StatusPoller().run()
            if bgId != .invalid {
                UIApplication.shared.endBackgroundTask(bgId)
                bgId = .invalid
            }
        }
    }
}
