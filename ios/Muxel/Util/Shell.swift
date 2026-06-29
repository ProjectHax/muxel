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
}
