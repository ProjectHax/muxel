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
                state.stopPolling()
                appDelegate.scheduleNextPoll()
            case .active:
                if state.selectedProject != nil {
                    Task { await state.refreshLayout() }
                }
            default:
                break
            }
        }
    }
}
