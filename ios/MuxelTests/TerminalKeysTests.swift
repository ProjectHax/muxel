import XCTest
@testable import muxel

/// Byte-for-byte coverage of the accessory-row key encodings, mirroring
/// SwiftTerm's `applyControlToEventCharacters` table and CSI/SS3 arrow forms.
final class TerminalKeysTests: XCTestCase {

    func testControlLetters() {
        XCTAssertEqual(TerminalKeys.control("c"), [0x03])
        XCTAssertEqual(TerminalKeys.control("C"), [0x03])
        XCTAssertEqual(TerminalKeys.control("a"), [0x01])
        XCTAssertEqual(TerminalKeys.control("z"), [0x1a])
        XCTAssertEqual(TerminalKeys.control("h"), [0x08]) // Ctrl-H = backspace
    }

    func testControlPunctuation() {
        XCTAssertEqual(TerminalKeys.control(" "), [0x00])
        XCTAssertEqual(TerminalKeys.control("\\"), [0x1c])
        XCTAssertEqual(TerminalKeys.control("_"), [0x1f])
        XCTAssertEqual(TerminalKeys.control("]"), [0x1d])
        XCTAssertEqual(TerminalKeys.control("["), [0x1b])
        XCTAssertEqual(TerminalKeys.control("^"), [0x1e])
        XCTAssertEqual(TerminalKeys.control("6"), [0x1e])
    }

    func testNonCombinableIsNil() {
        XCTAssertNil(TerminalKeys.control("é"))
        XCTAssertNil(TerminalKeys.control("1"))
        XCTAssertNil(TerminalKeys.control("."))
    }

    func testArrowsNormalAndApplicationCursor() {
        // CSI: ESC [ A-D
        XCTAssertEqual(TerminalKeys.arrow(.up, applicationCursor: false), [0x1b, 0x5b, 0x41])
        XCTAssertEqual(TerminalKeys.arrow(.down, applicationCursor: false), [0x1b, 0x5b, 0x42])
        XCTAssertEqual(TerminalKeys.arrow(.right, applicationCursor: false), [0x1b, 0x5b, 0x43])
        XCTAssertEqual(TerminalKeys.arrow(.left, applicationCursor: false), [0x1b, 0x5b, 0x44])
        // SS3 (application-cursor mode): ESC O A-D
        XCTAssertEqual(TerminalKeys.arrow(.up, applicationCursor: true), [0x1b, 0x4f, 0x41])
        XCTAssertEqual(TerminalKeys.arrow(.down, applicationCursor: true), [0x1b, 0x4f, 0x42])
        XCTAssertEqual(TerminalKeys.arrow(.right, applicationCursor: true), [0x1b, 0x4f, 0x43])
        XCTAssertEqual(TerminalKeys.arrow(.left, applicationCursor: true), [0x1b, 0x4f, 0x44])
    }

    func testEscAndTab() {
        XCTAssertEqual(TerminalKeys.esc, [0x1b])
        XCTAssertEqual(TerminalKeys.tab, [0x09])
        XCTAssertEqual(TerminalKeys.shiftTab, [0x1b, 0x5b, 0x5a])
        XCTAssertEqual(TerminalKeys.ctrlC, [0x03])
    }

    func testOpenableURLAllowsOnlyHttpAndHttps() {
        XCTAssertEqual(TerminalKeys.openableURL("https://example.com/pr/42")?.absoluteString,
                       "https://example.com/pr/42")
        XCTAssertEqual(TerminalKeys.openableURL("  http://localhost:8080/auth  ")?.absoluteString,
                       "http://localhost:8080/auth")
        XCTAssertNil(TerminalKeys.openableURL("file:///etc/passwd"))
        XCTAssertNil(TerminalKeys.openableURL("javascript:alert(1)"))
        XCTAssertNil(TerminalKeys.openableURL("tel:5551234"))
        XCTAssertNil(TerminalKeys.openableURL("https://"))
        XCTAssertNil(TerminalKeys.openableURL(""))
    }

    func testSnappedFontSize() {
        XCTAssertEqual(TerminalKeys.snappedFontSize(12.4), 12)
        XCTAssertEqual(TerminalKeys.snappedFontSize(12.6), 13)
        XCTAssertEqual(TerminalKeys.snappedFontSize(3), 9)     // clamp low
        XCTAssertEqual(TerminalKeys.snappedFontSize(40), 24)   // clamp high
        XCTAssertEqual(TerminalKeys.snappedFontSize(16), 16)   // identity in range
    }

    func testBackspaceAccelerationIgnoresProgrammaticBursts() {
        // UIKit replace(_:withText:) fires 0x7f bytes microseconds apart — must never multiply!
        let burst = TerminalKeys.acceleratedBackspaces(interval: 0.001, streak: 9)
        XCTAssertEqual(burst.nextStreak, 0)
        XCTAssertEqual(burst.extra, 0)

        // Single slow tap (>200ms) resets the streak.
        let slow = TerminalKeys.acceleratedBackspaces(interval: 0.35, streak: 6)
        XCTAssertEqual(slow.nextStreak, 0)
        XCTAssertEqual(slow.extra, 0)

        // Genuine key-repeat hold (~80ms) ramps smoothly up to +5.
        let hold1 = TerminalKeys.acceleratedBackspaces(interval: 0.08, streak: 2)
        XCTAssertEqual(hold1.nextStreak, 3)
        XCTAssertEqual(hold1.extra, 1)

        let holdMax = TerminalKeys.acceleratedBackspaces(interval: 0.08, streak: 20)
        XCTAssertEqual(holdMax.extra, 5)
    }

    func testScrollToBottomTicksAddsCushionAndCapsBurst() {
        XCTAssertEqual(TerminalKeys.scrollToBottomTicks(forScrollUpTicks: 0), 0)
        XCTAssertEqual(TerminalKeys.scrollToBottomTicks(forScrollUpTicks: 1), 60)
        XCTAssertEqual(TerminalKeys.scrollToBottomTicks(forScrollUpTicks: 30), 100)
        XCTAssertEqual(TerminalKeys.scrollToBottomTicks(forScrollUpTicks: 500), 400)
    }

    func testTmuxAttachScreenOffsetStripsBootstrapPrelude() {
        let prelude = "Last login: Sun Nov 22\r\n$ export PATH=\"$PATH:/opt\"\r\n"
        let screen = "\u{1b}[?1049h\u{1b}[2Jhello"
        let bytes = Array((prelude + screen).utf8)
        let offset = TerminalKeys.tmuxAttachScreenOffset(in: bytes)
        XCTAssertEqual(offset, prelude.utf8.count)
        XCTAssertNil(TerminalKeys.tmuxAttachScreenOffset(in: Array(prelude.utf8)))
    }

    func testTmuxAltScreenSliceStripsPreAttachAndPostDetach() {
        let raw = Array("banner\r\n\u{1b}[?1049h\u{1b}[2JHello Claude (esc to interrupt)\u{1b}[?1049l[detached]\r\n".utf8)
        let slice = TerminalKeys.tmuxAltScreenSlice(in: raw)
        let text = String(bytes: slice, encoding: .utf8) ?? ""
        XCTAssertTrue(text.hasPrefix("\u{1b}[?1049h"))
        XCTAssertTrue(text.contains("esc to interrupt"))
        XCTAssertFalse(text.contains("banner"))
        XCTAssertFalse(text.contains("detached"))

        // Rendering the raw bytes via TerminalStore.renderScreenText must keep the
        // alt-screen content rather than switching back to the empty normal buffer.
        let rendered = TerminalStore.renderScreenText(from: raw, cols: 80, rows: 10)
        XCTAssertTrue(rendered.contains("esc to interrupt"), "rendered was: \(rendered)")
        XCTAssertFalse(rendered.contains("detached"))
    }

    func testBatchProbeCommandAndParserHandleMultipleSessionsOverOnePTY() {
        let cmd = TmuxCommands.batchProbeReadOnlyPTYCommand(
            targets: [("muxel_a_11111111", "mxl"), ("muxel_b_22222222", "default")],
            nonce: "deadbeef")
        XCTAssertTrue(cmd.hasPrefix("sh -c "))
        XCTAssertTrue(cmd.contains("attach-session -r"))
        XCTAssertTrue(cmd.contains("detach-client -t \"$t\""))
        XCTAssertFalse(cmd.contains("capture-pane"))

        let payload = Array(
            ("login banner\r\n__MUXEL_PB_deadbeef_0__\r\n\u{1b}[?1049hWorking… esc to interrupt\u{1b}[?1049l\r\n__MUXEL_PE_deadbeef_0__\r\n"
            + "__MUXEL_PB_deadbeef_1__\r\n\u{1b}[?1049h> ready\u{1b}[?1049l\r\n__MUXEL_PE_deadbeef_1__\r\n__MUXEL_PD_deadbeef__\r\n")
            .utf8
        )
        let slices = TerminalStore.parseBatchProbeOutput(payload, count: 2, nonce: "deadbeef")
        XCTAssertEqual(slices.count, 2)
        XCTAssertTrue(TerminalStore.renderScreenText(from: slices[0] ?? [], cols: 80, rows: 10).contains("esc to interrupt"))
        XCTAssertTrue(TerminalStore.renderScreenText(from: slices[1] ?? [], cols: 80, rows: 10).contains("> ready"))
    }
}
