import Foundation
import CryptoKit

/// Trust-on-first-use store for SSH server host keys (mirrors muxel's
/// `accept-new`). The first time we connect to a host we record its key
/// fingerprint; on later connects a mismatch is refused (`SSHError.hostKeyChanged`)
/// until the user explicitly re-trusts.
struct HostKeyStore {
    private let defaults: UserDefaults
    init(defaults: UserDefaults = .standard) { self.defaults = defaults }

    private func key(_ host: UUID) -> String { "hostkey:\(host.uuidString)" }

    func fingerprint(for host: UUID) -> String? { defaults.string(forKey: key(host)) }
    func setFingerprint(_ fp: String, for host: UUID) { defaults.set(fp, forKey: key(host)) }
    func clear(for host: UUID) { defaults.removeObject(forKey: key(host)) }

    /// `SHA256:<base64>` fingerprint of a raw SSH public-key blob — the same format
    /// OpenSSH prints. Compute this from the key the server presents in the SSH
    /// host-key validation callback, then compare/store.
    static func fingerprint(ofPublicKeyBlob blob: Data) -> String {
        let digest = SHA256.hash(data: blob)
        let b64 = Data(digest).base64EncodedString().trimmingCharacters(
            in: CharacterSet(charactersIn: "="))
        return "SHA256:\(b64)"
    }

    /// Validate a presented fingerprint against the stored one (TOFU). Returns the
    /// fingerprint to persist on success; throws on a changed key.
    func validate(presented: String, for host: UUID) throws {
        if let known = fingerprint(for: host) {
            if known != presented {
                throw SSHError.hostKeyChanged(expected: known, got: presented)
            }
        } else {
            setFingerprint(presented, for: host) // trust on first use
        }
    }
}
