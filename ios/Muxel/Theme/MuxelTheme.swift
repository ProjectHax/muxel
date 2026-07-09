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

    /// A label color guaranteed to be readable on `hex` — near-white on a dark
    /// background, near-black on a light one. Used where the theme's own `fg`/`muted`
    /// can't be trusted to contrast with a surface (some ported palettes have weak
    /// fg-on-surface contrast). Pure luminance, so it works on any palette.
    func readableText(on hex: String) -> Color {
        let (r, g, b, _) = muxelHexComponents(hex)
        let luminance = 0.2126 * r + 0.7152 * g + 0.0722 * b
        return luminance < 0.55 ? Color(white: 0.96) : Color(white: 0.12)
    }

    // Terminal grid colors. The grid always renders **dark**, even under a light
    // chrome theme: remote programs (shells, agents) assume a dark terminal and
    // hardcode near-white text via 256-color / truecolor, which muxel can't
    // remap and which vanishes on a light background. For dark themes this is
    // just `bg`/`fg`; a light theme swaps to its own dark `fg` as the grid
    // background and light `bg` as the text, keeping the theme's identity while
    // staying readable.
    var terminalBgHex: String { isDark ? bg : fg }
    var terminalFgHex: String { isDark ? fg : bg }
    /// The dark grid's black (ANSI 0): the near-bg subtle black for dark themes,
    /// the muted grey (readable on the dark grid) for light themes.
    var terminalBlackHex: String { isDark ? bg : muted }
    var terminalBackground: Color { Color(hex: terminalBgHex) }
}

extension MuxelTheme {
    /// The curated switcher set. Order = display order; Mocha is the default.
    static let all: [MuxelTheme] = [
        // Curated originals (Mocha is the default), then the ported dark + light sets.
        mocha, macchiato, frappe, latte, tokyoNight, gruvbox, everforest, solarized, matrix,
        adventure, adventureTime, alduin, asciinema, ayuDark, fahrenheit,
        flexokiDark, harper, hybridDark, jellybeans, kibble, macosClassicDark,
        mellifluousDark, molokaiDark, spaceduck, twilight,
        ayuLight, flexokiLight, hybridLight, macosClassicLight, mellifluousLight, molokaiLight,
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

    // ---- Ported from desktop theme JSONs (crates/muxel/assets/themes) ----
    static let adventure = MuxelTheme(
        id: "adventure", name: "Adventure", isDark: true,
        bg: "#040404", surface: "#003a5b", surfaceAlt: "#0e0e0e", border: "#282828",
        fg: "#feffff", muted: "#5d6165", accent: "#4384ad",
        red: "#d84a33", green: "#5da602", yellow: "#aa7900", blue: "#417ab3",
        magenta: "#882252", cyan: "#41b3a9")

    static let adventureTime = MuxelTheme(
        id: "adventure-time", name: "Adventure Time", isDark: true,
        bg: "#1f1d45", surface: "#003a5b", surfaceAlt: "#1c1a37", border: "#333150",
        fg: "#C7C7D4", muted: "#717192", accent: "#5f72c6",
        red: "#a02733", green: "#549235", yellow: "#ce7837", blue: "#2b53ab",
        magenta: "#665993", cyan: "#26977b")

    static let alduin = MuxelTheme(
        id: "alduin", name: "Alduin", isDark: true,
        bg: "#1C1C1C", surface: "#282828", surfaceAlt: "#262626", border: "#3a3a3a",
        fg: "#9E9E9E", muted: "#878787", accent: "#458588",
        red: "#8b5f61", green: "#7a875f", yellow: "#9d906c", blue: "#87afaf",
        magenta: "#af8787", cyan: "#878787")

    static let asciinema = MuxelTheme(
        id: "asciinema", name: "Asciinema", isDark: true,
        bg: "#121314", surface: "#181919", surfaceAlt: "#1a1b1c", border: "#3a3a3a",
        fg: "#cccccc", muted: "#6d6d6d", accent: "#26b0d7",
        red: "#dd3c69", green: "#4ebf22", yellow: "#ddaf3c", blue: "#26b0d7",
        magenta: "#b954e1", cyan: "#54e1b9")

    static let ayuLight = MuxelTheme(
        id: "ayu-light", name: "Ayu Light", isDark: false,
        bg: "#FCFCFC", surface: "#F3F4F5", surfaceAlt: "#E6E6E6", border: "#cfd1d2",
        fg: "#5c6166", muted: "#99a0a6", accent: "#55b4d3",
        red: "#F07171", green: "#85b304", yellow: "#F1AD49", blue: "#55b4d3",
        magenta: "#9371f0", cyan: "#4dbf99")

    static let ayuDark = MuxelTheme(
        id: "ayu-dark", name: "Ayu Dark", isDark: true,
        bg: "#0D1016", surface: "#16191F", surfaceAlt: "#191F2A", border: "#292a2c",
        fg: "#B3B1AD", muted: "#52514f", accent: "#5ac1fe",
        red: "#ef7177", green: "#aad84c", yellow: "#FEB454", blue: "#5ac1fe",
        magenta: "#d2a6ff", cyan: "#5a728b")

    static let fahrenheit = MuxelTheme(
        id: "fahrenheit", name: "Fahrenheit", isDark: true,
        bg: "#000000", surface: "#1e1e1e", surfaceAlt: "#090909", border: "#252525",
        fg: "#FFFFCE", muted: "#828282", accent: "#720202",
        red: "#723202", green: "#027225", yellow: "#726302", blue: "#022d72",
        magenta: "#3a286c", cyan: "#027272")

    static let flexokiLight = MuxelTheme(
        id: "flexoki-light", name: "Flexoki Light", isDark: false,
        bg: "#FFFCF0", surface: "#F2F0E5", surfaceAlt: "#F2F0E5", border: "#E6E4D9",
        fg: "#100F0F", muted: "#6F6E69", accent: "#3AA99F",
        red: "#D14D41", green: "#879A39", yellow: "#D0A215", blue: "#4385BE",
        magenta: "#CE5D97", cyan: "#3AA99F")

    static let flexokiDark = MuxelTheme(
        id: "flexoki-dark", name: "Flexoki Dark", isDark: true,
        bg: "#100F0F", surface: "#1C1B1A", surfaceAlt: "#1C1B1A", border: "#282726",
        fg: "#CECDC3", muted: "#878580", accent: "#24837B",
        red: "#AF3029", green: "#66800B", yellow: "#AD8301", blue: "#205EA6",
        magenta: "#A02F6F", cyan: "#24837B")

    static let harper = MuxelTheme(
        id: "harper", name: "Harper", isDark: true,
        bg: "#010101", surface: "#003a5b", surfaceAlt: "#18151B", border: "#333333",
        fg: "#a8a49d", muted: "#726E69", accent: "#B196C6",
        red: "#ff5874", green: "#489e48", yellow: "#f8b63f", blue: "#7fb5e1",
        magenta: "#b296c6", cyan: "#bff5e5")

    static let hybridLight = MuxelTheme(
        id: "hybrid-light", name: "Hybrid Light", isDark: false,
        bg: "#E4E4E4", surface: "#d7d7d7", surfaceAlt: "#DFDFDF", border: "#CACACA",
        fg: "#1c1c1c", muted: "#5f5f5f", accent: "#005f87",
        red: "#5F0000", green: "#005F00", yellow: "#948000", blue: "#00195f",
        magenta: "#5f1c51", cyan: "#005a5f")

    static let hybridDark = MuxelTheme(
        id: "hybrid-dark", name: "Hybrid Dark", isDark: true,
        bg: "#1D1F21", surface: "#1D1F21", surfaceAlt: "#282A2E", border: "#34373c",
        fg: "#e8e8e8", muted: "#878787", accent: "#15678a",
        red: "#8a1515", green: "#7c8a15", yellow: "#8a7c15", blue: "#15678a",
        magenta: "#8a1567", cyan: "#15678a")

    static let jellybeans = MuxelTheme(
        id: "jellybeans", name: "Jellybeans", isDark: true,
        bg: "#151515", surface: "#151515", surfaceAlt: "#1C1C1C", border: "#2e2e2e",
        fg: "#E8E8D3", muted: "#767676", accent: "#97bedc",
        red: "#e27373", green: "#94b979", yellow: "#ffba7b", blue: "#97bedc",
        magenta: "#B294BB", cyan: "#00988e")

    static let kibble = MuxelTheme(
        id: "kibble", name: "Kibble", isDark: true,
        bg: "#0e100a", surface: "#003a5b", surfaceAlt: "#242223", border: "#292a24",
        fg: "#f7f7f7", muted: "#777777", accent: "#6ce05c",
        red: "#C70231", green: "#2BCF13", yellow: "#c7a302", blue: "#3449d1",
        magenta: "#8400ff", cyan: "#0798ab")

    static let macosClassicLight = MuxelTheme(
        id: "macos-classic-light", name: "macOS Classic Light", isDark: false,
        bg: "#F9F9F9", surface: "#EAEAEA", surfaceAlt: "#EFEFEF", border: "#D2D2D2",
        fg: "#000000", muted: "#707070", accent: "#0060de",
        red: "#d21f07", green: "#319a00", yellow: "#B59A00", blue: "#0060de",
        magenta: "#9A0068", cyan: "#007E8A")

    static let macosClassicDark = MuxelTheme(
        id: "macos-classic-dark", name: "macOS Classic Dark", isDark: true,
        bg: "#131313", surface: "#202020", surfaceAlt: "#232323", border: "#303030",
        fg: "#DEDEDE", muted: "#9D9D9D", accent: "#077CFD",
        red: "#FF5257", green: "#30D158", yellow: "#FFC600", blue: "#419CFF",
        magenta: "#A550A7", cyan: "#07FDD3")

    static let mellifluousLight = MuxelTheme(
        id: "mellifluous-light", name: "Mellifluous Light", isDark: false,
        bg: "#E7E7E7", surface: "#fafafa", surfaceAlt: "#F0F0F0", border: "#CACACA",
        fg: "#383a42", muted: "#828997", accent: "#5A6599",
        red: "#C95954", green: "#828040", yellow: "#c98f54", blue: "#a8a1be",
        magenta: "#b39fb0", cyan: "#54c981")

    static let mellifluousDark = MuxelTheme(
        id: "mellifluous-dark", name: "Mellifluous Dark", isDark: true,
        bg: "#1A1A1A", surface: "#282c34", surfaceAlt: "#292929", border: "#444444",
        fg: "#abb2bf", muted: "#828997", accent: "#5A6599",
        red: "#C95954", green: "#828040", yellow: "#c98d54", blue: "#5481c9",
        magenta: "#9C6995", cyan: "#54c9bd")

    static let molokaiLight = MuxelTheme(
        id: "molokai-light", name: "Molokai Light", isDark: false,
        bg: "#FEFAF9", surface: "#FEFAF9", surfaceAlt: "#E4DEDA", border: "#E4DEDA",
        fg: "#0a0a0a", muted: "#767676", accent: "#E14774",
        red: "#e17047", green: "#82cc0a", yellow: "#ccac0a", blue: "#e16032",
        magenta: "#7058be", cyan: "#0acc78")

    static let molokaiDark = MuxelTheme(
        id: "molokai-dark", name: "Molokai Dark", isDark: true,
        bg: "#1b1d1e", surface: "#1b1d1e", surfaceAlt: "#292c2e", border: "#393939",
        fg: "#f8f8f2", muted: "#5b5a54", accent: "#dc1860",
        red: "#dc1860", green: "#82cc0a", yellow: "#807607", blue: "#075880",
        magenta: "#800780", cyan: "#07807e")

    static let spaceduck = MuxelTheme(
        id: "spaceduck", name: "Spaceduck", isDark: true,
        bg: "#0F111B", surface: "#003a5b", surfaceAlt: "#161928", border: "#272C42",
        fg: "#a3a49d", muted: "#4b6479", accent: "#089CC5",
        red: "#e33400", green: "#5ccc96", yellow: "#b89c00", blue: "#00a3cc",
        magenta: "#C86D8C", cyan: "#089CC5")

    static let twilight = MuxelTheme(
        id: "twilight", name: "Twilight", isDark: true,
        bg: "#141414", surface: "#2e2e2e", surfaceAlt: "#1E1E1E", border: "#343434",
        fg: "#dcdcdc", muted: "#828282", accent: "#CDA869",
        red: "#c06d44", green: "#afb97a", yellow: "#c2a86c", blue: "#44474a",
        magenta: "#b4be7c", cyan: "#778385")
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
