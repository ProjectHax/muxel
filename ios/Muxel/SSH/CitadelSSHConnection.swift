import Foundation
import Citadel
import Crypto
import NIOCore
import NIOSSH

/// Citadel-backed `SSHConnection`. The exec (`run`) path is the load-bearing one
/// for v1 (it powers reading `.muxel/workspace.json`, polling tmux status, and
/// launching sessions). Live PTY attach for the terminal view is handled separately
/// in the terminal layer.
///
/// One connection is shared by the foreground poll loop, every visible terminal's
/// capture loop, and layout/launch calls — so it serializes connects behind a single
/// in-flight task (two overlapping handshakes can race into NIO
/// `ChannelError.connectPending`) and transparently reconnects a dropped channel.
///
/// Auth: password and SSH key (ed25519 / RSA, with optional passphrase) are wired to
/// Citadel here. ECDSA keys are detected but not yet parseable by the resolved
/// Citadel; we surface a clear "use ed25519/RSA" error for those.
///
/// SPIKE remaining: host-key validation is still `.acceptAnything()` (the TOFU
/// validator in `HostKeyStore` is a security follow-up), and jump-host
/// (`host.jumpHost`) support is unimplemented.
actor CitadelSSHConnection: SSHConnection {
    private let host: Host
    /// Resolved shared-identity credential, if the host references one. When set, it
    /// overrides the host's inline user/auth and names the Keychain secret owner.
    private let credential: ResolvedCredential?
    private let hostKeys: HostKeyStore
    private var client: SSHClient?
    /// In-flight connect, so concurrent callers share one handshake (actor isolation
    /// makes the check-and-set of this and `client` race-free).
    private var connecting: Task<SSHClient, Error>?
    /// Async mutex serializing command execution. Citadel's exec channels don't
    /// tolerate many concurrent commands over one connection (intermittent channel
    /// errors), so the poll loop, terminal capture loops, and launch/layout calls run
    /// one at a time rather than racing channels.
    private var commandBusy = false
    private var commandWaiters: [CheckedContinuation<Void, Never>] = []

    init(host: Host, credential: ResolvedCredential? = nil, hostKeys: HostKeyStore = HostKeyStore()) {
        self.host = host
        self.credential = credential
        self.hostKeys = hostKeys
    }

    func connect() async throws {
        _ = try await connectedClient()
    }

    func run(_ command: String) async throws -> String {
        await acquireCommandSlot()
        defer { releaseCommandSlot() }
        do {
            return try await execute(command)
        } catch {
            // A dead TCP transport (iOS backgrounding, server idle-close): drop the
            // client so the *next* call reconnects. We fail this call fast rather than
            // awaiting a fresh handshake inline (which can take up to the connect
            // timeout and read as a hang); the caller's poll cadence re-drives.
            if Self.isTransportFailure(error) { client = nil }
            throw Self.mapRunError(error)
        }
    }

    /// Acquire the single command slot, suspending until the in-flight command (if
    /// any) finishes. Actor isolation makes this a correct async mutex.
    private func acquireCommandSlot() async {
        if !commandBusy { commandBusy = true; return }
        await withCheckedContinuation { commandWaiters.append($0) }
    }

    /// Release the command slot, handing it to the next waiter if there is one.
    private func releaseCommandSlot() {
        if commandWaiters.isEmpty {
            commandBusy = false
        } else {
            commandWaiters.removeFirst().resume()
        }
    }

    /// The connected client, for opening a live PTY channel. Connects if needed.
    func sshClient() async throws -> SSHClient? {
        try await connectedClient()
    }

    func close() async {
        connecting?.cancel()
        connecting = nil
        try? await client?.close()
        client = nil
    }

    // MARK: Connect (serialized + reconnecting)

    private func execute(_ command: String) async throws -> String {
        let client = try await connectedClient()
        // Use the streaming API (not `executeCommand`, which discards collected stdout
        // and throws `CommandFailed` on any non-zero exit). Our tmux read commands can
        // exit non-zero transiently (e.g. a session that just ended) yet still be
        // perfectly recoverable; keep whatever stdout arrived, and only surface an
        // error — the command's own stderr — when there's nothing usable.
        let stream = try await client.executeCommandStream(command)
        var stdout = ByteBuffer()
        var stderr = ByteBuffer()
        do {
            for try await chunk in stream {
                switch chunk {
                case .stdout(let b): stdout.writeImmutableBuffer(b)
                case .stderr(let b): stderr.writeImmutableBuffer(b)
                }
            }
        } catch let failure as SSHClient.CommandFailed {
            let out = stdout.getString(at: stdout.readerIndex, length: stdout.readableBytes) ?? ""
            if !out.isEmpty { return out } // command printed output, just exited non-zero
            let err = (stderr.getString(at: stderr.readerIndex, length: stderr.readableBytes) ?? "")
                .trimmingCharacters(in: .whitespacesAndNewlines)
            throw SSHError.command(err.isEmpty ? "command exited \(failure.exitCode)" : err)
        }
        return stdout.getString(at: stdout.readerIndex, length: stdout.readableBytes) ?? ""
    }

    /// Return a connected client, establishing one if needed. Concurrent callers
    /// await the same in-flight connect rather than starting a second handshake.
    private func connectedClient() async throws -> SSHClient {
        if let client { return client }
        if let connecting { return try await connecting.value }

        // Resolve credentials synchronously so a missing/invalid key fails fast and is
        // never cached behind the connect task.
        let auth = try authenticationMethod()
        let host = self.host
        let task = Task { () throws -> SSHClient in
            do {
                // SPIKE: replace `.acceptAnything()` with the TOFU validator; wire
                // `host.jumpHost` for bastion hops.
                return try await SSHClient.connect(
                    host: host.hostname,
                    port: host.displayPort,
                    authenticationMethod: auth,
                    hostKeyValidator: .acceptAnything(),
                    reconnect: .never
                )
            } catch let error as SSHClientError {
                throw SSHError.auth(Self.describe(error, auth: host.auth))
            } catch {
                throw SSHError.connection(Self.describeTransport(error))
            }
        }
        connecting = task
        defer { connecting = nil }
        let c = try await task.value
        client = c
        return c
    }

    // MARK: Auth

    private func authenticationMethod() throws -> SSHAuthenticationMethod {
        // Credentials come from the referenced identity when set, else the host's own
        // inline fields. `owner` is the Keychain id that holds the secret.
        let rawUser = credential?.user ?? host.user
        let auth = credential?.auth ?? host.auth
        let keyHasPassphrase = credential?.keyHasPassphrase ?? host.keyHasPassphrase
        let owner = credential?.secretOwner ?? host.id
        let user = rawUser.isEmpty ? "root" : rawUser
        switch auth {
        case .password:
            guard let pw = Keychain.password(for: owner) else { throw SSHError.missingCredential }
            return .passwordBased(username: user, password: pw)

        case .key:
            guard let keyData = Keychain.privateKey(for: owner) else { throw SSHError.missingCredential }
            guard let keyText = String(data: keyData, encoding: .utf8) else {
                throw SSHError.auth("The imported key isn't text. Export an OpenSSH private key " +
                                    "(it begins with \u{201C}-----BEGIN OPENSSH PRIVATE KEY-----\u{201D}).")
            }
            let passphrase: Data? = keyHasPassphrase
                ? Keychain.keyPassphrase(for: owner).flatMap { $0.data(using: .utf8) }
                : nil
            return try keyAuth(user: user, keyText: keyText, passphrase: passphrase)
        }
    }

    /// Detect the key type, parse it (with passphrase if encrypted), and build the
    /// matching Citadel auth method. Only ed25519 + RSA are parseable by the resolved
    /// Citadel; ECDSA is detected so we can give a precise error.
    private func keyAuth(user: String, keyText: String, passphrase: Data?) throws -> SSHAuthenticationMethod {
        let type: SSHKeyType
        do {
            type = try SSHKeyDetection.detectPrivateKeyType(from: keyText)
        } catch {
            throw SSHError.auth("Couldn't read the private key (\(error.localizedDescription)). " +
                                "Export it in OpenSSH format — the default from `ssh-keygen`.")
        }
        do {
            if type == .ed25519 {
                let key = try Curve25519.Signing.PrivateKey(sshEd25519: keyText, decryptionKey: passphrase)
                return .ed25519(username: user, privateKey: key)
            } else if type == .rsa {
                let key = try Insecure.RSA.PrivateKey(sshRsa: keyText, decryptionKey: passphrase)
                return .rsa(username: user, privateKey: key)
            } else {
                throw SSHError.auth("\(type) keys aren't supported yet — use an ed25519 or RSA key " +
                                    "(e.g. `ssh-keygen -t ed25519`).")
            }
        } catch let e as SSHError {
            throw e
        } catch {
            throw SSHError.auth("Couldn't load the \(type) key (\(error.localizedDescription)). " +
                                "If it's passphrase-protected, set the passphrase on the host.")
        }
    }

    // MARK: Error translation

    /// Whether `error` means the whole TCP transport is dead (so the shared client
    /// should be dropped + reconnected). Only NIO `ChannelError` qualifies — Citadel
    /// per-command channel errors are transient and must NOT nuke the shared client,
    /// which previously caused reconnect/flap loops.
    private static func isTransportFailure(_ error: Error) -> Bool {
        error is ChannelError
    }

    /// Map a `run` failure to a readable `SSHError` (never an opaque NIO/NSError code).
    private static func mapRunError(_ error: Error) -> SSHError {
        if let e = error as? SSHError { return e }
        if error is ChannelError { return .connection(describeTransport(error)) }
        return .command(error.localizedDescription)
    }

    /// Human-readable, actionable text for the Citadel client errors users hit.
    private static func describe(_ error: SSHClientError, auth: SshAuthKind) -> String {
        switch error {
        case .allAuthenticationOptionsFailed:
            switch auth {
            case .password:
                return "The server rejected the password. Double-check it — and note that many " +
                    "servers disable password login (PasswordAuthentication no). If so, add this " +
                    "host with an SSH key instead."
            case .key:
                return "The server rejected the key. Make sure its public half is in the host's " +
                    "~/.ssh/authorized_keys, and the passphrase (if any) is correct."
            }
        case .unsupportedPasswordAuthentication:
            return "This server doesn't offer password authentication. Add the host with an SSH key instead."
        case .unsupportedPrivateKeyAuthentication:
            return "This server doesn't offer public-key authentication for this account."
        default:
            return "SSH connection failed (\(error))."
        }
    }

    /// Readable text for NIO/transport-level connect failures.
    private static func describeTransport(_ error: Error) -> String {
        guard let ch = error as? ChannelError else { return error.localizedDescription }
        switch ch {
        case .connectTimeout:
            return "The connection timed out. Check the hostname/port and that the device can " +
                "reach the host (VPN/network)."
        case .connectPending:
            return "A previous connection attempt was still in progress. Try again."
        case .eof, .ioOnClosedChannel, .alreadyClosed, .inputClosed, .outputClosed:
            return "The connection was closed by the server. Try again."
        default:
            return "Network error (\(ch))."
        }
    }
}
