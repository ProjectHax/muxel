import Foundation

/// SSH auth supported on iOS. There is no ssh-agent on iOS, so muxel's
/// `SshAuth::Agent` is intentionally omitted — only `password` and `key`.
enum SshAuthKind: String, Codable, CaseIterable, Identifiable {
    case password
    case key

    var id: String { rawValue }
    var label: String { self == .password ? "Password" : "SSH key" }
}

/// A device-local SSH host (the iOS app's own copy; desktop's host library lives on
/// the desktop). Secrets — the password, or the imported private key + its
/// passphrase — live in the Keychain keyed by `id`, never in this Codable struct.
struct Host: Codable, Identifiable, Equatable, Hashable {
    var id = UUID()
    var name: String
    var hostname: String
    var port: Int? = nil
    var user: String = ""
    var auth: SshAuthKind = .password
    /// Whether the stored private key is passphrase-protected (the passphrase is in
    /// the Keychain). Only meaningful when `auth == .key`.
    var keyHasPassphrase: Bool = false
    /// `ProxyJump`-style jump host (`[user@]host[:port]`), applied as a nested SSH
    /// connection by the SSH layer.
    var jumpHost: String? = nil
    /// ServerAliveInterval equivalent (seconds) for the SSH keepalive.
    var keepaliveSecs: Int? = nil
    /// When set, this host's credentials (user, auth, key/password) come from the
    /// shared [`Identity`] with this id instead of the host's own inline fields.
    var identityId: UUID? = nil

    var displayPort: Int { port ?? 22 }
}

/// A reusable, named SSH login identity — the credential half of a host (user +
/// auth + optional passphrase-protected key) that many hosts can share via
/// `Host.identityId`. Secrets (password, or the imported key + passphrase) live in
/// the Keychain keyed by `id`, never in this struct — mirroring `Host`.
struct Identity: Codable, Identifiable, Equatable, Hashable {
    var id = UUID()
    var name: String
    var user: String = ""
    var auth: SshAuthKind = .password
    /// Whether the stored key is passphrase-protected (passphrase in the Keychain).
    var keyHasPassphrase: Bool = false
}

/// The credential a host connects with, resolved from its referenced identity (or
/// `nil` for the host's own inline fields). `secretOwner` is the Keychain owner id —
/// the identity's id, so hosts sharing an identity share one stored secret.
struct ResolvedCredential: Equatable {
    var user: String
    var auth: SshAuthKind
    var keyHasPassphrase: Bool
    var secretOwner: UUID
}

extension Host {
    /// The effective credential for this host given the identity library: `nil` when
    /// it uses its own inline fields, else the referenced identity's login + secret
    /// owner. Shared by the app-state and background-poller connection paths.
    func resolvedCredential(in identities: [Identity]) -> ResolvedCredential? {
        guard let iid = identityId, let id = identities.first(where: { $0.id == iid })
        else { return nil }
        return ResolvedCredential(user: id.user, auth: id.auth,
                                  keyHasPassphrase: id.keyHasPassphrase, secretOwner: id.id)
    }
}

/// A device-local remote project: a host + the absolute project root on that host.
/// Mirrors desktop's `Project.remote` (`RemoteRef`). The live layout is read from
/// `<remoteRoot>/.muxel/workspace.json`.
struct RemoteProject: Codable, Identifiable, Equatable, Hashable {
    var id = UUID()
    var name: String
    var hostId: UUID
    var remoteRoot: String
}

/// A launch template for the "new instance" sheet — a small on-device subset of
/// desktop's `AgentPreset` (program + args). Status markers are derived from
/// `program` via `defaultMarkers`, so they aren't stored here.
struct Preset: Codable, Identifiable, Equatable, Hashable {
    var id = UUID()
    var name: String
    var program: String?   // nil = the remote login shell
    var args: [String] = []

    /// Mirrors `AgentPreset::defaults` (names/programs), for the launch picker.
    static let builtins: [Preset] = [
        Preset(name: "Shell", program: nil),
        Preset(name: "Claude", program: "claude"),
        Preset(name: "opencode", program: "opencode"),
        Preset(name: "Amp", program: "amp"),
        Preset(name: "Grok", program: "grok"),
        Preset(name: "Hermes", program: "hermes"),
        Preset(name: "Ollama", program: "ollama", args: ["run", "llama3.2"]),
        Preset(name: "Ollama Code", program: "ollama",
               args: ["launch", "opencode", "--model", "glm-5.2:cloud"]),
        Preset(name: "Pi", program: "pi"),
    ]
}

/// The persisted on-device document (no secrets — those are in the Keychain).
struct StoreDocument: Codable, Equatable {
    var hosts: [Host] = []
    var projects: [RemoteProject] = []
    var identities: [Identity] = []

    init() {}

    /// Custom decoder so an older `store.json` (written before `identities` existed)
    /// still loads: synthesized `Decodable` would throw on the missing key and reset
    /// the whole document to empty. Each field defaults when absent.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        hosts = try c.decodeIfPresent([Host].self, forKey: .hosts) ?? []
        projects = try c.decodeIfPresent([RemoteProject].self, forKey: .projects) ?? []
        identities = try c.decodeIfPresent([Identity].self, forKey: .identities) ?? []
    }
}

/// Loads/saves `StoreDocument` as JSON under Application Support. Pure file I/O;
/// the observable app state wraps this.
struct LocalStore {
    private let url: URL

    init(filename: String = "store.json") {
        let dir = FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        url = dir.appendingPathComponent(filename)
    }

    func load() -> StoreDocument {
        guard let data = try? Data(contentsOf: url),
              let doc = try? JSONDecoder().decode(StoreDocument.self, from: data)
        else { return StoreDocument() }
        return doc
    }

    func save(_ doc: StoreDocument) {
        guard let data = try? JSONEncoder().encode(doc) else { return }
        try? data.write(to: url, options: .atomic)
    }
}
