import Foundation
import CryptoKit

/// Trust-on-first-use store for SSH server host keys (mirrors muxel's
/// `accept-new`). The first time we connect to a host we record its key
/// fingerprint; on later connects a mismatch is refused (`SSHError.hostKeyChanged`)
/// until the user explicitly re-trusts.
struct HostKeyStore {
    /// Which presented key a fingerprint belongs to: the target host itself, or the
    /// jump host (bastion) in front of it. Each gets its own TOFU slot under the
    /// same host id, so a bastion swap can't masquerade as the target (or vice
    /// versa).
    enum Scope: String, Sendable {
        case target
        case jump
    }

    private let defaults: UserDefaults
    init(defaults: UserDefaults = .standard) { self.defaults = defaults }

    private func key(_ host: UUID, _ scope: Scope) -> String {
        switch scope {
        case .target: return "hostkey:\(host.uuidString)"
        case .jump: return "hostkey:\(host.uuidString):jump"
        }
    }

    func fingerprint(for host: UUID, scope: Scope = .target) -> String? {
        defaults.string(forKey: key(host, scope))
    }
    func setFingerprint(_ fp: String, for host: UUID, scope: Scope = .target) {
        defaults.set(fp, forKey: key(host, scope))
    }
    /// Forget both the target and jump fingerprints (called when the host is deleted).
    func clear(for host: UUID) {
        defaults.removeObject(forKey: key(host, .target))
        defaults.removeObject(forKey: key(host, .jump))
    }

    /// `SHA256:<base64>` fingerprint of a raw SSH public-key blob — the same format
    /// OpenSSH prints. Compute this from the key the server presents in the SSH
    /// host-key validation callback, then compare/store.
    static func fingerprint(ofPublicKeyBlob blob: Data) -> String {
        let digest = SHA256.hash(data: blob)
        let b64 = Data(digest).base64EncodedString().trimmingCharacters(
            in: CharacterSet(charactersIn: "="))
        return "SHA256:\(b64)"
    }

    /// Validate a presented fingerprint against the stored one (TOFU): first use
    /// persists it silently; a later mismatch throws `SSHError.hostKeyChanged`.
    func validate(presented: String, for host: UUID, scope: Scope = .target) throws {
        if let known = fingerprint(for: host, scope: scope) {
            if known != presented {
                throw SSHError.hostKeyChanged(expected: known, got: presented, scope: scope)
            }
        } else {
            setFingerprint(presented, for: host, scope: scope) // trust on first use
        }
    }
}
