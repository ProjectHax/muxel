import XCTest
@testable import muxel

final class PTYOpenGateTests: XCTestCase {
    /// A limit-2 gate never lets more than 2 acquirers run at once, even under 5
    /// concurrent cold-starts.
    func testBoundsConcurrentOpens() async {
        let gate = PTYOpenGate(limit: 2)
        let counter = InFlightCounter()
        await withTaskGroup(of: Void.self) { group in
            for _ in 0..<5 {
                group.addTask {
                    await gate.acquire()
                    await counter.enter()
                    try? await Task.sleep(nanoseconds: 15_000_000)
                    await counter.leave()
                    await gate.release()
                }
            }
        }
        let peak = await counter.peak
        let current = await counter.current
        XCTAssertLessThanOrEqual(peak, 2)
        XCTAssertEqual(current, 0)
    }
}

private actor InFlightCounter {
    private(set) var current = 0
    private(set) var peak = 0
    func enter() { current += 1; peak = max(peak, current) }
    func leave() { current -= 1 }
}
