import CoreGraphics
import Foundation

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
    /// Reverse-tab (CSI Z) — toggles modes in Claude Code, opencode, Codex, etc.
    static let shiftTab: [UInt8] = [0x1b, 0x5b, 0x5a]
    /// Interrupt (ETX / ^C).
    static let ctrlC: [UInt8] = [0x03]

    /// Validate a terminal hyperlink (OSC-8) before handing it to iOS: allow only
    /// `http`/`https` URLs with a non-empty host so remote output can't invoke
    /// arbitrary URL schemes.
    static func openableURL(_ raw: String) -> URL? {
        let trimmed = raw.trimmingCharacters(in: .whitespacesAndNewlines)
        guard let url = URL(string: trimmed),
              let scheme = url.scheme?.lowercased(),
              scheme == "http" || scheme == "https",
              let host = url.host, !host.isEmpty
        else { return nil }
        return url
    }

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

    /// Decide whether a single backspace byte should be accelerated as a held-key
    /// repeat. Programmatic bursts from UIKit's `replace(_:withText:)` (autocorrect /
    /// dictation / IME) arrive <30ms apart and must NEVER be multiplied or they
    /// over-delete and corrupt earlier text in the agent's prompt box.
    static func acceleratedBackspaces(interval: TimeInterval, streak: Int) -> (nextStreak: Int, extra: Int) {
        guard interval >= 0.03, interval < 0.2 else {
            return (0, 0)
        }
        let next = streak + 1
        return (next, min(next / 3, 5))
    }

    /// Number of SGR wheel-down ticks to emit when jumping to the bottom of a
    /// mouse-aware TUI (Grok, OpenCode, etc.) after `scrollUpTicks` upward ticks.
    /// Doubles the tracked count plus a generous cushion (so conversations that kept
    /// streaming while scrolled up still reach the very bottom), capped at 400 ticks.
    static func scrollToBottomTicks(forScrollUpTicks ticks: Int) -> Int {
        guard ticks > 0 else { return 0 }
        return min(max(ticks * 2 + 40, 60), 400)
    }

    /// Locate the byte offset where `tmux` enters full-screen alternate mode
    /// (`ESC [ ? 1 0 4 9 h` or `ESC [ ? 4 7 h`). Anything before this offset on a
    /// freshly opened SSH PTY is the remote login banner, shell prompt, or command
    /// prelude — skipping it makes agent attachment seamless with zero command flash.
    static func tmuxAttachScreenOffset(in bytes: [UInt8]) -> Int? {
        firstMatchOffset(in: bytes, from: 0, needles: [
            [0x1b, 0x5b, 0x3f, 0x31, 0x30, 0x34, 0x39, 0x68], // \e[?1049h
            [0x1b, 0x5b, 0x3f, 0x34, 0x37, 0x68],             // \e[?47h
        ])
    }

    /// Extract the rendered alternate-screen byte slice from a brief `tmux attach -r`
    /// capture: starts at `\e[?1049h`/`\e[?47h` and stops BEFORE `\e[?1049l`/`\e[?47l`
    /// (which tmux emits on detach to switch back to the blank normal buffer).
    static func tmuxAltScreenSlice(in bytes: [UInt8]) -> ArraySlice<UInt8> {
        let start = tmuxAttachScreenOffset(in: bytes) ?? 0
        let exitNeedles: [[UInt8]] = [
            [0x1b, 0x5b, 0x3f, 0x31, 0x30, 0x34, 0x39, 0x6c], // \e[?1049l
            [0x1b, 0x5b, 0x3f, 0x34, 0x37, 0x6c],             // \e[?47l
        ]
        if let end = firstMatchOffset(in: bytes, from: start, needles: exitNeedles) {
            return bytes[start..<end]
        }
        return bytes[start...]
    }

    private static func firstMatchOffset(in bytes: [UInt8], from startIndex: Int, needles: [[UInt8]]) -> Int? {
        var earliest: Int?
        for needle in needles where bytes.count >= startIndex + needle.count {
            let limit = bytes.count - needle.count
            for i in startIndex...limit {
                if bytes[i] == needle[0], Array(bytes[i..<(i + needle.count)]) == needle {
                    if earliest == nil || i < earliest! { earliest = i }
                    break
                }
            }
        }
        return earliest
    }
}
