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
    }

    func testSnappedFontSize() {
        XCTAssertEqual(TerminalKeys.snappedFontSize(12.4), 12)
        XCTAssertEqual(TerminalKeys.snappedFontSize(12.6), 13)
        XCTAssertEqual(TerminalKeys.snappedFontSize(3), 9)     // clamp low
        XCTAssertEqual(TerminalKeys.snappedFontSize(40), 24)   // clamp high
        XCTAssertEqual(TerminalKeys.snappedFontSize(16), 16)   // identity in range
    }
}
