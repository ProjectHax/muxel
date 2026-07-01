import CoreGraphics

/// Pure byte-sequence logic for the terminal's accessory key row — kept UIKit-free
/// so the key encodings are unit-testable.
enum TerminalKeys {
    enum Arrow {
        case up, down, left, right

        fileprivate var final: UInt8 {
            switch self {
            case .up: return 0x41    // A
            case .down: return 0x42  // B
            case .right: return 0x43 // C
            case .left: return 0x44  // D
            }
        }
    }

    static let esc: [UInt8] = [0x1b]
    static let tab: [UInt8] = [0x09]

    /// Arrow-key bytes: CSI (`ESC [ x`) normally, SS3 (`ESC O x`) when the remote
    /// app enabled application-cursor mode (DECCKM) — the same distinction SwiftTerm
    /// makes for hardware-keyboard arrows.
    static func arrow(_ dir: Arrow, applicationCursor: Bool) -> [UInt8] {
        [0x1b, applicationCursor ? 0x4f : 0x5b, dir.final]
    }

    /// Ctrl-combine a typed character, mirroring SwiftTerm's
    /// `applyControlToEventCharacters` table byte-for-byte (letters plus the
    /// punctuation controls). nil when the character has no control mapping.
    static func control(_ ch: Character) -> [UInt8]? {
        guard let ascii = ch.asciiValue else { return nil }
        switch ascii {
        case UInt8(ascii: "A")...UInt8(ascii: "Z"): return [ascii - 0x40]
        case UInt8(ascii: "a")...UInt8(ascii: "z"): return [ascii - 0x60]
        case UInt8(ascii: " "): return [0x00]
        case UInt8(ascii: "\\"): return [0x1c]
        case UInt8(ascii: "_"): return [0x1f]
        case UInt8(ascii: "]"): return [0x1d]
        case UInt8(ascii: "["): return [0x1b]
        case UInt8(ascii: "^"), UInt8(ascii: "6"): return [0x1e]
        default: return nil
        }
    }

    /// Pinch-zoom font sizing: snap to integer point sizes and clamp to a readable
    /// range, so mid-gesture we re-rasterize at most once per point step.
    static func snappedFontSize(_ raw: CGFloat) -> CGFloat {
        min(24, max(9, raw.rounded()))
    }
}
