import Foundation

/// A parsed `[user@]host[:port]` jump-host spec — the ProxyJump grammar desktop
/// muxel passes to `ssh -J`. Jump *chains* (comma-separated) are not supported on
/// iOS; `parse` rejects them so the UI can say so instead of silently using the
/// first hop.
struct JumpHostSpec: Equatable {
    var user: String?
    var host: String
    var port: Int

    /// nil on empty/garbage input (empty user, non-numeric or out-of-range port,
    /// embedded whitespace, or a comma chain).
    static func parse(_ s: String) -> JumpHostSpec? {
        let trimmed = s.trimmingCharacters(in: .whitespacesAndNewlines)
        guard !trimmed.isEmpty, !trimmed.contains(",") else { return nil }

        var rest = Substring(trimmed)
        var user: String?
        if let at = rest.lastIndex(of: "@") {
            let u = rest[..<at]
            guard !u.isEmpty else { return nil }
            user = String(u)
            rest = rest[rest.index(after: at)...]
        }

        var port = 22
        if let colon = rest.lastIndex(of: ":") {
            guard let n = Int(rest[rest.index(after: colon)...]), (1...65535).contains(n)
            else { return nil }
            port = n
            rest = rest[..<colon]
        }

        guard !rest.isEmpty, !rest.contains(where: \.isWhitespace) else { return nil }
        return JumpHostSpec(user: user, host: String(rest), port: port)
    }
}
