import Foundation
import UserNotifications

/// Thin wrapper over `UNUserNotificationCenter` for the local notifications fired
/// when a backgrounded agent becomes blocked or finishes.
enum NotificationManager {
    static func requestAuthorization() {
        UNUserNotificationCenter.current()
            .requestAuthorization(options: [.alert, .sound, .badge]) { _, _ in }
    }

    /// The user's current authorization — `.denied` drives the sidebar's quiet
    /// "notifications off" pointer (a denial otherwise fails silently forever).
    static func authorizationStatus() async -> UNAuthorizationStatus {
        await UNUserNotificationCenter.current().notificationSettings().authorizationStatus
    }

    static func notify(title: String, body: String, identifier: String = UUID().uuidString) {
        let content = UNMutableNotificationContent()
        content.title = title
        content.body = body
        content.sound = .default
        let request = UNNotificationRequest(identifier: identifier, content: content, trigger: nil)
        UNUserNotificationCenter.current().add(request)
    }
}
