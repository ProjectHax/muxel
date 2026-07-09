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
    /// `ProxyJump`-style jump host (`[user@]host[:port]`), tunneled through by the
    /// SSH layer (a direct-tcpip channel on the bastion, like `ssh -J`).
    var jumpHost: String? = nil
    /// Identity used to authenticate to `jumpHost`; nil = reuse this host's own
    /// resolved credential (a `user@` in the jump string still wins for the user).
    var jumpIdentityId: UUID? = nil
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

/// The pair of credentials a connection may need: the target host's and (when a
/// jump host is configured) the bastion's. Resolved by the app state / background
/// poller and handed to the connection factory together.
struct ConnectionCredentials {
    var target: ResolvedCredential?
    var jump: ResolvedCredential?

    static let none = ConnectionCredentials(target: nil, jump: nil)
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

    /// The bastion's credential, when a jump host is configured: the referenced
    /// jump identity if set, else "same as host" — the host's own effective
    /// credential (identity or inline fields). nil when there is no jump host.
    func resolvedJumpCredential(in identities: [Identity]) -> ResolvedCredential? {
        guard let jump = jumpHost, !jump.isEmpty else { return nil }
        if let jid = jumpIdentityId, let id = identities.first(where: { $0.id == jid }) {
            return ResolvedCredential(user: id.user, auth: id.auth,
                                      keyHasPassphrase: id.keyHasPassphrase, secretOwner: id.id)
        }
        if let same = resolvedCredential(in: identities) { return same }
        return ResolvedCredential(user: user, auth: auth,
                                  keyHasPassphrase: keyHasPassphrase, secretOwner: id)
    }

    /// Both credentials for one connection attempt.
    func connectionCredentials(in identities: [Identity]) -> ConnectionCredentials {
        ConnectionCredentials(target: resolvedCredential(in: identities),
                              jump: resolvedJumpCredential(in: identities))
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

/// A launch template for the "new instance" sheet — an on-device port of desktop's
/// `AgentPreset` (the fields the launch flow uses). Status markers are still derived
/// from `program` via `defaultMarkers`, so they aren't stored here.
struct Preset: Codable, Identifiable, Equatable, Hashable {
    var id = UUID()
    var name: String
    var program: String?   // nil = the remote login shell
    var args: [String] = []
    /// System-prompt / model / effort / session-resume, mirroring `AgentPreset`.
    var model: String?
    var modelFlag: String?
    var effort: String?
    var effortFlag: String?
    var systemPrompt: String?
    var injection: InjectionMode = .none
    var startupDelayMs: Int = 0
    var sessionIdFlag: String?
    var resumeFlag: String?

    /// Mirrors `AgentPreset::defaults` (name/program/flags/injection/resume), for the
    /// launch picker. Kept in sync with `crates/muxel-core/src/agent.rs`.
    static let builtins: [Preset] = [
        Preset(name: "Shell", program: nil),
        Preset(name: "Claude", program: "claude", modelFlag: "--model",
               injection: .cliFlag(flag: "--append-system-prompt"),
               sessionIdFlag: "--session-id", resumeFlag: "--resume"),
        Preset(name: "opencode", program: "opencode", modelFlag: "--model",
               injection: .typeIn, startupDelayMs: 6000),
        Preset(name: "Amp", program: "amp", injection: .typeIn),
        Preset(name: "Grok", program: "grok", modelFlag: "--model", injection: .typeIn),
        Preset(name: "Hermes", program: "hermes", modelFlag: "--model", injection: .typeIn),
        Preset(name: "Ollama", program: "ollama", args: ["run", "llama3.2"], injection: .typeIn),
        Preset(name: "Ollama Code", program: "ollama",
               args: ["launch", "opencode", "--model", "glm-5.2:cloud"],
               injection: .typeIn, startupDelayMs: 6000),
        Preset(name: "Pi", program: "pi", modelFlag: "--model", injection: .typeIn),
    ]
}

extension Preset {
    /// Defaults-tolerant decode so a persisted custom preset written before a new
    /// field existed still loads (the synthesized decoder would throw on the missing
    /// key). Builtins are hardcoded, so this only matters once custom presets persist.
    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = (try c.decodeIfPresent(UUID.self, forKey: .id)) ?? UUID()
        name = try c.decode(String.self, forKey: .name)
        program = try c.decodeIfPresent(String.self, forKey: .program)
        args = (try c.decodeIfPresent([String].self, forKey: .args)) ?? []
        model = try c.decodeIfPresent(String.self, forKey: .model)
        modelFlag = try c.decodeIfPresent(String.self, forKey: .modelFlag)
        effort = try c.decodeIfPresent(String.self, forKey: .effort)
        effortFlag = try c.decodeIfPresent(String.self, forKey: .effortFlag)
        systemPrompt = try c.decodeIfPresent(String.self, forKey: .systemPrompt)
        injection = (try c.decodeIfPresent(InjectionMode.self, forKey: .injection)) ?? .none
        startupDelayMs = (try c.decodeIfPresent(Int.self, forKey: .startupDelayMs)) ?? 0
        sessionIdFlag = try c.decodeIfPresent(String.self, forKey: .sessionIdFlag)
        resumeFlag = try c.decodeIfPresent(String.self, forKey: .resumeFlag)
    }
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

extension StoreDocument {
    /// Hosts whose login references `identity` (for delete-confirmation copy).
    func hosts(using identity: Identity) -> [Host] {
        hosts.filter { $0.identityId == identity.id }
    }

    /// Projects that live on `host` (for delete-confirmation copy).
    func projects(under host: Host) -> [RemoteProject] {
        projects.filter { $0.hostId == host.id }
    }
}

/// Errors from the on-device store — surfaced to the user, because a corrupt or
/// unwritable store must never silently lose the host/project list.
enum StoreError: LocalizedError {
    /// The store file existed but couldn't be decoded; it was preserved (renamed to
    /// `backup`), never deleted.
    case corrupt(backup: String, underlying: Error)
    case saveFailed(underlying: Error)

    var errorDescription: String? {
        switch self {
        case let .corrupt(backup, _):
            return "Your saved hosts and projects couldn't be read — the file was damaged. "
                + "It was kept as \(backup) in the app's data folder; starting with an "
                + "empty list. Credentials are unaffected (they live in the Keychain)."
        case let .saveFailed(underlying):
            return "Couldn't save your hosts and projects (\(underlying.localizedDescription)). "
                + "Recent changes may be lost when the app quits."
        }
    }
}

/// Loads/saves `StoreDocument` as JSON under Application Support. Pure file I/O;
/// the observable app state wraps this.
struct LocalStore {
    private let url: URL
    private static let corruptNoticeKey = "muxel.storeCorruptNotice"

    /// `directory` is injectable for tests; defaults to Application Support.
    init(directory: URL? = nil, filename: String = "store.json") {
        let dir = directory ?? FileManager.default
            .urls(for: .applicationSupportDirectory, in: .userDomainMask)[0]
        try? FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        url = dir.appendingPathComponent(filename)
    }

    /// Load the store. A missing file is a normal first launch (empty document). An
    /// unreadable/undecodable file is **preserved** as `<filename>.corrupt` (never
    /// deleted), a pending notice is recorded so the app can tell the user even when
    /// the background poller tripped it first, and `StoreError.corrupt` is thrown.
    func load() throws -> StoreDocument {
        guard FileManager.default.fileExists(atPath: url.path) else { return StoreDocument() }
        do {
            let data = try Data(contentsOf: url)
            return try JSONDecoder().decode(StoreDocument.self, from: data)
        } catch {
            let backup = url.lastPathComponent + ".corrupt"
            let backupURL = url.deletingLastPathComponent().appendingPathComponent(backup)
            try? FileManager.default.removeItem(at: backupURL)
            try? FileManager.default.moveItem(at: url, to: backupURL)
            let wrapped = StoreError.corrupt(backup: backup, underlying: error)
            UserDefaults.standard.set(wrapped.localizedDescription,
                                      forKey: Self.corruptNoticeKey)
            throw wrapped
        }
    }

    func save(_ doc: StoreDocument) throws {
        do {
            let data = try JSONEncoder().encode(doc)
            try data.write(to: url, options: .atomic)
        } catch {
            throw StoreError.saveFailed(underlying: error)
        }
    }

    /// One-shot pending corruption notice (set when `load()` renamed a damaged
    /// file). Whichever process path hit the corruption first — the app or a
    /// background task — records it; the app takes and shows it on next launch.
    static func takeCorruptNotice() -> String? {
        let d = UserDefaults.standard
        guard let msg = d.string(forKey: corruptNoticeKey) else { return nil }
        d.removeObject(forKey: corruptNoticeKey)
        return msg
    }
}
