import SwiftUI

/// A muxel color theme — the chrome + terminal palette, ported from the desktop theme
/// JSONs (`crates/muxel/assets/themes/*.json`, the `colors` block). Colors are stored
/// as hex and exposed as SwiftUI `Color`s here; the terminal ANSI mapping lives in
/// `TerminalTheme.swift`. Keep these in sync if the desktop palettes change.
///
/// The identity anchor is Catppuccin **Mocha** — the same palette as `muxel.svg`
/// (accent `#89b4fa`, running `#a6e3a1`, working `#f9e2af`, blocked `#f38ba8`).
struct MuxelTheme: Identifiable, Equatable {
    let id: String
    let name: String
    let isDark: Bool
    // Chrome (hex strings).
    let bg: String
    let surface: String
    let surfaceAlt: String
    let border: String
    let fg: String
    let muted: String
    let accent: String
    // Base hues — drive status colors and the terminal ANSI palette.
    let red: String
    let green: String
    let yellow: String
    let blue: String
    let magenta: String
    let cyan: String

    // Chrome accessors.
    var background: Color { Color(hex: bg) }
    var surfaceColor: Color { Color(hex: surface) }
    var surfaceAltColor: Color { Color(hex: surfaceAlt) }
    var borderColor: Color { Color(hex: border) }
    var textColor: Color { Color(hex: fg) }
    var mutedColor: Color { Color(hex: muted) }
    var accentColor: Color { Color(hex: accent) }
    // Semantic status colors (match the brand mark's pane dots).
    var runningColor: Color { Color(hex: green) }
    var workingColor: Color { Color(hex: yellow) }
    var blockedColor: Color { Color(hex: red) }
}

extension MuxelTheme {
    /// The curated switcher set. Order = display order; Mocha is the default.
    static let all: [MuxelTheme] = [
        mocha, macchiato, frappe, latte, tokyoNight, gruvbox, everforest, solarized, matrix,
    ]

    static func byId(_ id: String?) -> MuxelTheme {
        all.first { $0.id == id } ?? mocha
    }

    static let mocha = MuxelTheme(
        id: "catppuccin-mocha", name: "Catppuccin Mocha", isDark: true,
        bg: "#181825", surface: "#302d41", surfaceAlt: "#1e1e2e", border: "#313244",
        fg: "#cdd6f4", muted: "#6c7086", accent: "#89b4fa",
        red: "#f38ba8", green: "#a6e3a1", yellow: "#f9e2af", blue: "#89b4fa",
        magenta: "#f5c2e7", cyan: "#94e2d5")

    static let macchiato = MuxelTheme(
        id: "catppuccin-macchiato", name: "Catppuccin Macchiato", isDark: true,
        bg: "#1e2030", surface: "#363a4f", surfaceAlt: "#24273a", border: "#494d64",
        fg: "#cad3f5", muted: "#b8c0e0", accent: "#8aadf4",
        red: "#ed8796", green: "#a6da95", yellow: "#eed49f", blue: "#8aadf4",
        magenta: "#f5bde6", cyan: "#8bd5ca")

    static let frappe = MuxelTheme(
        id: "catppuccin-frappe", name: "Catppuccin Frappé", isDark: true,
        bg: "#232634", surface: "#414559", surfaceAlt: "#303446", border: "#3e4255",
        fg: "#c6d0f5", muted: "#979db5", accent: "#8caaee",
        red: "#e78284", green: "#a6d189", yellow: "#e7d682", blue: "#8caaee",
        magenta: "#ca9ee6", cyan: "#81c8be")

    static let latte = MuxelTheme(
        id: "catppuccin-latte", name: "Catppuccin Latte", isDark: false,
        bg: "#e5e9ef", surface: "#dce0e8", surfaceAlt: "#eff1f5", border: "#ccd0da",
        fg: "#4c4f69", muted: "#8c8fa1", accent: "#1e66f5",
        red: "#d20f39", green: "#40a02b", yellow: "#df8e1d", blue: "#1e66f5",
        magenta: "#ea76cb", cyan: "#179299")

    static let tokyoNight = MuxelTheme(
        id: "tokyo-night", name: "Tokyo Night", isDark: true,
        bg: "#1a1b26", surface: "#292e42", surfaceAlt: "#24283b", border: "#292e42",
        fg: "#c0caf5", muted: "#565f89", accent: "#7aa2f7",
        red: "#f7768e", green: "#9ece6a", yellow: "#e0af68", blue: "#7aa2f7",
        magenta: "#bb9af7", cyan: "#7dcfff")

    static let gruvbox = MuxelTheme(
        id: "gruvbox-dark", name: "Gruvbox Dark", isDark: true,
        bg: "#1d2021", surface: "#282828", surfaceAlt: "#32302f", border: "#3e3936",
        fg: "#ebdbb2", muted: "#928374", accent: "#fabd2f",
        red: "#fb4934", green: "#b8bb26", yellow: "#fabd2f", blue: "#83a598",
        magenta: "#d3869b", cyan: "#8ec07c")

    static let everforest = MuxelTheme(
        id: "everforest-dark", name: "Everforest Dark", isDark: true,
        bg: "#262e34", surface: "#2e383b", surfaceAlt: "#343f44", border: "#40484c",
        fg: "#d3c6aa", muted: "#859289", accent: "#a7c080",
        red: "#e67e80", green: "#a7c080", yellow: "#dbbc7f", blue: "#7fbbb3",
        magenta: "#d699b6", cyan: "#83c092")

    static let solarized = MuxelTheme(
        id: "solarized-dark", name: "Solarized Dark", isDark: true,
        bg: "#002b36", surface: "#073642", surfaceAlt: "#0a4250", border: "#103a44",
        fg: "#eee8d5", muted: "#839496", accent: "#268bd2",
        red: "#dc322f", green: "#859900", yellow: "#b58900", blue: "#268bd2",
        magenta: "#d33682", cyan: "#2aa198")

    static let matrix = MuxelTheme(
        id: "matrix", name: "Matrix", isDark: true,
        bg: "#020d02", surface: "#002900", surfaceAlt: "#001500", border: "#12410e",
        fg: "#88ff88", muted: "#00aa00", accent: "#00ff41",
        red: "#ff5555", green: "#00ff41", yellow: "#ffea00", blue: "#39a0ff",
        magenta: "#ff5fff", cyan: "#00ffd5")
}

// MARK: - Hex parsing

extension Color {
    /// Parse `#RGB`, `#RRGGBB`, or `#RRGGBBAA`. Malformed input → opaque black.
    init(hex: String) {
        let (r, g, b, a) = muxelHexComponents(hex)
        self.init(.sRGB, red: r, green: g, blue: b, opacity: a)
    }
}

/// Shared hex → (r, g, b, a) in 0...1. Accepts an optional leading `#` and 3/6/8 digits.
func muxelHexComponents(_ hex: String) -> (Double, Double, Double, Double) {
    var s = hex.trimmingCharacters(in: .whitespacesAndNewlines)
    if s.hasPrefix("#") { s.removeFirst() }
    if s.count == 3 { s = s.map { "\($0)\($0)" }.joined() } // #rgb → #rrggbb
    guard let v = UInt64(s, radix: 16) else { return (0, 0, 0, 1) }
    let r, g, b, a: UInt64
    if s.count == 8 {
        r = (v >> 24) & 0xff; g = (v >> 16) & 0xff; b = (v >> 8) & 0xff; a = v & 0xff
    } else {
        r = (v >> 16) & 0xff; g = (v >> 8) & 0xff; b = v & 0xff; a = 0xff
    }
    return (Double(r) / 255, Double(g) / 255, Double(b) / 255, Double(a) / 255)
}

// MARK: - Environment + store

private struct ThemeKey: EnvironmentKey {
    static let defaultValue = MuxelTheme.mocha
}

extension EnvironmentValues {
    /// The active theme, injected at the app root. Read with `@Environment(\.theme)`.
    var theme: MuxelTheme {
        get { self[ThemeKey.self] }
        set { self[ThemeKey.self] = newValue }
    }
}

/// Holds the selected theme and persists the choice. Mutated by the theme picker;
/// its `current` is injected into the environment (and drives tint / color scheme).
@MainActor
final class ThemeStore: ObservableObject {
    @Published var current: MuxelTheme
    let all = MuxelTheme.all
    private let key = "muxel.theme.id"

    init() {
        current = MuxelTheme.byId(UserDefaults.standard.string(forKey: key))
    }

    func select(_ theme: MuxelTheme) {
        current = theme
        UserDefaults.standard.set(theme.id, forKey: key)
    }
}
