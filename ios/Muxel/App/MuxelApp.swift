import SwiftUI

@main
struct MuxelApp: App {
    @UIApplicationDelegateAdaptor(AppDelegate.self) private var appDelegate
    @StateObject private var state = AppState()
    @Environment(\.scenePhase) private var scenePhase

    var body: some Scene {
        WindowGroup {
            RootView()
                .environmentObject(state)
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
