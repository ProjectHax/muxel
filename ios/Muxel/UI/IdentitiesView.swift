import SwiftUI
import UniformTypeIdentifiers

/// Manage reusable **login identities** — shared SSH credentials (user + auth +
/// key/password) a host can reference instead of storing its own. Presented as a
/// sheet from the sidebar. Secrets go to the Keychain via `AppState`.
struct IdentitiesView: View {
    @EnvironmentObject var state: AppState
    @Environment(\.dismiss) private var dismiss
    @State private var editing: Identity?
    @State private var addingNew = false

    var body: some View {
        NavigationStack {
            List {
                if state.doc.identities.isEmpty {
                    Text("No identities yet. Add one to reuse a login across hosts.")
                        .foregroundStyle(.secondary)
                }
                ForEach(state.doc.identities) { id in
                    Button { editing = id } label: {
                        HStack(spacing: 10) {
                            Image(systemName: "person.badge.key").foregroundStyle(.secondary)
                            VStack(alignment: .leading, spacing: 1) {
                                Text(id.name)
                                Text("\(id.user.isEmpty ? "default user" : id.user) · \(id.auth.label)")
                                    .font(.caption).foregroundStyle(.secondary)
                            }
                        }
                    }
                    .tint(.primary)
                }
                .onDelete { offsets in
                    offsets.map { state.doc.identities[$0] }.forEach(state.deleteIdentity)
                }
            }
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
        }
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
        // On edit the secret already exists, so a name change alone is savable.
        !name.isEmpty && (isEdit || (auth == .password ? !password.isEmpty : keyData != nil))
    }

    var body: some View {
        NavigationStack {
            Form {
                Section("Identity") {
                    TextField("Name", text: $name)
                    TextField("User", text: $user)
                        .textInputAutocapitalization(.never).autocorrectionDisabled()
                }
                Section("Authentication") {
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
                guard case let .success(url) = result else { return }
                let scoped = url.startAccessingSecurityScopedResource()
                defer { if scoped { url.stopAccessingSecurityScopedResource() } }
                keyData = try? Data(contentsOf: url)
                keyName = url.lastPathComponent
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
