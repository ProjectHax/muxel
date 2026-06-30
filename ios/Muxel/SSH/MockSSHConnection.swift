import Foundation
import Citadel

/// An in-memory `SSHConnection` that answers commands from a closure — used by
/// SwiftUI previews and unit tests so the whole interop/poll/status pipeline runs
/// without a real SSH transport. The default responder simulates one muxel tmux
/// session running an idle Claude.
final class MockSSHConnection: SSHConnection {
    private let responder: (String) -> String

    init(responder: @escaping (String) -> String = MockSSHConnection.defaultResponder) {
        self.responder = responder
    }

    func connect() async throws {}
    func run(_ command: String) async throws -> String { responder(command) }
    func sshClient() async throws -> SSHClient? { nil }
    func close() async {}

    /// A tiny canned remote: one project with one Claude pane, sitting idle.
    static func defaultResponder(_ cmd: String) -> String {
        if cmd.contains("capture-pane") { return "claude> ready\n$ " }
        if cmd.contains("display-message") { return "0\t0\t\(Int(Date().timeIntervalSince1970) - 30)" }
        if cmd.contains("list-sessions") { return "muxel_demo_abcdef12\n" }
        // Project discovery: a `find` for `.muxel/workspace.json` markers.
        if cmd.hasPrefix("find ") {
            return "/srv/app/.muxel/workspace.json\n/home/dev/web/.muxel/workspace.json\n"
        }
        if cmd.contains("workspace.json") {
            return """
            {"version":1,"updated_at":0,"remote_root":"/srv/app",
             "layout":{"kind":"leaf","tabs":["abcdef12-0000-0000-0000-000000000000"],"active":0},
             "instances":[{"id":"abcdef12-0000-0000-0000-000000000000","project_id":"11110000-0000-0000-0000-000000000000","title":"Claude","program":"claude"}],
             "worktrees":[]}
            """
        }
        return ""
    }
}
