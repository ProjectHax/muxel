import Foundation

/// Classification of a unified-diff line, for coloring the read-only diff viewer.
enum DiffLineKind: Equatable {
    case add      // +…  (but not the +++ file header)
    case remove   // -…  (but not the --- file header)
    case hunk     // @@ … @@
    case meta     // file headers, index lines, the "# Changes in …" preamble
    case context  // unchanged line
}

/// Classify one diff line. Order matters: `@@` and the `+++`/`---` file headers are
/// checked before the plain `+`/`-` add/remove tests.
func diffLineKind(_ line: String) -> DiffLineKind {
    if line.hasPrefix("@@") { return .hunk }
    if line.hasPrefix("+++") || line.hasPrefix("---") { return .meta }
    if line.hasPrefix("diff --git") || line.hasPrefix("index ")
        || line.hasPrefix("new file") || line.hasPrefix("deleted file")
        || line.hasPrefix("rename ") || line.hasPrefix("similarity ")
        || line.hasPrefix("Binary files") || line.hasPrefix("# ") {
        return .meta
    }
    if line.hasPrefix("+") { return .add }
    if line.hasPrefix("-") { return .remove }
    return .context
}
