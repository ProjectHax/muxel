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
/// Host keys are enforced with trust-on-first-use (`TOFUHostKeyValidator` over
/// `HostKeyStore`, mirroring desktop's `accept-new`): a changed key refuses the
/// connection with `SSHError.hostKeyChanged` until the user explicitly re-trusts.
actor CitadelSSHConnection: SSHConnection {
    private let host: Host
    /// Resolved credentials: the target's (overrides the host's inline user/auth
    /// and names the Keychain secret owner) and, when `host.jumpHost` is set, the
    /// bastion's.
    private let credentials: ConnectionCredentials
    private let hostKeys: HostKeyStore
    private var client: SSHClient?
    /// The bastion client carrying the tunnel when `host.jumpHost` is set —
    /// dropped and rebuilt together with `client`.
    private var bastion: SSHClient?
    /// In-flight connect, so concurrent callers share one handshake (actor isolation
    /// makes the check-and-set of this and `client` race-free).
    private var connecting: Task<(client: SSHClient, bastion: SSHClient?), Error>?
    /// Async mutex serializing command execution. Citadel's exec channels don't
    /// tolerate many concurrent commands over one connection (intermittent channel
    /// errors), so the poll loop, terminal capture loops, and launch/layout calls run
    /// one at a time rather than racing channels.
    private var commandBusy = false
    private var commandWaiters: [CheckedContinuation<Void, Never>] = []

    init(host: Host, credentials: ConnectionCredentials = .none,
         hostKeys: HostKeyStore = HostKeyStore()) {
        self.host = host
        self.credentials = credentials
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
            if Self.isTransportFailure(error) { dropTransport() }
            throw Self.mapRunError(error)
        }
    }

    /// Drop the dead transport — the tunnel's bastion goes with it, since the next
    /// `connectedClient()` rebuilds bastion-then-tunnel from scratch.
    private func dropTransport() {
        client = nil
        if let b = bastion {
            bastion = nil
            Task { try? await b.close() }
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
        keepaliveTask?.cancel()
        keepaliveTask = nil
        connecting?.cancel()
        connecting = nil
        try? await client?.close()
        client = nil
        // A jumped client's session channel doesn't close its bastion — do it here.
        try? await bastion?.close()
        bastion = nil
    }

    // MARK: Keepalive (ServerAliveInterval equivalent)

    private var keepaliveTask: Task<Void, Never>?

    /// Interval for the app-level keepalive ping — Citadel/NIOSSH expose no
    /// protocol-level keepalive, so we ping through the serialized exec path, which
    /// exercises the full chain (bastion tunnel + transport + a channel round-trip)
    /// and keeps NAT/firewall entries warm. nil disables; clamped to ≥5s.
    static func keepaliveInterval(fromSecs secs: Int?) -> Duration? {
        guard let secs, secs > 0 else { return nil }
        return .seconds(max(5, secs))
    }

    /// Start the keepalive loop (idempotent; runs until `close()`). Each tick pings
    /// only an already-connected client — it never initiates a connect, so
    /// reconnects stay caller-driven. A failed ping goes through `run()`'s
    /// transport-failure handling, which drops the dead client so the next caller
    /// reconnects — exactly the ServerAlive "declare it dead" semantics.
    private func startKeepaliveIfNeeded() {
        guard keepaliveTask == nil,
              let interval = Self.keepaliveInterval(fromSecs: host.keepaliveSecs)
        else { return }
        keepaliveTask = Task { [weak self] in
            while !Task.isCancelled {
                try? await Task.sleep(for: interval)
                guard !Task.isCancelled, let self else { return }
                guard await self.hasLiveClient() else { continue }
                _ = try? await self.run("true")
            }
        }
    }

    private func hasLiveClient() -> Bool { client != nil }

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

    /// Return a connected client, establishing one if needed — directly, or through
    /// the bastion tunnel when `host.jumpHost` is set (Citadel `jump(to:)`, the
    /// `ssh -J` equivalent: a direct-tcpip channel on the bastion carrying a full
    /// SSH session to the target). Concurrent callers await the same in-flight
    /// connect rather than starting a second handshake.
    private func connectedClient() async throws -> SSHClient {
        if let client { return client }
        if let connecting { return try await connecting.value.client }

        // Resolve credentials synchronously so a missing/invalid key fails fast and is
        // never cached behind the connect task.
        let targetCred = credentials.target ?? ResolvedCredential(
            user: host.user, auth: host.auth,
            keyHasPassphrase: host.keyHasPassphrase, secretOwner: host.id)
        let targetAuth = try authenticationMethod(for: targetCred, defaultUser: "root")
        let jump = try resolveJump(targetCred: targetCred)
        let host = self.host
        let hostKeys = self.hostKeys

        let task = Task { () throws -> (client: SSHClient, bastion: SSHClient?) in
            let targetSettings = SSHClientSettings(
                host: host.hostname,
                port: host.displayPort,
                authenticationMethod: { targetAuth },
                hostKeyValidator: .custom(TOFUHostKeyValidator(
                    hostId: host.id, scope: .target, store: hostKeys))
            )

            guard let jump else {
                do {
                    return (try await SSHClient.connect(to: targetSettings), nil)
                } catch let error as SSHError {
                    // Our own errors (hostKeyChanged from the TOFU validator) must
                    // survive verbatim — the UI matches on them.
                    throw error
                } catch let error as SSHClientError {
                    throw SSHError.auth(Self.describe(error, auth: targetCred.auth))
                } catch {
                    // The validator's failure can also surface wrapped — unwrap it
                    // before stringifying the transport error.
                    if let ssh = Self.unwrapSSHError(error) { throw ssh }
                    throw SSHError.connection(Self.describeTransport(error))
                }
            }

            // Hop 1: the bastion. Its failures name the bastion so the user
            // doesn't debug the wrong login.
            let bastionSettings = SSHClientSettings(
                host: jump.spec.host,
                port: jump.spec.port,
                authenticationMethod: { jump.auth },
                hostKeyValidator: .custom(TOFUHostKeyValidator(
                    hostId: host.id, scope: .jump, store: hostKeys))
            )
            let bastionClient: SSHClient
            do {
                bastionClient = try await SSHClient.connect(to: bastionSettings)
            } catch let error as SSHError {
                throw error
            } catch let error as SSHClientError {
                throw SSHError.auth("Jump host \(jump.spec.host): "
                    + Self.describe(error, auth: jump.authKind))
            } catch {
                if let ssh = Self.unwrapSSHError(error) { throw ssh }
                throw SSHError.connection("Jump host \(jump.spec.host): "
                    + Self.describeTransport(error))
            }

            // Hop 2: the target, tunneled. On any failure the bastion must not
            // leak — nothing else holds it yet.
            do {
                return (try await bastionClient.jump(to: targetSettings), bastionClient)
            } catch {
                try? await bastionClient.close()
                if let ssh = (error as? SSHError) ?? Self.unwrapSSHError(error) { throw ssh }
                if let e = error as? SSHClientError {
                    throw SSHError.auth(Self.describe(e, auth: targetCred.auth))
                }
                throw SSHError.connection(
                    "The jump host connected, but couldn't reach "
                    + "\(host.hostname):\(host.displayPort) through it "
                    + "(\(Self.describeTransport(error))). "
                    + "Check AllowTcpForwarding on the bastion.")
            }
        }
        connecting = task
        defer { connecting = nil }
        let result = try await task.value
        client = result.client
        bastion = result.bastion
        startKeepaliveIfNeeded()
        return result.client
    }

    /// The bastion spec + resolved auth, when the host has a jump host. User
    /// precedence: `user@` in the jump string > the jump identity's user > the
    /// target's effective user. Throws on an unparseable spec so a typo fails
    /// loudly instead of silently connecting direct.
    private func resolveJump(targetCred: ResolvedCredential)
        throws -> (spec: JumpHostSpec, auth: SSHAuthenticationMethod, authKind: SshAuthKind)?
    {
        guard let raw = host.jumpHost, !raw.isEmpty else { return nil }
        guard let spec = JumpHostSpec.parse(raw) else {
            throw SSHError.connection("Couldn't parse the jump host \u{201C}\(raw)\u{201D} — "
                + "use user@host:port (chains aren't supported).")
        }
        var cred = credentials.jump ?? targetCred
        if let user = spec.user { cred.user = user }
        let fallbackUser = targetCred.user.isEmpty ? "root" : targetCred.user
        let auth = try authenticationMethod(for: cred, defaultUser: fallbackUser)
        return (spec, auth, cred.auth)
    }

    /// Best-effort recovery of one of our own `SSHError`s from an error that
    /// crossed the NIO pipeline (the TOFU validator fails the KEX promise with
    /// `hostKeyChanged`; NIOSSH is expected to deliver it unwrapped, but the UI's
    /// match must not bet on that).
    private static func unwrapSSHError(_ error: Error) -> SSHError? {
        if let e = error as? SSHError { return e }
        for child in Mirror(reflecting: error).children {
            if let e = child.value as? SSHError { return e }
        }
        return nil
    }

    // MARK: Auth

    /// Build the Citadel auth method for `cred`, reading its secret from the
    /// Keychain under `cred.secretOwner`. Shared by the target and bastion hops.
    private func authenticationMethod(for cred: ResolvedCredential,
                                      defaultUser: String) throws -> SSHAuthenticationMethod {
        let user = cred.user.isEmpty ? defaultUser : cred.user
        switch cred.auth {
        case .password:
            guard let pw = Keychain.password(for: cred.secretOwner) else {
                throw SSHError.missingCredential
            }
            return .passwordBased(username: user, password: pw)

        case .key:
            guard let keyData = Keychain.privateKey(for: cred.secretOwner) else {
                throw SSHError.missingCredential
            }
            guard let keyText = String(data: keyData, encoding: .utf8) else {
                throw SSHError.auth("The imported key isn't text. Export an OpenSSH private key " +
                                    "(it begins with \u{201C}-----BEGIN OPENSSH PRIVATE KEY-----\u{201D}).")
            }
            let passphrase: Data? = cred.keyHasPassphrase
                ? Keychain.keyPassphrase(for: cred.secretOwner).flatMap { $0.data(using: .utf8) }
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
