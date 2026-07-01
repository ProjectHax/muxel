import Foundation
import NIOCore
import NIOSSH

/// NIOSSH server-authentication delegate enforcing trust-on-first-use via
/// `HostKeyStore`: the first key a host (or its bastion, `scope: .jump`) presents
/// is fingerprinted and stored silently; a *different* key on a later connect fails
/// the handshake with `SSHError.hostKeyChanged`, which surfaces as the trust prompt
/// (old vs new fingerprint) instead of connecting.
///
/// Note the TOFU half persists the first fingerprint even if authentication later
/// fails — the same behavior as OpenSSH `accept-new`.
///
/// Runs on the NIO event loop; `HostKeyStore` is UserDefaults-backed (thread-safe).
struct TOFUHostKeyValidator: NIOSSHClientServerAuthenticationDelegate {
    let hostId: UUID
    let scope: HostKeyStore.Scope
    let store: HostKeyStore

    /// OpenSSH-format `SHA256:…` fingerprint of a presented key: the standard wire
    /// blob (`NIOSSHPublicKey.write`) hashed exactly the way `ssh-keygen -lf` prints.
    static func fingerprint(of hostKey: NIOSSHPublicKey) -> String {
        var buf = ByteBufferAllocator().buffer(capacity: 256)
        _ = hostKey.write(to: &buf)
        return HostKeyStore.fingerprint(ofPublicKeyBlob: Data(buf.readableBytesView))
    }

    func validateHostKey(hostKey: NIOSSHPublicKey,
                         validationCompletePromise: EventLoopPromise<Void>) {
        do {
            try store.validate(presented: Self.fingerprint(of: hostKey),
                               for: hostId, scope: scope)
            validationCompletePromise.succeed(())
        } catch {
            validationCompletePromise.fail(error)
        }
    }
}
