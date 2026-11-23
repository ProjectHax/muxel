import XCTest
@testable import muxel

/// The terminal grid must render dark even under a light chrome theme, so remote
/// programs that assume a dark background (and hardcode near-white text) stay
/// readable. See `MuxelTheme.terminalBgHex`.
final class ThemeTerminalTests: XCTestCase {

    func testDarkThemeUsesOwnBackground() {
        let mocha = MuxelTheme.mocha
        XCTAssertTrue(mocha.isDark)
        XCTAssertEqual(mocha.terminalBgHex, mocha.bg)
        XCTAssertEqual(mocha.terminalFgHex, mocha.fg)
    }

    func testLightThemeSwapsToADarkGrid() {
        let latte = MuxelTheme.latte
        XCTAssertFalse(latte.isDark, "Latte is the light chrome theme")
        // The grid background becomes the theme's dark fg, and the text its
        // light bg — so the grid is dark and the text is light.
        XCTAssertEqual(latte.terminalBgHex, latte.fg)
        XCTAssertEqual(latte.terminalFgHex, latte.bg)
        XCTAssertLessThan(
            luminance(latte.terminalBgHex), luminance(latte.terminalFgHex),
            "grid background must be darker than the grid text")
    }

    func testEveryThemeGridIsDark() {
        // Whatever the chrome, the grid background must be darker than its text.
        for theme in MuxelTheme.all {
            XCTAssertLessThan(
                luminance(theme.terminalBgHex), luminance(theme.terminalFgHex),
                "\(theme.name): grid background should be darker than grid text")
        }
    }

    func testAllThemeIdsAreUnique() {
        let ids = MuxelTheme.all.map(\.id)
        XCTAssertEqual(Set(ids).count, ids.count, "theme ids must be unique")
    }

    func testByIdRoundTripsEveryTheme() {
        for t in MuxelTheme.all {
            XCTAssertEqual(MuxelTheme.byId(t.id).id, t.id, "byId should round-trip \(t.name)")
        }
        // An unknown id falls back to the default (Mocha).
        XCTAssertEqual(MuxelTheme.byId("nope").id, MuxelTheme.mocha.id)
    }

    func testAccentIsNotAParseFallbackForPortedThemes() {
        // A broken hex parses to pure black; every theme's accent should be a real color.
        for t in MuxelTheme.all {
            let (r, g, b, _) = muxelHexComponents(t.accent)
            XCTAssertFalse(r == 0 && g == 0 && b == 0, "\(t.name) accent looks like a parse fallback")
        }
    }

    func testStatusDotStyleDistinguishesRunningStates() {
        XCTAssertEqual(StatusDotStyle.resolve(status: .idle, running: false), .stopped)
        XCTAssertEqual(StatusDotStyle.resolve(status: .working, running: false), .stopped)
        XCTAssertEqual(StatusDotStyle.resolve(status: .idle, running: true), .idle)
        XCTAssertEqual(StatusDotStyle.resolve(status: .working, running: true), .working)
        XCTAssertEqual(StatusDotStyle.resolve(status: .done, running: true), .done)
        XCTAssertEqual(StatusDotStyle.resolve(status: .blocked, running: true), .blocked)
    }

    func testStatusDotsStayLegibleAcrossEveryThemeAndSurface() {
        let cases: [(StatusDotStyle, Bool?)] = [
            (.blocked, true), (.working, true), (.done, true), (.idle, true), (.stopped, false),
        ]
        for theme in MuxelTheme.all {
            for surface in [theme.bg, theme.surface, theme.accent] {
                let bg = StatusDotContrast.RGB(hex: surface)
                for (style, running) in cases {
                    let raw = StatusDotContrast.rawHex(for: style, running: running, in: theme)
                    let minContrast = style == .stopped ? 2.8 : 3.4
                    let adjusted = StatusDotContrast.legibleRGB(hex: raw, on: surface, minContrast: minContrast)
                    let ratio = StatusDotContrast.contrastRatio(adjusted, bg)
                    XCTAssertGreaterThanOrEqual(
                        ratio, minContrast - 0.05,
                        "\(theme.name) (\(style) on \(surface)): contrast \(ratio) too low")
                }
            }
        }
    }

    func testExitCopyModeCommandShape() {
        XCTAssertEqual(TmuxCommands.exitCopyMode(session: "muxel_box_12345678"),
                       ["copy-mode", "-q", "-t", "=muxel_box_12345678:"])
        XCTAssertEqual(TmuxCommands.setWindowSizeLatest(session: "muxel_box_12345678"),
                       ["set-option", "-w", "-t", "=muxel_box_12345678:", "window-size", "latest"])
    }

    /// Rough relative luminance (0 = black, 1 = white) from a `#rrggbb` hex.
    private func luminance(_ hex: String) -> Double {
        let (r, g, b, _) = muxelHexComponents(hex)
        return 0.2126 * r + 0.7152 * g + 0.0722 * b
    }
}
