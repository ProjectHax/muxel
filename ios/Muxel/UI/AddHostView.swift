import SwiftUI
import UniformTypeIdentifiers

/// Form to add an SSH host. The password / private key + passphrase go straight to
/// the Keychain (via `AppState.addHost`); only non-secret metadata is persisted in
/// the on-device store.
struct AddHostView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.dismiss) private var dismiss

    @State private var name = ""
    @State private var hostname = ""
    @State private var port = ""
    @State private var user = ""
    @State private var auth: SshAuthKind = .password
    @State private var password = ""
    @State private var keyData: Data?
    @State private var keyName = ""
    @State private var passphrase = ""
    @State private var jumpHost = ""
    @State private var keepalive = ""
    @State private var importingKey = false
    @State private var identityId: UUID?

    private var usingIdentity: Bool { identityId != nil }

    private var canSave: Bool {
        !name.isEmpty && !hostname.isEmpty
            && (usingIdentity || (auth == .password ? !password.isEmpty : keyData != nil))
    }

    var body: some View {
        NavigationStack {
            Form {
                Section("Host") {
                    TextField("Name", text: $name)
                    TextField("Hostname or IP", text: $hostname)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                    TextField("Port (22)", text: $port).keyboardType(.numberPad)
                    if !usingIdentity {
                        TextField("User", text: $user)
                            .textInputAutocapitalization(.never).autocorrectionDisabled()
                    }
                }
                if !state.doc.identities.isEmpty {
                    Section("Credentials") {
                        Picker("Login", selection: $identityId) {
                            Text("Enter below").tag(UUID?.none)
                            ForEach(state.doc.identities) { id in
                                Text(id.name).tag(Optional(id.id))
                            }
                        }
                        if usingIdentity {
                            Text("Uses the selected identity's user, auth, and key/password.")
                                .font(.caption).foregroundStyle(.secondary)
                        }
                    }
                }
                if !usingIdentity {
                    Section("Authentication") {
                        Picker("Method", selection: $auth) {
                            ForEach(SshAuthKind.allCases) { Text($0.label).tag($0) }
                        }
                        if auth == .password {
                            SecureField("Password", text: $password)
                        } else {
                            Button { importingKey = true } label: {
                                Label(keyName.isEmpty ? "Import private key" : keyName,
                                      systemImage: "key.fill")
                            }
                            SecureField("Key passphrase (optional)", text: $passphrase)
                        }
                    }
                }
                Section("Advanced (optional)") {
                    TextField("Jump host  user@bastion", text: $jumpHost)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                    TextField("Keepalive seconds", text: $keepalive).keyboardType(.numberPad)
                }
            }
            .navigationTitle("Add host")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save", action: save).disabled(!canSave)
                }
            }
            .fileImporter(isPresented: $importingKey, allowedContentTypes: [.data, .text, .item]) { result in
                guard case let .success(url) = result else { return }
                let scoped = url.startAccessingSecurityScopedResource()
                defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                keyData = try? Data(contentsOf: url)
                keyName = url.lastPathComponent
            }
        }
    }

    private func save() {
        var host = Host(name: name, hostname: hostname)
        host.port = Int(port)
        host.jumpHost = jumpHost.isEmpty ? nil : jumpHost
        host.keepaliveSecs = Int(keepalive)
        host.identityId = identityId
        if usingIdentity {
            // Credentials come from the identity; store no host secrets.
            state.addHost(host, password: nil, keyData: nil, passphrase: nil)
        } else {
            host.user = user
            host.auth = auth
            host.keyHasPassphrase = auth == .key && !passphrase.isEmpty
            state.addHost(
                host,
                password: auth == .password ? password : nil,
                keyData: auth == .key ? keyData : nil,
                passphrase: auth == .key ? passphrase : nil
            )
        }
        dismiss()
    }
}
