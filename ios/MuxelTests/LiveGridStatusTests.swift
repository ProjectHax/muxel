import XCTest
@testable import muxel

/// The live-grid status path: an attached pane's screen text feeds the ported
/// `classify` + `defaultMarkers` so working/blocked is real. Mirrors the `view.rs`
/// classify priority tests, driven through the `PollService.classify` pipeline.
final class LiveGridStatusTests: XCTestCase {
    private let claudeId = "aaaaaaaa-0000-0000-0000-000000000000"

    private func claude() -> Instance {
        Instance(id: claudeId, projectId: "p", title: "Claude", program: "claude", args: [])
    }
    private func shell() -> Instance {
        Instance(id: claudeId, projectId: "p", title: "Shell", program: nil, args: [])
    }
    private func row(exited: Bool = false, bell: Bool = false, idle: TimeInterval = 100) -> PaneRow {
        PaneRow(session: TmuxSession.name(hostName: "host", instanceId: claudeId),
                exited: exited, bell: bell, idle: idle)
    }

    func testWorkingMarkerOnLiveScreen() {
        let p = PollService()
        let screen = LiveScreen(text: "thinking…\nesc to interrupt", idle: 0.5, bell: false)
        let out = p.classify(rows: [row()], instances: [claude()], liveScreens: [claudeId: screen])
        XCTAssertEqual(out.first?.status, .working)
        XCTAssertEqual(out.first?.running, true)
    }

    func testBlockedMarkerOnLiveScreen() {
        let p = PollService()
        let screen = LiveScreen(text: "Do you want to proceed?", idle: 5, bell: false)
        let out = p.classify(rows: [row()], instances: [claude()], liveScreens: [claudeId: screen])
        XCTAssertEqual(out.first?.status, .blocked)
    }

    func testLatchedDoneAfterMarkerLeaves() {
        let p = PollService()
        // First poll: working (marker present).
        _ = p.classify(rows: [row()], instances: [claude()],
                       liveScreens: [claudeId: LiveScreen(text: "esc to interrupt", idle: 0.1, bell: false)])
        // Next poll: marker gone + quiet → a marker agent that finished latches to done.
        let out = p.classify(rows: [row(idle: 10)], instances: [claude()],
                             liveScreens: [claudeId: LiveScreen(text: "❯ ", idle: 10, bell: false)])
        XCTAssertEqual(out.first?.status, .done)
    }

    func testMarkerLessProgramIgnoresScreen() {
        let p = PollService()
        // A shell has no markers → the live screen is ignored (even one that happens to
        // contain a marker string), and the vars path never fakes `.working` from redraw.
        let screen = LiveScreen(text: "esc to interrupt", idle: 0.1, bell: false)
        let out = p.classify(rows: [row(idle: 0.1)], instances: [shell()], liveScreens: [claudeId: screen])
        XCTAssertEqual(out.first?.status, .idle)
    }

    func testNoLiveScreenUsesVarsOnlyPath() {
        let p = PollService()
        // Attached-but-no-screen (background poll): a bell on a live pane → blocked.
        let out = p.classify(rows: [row(bell: true)], instances: [claude()], liveScreens: [:])
        XCTAssertEqual(out.first?.status, .blocked)
    }

    func testExitedIsDoneRegardlessOfScreen() {
        let p = PollService()
        let screen = LiveScreen(text: "esc to interrupt", idle: 0, bell: false)
        let out = p.classify(rows: [row(exited: true)], instances: [claude()], liveScreens: [claudeId: screen])
        XCTAssertEqual(out.first?.status, .done)
    }
}
