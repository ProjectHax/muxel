import XCTest
@testable import muxel

/// The pure Live Activity aggregation (poll results → per-instance payload). The
/// ActivityKit lifecycle itself isn't unit-testable headlessly, so all logic lives here.
final class ActivitySummaryTests: XCTestCase {

    private typealias State = MuxelActivityAttributes.InstanceState

    func testStateMapping() {
        XCTAssertEqual(ActivitySummaryBuilder.state(status: .done, running: true), .attention)
        XCTAssertEqual(ActivitySummaryBuilder.state(status: .done, running: false), .attention)
        XCTAssertEqual(ActivitySummaryBuilder.state(status: .working, running: true), .working)
        XCTAssertEqual(ActivitySummaryBuilder.state(status: .working, running: false), .idle)
        XCTAssertEqual(ActivitySummaryBuilder.state(status: .idle, running: true), .idle)
    }

    func testRowTruncatesNameAndProject() {
        let long = String(repeating: "x", count: 40)
        let r = ActivitySummaryBuilder.row(
            id: "i", name: long, project: long, status: .working, running: true)
        XCTAssertEqual(r.name.count, ActivitySummaryBuilder.nameCap)
        XCTAssertEqual(r.project.count, ActivitySummaryBuilder.nameCap)
        XCTAssertEqual(r.state, .working)
    }

    func testContentStateCountsAndAttentionFirstSort() {
        let rows = [
            MuxelActivityAttributes.InstanceRow(id: "1", name: "idle", project: "p", state: .idle),
            MuxelActivityAttributes.InstanceRow(id: "2", name: "work", project: "p", state: .working),
            MuxelActivityAttributes.InstanceRow(id: "3", name: "attn", project: "p", state: .attention),
        ]
        let cs = ActivitySummaryBuilder.contentState(rows: rows, now: Date())
        XCTAssertEqual(cs.attentionCount, 1)
        XCTAssertEqual(cs.workingCount, 1)
        XCTAssertEqual(cs.instanceCount, 3)
        XCTAssertEqual(cs.instances.first?.id, "3", "attention sorts first")
        XCTAssertEqual(cs.instances[1].id, "2", "then working")
        XCTAssertEqual(cs.instances[2].id, "1", "then idle")
        XCTAssertFalse(cs.isEmpty)
    }

    func testContentStateCapsListButKeepsTrueCount() {
        let rows = (0..<20).map {
            MuxelActivityAttributes.InstanceRow(
                id: "\($0)", name: "a\($0)", project: "p", state: .working)
        }
        let cs = ActivitySummaryBuilder.contentState(rows: rows, now: Date())
        XCTAssertEqual(cs.instances.count, ActivitySummaryBuilder.rowCap)
        XCTAssertEqual(cs.instanceCount, 20)
        XCTAssertEqual(cs.workingCount, 20)
    }

    func testEmptyWhenNoInstances() {
        XCTAssertTrue(ActivitySummaryBuilder.contentState(rows: [], now: Date()).isEmpty)
        // Idle instances are NOT empty — the bar stays present.
        let idle = ActivitySummaryBuilder.contentState(
            rows: [MuxelActivityAttributes.InstanceRow(
                id: "1", name: "i", project: "p", state: .idle)],
            now: Date())
        XCTAssertFalse(idle.isEmpty)
    }

    func testContentStateCodableRoundTrip() throws {
        let cs = ActivitySummaryBuilder.contentState(
            rows: [MuxelActivityAttributes.InstanceRow(
                id: "1", name: "claude", project: "web", state: .attention)],
            now: Date(timeIntervalSince1970: 1_700_000_000))
        let data = try JSONEncoder().encode(cs)
        let back = try JSONDecoder().decode(MuxelActivityAttributes.ContentState.self, from: data)
        XCTAssertEqual(cs, back)
    }
}
