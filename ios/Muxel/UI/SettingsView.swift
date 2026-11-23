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
    @State private var showAutoWatchPicker = false
    @State private var terminalFontSize = Int(TerminalFontPreference.size)
    @AppStorage(SidebarPreferences.showAgentsKey) private var showAgentsOnHome = true

    private var autoWatchSummary: String {
        let count = state.autoWatchProjectIds.count
        return count == 0 ? "None" : "\(count) selected"
    }

    private var autoWatchActiveBinding: Binding<Bool> {
        Binding(
            get: { state.autoWatchActiveOnLaunch },
            set: { state.setAutoWatchActiveOnLaunch($0) }
        )
    }

    var body: some View {
        NavigationStack {
            Form {
                MuxelSection("Appearance") {
                    Button { showThemePicker = true } label: {
                        rowLabel("Theme", systemImage: "paintpalette", value: theme.name)
                    }
                    Stepper(value: $terminalFontSize, in: 9...24) {
                        HStack {
                            Label("Terminal font", systemImage: "textformat.size")
                                .foregroundStyle(theme.textColor)
                            Spacer()
                            Text("\(terminalFontSize) pt")
                                .font(.mono(.footnote))
                                .foregroundStyle(theme.mutedColor)
                        }
                    }
                    .onChange(of: terminalFontSize) { _, newSize in
                        state.terminals.setFontSize(CGFloat(newSize))
                    }
                } footer: {
                    Text("You can also pinch-to-zoom directly inside any terminal pane.")
                }
                MuxelSection("Home screen") {
                    Toggle(isOn: $showAgentsOnHome) {
                        Label("Show agents under projects", systemImage: "list.bullet.indent")
                            .foregroundStyle(theme.textColor)
                    }
                } footer: {
                    Text("List each project's agents and statuses below the project name. Status dots on each project stay visible either way.")
                }
                MuxelSection("Monitoring") {
                    Button { showAutoWatchPicker = true } label: {
                        rowLabel("Auto-watch on launch", systemImage: "pin", value: autoWatchSummary)
                    }
                    Toggle(isOn: autoWatchActiveBinding) {
                        Label("Auto-watch working projects", systemImage: "bolt.badge.clock")
                            .foregroundStyle(theme.textColor)
                    }
                    Button {
                        Task { _ = await state.scanAndWatchWorkingProjects(announce: true) }
                    } label: {
                        VStack(alignment: .leading, spacing: 4) {
                            HStack {
                                Label("Scan & watch active projects now", systemImage: "eye")
                                    .foregroundStyle(theme.accentColor)
                                Spacer()
                                if state.isScanningActiveProjects {
                                    ProgressView().controlSize(.small)
                                }
                            }
                            if let status = state.scanStatusText {
                                Text(status)
                                    .font(.mono(.caption2))
                                    .foregroundStyle(theme.mutedColor)
                            }
                        }
                    }
                    .disabled(state.isScanningActiveProjects || state.doc.projects.isEmpty)
                } footer: {
                    Text("Connects read-only to running agents to detect idle vs. working without resizing desktop panes. Watched projects get a highlighted folder icon and faster polling.")
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
            .sheet(isPresented: $showAutoWatchPicker) { AutoWatchProjectsView() }
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

    private func rowLabel(_ title: String, systemImage: String, value: String? = nil) -> some View {
        HStack {
            Label(title, systemImage: systemImage)
                .foregroundStyle(theme.textColor)
            Spacer()
            if let value {
                Text(value)
                    .font(.mono(.caption))
                    .foregroundStyle(theme.mutedColor)
                    .lineLimit(1)
            }
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

/// Checklist of all configured projects grouped by host so the user can choose which
/// projects automatically start monitoring whenever the app opens.
private struct AutoWatchProjectsView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            Form {
                if state.doc.projects.isEmpty {
                    MuxelSection("Projects") {
                        Text("Add a host and projects first to choose which ones auto-watch on launch.")
                            .font(.mono(.footnote))
                            .foregroundStyle(theme.mutedColor)
                    }
                } else {
                    ForEach(state.doc.hosts) { host in
                        let hostProjects = state.projects(for: host)
                        if !hostProjects.isEmpty {
                            MuxelSection(host.name) {
                                ForEach(hostProjects) { project in
                                    Toggle(isOn: Binding(
                                        get: { state.isProjectAutoWatched(project.id) },
                                        set: { enabled in
                                            UISelectionFeedbackGenerator().selectionChanged()
                                            state.setProjectAutoWatched(project.id, enabled)
                                        }
                                    )) {
                                        VStack(alignment: .leading, spacing: 2) {
                                            Text(project.name)
                                                .font(.mono(.callout))
                                                .foregroundStyle(theme.textColor)
                                                .lineLimit(1)
                                            Text(project.remoteRoot)
                                                .font(.mono(.caption2))
                                                .foregroundStyle(theme.mutedColor)
                                                .lineLimit(1)
                                                .truncationMode(.head)
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
            .muxelSheet()
            .navigationTitle("Auto-watch on launch")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar { ToolbarItem(placement: .confirmationAction) { Button("Done") { dismiss() } } }
        }
    }
}
