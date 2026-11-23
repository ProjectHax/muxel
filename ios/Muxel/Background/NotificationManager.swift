import Foundation
import UserNotifications

/// Target project + agent encoded into a local notification's `userInfo`, used to
/// jump straight to the finishing/blocked agent when the user taps the banner.
struct NotificationRoute: Equatable {
    let projectId: UUID
    let instanceId: String
}

/// Bridges tapped local notifications from `AppDelegate` into SwiftUI / `AppState`,
/// buffering a route if the user tapped a notification during a cold launch before
/// `RootView` mounted.
@MainActor
final class NotificationRouter: ObservableObject {
    static let shared = NotificationRouter()
    @Published var pendingRoute: NotificationRoute?

    func enqueue(_ route: NotificationRoute) {
        pendingRoute = route
    }

    func consume() -> NotificationRoute? {
        defer { pendingRoute = nil }
        return pendingRoute
    }
}

/// Thin wrapper over `UNUserNotificationCenter` for the local notifications fired
/// when a backgrounded agent becomes blocked or finishes.
enum NotificationManager {
    static let projectIdKey = "muxel.projectId"
    static let instanceIdKey = "muxel.instanceId"

    static func requestAuthorization() {
        UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }
    }

    /// Prompt for notification permission only if the user hasn't decided yet, returning
    /// the resulting status. Called after the first host/project is configured instead of
    /// ambushing the user on a cold first launch.
    @discardableResult
    static func requestAuthorizationIfNotDetermined() async -> UNAuthorizationStatus {
        let current = await authorizationStatus()
        guard current == .notDetermined else { return current }
        _ = try? await UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound, .badge])
        return await authorizationStatus()
    }

    /// The user's current authorization — `.denied` drives the sidebar's quiet
    /// "notifications off" pointer (a denial otherwise fails silently forever).
    static func authorizationStatus() async -> UNAuthorizationStatus {
        await UNUserNotificationCenter.current().notificationSettings().authorizationStatus
    }

    static func userInfo(projectId: UUID, instanceId: String) -> [String: String] {
        [projectIdKey: projectId.uuidString, instanceIdKey: instanceId]
    }

    static func route(from userInfo: [AnyHashable: Any]) -> NotificationRoute? {
        guard let rawProject = userInfo[projectIdKey] as? String,
              let projectId = UUID(uuidString: rawProject),
              let instanceId = userInfo[instanceIdKey] as? String,
              !instanceId.isEmpty
        else { return nil }
        return NotificationRoute(projectId: projectId, instanceId: instanceId)
    }

    static func notify(
        title: String,
        body: String,
        projectId: UUID? = nil,
        instanceId: String? = nil,
        identifier: String = UUID().uuidString
    ) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        if let projectId, let instanceId {
            content.userInfo = userInfo(projectId: projectId, instanceId: instanceId)
        }
        let request = UNNotificationRequest(identifier: identifier, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }
}
