import XCTest
import NIOCore
@testable import muxel

/// Connect failures read as their cause, and on cellular point at the usual
/// "works on Wi‑Fi, not on 5G" reasons.
final class ConnectFailureTests: XCTestCase {
    func testTimeoutsTellTheLoginDeadlineFromTheTCPConnect() {
        XCTAssertEqual(ConnectFailure(ChannelError.connectTimeout(.seconds(10))), .handshakeTimedOut)
        XCTAssertEqual(ConnectFailure(ChannelError.connectTimeout(.seconds(30))), .timedOut)
        XCTAssertEqual(ConnectFailure(ChannelError.eof), .closed)
    }

    func testErrnoClassification() {
        XCTAssertEqual(ConnectFailure(IOError(errnoCode: ECONNREFUSED, reason: "connect")), .refused)
        XCTAssertEqual(ConnectFailure(IOError(errnoCode: EHOSTUNREACH, reason: "connect")), .unreachable)
        XCTAssertEqual(ConnectFailure(IOError(errnoCode: ENETUNREACH, reason: "connect")), .unreachable)
        XCTAssertEqual(ConnectFailure(IOError(errnoCode: ETIMEDOUT, reason: "connect")), .timedOut)
    }

    func testHappyEyeballsVerdictPrefersRefusalThenTimeout() {
        XCTAssertEqual(ConnectFailure.classify(errnos: [ENETUNREACH, ECONNREFUSED], lookupFailed: false, fallback: ""),
                       .refused)
        XCTAssertEqual(ConnectFailure.classify(errnos: [ENETUNREACH, ETIMEDOUT], lookupFailed: false, fallback: ""),
                       .timedOut)
        XCTAssertEqual(ConnectFailure.classify(errnos: [], lookupFailed: true, fallback: ""), .lookupFailed)
        XCTAssertEqual(ConnectFailure.classify(errnos: [EPERM], lookupFailed: false, fallback: "x"), .other("x"))
    }

    func testLocalOnlyAddresses() {
        for host in ["192.168.1.20", "10.0.0.5", "172.20.3.4", "169.254.1.1", "127.0.0.1",
                     "fe80::1%en0", "[fd12:3456::1]", "devbox.local", "nas.lan", "box.home.arpa", "pi.local."] {
            XCTAssertTrue(ConnectFailure.isLocalOnly(host), host)
        }
        // Public addresses, Tailscale's 100.64/10, and bare names (MagicDNS) aren't.
        for host in ["172.32.0.1", "8.8.8.8", "100.101.102.103", "2001:db8::1", "example.com", "devbox"] {
            XCTAssertFalse(ConnectFailure.isLocalOnly(host), host)
        }
    }

    func testCellularDeniedExplainsANetworkFailure() {
        let denied = NetworkSnapshot(blocker: .cellularDenied)
        let text = ConnectFailure.timedOut.message(host: "box", port: 22, network: denied)
        XCTAssertTrue(text.contains("Cellular data is turned off for muxel"), text)
        // A refusal proves the network works, so the blocker isn't blamed.
        let refused = ConnectFailure.refused.message(host: "box", port: 2222, network: denied)
        XCTAssertTrue(refused.contains("refused the connection on port 2222"), refused)
    }

    func testCellularHintNamesLocalAddresses() {
        let cellular = NetworkSnapshot(cellularOnly: true)
        let local = ConnectFailure.unreachable.message(host: "192.168.1.20", port: 22, network: cellular)
        XCTAssertTrue(local.contains("local-network address"), local)
        let remote = ConnectFailure.timedOut.message(host: "devbox", port: 22, network: cellular)
        XCTAssertTrue(remote.contains("You're on cellular"), remote)
        let wifi = ConnectFailure.timedOut.message(host: "devbox", port: 22, network: NetworkSnapshot())
        XCTAssertFalse(wifi.contains("cellular"), wifi)
    }
}
