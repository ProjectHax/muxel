import Foundation

/// Per-agent on-screen status markers. Port of `default_markers`
/// (`crates/muxel/src/app.rs`), keyed by the program basename.
///
/// Empty working markers mean "use the output-activity heuristic" (see `classify`).
/// The iOS app only knows the running instance's `program` (not the desktop's
/// per-preset marker overrides), so custom presets fall back to these defaults or
/// the heuristic — fine for the marker agents (claude, opencode).
func defaultMarkers(program: String?, args: [String] = []) -> (working: [String], blocked: [String]) {
    guard
        let program,
        let base = program.split(whereSeparator: { $0 == "/" || $0 == "\\" }).last.map(String.init)
    else {
        return ([], [])
    }
    if base.contains("claude") {
        return (["esc to interrupt"], ["❯ 1.", "Do you want to proceed"])
    }
    if base.contains("opencode") {
        return (["esc interrupt"], ["Permission required"])
    }
    if base == "ollama", args.contains(where: { $0.contains("opencode") }) {
        return (["esc interrupt"], ["Permission required"])
    }
    if base.contains("grok") {
        // Grok's question card keeps `[stop]` on its status row, so the working
        // marker alone can't tell a turn from a question — `classify` checks
        // `isGrokQuestion` first.
        return (["[stop]", "Thinking ("], ["Action Required"])
    }
    if base == "agy" || base == "antigravity" {
        // Antigravity's status reports don't cross tmux, so read its screen.
        // `esc to cancel` sits right under the composer's lower border only while
        // a turn runs; a card or the slash menu puts its controls there instead.
        // Its idle screen repaints every couple of seconds, so having a working
        // marker also keeps the output-activity fallback off. The blocked
        // markers are its trust, command, file-edit and question cards.
        return (["─\nesc to cancel"],
                ["Do you trust the contents", "Run this command?", "Accept this file edit?", "Write-in..."])
    }
    if base.contains("codex") {
        return (["esc to interrupt", "Esc to interrupt"], [])
    }
    return ([], [])
}

/// Grok's unanswered question card (`ask_user_question`). Port of desktop's
/// `is_grok_question` (`crates/muxel-terminal/src/view.rs`): the card's own footer
/// (`Tab:next answer … Shift+x:dismiss`) must close the screen, with its
/// `♦ Waiting on answers for` row, its `↑/↓ navigate · y copy … Enter:submit` row,
/// and at least two numbered options. The card still shows Grok's `[stop]` working
/// marker, so `classify` checks this before the working markers.
///
/// Desktop requires the footer to be the very last row and reads the title
/// otherwise. iOS has no title, so one trailing row (a tmux status bar) is allowed
/// after the footer. A card quoted above the composer is still rejected: the
/// composer box follows it.
func isGrokQuestion(_ screen: String) -> Bool {
    let rows = screen
        .split(separator: "\n", omittingEmptySubsequences: false)
        .map { $0.trimmingCharacters(in: .whitespacesAndNewlines) }
        .filter { !$0.isEmpty }
    func isFooter(_ row: String) -> Bool {
        row.hasPrefix("Tab:next answer")
            && row.contains("Esc:scrollback")
            && row.hasSuffix("Shift+x:dismiss")
    }
    guard let last = rows.last else { return false }
    let footerIndex: Int
    if isFooter(last) {
        footerIndex = rows.count - 1
    } else if rows.count >= 2, isFooter(rows[rows.count - 2]) {
        footerIndex = rows.count - 2
    } else {
        return false
    }
    let card = rows[..<footerIndex]
    guard card.contains(where: { $0.hasPrefix("♦ Waiting on answers for ") }),
          card.contains(where: { $0.contains("↑/↓ navigate · y copy") && $0.contains("Enter:submit") })
    else { return false }
    let options = card.filter { raw in
        let row = raw.drop(while: { $0 == "│" }).trimmingCharacters(in: .whitespaces)
        let split = row.range(of: " (○) ") ?? row.range(of: " (●) ")
        guard let split,
              let number = Int(row[..<split.lowerBound]), number > 0
        else { return false }
        return !row[split.upperBound...].trimmingCharacters(in: .whitespaces).isEmpty
    }
    return options.count >= 2
}
