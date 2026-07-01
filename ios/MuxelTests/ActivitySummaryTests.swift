import XCTest
@testable import muxel

/// The pure Live Activity aggregation (poll results → per-instance payload). The
/// ActivityKit lifecycle itself isn't unit-testable headlessly, so all logic lives here.
final class ActivitySummaryTests: XCTestCase {

    private func row(_ id: String, _ state: MuxelActivityAttributes.InstanceState)
        -> MuxelActivityAttributes.InstanceRow {
        .init(id: id, name: id, project: "p", state: state)
    }

    func testStateMapping() {
        // A bell/blocked pane is "needs input"; a clean exit is "finished".
        XCTAssertEqual(ActivitySummaryBuilder.state(status: .blocked, running: true), .needsInput)
        XCTAssertEqual(ActivitySummaryBuilder.state(status: .done, running: true), .finished)
        XCTAssertEqual(ActivitySummaryBuilder.state(status: .working, running: true), .working)
        XCTAssertEqual(ActivitySummaryBuilder.state(status: .working, running: false), .idle)
        XCTAssertEqual(ActivitySummaryBuilder.state(status: .idle, running: true), .idle)
    }

    func testRowTruncatesNameAndProject() {
        let long = String(repeating: "x", count: 40)
        let r = ActivitySummaryBuilder.row(
            id: "i", name: long, project: long, status: .blocked, running: true)
        XCTAssertEqual(r.name.count, ActivitySummaryBuilder.nameCap)
        XCTAssertEqual(r.project.count, ActivitySummaryBuilder.nameCap)
        XCTAssertEqual(r.state, .needsInput)
    }

    func testNeedsInputSortsFirstThenFinishedThenWorking() {
        let rows = [
            row("idle", .idle),
            row("work", .working),
            row("done", .finished),
            row("input", .needsInput),
        ]
        let cs = ActivitySummaryBuilder.contentState(rows: rows, now: Date())
        XCTAssertEqual(cs.instances.map(\.id), ["input", "done", "work", "idle"])
        XCTAssertEqual(cs.needsInputCount, 1)
        XCTAssertEqual(cs.finishedCount, 1)
        XCTAssertEqual(cs.workingCount, 1)
        XCTAssertEqual(cs.instanceCount, 4)
        XCTAssertFalse(cs.isEmpty)
    }

    func testContentStateCapsListButKeepsTrueCount() {
        let rows = (0..<20).map { row("\($0)", .working) }
        let cs = ActivitySummaryBuilder.contentState(rows: rows, now: Date())
        XCTAssertEqual(cs.instances.count, ActivitySummaryBuilder.rowCap)
        XCTAssertEqual(cs.instanceCount, 20)
        XCTAssertEqual(cs.workingCount, 20)
    }

    func testEmptyWhenNoInstances() {
        XCTAssertTrue(ActivitySummaryBuilder.contentState(rows: [], now: Date()).isEmpty)
        // Idle instances are NOT empty — the bar stays present.
        let idle = ActivitySummaryBuilder.contentState(rows: [row("1", .idle)], now: Date())
        XCTAssertFalse(idle.isEmpty)
    }

    func testContentStateCodableRoundTrip() throws {
        let cs = ActivitySummaryBuilder.contentState(
            rows: [row("1", .needsInput)], now: Date(timeIntervalSince1970: 1_700_000_000))
        let data = try JSONEncoder().encode(cs)
        let back = try JSONDecoder().decode(MuxelActivityAttributes.ContentState.self, from: data)
        XCTAssertEqual(cs, back)
    }
}
