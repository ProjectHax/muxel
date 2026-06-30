import Foundation
import Security

/// Keychain storage for SSH secrets, keyed by host id. Items use
/// `kSecAttrAccessibleAfterFirstUnlock` so the background poller can read them while
/// the device is locked (but not before first unlock after boot).
///
/// Mirrors the desktop's keychain layout conceptually (per-host secret), but is a
/// separate device-local store — secrets do not sync from desktop.
enum Keychain {
    private static let service = "dev.muxel.ios"

    enum Slot {
        case password(UUID)
        case privateKey(UUID)
        case keyPassphrase(UUID)

        var account: String {
            switch self {
            case let .password(id): return "password:\(id.uuidString)"
            case let .privateKey(id): return "key:\(id.uuidString)"
            case let .keyPassphrase(id): return "keypass:\(id.uuidString)"
            }
        }
    }

    // MARK: Typed helpers

    @discardableResult
    static func setPassword(_ password: String, for host: UUID) -> Bool {
        setData(Data(password.utf8), slot: .password(host))
    }
    static func password(for host: UUID) -> String? {
        data(slot: .password(host)).flatMap { String(data: $0, encoding: .utf8) }
    }

    /// Store the raw private-key bytes (PEM/OpenSSH format) for `host`.
    @discardableResult
    static func setPrivateKey(_ key: Data, for host: UUID) -> Bool {
        setData(key, slot: .privateKey(host))
    }
    static func privateKey(for host: UUID) -> Data? {
        data(slot: .privateKey(host))
    }

    @discardableResult
    static func setKeyPassphrase(_ passphrase: String, for host: UUID) -> Bool {
        setData(Data(passphrase.utf8), slot: .keyPassphrase(host))
    }
    static func keyPassphrase(for host: UUID) -> String? {
        data(slot: .keyPassphrase(host)).flatMap { String(data: $0, encoding: .utf8) }
    }

    /// Remove every secret for a host (called when the host is deleted).
    static func deleteAll(for host: UUID) {
        for slot in [Slot.password(host), .privateKey(host), .keyPassphrase(host)] {
            delete(slot: slot)
        }
    }

    // MARK: Generic item ops

    private static func baseQuery(_ slot: Slot) -> [String: Any] {
        [
            kSecClass as String: kSecClassGenericPassword,
            kSecAttrService as String: service,
            kSecAttrAccount as String: slot.account,
        ]
    }

    /// Write `data` to `slot`, replacing any existing item. Returns whether the write
    /// succeeded (so callers can surface a Keychain failure instead of silently
    /// dropping the secret).
    @discardableResult
    static func setData(_ data: Data, slot: Slot) -> Bool {
        var query = baseQuery(slot)
        // Replace any existing item.
        SecItemDelete(query as CFDictionary)
        query[kSecValueData as String] = data
        query[kSecAttrAccessible as String] = kSecAttrAccessibleAfterFirstUnlock
        return SecItemAdd(query as CFDictionary, nil) == errSecSuccess
    }

    static func data(slot: Slot) -> Data? {
        var query = baseQuery(slot)
        query[kSecReturnData as String] = true
        query[kSecMatchLimit as String] = kSecMatchLimitOne
        var out: CFTypeRef?
        let status = SecItemCopyMatching(query as CFDictionary, &out)
        guard status == errSecSuccess else { return nil }
        return out as? Data
    }

    static func delete(slot: Slot) {
        SecItemDelete(baseQuery(slot) as CFDictionary)
    }
}
