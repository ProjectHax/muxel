import Foundation
import Citadel
import NIOCore
import NIOSSH

/// Citadel-backed `SSHConnection`. The exec (`run`) path is the load-bearing one
/// for v1 (it powers reading `.muxel/workspace.json`, polling tmux status, and
/// launching sessions). Live PTY attach for the terminal view is handled separately
/// in the terminal layer.
///
/// SPIKE (per the plan's step 1): a few Citadel symbols below must be confirmed
/// against the resolved package version — they're marked `// SPIKE:`. Validate on a
/// Mac with a real host before wiring the UI. The protocol boundary means nothing
/// else in the app depends on these specifics.
final class CitadelSSHConnection: SSHConnection {
    private let host: Host
    private let hostKeys: HostKeyStore
    private var client: SSHClient?

    init(host: Host, hostKeys: HostKeyStore = HostKeyStore()) {
        self.host = host
        self.hostKeys = hostKeys
    }

    var isConnected: Bool { client != nil }

    func connect() async throws {
        if client != nil { return }
        let auth = try authenticationMethod()

        // SPIKE: confirm SSHClient.connect signature + the host-key validator API.
        // For TOFU we want a custom validator that computes the presented key's
        // SHA256 fingerprint and runs it through `hostKeys.validate(presented:for:)`.
        // If Citadel doesn't expose the key blob to a closure validator in this
        // version, temporarily use `.acceptAnything()` and finish TOFU in the spike
        // (tracked as a security follow-up — do not ship without it).
        let validator: SSHHostKeyValidator = .acceptAnything() // SPIKE: replace with TOFU validator

        // SPIKE: jump host (`host.jumpHost`) — Citadel reaches a target through a
        // bastion by connecting to the bastion first and opening a direct-tcpip
        // channel. Wire this when a jumpHost is set; single-hop works without it.
        client = try await SSHClient.connect(
            host: host.hostname,
            port: host.displayPort,
            authenticationMethod: auth,
            hostKeyValidator: validator,
            reconnect: .never
        )
    }

    func run(_ command: String) async throws -> String {
        guard let client else { throw SSHError.notConnected }
        do {
            let buffer = try await client.executeCommand(command)
            return buffer.getString(at: buffer.readerIndex, length: buffer.readableBytes) ?? ""
        } catch {
            throw SSHError.command(error.localizedDescription)
        }
    }

    func close() async {
        try? await client?.close()
        client = nil
    }

    // MARK: Auth

    private func authenticationMethod() throws -> SSHAuthenticationMethod {
        let user = host.user.isEmpty ? "root" : host.user
        switch host.auth {
        case .password:
            guard let pw = Keychain.password(for: host.id) else { throw SSHError.missingCredential }
            return .passwordBased(username: user, password: pw)

        case .key:
            guard let keyData = Keychain.privateKey(for: host.id) else { throw SSHError.missingCredential }
            // SPIKE: parse the user's OpenSSH/PEM private key (RSA / ed25519 / ECDSA,
            // optionally passphrase-protected from `Keychain.keyPassphrase(for:)`) into
            // the type Citadel's `.rsa` / `.ed25519` / `.ecdsa*` auth methods expect.
            // OpenSSH-format parsing with passphrase is the part to validate in the
            // spike; pick the Citadel/NIOSSH key initializer for the key type.
            _ = keyData
            throw SSHError.auth("SSH key auth pending spike (see CitadelSSHConnection)")
        }
    }
}
