import Foundation

/// Minimal POSIX shell quoting for building remote command lines sent over an SSH
/// exec channel. Single-quote everything and escape embedded single quotes the
/// standard way (`'\''`), so arbitrary arguments (paths, tmux targets) are safe.
enum Shell {
    static func quote(_ s: String) -> String {
        "'" + s.replacingOccurrences(of: "'", with: "'\\''") + "'"
    }

    /// Join a program + args into a single shell command line, each part quoted.
    static func command(_ parts: [String]) -> String {
        parts.map(quote).joined(separator: " ")
    }

    /// Split a command line into shell words — the decoding inverse of `command(_:)`,
    /// used for the launch sheet's custom command. Whitespace separates words;
    /// single quotes are literal (no escapes inside); double quotes group, with
    /// backslash escaping `\` and `"` inside; a backslash outside quotes escapes the
    /// next character. Adjacent segments concatenate into one word (`a"b c"` →
    /// `ab c`). Returns nil on an unbalanced quote or a trailing backslash.
    ///
    /// For quote-free input this matches Rust's `split_whitespace` (desktop's
    /// `parse_args`): any whitespace run separates, leading/trailing is ignored.
    static func splitWords(_ line: String) -> [String]? {
        var words: [String] = []
        var current = ""
        var inWord = false
        var i = line.startIndex

        func flush() {
            if inWord {
                words.append(current)
                current = ""
                inWord = false
            }
        }

        while i < line.endIndex {
            let ch = line[i]
            switch ch {
            case " ", "\t", "\n", "\r":
                flush()
                i = line.index(after: i)
            case "'":
                inWord = true
                i = line.index(after: i)
                guard let close = line[i...].firstIndex(of: "'") else { return nil }
                current += line[i..<close]
                i = line.index(after: close)
            case "\"":
                inWord = true
                i = line.index(after: i)
                var closed = false
                while i < line.endIndex {
                    let c = line[i]
                    if c == "\\" {
                        let next = line.index(after: i)
                        guard next < line.endIndex else { return nil }
                        let escaped = line[next]
                        if escaped == "\"" || escaped == "\\" {
                            current.append(escaped)
                        } else {
                            // POSIX keeps the backslash when it doesn't escape
                            // anything special inside double quotes.
                            current.append(c)
                            current.append(escaped)
                        }
                        i = line.index(after: next)
                    } else if c == "\"" {
                        closed = true
                        i = line.index(after: i)
                        break
                    } else {
                        current.append(c)
                        i = line.index(after: i)
                    }
                }
                guard closed else { return nil }
            case "\\":
                inWord = true
                let next = line.index(after: i)
                guard next < line.endIndex else { return nil }
                current.append(line[next])
                i = line.index(after: next)
            default:
                inWord = true
                current.append(ch)
                i = line.index(after: i)
            }
        }
        flush()
        return words
    }
}
