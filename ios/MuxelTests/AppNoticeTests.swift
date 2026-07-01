import XCTest
@testable import muxel

/// Covers the transient-notice funnel that replaced the blocking error alert.
@MainActor
final class AppNoticeTests: XCTestCase {

    func testReportPublishesNotice() {
        let state = TestFixtures.makeState()
        XCTAssertNil(state.notice)

        state.report("keychain write failed", duration: 8)
        XCTAssertEqual(state.notice?.text, "keychain write failed")
        XCTAssertEqual(state.notice?.style, .error)
        XCTAssertEqual(state.notice?.duration, 8)

        state.report("connected", style: .success)
        XCTAssertEqual(state.notice?.style, .success)
        XCTAssertEqual(state.notice?.duration, 4, "default duration")
    }

    func testCloseFailureReportsNotice() async {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        state.connectionFactory = { _, _ in ThrowingSSHConnection() }

        let instance = Instance(id: UUID().uuidString.lowercased(),
                                projectId: UUID().uuidString.lowercased(),
                                title: "claude", program: "claude", args: [])
        await state.close(instance, in: project)

        XCTAssertNotNil(state.notice, "a failed close surfaces a banner")
        XCTAssertEqual(state.notice?.style, .error)
    }

    func testRenameFailureReportsNotice() async {
        let state = TestFixtures.makeState()
        let (_, project) = TestFixtures.seedProject(state)
        state.connectionFactory = { _, _ in ThrowingSSHConnection() }

        let instance = Instance(id: UUID().uuidString.lowercased(),
                                projectId: UUID().uuidString.lowercased(),
                                title: "claude", program: "claude", args: [])
        await state.rename(instance, to: "new name", in: project)

        XCTAssertNotNil(state.notice)
    }

    func testCorruptStoreNoticeSurfacesOnInit() throws {
        _ = LocalStore.takeCorruptNotice()
        let dir = FileManager.default.temporaryDirectory
            .appendingPathComponent("muxel-corrupt-init-\(UUID().uuidString)")
        defer { try? FileManager.default.removeItem(at: dir) }
        try FileManager.default.createDirectory(at: dir, withIntermediateDirectories: true)
        try Data("garbage{{".utf8).write(to: dir.appendingPathComponent("store.json"))

        let state = AppState(store: LocalStore(directory: dir))
        XCTAssertEqual(state.doc, StoreDocument(), "starts empty, never crashes")
        XCTAssertNotNil(state.notice, "the data-loss notice is shown, not swallowed")
        XCTAssertTrue(state.notice?.text.contains("store.json.corrupt") == true)
    }
}
