import SwiftUI
import UniformTypeIdentifiers

/// The save/test gate for the host editor, extracted pure so the auth-switch edge
/// cases are unit-testable. `existingAuth` is non-nil only when editing a host that
/// has its *own* stored inline secret (not an identity reference) — the one case
/// where "no new secret entered" still authenticates.
enum HostEditorLogic {
    /// Whether the form state names a usable credential: a picked identity, a newly
    /// entered secret, or (editing, same auth method) the stored one. Switching the
    /// auth method on edit requires a new secret — the stored one lives in the other
    /// slot and doesn't apply.
    static func hasUsableCredential(usingIdentity: Bool, auth: SshAuthKind,
                                    existingAuth: SshAuthKind?,
                                    hasPassword: Bool, hasKey: Bool) -> Bool {
        if usingIdentity { return true }
        if let existingAuth, auth == existingAuth { return true }
        return auth == .password ? hasPassword : hasKey
    }

    static func canSave(name: String, hostname: String, usingIdentity: Bool,
                        auth: SshAuthKind, existingAuth: SshAuthKind?,
                        hasPassword: Bool, hasKey: Bool) -> Bool {
        !name.isEmpty && !hostname.isEmpty
            && hasUsableCredential(usingIdentity: usingIdentity, auth: auth,
                                   existingAuth: existingAuth,
                                   hasPassword: hasPassword, hasKey: hasKey)
    }

    /// Test needs a reachable target + a usable credential, but no name yet.
    static func canTest(hostname: String, usingIdentity: Bool, auth: SshAuthKind,
                        existingAuth: SshAuthKind?,
                        hasPassword: Bool, hasKey: Bool) -> Bool {
        !hostname.isEmpty
            && hasUsableCredential(usingIdentity: usingIdentity, auth: auth,
                                   existingAuth: existingAuth,
                                   hasPassword: hasPassword, hasKey: hasKey)
    }
}

/// Add **or edit** an SSH host. The password / private key + passphrase go straight
/// to the Keychain (via `AppState.addHost`/`updateHost`); only non-secret metadata
/// is persisted in the on-device store. On edit, secrets are never read back into
/// the form — a blank password / un-replaced key keeps what's stored, mirroring
/// `IdentityEditorView`.
struct HostEditorView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.theme) private var theme
    @Environment(\.dismiss) private var dismiss
    /// nil = add a new host.
    let existing: Host?
    /// Called with the freshly-added host after an add-mode save (not on edit), so
    /// the caller can chain straight into "Scan for projects".
    var onSaved: ((Host) -> Void)? = nil

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
    @State private var jumpIdentityId: UUID?
    @State private var keepalive = ""
    @State private var importingKey = false
    @State private var identityId: UUID?
    /// After Save with inline credentials (add only), offer to save them as a
    /// reusable identity.
    @State private var askSaveIdentity = false
    @State private var identityName = ""
    @State private var testState: FormTestState = .idle

    enum FormTestState: Equatable {
        case idle, testing
        case passed
        case failed(String)
    }

    private var isEdit: Bool { existing != nil }
    private var usingIdentity: Bool { identityId != nil }

    /// The stored inline auth that still applies without a new secret — only when
    /// editing a host that authenticates with its own credential (not an identity).
    private var existingInlineAuth: SshAuthKind? {
        guard let existing, existing.identityId == nil else { return nil }
        return existing.auth
    }

    /// A jump host is optional, but a non-empty one must parse — a typo should
    /// block Save, not silently connect direct.
    private var jumpValid: Bool {
        jumpHost.isEmpty || JumpHostSpec.parse(jumpHost) != nil
    }

    private var canSave: Bool {
        jumpValid && HostEditorLogic.canSave(
            name: name, hostname: hostname,
            usingIdentity: usingIdentity, auth: auth,
            existingAuth: existingInlineAuth,
            hasPassword: !password.isEmpty, hasKey: keyData != nil)
    }

    private var canTest: Bool {
        jumpValid && HostEditorLogic.canTest(
            hostname: hostname, usingIdentity: usingIdentity,
            auth: auth, existingAuth: existingInlineAuth,
            hasPassword: !password.isEmpty, hasKey: keyData != nil)
    }

    var body: some View {
        NavigationStack {
            Form {
                MuxelSection("Host") {
                    TextField("Name", text: $name)
                    TextField("Hostname or IP", text: $hostname)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                    TextField("Port (22)", text: $port).keyboardType(.numberPad)
                    if !usingIdentity {
                        TextField("User", text: $user)
                            .textInputAutocapitalization(.never).autocorrectionDisabled()
                            .noPasswordAutoFill()
                    }
                }
                if !state.doc.identities.isEmpty {
                    MuxelSection("Credentials") {
                        Picker("Login", selection: $identityId) {
                            Text("Enter below").tag(UUID?.none)
                            ForEach(state.doc.identities) { id in
                                Text(id.name).tag(Optional(id.id))
                            }
                        }
                        if usingIdentity {
                            Text("Uses the selected identity's user, auth, and key/password.")
                                .font(.mono(.caption))
                                .foregroundStyle(theme.mutedColor)
                        }
                    }
                }
                if !usingIdentity {
                    MuxelSection("Authentication") {
                        Picker("Method", selection: $auth) {
                            ForEach(SshAuthKind.allCases) { Text($0.label).tag($0) }
                        }
                        if auth == .password {
                            SecureField(isEdit ? "New password (blank = keep)" : "Password",
                                        text: $password)
                                .noPasswordAutoFill()
                        } else {
                            Button { importingKey = true } label: {
                                Label(keyLabel, systemImage: "key.fill")
                            }
                            SecureField("Key passphrase (optional)", text: $passphrase)
                                .noPasswordAutoFill()
                        }
                    }
                }
                MuxelSection("Advanced (optional)") {
                    TextField("Jump host  user@bastion", text: $jumpHost)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                    if !jumpValid {
                        Label("can't parse — use user@host:port (no chains)",
                              systemImage: "exclamationmark.triangle")
                            .font(.mono(.caption))
                            .foregroundStyle(theme.blockedColor)
                    }
                    if !jumpHost.isEmpty, !state.doc.identities.isEmpty {
                        Picker("Bastion login", selection: $jumpIdentityId) {
                            Text("Same as host").tag(UUID?.none)
                            ForEach(state.doc.identities) { id in
                                Text(id.name).tag(Optional(id.id))
                            }
                        }
                    }
                    TextField("Keepalive seconds", text: $keepalive).keyboardType(.numberPad)
                }
                testSection
                if let existing, existing.name != name, !name.isEmpty {
                    MuxelSection {
                        EmptyView()
                    } footer: {
                        Text("Renaming changes the tmux session prefix for new panes; "
                            + "existing sessions keep working (they're matched by id).")
                    }
                }
            }
            .muxelSheet()
            .navigationTitle(isEdit ? "Edit host" : "Add host")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save", action: attemptSave).disabled(!canSave)
                }
            }
            .fileImporter(isPresented: $importingKey, allowedContentTypes: [.data, .text, .item]) { result in
                guard let file = ImportedFile.read(result) else { return }
                keyData = file.data
                keyName = file.name
            }
            .alert("Reuse this login?", isPresented: $askSaveIdentity) {
                TextField("Identity name", text: $identityName)
                Button("Save as identity") { saveAsIdentity(); dismiss() }
                Button("Just this host") { saveHostOnly(); dismiss() }
                Button("Cancel", role: .cancel) {}
            } message: {
                Text("Save these credentials as a reusable login identity so other "
                    + "hosts can use them without re-entering.")
            }
            .onAppear(perform: prefill)
            .onChange(of: formFingerprint) { _ in testState = .idle }
        }
    }

    /// Every input that affects a connection attempt — a change invalidates the
    /// last in-form test result.
    private var formFingerprint: [String] {
        [hostname, port, user, auth.rawValue, password, keyName, passphrase,
         jumpHost, jumpIdentityId?.uuidString ?? "", identityId?.uuidString ?? ""]
    }

    @ViewBuilder private var testSection: some View {
        MuxelSection {
            Button(action: runTest) {
                if testState == .testing {
                    HStack(spacing: 8) {
                        ProgressView()
                        Text("Testing…")
                    }
                } else {
                    Label("Test connection", systemImage: "bolt.horizontal.circle")
                }
            }
            .disabled(!canTest || testState == .testing)

            switch testState {
            case .passed:
                Text("✓ connected")
                    .font(.mono(.footnote, weight: .semibold))
                    .foregroundStyle(theme.runningColor)
            case let .failed(message):
                Text("✗ \(message)")
                    .font(.mono(.footnote))
                    .foregroundStyle(theme.blockedColor)
            case .idle, .testing:
                EmptyView()
            }
        } footer: {
            Text("Tries the connection with the form's credentials without saving anything.")
        }
    }

    private func runTest() {
        testState = .testing
        let draft = makeHost(withCredentials: !usingIdentity)
        let idRef = identityId
        let pw = auth == .password ? (password.isEmpty ? nil : password) : nil
        let kd = auth == .key ? keyData : nil
        let pp = auth == .key ? (passphrase.isEmpty ? nil : passphrase) : nil
        Task {
            let result = await state.testConnection(draft: draft, identityId: idRef,
                                                    password: pw, keyData: kd,
                                                    passphrase: pp)
            testState = result.ok ? .passed : .failed(result.message)
        }
    }

    private func prefill() {
        guard let e = existing else { return }
        name = e.name
        hostname = e.hostname
        port = e.port.map(String.init) ?? ""
        user = e.user
        auth = e.auth
        jumpHost = e.jumpHost ?? ""
        jumpIdentityId = e.jumpIdentityId
        keepalive = e.keepaliveSecs.map(String.init) ?? ""
        identityId = e.identityId
    }

    private var keyLabel: String {
        if !keyName.isEmpty { return keyName }
        return isEdit ? "Replace private key" : "Import private key"
    }

    /// Non-credential host fields (shared by every save/test path). Editing keeps
    /// the existing id so Keychain secrets and pooled connections stay attached.
    private func makeHost(withCredentials: Bool) -> Host {
        var host = existing ?? Host(name: name, hostname: hostname)
        host.name = name
        host.hostname = hostname
        host.port = Int(port)
        host.jumpHost = jumpHost.isEmpty ? nil : jumpHost
        host.jumpIdentityId = jumpHost.isEmpty ? nil : jumpIdentityId
        host.keepaliveSecs = Int(keepalive)
        if withCredentials {
            host.user = user
            host.auth = auth
            host.keyHasPassphrase = auth == .key
                && (!passphrase.isEmpty
                    || (keyData == nil && (existing?.keyHasPassphrase ?? false)))
        }
        return host
    }

    private func attemptSave() {
        if isEdit || usingIdentity {
            // Editing, or already using a shared identity — nothing new to offer.
            saveHostOnly()
            dismiss()
        } else {
            // Inline credentials entered — offer to make them a reusable identity.
            identityName = name
            askSaveIdentity = true
        }
    }

    /// Save the host with its own inline credentials (or referencing the picked
    /// identity, in which case no host secret is stored).
    private func saveHostOnly() {
        var host = makeHost(withCredentials: !usingIdentity)
        host.identityId = identityId
        let pw = (!usingIdentity && auth == .password && !password.isEmpty) ? password : nil
        let kd = (!usingIdentity && auth == .key) ? keyData : nil
        let pp = (!usingIdentity && auth == .key && !passphrase.isEmpty) ? passphrase : nil
        if isEdit {
            state.updateHost(host, password: pw, keyData: kd, passphrase: pp)
        } else {
            state.addHost(host, password: pw, keyData: kd, passphrase: pp)
            onSaved?(host)
        }
    }

    /// Save the entered credentials as a new shared identity, then add the host
    /// referencing it (so the secret lives once, under the identity). Add-only.
    private func saveAsIdentity() {
        var identity = Identity(name: identityName.isEmpty ? name : identityName)
        identity.user = user
        identity.auth = auth
        identity.keyHasPassphrase = auth == .key && !passphrase.isEmpty
        state.addIdentity(
            identity,
            password: auth == .password ? password : nil,
            keyData: auth == .key ? keyData : nil,
            passphrase: auth == .key ? passphrase : nil
        )
        var host = makeHost(withCredentials: false)
        host.identityId = identity.id
        state.addHost(host, password: nil, keyData: nil, passphrase: nil)
        onSaved?(host)
    }
}
