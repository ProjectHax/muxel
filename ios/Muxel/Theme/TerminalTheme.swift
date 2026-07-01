import SwiftTerm
import UIKit

/// Maps a `MuxelTheme` onto a live SwiftTerm view — background/foreground/cursor plus
/// a 16-color ANSI palette derived from the theme's base hues. Kept separate from
/// `MuxelTheme.swift` so the SwiftTerm dependency stays isolated to the terminal path.
extension MuxelTheme {
    /// SwiftTerm expects exactly 16 ANSI colors (0..7 normal, 8..15 bright). The
    /// desktop theme JSONs don't ship a full ANSI palette, so we build one from the
    /// base hues: black from the background (fg on light themes), white from the
    /// foreground, bright-black from the muted grey, and bright hues lightened.
    var ansi16: [SwiftTerm.Color] {
        let blackHex = isDark ? bg : fg
        return [
            st(blackHex), st(red), st(green), st(yellow),
            st(blue), st(magenta), st(cyan), st(fg),
            st(muted), stLighten(red), stLighten(green), stLighten(yellow),
            stLighten(blue), stLighten(magenta), stLighten(cyan), st(fg),
        ]
    }

    /// Recolor a live terminal to this theme (bg/fg/cursor + ANSI palette).
    func apply(to view: TerminalView) {
        view.nativeBackgroundColor = UIColor(muxelHex: bg)
        view.nativeForegroundColor = UIColor(muxelHex: fg)
        view.caretColor = UIColor(muxelHex: accent)
        view.installColors(ansi16)
    }
}

extension UIColor {
    /// Build a UIColor from a `#RRGGBB(AA)` hex string (shares the theme parser).
    convenience init(muxelHex hex: String) {
        let (r, g, b, a) = muxelHexComponents(hex)
        self.init(red: r, green: g, blue: b, alpha: a)
    }
}

private func comp(_ x: Double) -> UInt16 { UInt16(max(0, min(1, x)) * 65535) }

private func st(_ hex: String) -> SwiftTerm.Color {
    let (r, g, b, _) = muxelHexComponents(hex)
    return SwiftTerm.Color(red: comp(r), green: comp(g), blue: comp(b))
}

/// A hex color lightened toward white by `f` — for the bright ANSI variants.
private func stLighten(_ hex: String, _ f: Double = 0.3) -> SwiftTerm.Color {
    let (r, g, b, _) = muxelHexComponents(hex)
    return SwiftTerm.Color(
        red: comp(r + (1 - r) * f),
        green: comp(g + (1 - g) * f),
        blue: comp(b + (1 - b) * f))
}
