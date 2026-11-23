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

    func testGroupByProjectAggregatesDotsAndHighlightsAttention() {
        let rows: [MuxelActivityAttributes.InstanceRow] = [
            .init(id: "1", name: "shell", project: "web", state: .idle),
            .init(id: "2", name: "codex", project: "api", state: .working),
            .init(id: "3", name: "claude", project: "muxel", state: .needsInput),
            .init(id: "4", name: "opencode", project: "muxel", state: .idle),
            .init(id: "5", name: "amp", project: "muxel", state: .working),
            .init(id: "6", name: "gemini", project: "api", state: .finished),
        ]
        let groups = MuxelActivityAttributes.groupByProject(rows)
        // Sorted by urgency: needsInput (muxel) → working (api) → idle (web).
        XCTAssertEqual(groups.map(\.project), ["muxel", "api", "web"])
        XCTAssertEqual(groups[0].states, [.needsInput, .working, .idle])
        XCTAssertEqual(groups[0].highlightState, .needsInput)
        XCTAssertEqual(groups[0].summaryText, "claude: needs input")

        XCTAssertEqual(groups[1].states, [.working, .finished])
        XCTAssertEqual(groups[1].highlightState, .working)
        XCTAssertEqual(groups[1].summaryText, "codex working")

        XCTAssertEqual(groups[2].states, [.idle])
        XCTAssertEqual(groups[2].highlightState, .idle)
        XCTAssertEqual(groups[2].summaryText, "shell idle")
    }

    func testMostRecentlyWorkedProjectsAndAgentsSortToTopOfLiveActivity() {
        let rows: [MuxelActivityAttributes.InstanceRow] = [
            .init(id: "1", name: "claude", project: "alpha", state: .idle, lastActiveAt: 100),
            .init(id: "2", name: "shell", project: "zeta", state: .idle, lastActiveAt: 500),
            .init(id: "3", name: "grok", project: "zeta", state: .idle, lastActiveAt: 900),
            .init(id: "4", name: "codex", project: "beta", state: .idle, lastActiveAt: 700),
        ]
        // ContentState keeps most recently worked agents first within the same tier.
        let cs = ActivitySummaryBuilder.contentState(rows: rows, now: Date())
        XCTAssertEqual(cs.instances.map(\.id), ["3", "4", "2", "1"])

        // Project groups sort by most recently worked project first (`zeta` → `beta` → `alpha`),
        // and highlight the agent that worked most recently (`grok idle`).
        let groups = MuxelActivityAttributes.groupByProject(rows)
        XCTAssertEqual(groups.map(\.project), ["zeta", "beta", "alpha"])
        XCTAssertEqual(groups[0].summaryText, "grok idle")
    }

    func testLastWorkedStorePersistsActiveWorkTimestamps() throws {
        let suite = "muxel.tests.lastworked.\(UUID().uuidString)"
        let defaults = try XCTUnwrap(UserDefaults(suiteName: suite))
        defer { defaults.removePersistentDomain(forName: suite) }

        LastWorkedStore.record([
            InstanceStatus(instanceId: "work-1", status: .working, running: true),
            InstanceStatus(instanceId: "idle-1", status: .idle, running: true),
        ], now: 1_700_000_500, defaults: defaults)

        XCTAssertEqual(LastWorkedStore.timestamp(for: "work-1", defaults: defaults), 1_700_000_500)
        XCTAssertNil(LastWorkedStore.timestamp(for: "idle-1", defaults: defaults))
    }


    func testSameNamedProjectsStaySeparate() {
        let a = UUID(), b = UUID()
        let rows = [
            ActivitySummaryBuilder.row(id: "1", name: "claude", project: "api", projectId: a,
                                       status: .blocked, running: true),
            ActivitySummaryBuilder.row(id: "2", name: "codex", project: "api", projectId: b,
                                       status: .working, running: true),
        ]
        XCTAssertEqual(rows[0].projectKey, MuxelActivityAttributes.InstanceRow.projectKey(for: a))
        let groups = MuxelActivityAttributes.groupByProject(rows)
        XCTAssertEqual(groups.count, 2)
        XCTAssertEqual(Set(groups.map(\.id)).count, 2, "ForEach needs distinct ids")
        XCTAssertEqual(groups.map(\.project), ["api", "api"])
        XCTAssertEqual(groups.map(\.summaryText), ["claude: needs input", "codex working"])
    }

    func testRowsWithoutProjectKeyStillGroupByName() throws {
        // A summary cached by an older build has no `projectKey`.
        let legacy = #"{"id":"1","name":"claude","project":"web","state":"idle"}"#
        let row = try JSONDecoder().decode(MuxelActivityAttributes.InstanceRow.self, from: Data(legacy.utf8))
        XCTAssertNil(row.projectKey)
        XCTAssertEqual(MuxelActivityAttributes.groupByProject([row]).map(\.id), ["web"])
    }

    func testKeepingRowsDropsRemovedProjects() {
        let kept = UUID(), deleted = UUID()
        let rows: [MuxelActivityAttributes.InstanceRow] = [
            ActivitySummaryBuilder.row(id: "1", name: "a", project: "api", projectId: kept,
                                       status: .working, running: true),
            ActivitySummaryBuilder.row(id: "2", name: "b", project: "gone", projectId: deleted,
                                       status: .working, running: true),
            // Older cached rows, matched by name.
            .init(id: "3", name: "c", project: "api", state: .idle),
            .init(id: "4", name: "d", project: "gone", state: .idle),
        ]
        let left = ActivitySummaryBuilder.keepingRows(rows, ofProjects: [(id: kept, name: "api")])
        XCTAssertEqual(left.map(\.id), ["1", "3"])
    }
}
