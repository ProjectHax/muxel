import Foundation
import Citadel
@testable import muxel

/// Shared fixtures for AppState-level tests: connection stubs and an AppState
/// backed by a throwaway store directory (never the app's real store).

/// An `SSHConnection` whose every operation fails — the "host unreachable" stub.
final class ThrowingSSHConnection: SSHConnection {
    struct Boom: LocalizedError {
        var errorDescription: String? { "boom: host unreachable" }
    }

    func connect() async throws { throw Boom() }
    func run(_ command: String) async throws -> String { throw Boom() }
    func sshClient() async throws -> SSHClient? { nil }
    func close() async {}
}

/// A mock connection that can be flipped into failure mid-test — for "it worked,
/// then the transport dropped" scenarios.
final class FlakyMockConnection: SSHConnection {
    var failNow = false
    private let mock = MockSSHConnection()

    func connect() async throws {
        if failNow { throw ThrowingSSHConnection.Boom() }
    }
    func run(_ command: String) async throws -> String {
        if failNow { throw ThrowingSSHConnection.Boom() }
        return try await mock.run(command)
    }
    func sshClient() async throws -> SSHClient? { nil }
    func close() async {}
}

@MainActor
enum TestFixtures {
    /// An AppState over a fresh temp-directory store, with any leftover pending
    /// corrupt-store notice drained so it can't leak into assertions.
    static func makeState() -> AppState {
        _ = LocalStore.takeCorruptNotice()
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxel-appstate-tests-\(UUID().uuidString)")
        return AppState(store: LocalStore(directory: dir))
    }

    /// A host + project wired into `state.doc` (not persisted), returned for use.
    static func seedProject(_ state: AppState) -> (Host, RemoteProject) {
        let host = Host(name: "web", hostname: "example.com")
        let project = RemoteProject(name: "api", hostId: host.id, remoteRoot: "/srv/app")
        state.doc.hosts = [host]
        state.doc.projects = [project]
        return (host, project)
    }
}
