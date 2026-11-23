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
        return (["[stop]", "Thinking ("], ["Action Required"])
    }
    if base.contains("codex") {
        return (["esc to interrupt", "Esc to interrupt"], [])
    }
    return ([], [])
}
