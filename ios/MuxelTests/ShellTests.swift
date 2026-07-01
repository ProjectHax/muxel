import XCTest
@testable import muxel

/// Covers `Shell.splitWords`, the quote-aware splitter behind the launch sheet's
/// custom command (the decoding inverse of `Shell.command`).
final class ShellTests: XCTestCase {

    // Quote-free input must match Rust `split_whitespace` (desktop's parse_args):
    // any whitespace run separates, leading/trailing ignored.
    func testPlainWordsMatchSplitWhitespace() {
        XCTAssertEqual(Shell.splitWords("claude --model opus"), ["claude", "--model", "opus"])
        XCTAssertEqual(Shell.splitWords("  a   b\tc  "), ["a", "b", "c"])
        XCTAssertEqual(Shell.splitWords(""), [])
        XCTAssertEqual(Shell.splitWords("   \t "), [])
    }

    func testSingleQuotesAreLiteral() {
        XCTAssertEqual(Shell.splitWords("echo 'hello world'"), ["echo", "hello world"])
        XCTAssertEqual(Shell.splitWords("'has \"double\" quotes'"), ["has \"double\" quotes"])
        // No escapes inside single quotes: backslash is a literal character.
        XCTAssertEqual(Shell.splitWords(#"'a\nb'"#), [#"a\nb"#])
    }

    func testDoubleQuotesWithEscapes() {
        XCTAssertEqual(Shell.splitWords(#"say "hi there""#), ["say", "hi there"])
        XCTAssertEqual(Shell.splitWords(#""a \"quoted\" word""#), [#"a "quoted" word"#])
        XCTAssertEqual(Shell.splitWords(#""back\\slash""#), [#"back\slash"#])
        // A backslash that escapes nothing special stays literal (POSIX).
        XCTAssertEqual(Shell.splitWords(#""a\nb""#), [#"a\nb"#])
    }

    func testEmptyQuotedArgIsAWord() {
        XCTAssertEqual(Shell.splitWords("prog ''"), ["prog", ""])
        XCTAssertEqual(Shell.splitWords("prog \"\""), ["prog", ""])
    }

    func testAdjacentSegmentsConcatenate() {
        XCTAssertEqual(Shell.splitWords(#"a"b c"d"#), ["ab cd"])
        XCTAssertEqual(Shell.splitWords("'a'\\''b'"), ["a'b"])
    }

    func testBackslashOutsideQuotes() {
        XCTAssertEqual(Shell.splitWords(#"path\ with\ spaces"#), ["path with spaces"])
    }

    func testUnbalancedInputIsNil() {
        XCTAssertNil(Shell.splitWords("echo 'unclosed"))
        XCTAssertNil(Shell.splitWords("echo \"unclosed"))
        XCTAssertNil(Shell.splitWords("trailing\\"))
        XCTAssertNil(Shell.splitWords("\"trailing in quotes\\"))
    }

    /// `splitWords` must invert `command` for arbitrary words (spaces, quotes,
    /// unicode) — the round-trip property that makes the pair trustworthy.
    func testRoundTripWithCommand() {
        let cases: [[String]] = [
            ["claude", "--model", "opus"],
            ["echo", "hello world", "it's"],
            ["prog", "", "två ord", "a\"b", #"back\slash"#],
        ]
        for parts in cases {
            XCTAssertEqual(Shell.splitWords(Shell.command(parts)), parts,
                           "round-trip failed for \(parts)")
        }
    }
}
