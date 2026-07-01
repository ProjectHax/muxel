import SwiftUI
import UniformTypeIdentifiers

/// Manage reusable **login identities** — shared SSH credentials (user + auth +
/// key/password) a host can reference instead of storing its own. Presented as a
/// sheet from the sidebar. Secrets go to the Keychain via `AppState`.
struct IdentitiesView: View {
    @EnvironmentObject var state: AppState
    @EnvironmentObject var appLock: AppLock
    @Environment(\.theme) private var theme
    @Environment(\.dismiss) private var dismiss
    @State private var editing: Identity?
    @State private var addingNew = false
    @State private var deleteTarget: Identity?

    var body: some View {
        NavigationStack {
            List {
                MuxelSection {
                    if state.doc.identities.isEmpty {
                        Text("No identities yet. Add one to reuse a login across hosts.")
                            .font(.mono(.footnote))
                            .foregroundStyle(theme.mutedColor)
                    }
                    ForEach(state.doc.identities) { id in
                        Button { editing = id } label: {
                            HStack(spacing: 10) {
                                Image(systemName: "person.badge.key")
                                    .foregroundStyle(theme.mutedColor)
                                VStack(alignment: .leading, spacing: 1) {
                                    Text(id.name)
                                        .font(.mono(.callout))
                                    Text("\(id.user.isEmpty ? "default user" : id.user) · \(id.auth.label)")
                                        .font(.mono(.caption))
                                        .foregroundStyle(theme.mutedColor)
                                }
                            }
                        }
                        .tint(theme.textColor)
                    }
                    .onDelete { offsets in
                        // Confirm one at a time (swipe-delete passes a single offset).
                        deleteTarget = offsets.map { state.doc.identities[$0] }.first
                    }
                }
                securitySection
            }
            .muxelSheet()
            .navigationTitle("Identities")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Done") { dismiss() } }
                ToolbarItem(placement: .primaryAction) {
                    Button { addingNew = true } label: { Label("Add identity", systemImage: "plus") }
                }
            }
            .sheet(isPresented: $addingNew) { IdentityEditorView(existing: nil) }
            .sheet(item: $editing) { id in IdentityEditorView(existing: id) }
            .confirmationDialog(
                deleteTarget.map {
                    ConfirmationCopy.deleteIdentity($0, hostCount: state.doc.hosts(using: $0).count).title
                } ?? "Delete identity?",
                isPresented: Binding(
                    get: { deleteTarget != nil },
                    set: { if !$0 { deleteTarget = nil } }
                ),
                titleVisibility: .visible,
                presenting: deleteTarget
            ) { identity in
                Button("Delete identity", role: .destructive) {
                    state.deleteIdentity(identity)
                    deleteTarget = nil
                }
                Button("Cancel", role: .cancel) { deleteTarget = nil }
            } message: { identity in
                Text(ConfirmationCopy.deleteIdentity(
                    identity, hostCount: state.doc.hosts(using: identity).count).message)
            }
        }
    }

    /// App Lock lives here: this sheet is the app's credential manager, the natural
    /// home for the toggle that protects those credentials' UI.
    private var securitySection: some View {
        MuxelSection("Security") {
            Toggle("Require Face ID / passcode to open muxel", isOn: appLockBinding)
                .disabled(!appLock.isAvailable)
        } footer: {
            Text(appLock.isAvailable
                ? "Protects the app UI. Background status polling and notifications "
                    + "keep working while locked."
                : "Set a device passcode to use App Lock.")
        }
    }

    private var appLockBinding: Binding<Bool> {
        Binding(
            get: { appLock.isEnabled },
            set: { on in
                appLock.isEnabled = on
                if on {
                    // Prove the user can pass before the lock can ever engage;
                    // a failed/canceled check rolls the toggle back.
                    Task { await appLock.confirmEnable() }
                }
            }
        )
    }
}

/// Add or edit a single login identity. On edit, a blank password / no re-imported
/// key leaves the stored secret untouched.
struct IdentityEditorView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.dismiss) private var dismiss
    let existing: Identity?

    @State private var name = ""
    @State private var user = ""
    @State private var auth: SshAuthKind = .password
    @State private var password = ""
    @State private var keyData: Data?
    @State private var keyName = ""
    @State private var passphrase = ""
    @State private var importingKey = false

    private var isEdit: Bool { existing != nil }

    private var canSave: Bool {
        // On edit the stored secret still applies — but only for the *same* auth
        // method; switching methods needs a new secret (the stored one lives in
        // the other Keychain slot).
        guard !name.isEmpty else { return false }
        let hasNewSecret = auth == .password ? !password.isEmpty : keyData != nil
        if let existing { return auth == existing.auth || hasNewSecret }
        return hasNewSecret
    }

    var body: some View {
        NavigationStack {
            Form {
                MuxelSection("Identity") {
                    TextField("Name", text: $name)
                    TextField("User", text: $user)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                }
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
            .muxelSheet()
            .navigationTitle(isEdit ? "Edit identity" : "Add identity")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .cancellationAction) { Button("Cancel") { dismiss() } }
                ToolbarItem(placement: .confirmationAction) {
                    Button("Save", action: save).disabled(!canSave)
                }
            }
            .fileImporter(isPresented: $importingKey,
                          allowedContentTypes: [.data, .text, .item]) { result in
                guard let file = ImportedFile.read(result) else { return }
                keyData = file.data
                keyName = file.name
            }
            .onAppear {
                if let e = existing {
                    name = e.name
                    user = e.user
                    auth = e.auth
                }
            }
        }
    }

    private var keyLabel: String {
        if !keyName.isEmpty { return keyName }
        return isEdit ? "Replace private key" : "Import private key"
    }

    private func save() {
        var id = existing ?? Identity(name: name)
        id.name = name
        id.user = user
        id.auth = auth
        // Keep the passphrase flag when editing a key that isn't being replaced.
        let keepingKey = isEdit && keyData == nil
        id.keyHasPassphrase = auth == .key
            && (!passphrase.isEmpty || (keepingKey && (existing?.keyHasPassphrase ?? false)))

        let pw: String? = auth == .password ? (password.isEmpty ? nil : password) : nil
        let kd: Data? = auth == .key ? keyData : nil
        let pp: String? = auth == .key ? (passphrase.isEmpty ? nil : passphrase) : nil
        if existing == nil {
            state.addIdentity(id, password: pw, keyData: kd, passphrase: pp)
        } else {
            state.updateIdentity(id, password: pw, keyData: kd, passphrase: pp)
        }
        dismiss()
    }
}
