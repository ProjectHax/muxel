import XCTest
@testable import muxel

/// Covers the per-project layout load state that distinguishes "can't reach the
/// host" from a genuinely empty project.
@MainActor
final class LayoutLoadStateTests: XCTestCase {

    func testInitialFailureIsAStateNotABanner() async {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        state.connectionFactory = { _, _ in ThrowingSSHConnection() }

        state.selectedProject = project
        await state.refreshLayout()

        guard case let .failed(message) = state.layoutLoad else {
            return XCTFail("expected .failed, got \(state.layoutLoad)")
        }
        XCTAssertTrue(message.contains("unreachable"))
        XCTAssertNil(state.layout)
        XCTAssertNil(state.notice, "nothing loaded yet → full-screen state, no banner")
    }

    func testSuccessfulLoadIsLoaded() async {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        state.connectionFactory = { _, _ in MockSSHConnection() }

        state.selectedProject = project
        await state.refreshLayout()

        XCTAssertEqual(state.layoutLoad, .loaded)
        XCTAssertNotNil(state.layout)
    }

    func testRefreshFailureKeepsLoadedAndBanners() async {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        let conn = FlakyMockConnection()
        state.connectionFactory = { _, _ in conn }

        state.selectedProject = project
        await state.refreshLayout()
        XCTAssertEqual(state.layoutLoad, .loaded)

        conn.failNow = true
        await state.refreshLayout()

        XCTAssertEqual(state.layoutLoad, .loaded,
                       "visible panes stay up through a refresh hiccup")
        XCTAssertNotNil(state.layout)
        XCTAssertNotNil(state.notice, "the hiccup is reported transiently")
    }

    func testDeselectResetsToIdle() async {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        state.connectionFactory = { _, _ in MockSSHConnection() }

        state.selectedProject = project
        await state.refreshLayout()
        XCTAssertEqual(state.layoutLoad, .loaded)

        state.deselect()
        XCTAssertEqual(state.layoutLoad, .idle)
        XCTAssertNil(state.layout)
        XCTAssertNil(state.selectedProject)
    }
}
