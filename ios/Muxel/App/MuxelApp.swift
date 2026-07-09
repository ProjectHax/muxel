import SwiftUI

@main
struct MuxelApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var state = AppState()
    @StateObject private var themeStore = ThemeStore()
    @StateObject private var appLock = AppLock()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            ZStack {
                RootView()
                // With App Lock enabled, shield the UI while locked and in the app
                // switcher (so snapshots don't leak terminal content).
                if appLock.isEnabled && (appLock.isLocked || scenePhase != .active) {
                    AppLockShield(locked: appLock.isLocked) {
                        Task { await appLock.unlock() }
                    }
                }
            }
            .environmentObject(state)
            .environmentObject(themeStore)
            .environmentObject(appLock)
            .environment(\.theme, themeStore.current)
            .tint(themeStore.current.accentColor)
            .preferredColorScheme(themeStore.current.isDark ? .dark : .light)
        }
        .onChange(of: scenePhase) { phase in
            switch phase {
            case .background:
                appLock.noteBackgrounded()
                // The activity was started while foreground; here we only push a final
                // update (updates are allowed from the background) and run one poll to
                // refresh it before iOS suspends us.
                let snapshot = state.currentSummarySnapshot()
                state.stopPolling()
                appDelegate.scheduleNextPoll()
                appDelegate.refreshLiveActivity(with: snapshot)
            case .active:
                appLock.noteActivated()
                if appLock.isLocked {
                    Task { await appLock.unlock() }
                }
                // Start / refresh the Live Activity while we're foreground (the only
                // time ActivityKit permits starting one); it then persists onto the
                // Lock Screen when minimized. The poll loop keeps refreshing it.
                state.syncLiveActivity()
                Task { await state.refreshNotificationStatus() }
                // Restart the foreground poll loop (cancelled on background) and
                // re-read the layout, so status keeps updating after a return.
                state.resumeForeground()
            default:
                break
            }
        }
    }
}
