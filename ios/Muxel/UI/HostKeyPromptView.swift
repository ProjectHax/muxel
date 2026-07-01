import SwiftUI

/// The changed-host-key decision sheet: the stored vs presented `SHA256:…`
/// fingerprints in selectable mono text, a MITM warning, and an explicit
/// destructive "Trust new key". A sheet rather than an alert — fingerprints are
/// long and need to be read side by side. Cancel keeps refusing the connection.
struct HostKeyPromptView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.dismiss) private var dismiss
    @Environment(\.theme) private var theme
    let prompt: AppState.HostKeyPrompt

    var body: some View {
        NavigationStack {
            List {
                MuxelSection {
                    Label {
                        Text(prompt.scope == .jump
                            ? "The jump host for \(prompt.host.name) presented a new key"
                            : "\(prompt.host.name) presented a new key")
                            .font(.mono(.callout, weight: .semibold))
                    } icon: {
                        Image(systemName: "exclamationmark.shield.fill")
                            .foregroundStyle(theme.blockedColor)
                    }
                } footer: {
                    Text("This can mean the server was reinstalled — or that something is "
                        + "intercepting the connection. Only trust the new key if you know "
                        + "why it changed. Until then, connections are refused.")
                }

                MuxelSection("Stored") { fingerprintRow(prompt.expected) }
                MuxelSection("Presented now") { fingerprintRow(prompt.presented) }

                MuxelSection {
                    Button(role: .destructive) {
                        state.acceptNewHostKey(prompt)
                        dismiss()
                    } label: {
                        Label("Trust new key", systemImage: "checkmark.shield")
                            .font(.mono(.callout, weight: .semibold))
                    }
                }
            }
            .muxelSheet()
            .navigationTitle("Host key changed")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) {
                    Button("Cancel") { dismiss() }
                }
            }
        }
    }

    private func fingerprintRow(_ fp: String) -> some View {
        Text(fp)
            .font(.mono(.caption))
            .foregroundStyle(theme.textColor)
            .textSelection(.enabled)
    }
}
