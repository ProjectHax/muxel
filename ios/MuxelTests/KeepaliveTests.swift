import XCTest
@testable import muxel

/// Covers the keepalive interval clamp (the loop itself is a live-transport
/// behavior, verified manually on device).
final class KeepaliveTests: XCTestCase {

    func testDisabledForNilOrNonPositive() {
        XCTAssertNil(CitadelSSHConnection.keepaliveInterval(fromSecs: nil))
        XCTAssertNil(CitadelSSHConnection.keepaliveInterval(fromSecs: 0))
        XCTAssertNil(CitadelSSHConnection.keepaliveInterval(fromSecs: -5))
    }

    func testClampedToAtLeastFiveSeconds() {
        XCTAssertEqual(CitadelSSHConnection.keepaliveInterval(fromSecs: 1), .seconds(5))
        XCTAssertEqual(CitadelSSHConnection.keepaliveInterval(fromSecs: 5), .seconds(5))
        XCTAssertEqual(CitadelSSHConnection.keepaliveInterval(fromSecs: 60), .seconds(60))
    }
}
