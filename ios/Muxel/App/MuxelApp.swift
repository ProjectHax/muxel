import SwiftUI

@main
struct MuxelApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var state = AppState()
    @StateObject private var themeStore = ThemeStore()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(state)
                .environmentObject(themeStore)
                .environment(\.theme, themeStore.current)
                .tint(themeStore.current.accentColor)
                .preferredColorScheme(themeStore.current.isDark ? .dark : .light)
        }
        .onChange(of: scenePhase) { phase in
            switch phase {
            case .background:
                // The activity was started while foreground; here we only push a final
                // update (updates are allowed from the background) and run one poll to
                // refresh it before iOS suspends us.
                let snapshot = state.currentSummarySnapshot()
                state.stopPolling()
                appDelegate.scheduleNextPoll()
                appDelegate.refreshLiveActivity(with: snapshot)
            case .active:
                // Start / refresh the Live Activity while we're foreground (the only
                // time ActivityKit permits starting one); it then persists onto the
                // Lock Screen when minimized. The poll loop keeps refreshing it.
                state.syncLiveActivity()
                if state.selectedProject != nil {
                    Task { await state.refreshLayout() }
                }
            default:
                break
            }
        }
    }
}
