import SwiftUI
import UIKit

/// The consolidated settings hub (the sidebar gear): appearance, login identities,
/// security (App Lock), and notification status — previously scattered across two
/// icon-only toolbar buttons and buried inside the Identities sheet.
struct SettingsView: View {
    @EnvironmentObject var state: AppState
    @EnvironmentObject var appLock: AppLock
    @Environment(\.theme) private var theme
    @Environment(\.dismiss) private var dismiss
    @State private var showThemePicker = false
    @State private var showIdentities = false

    var body: some View {
        NavigationStack {
            Form {
                MuxelSection("Appearance") {
                    Button { showThemePicker = true } label: {
                        rowLabel("Theme", systemImage: "paintpalette")
                    }
                }
                MuxelSection("Login") {
                    Button { showIdentities = true } label: {
                        rowLabel("Login identities", systemImage: "person.badge.key")
                    }
                }
                securitySection
                MuxelSection("Notifications") {
                    HStack {
                        Label("Agent alerts", systemImage: "bell")
                            .foregroundStyle(theme.textColor)
                        Spacer()
                        if state.notificationsDenied {
                            Button("Open Settings") { openSystemSettings() }
                                .font(.mono(.caption))
                        } else {
                            Text("enabled").font(.mono(.caption)).foregroundStyle(theme.mutedColor)
                        }
                    }
                } footer: {
                    Text(state.notificationsDenied
                         ? "Turn notifications on in the system Settings to get blocked / finished alerts while backgrounded."
                         : "Blocked / finished agent alerts arrive while the app is backgrounded.")
                }
            }
            .muxelSheet()
            .navigationTitle("Settings")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
            .sheet(isPresented: $showThemePicker) { ThemePickerView() }
            .sheet(isPresented: $showIdentities) { IdentitiesView() }
        }
    }

    /// App Lock (moved here from the Identities sheet — Settings is its natural home).
    private var securitySection: some View {
        MuxelSection("Security") {
            Toggle("Require Face ID / passcode to open muxel", isOn: appLockBinding)
                .disabled(!appLock.isAvailable)
        } footer: {
            Text(appLock.isAvailable
                ? "Protects the app UI. Background status polling and notifications keep working while locked."
                : "Set a device passcode to use App Lock.")
        }
    }

    private var appLockBinding: Binding<Bool> {
        Binding(
            get: { appLock.isEnabled },
            set: { on in
                appLock.isEnabled = on
                if on {
                    // Prove the user can pass before the lock can ever engage; a
                    // failed/canceled check rolls the toggle back.
                    Task { await appLock.confirmEnable() }
                }
            }
        )
    }

    private func rowLabel(_ title: String, systemImage: String) -> some View {
        HStack {
            Label(title, systemImage: systemImage)
                .foregroundStyle(theme.textColor)
            Spacer()
            Image(systemName: "chevron.right")
                .font(.caption)
                .foregroundStyle(theme.mutedColor)
        }
    }

    private func openSystemSettings() {
        if let url = URL(string: UIApplication.openSettingsURLString) {
            UIApplication.shared.open(url)
        }
    }
}
