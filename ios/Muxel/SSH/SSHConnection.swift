import Foundation
import Citadel

/// Errors surfaced by the SSH layer.
enum SSHError: LocalizedError {
    case notConnected
    case auth(String)
    case connection(String)
    case hostKeyChanged(expected: String, got: String)
    case command(String)
    case missingCredential

    var errorDescription: String? {
        switch self {
        case .notConnected: return "Not connected."
        case let .auth(m): return "Authentication failed: \(m)"
        case let .connection(m): return m
        case let .hostKeyChanged(expected, got):
            return "Host key changed!\nExpected \(expected)\nGot \(got)\nConnection refused for safety."
        case let .command(m): return "Command failed: \(m)"
        case .missingCredential: return "No saved password or key for this host."
        }
    }
}

/// One SSH connection to a host, multiplexing many commands (and, later, PTY
/// channels) over a single transport — the iOS equivalent of muxel's ControlMaster.
///
/// The rest of the app depends only on this protocol, so the UI / poll / launch
/// logic can run against `MockSSHConnection` in previews and tests while the real
/// Citadel transport is finalized by the SSH spike.
protocol SSHConnection: AnyObject {
    /// Establish (or verify) the connection. Performs TOFU host-key validation.
    func connect() async throws

    /// Run a one-shot remote command over an exec channel; returns combined stdout.
    func run(_ command: String) async throws -> String

    /// The connected Citadel client, for opening a live PTY channel (the terminal
    /// view). Returns nil when there's no real transport (e.g. `MockSSHConnection`),
    /// so previews/tests degrade gracefully instead of opening a PTY.
    func sshClient() async throws -> SSHClient?

    /// Tear down the transport.
    func close() async
}

extension SSHConnection {
    /// Convenience: run a `tmux …` command line built from an argument array.
    func tmux(_ args: [String]) async throws -> String {
        try await run(TmuxCommands.commandLine(args))
    }
}
