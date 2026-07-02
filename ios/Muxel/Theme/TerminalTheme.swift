import SwiftTerm
import UIKit

/// Maps a `MuxelTheme` onto a live SwiftTerm view — background/foreground/cursor plus
/// a 16-color ANSI palette derived from the theme's base hues. Kept separate from
/// `MuxelTheme.swift` so the SwiftTerm dependency stays isolated to the terminal path.
extension MuxelTheme {
    /// SwiftTerm expects exactly 16 ANSI colors (0..7 normal, 8..15 bright). The
    /// desktop theme JSONs don't ship a full ANSI palette, so we build one from the
    /// base hues: black from `terminalBlackHex`, white from the grid foreground,
    /// bright-black from the muted grey, and bright hues lightened. The grid is
    /// always dark (see `terminalBgHex`), so white maps to the light grid text.
    var ansi16: [SwiftTerm.Color] {
        return [
            st(terminalBlackHex), st(red), st(green), st(yellow),
            st(blue), st(magenta), st(cyan), st(terminalFgHex),
            st(muted), stLighten(red), stLighten(green), stLighten(yellow),
            stLighten(blue), stLighten(magenta), stLighten(cyan), st(terminalFgHex),
        ]
    }

    /// Recolor a live terminal to this theme (bg/fg/cursor + ANSI palette), plus
    /// its input chrome: keyboard appearance and the muxel accessory row. The
    /// grid uses `terminalBgHex`/`terminalFgHex` (always a dark pairing) rather
    /// than the chrome bg/fg, so remote programs that assume a dark terminal
    /// stay readable even under a light theme.
    func apply(to view: TerminalView) {
        view.nativeBackgroundColor = UIColor(muxelHex: terminalBgHex)
        view.nativeForegroundColor = UIColor(muxelHex: terminalFgHex)
        view.caretColor = UIColor(muxelHex: accent)
        view.installColors(ansi16)
        (view.inputAccessoryView as? TerminalAccessoryRow)?.apply(theme: self)
        // keyboardAppearance is only re-read on reloadInputViews(); guard so the
        // reload happens once per actual change, not on every updateUIView pass.
        let appearance: UIKeyboardAppearance = isDark ? .dark : .light
        if view.keyboardAppearance != appearance {
            view.keyboardAppearance = appearance
            if view.isFirstResponder { view.reloadInputViews() }
        }
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
