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
            _ = await StatusPoller().run()
            task.setTaskCompleted(success: true)
        }
        task.expirationHandler = { work.cancel() }
    }
}
