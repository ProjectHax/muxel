import Foundation
import SwiftUI

extension AgentStatus {
    /// Themed status color — matches the brand mark's pane dots (green running,
    /// yellow working, red blocked, muted idle).
    func color(_ theme: MuxelTheme) -> Color {
        switch self {
        case .working: return theme.workingColor
        case .idle: return theme.mutedColor
        case .blocked: return theme.blockedColor
        case .done: return theme.runningColor
        }
    }

    var label: String {
        switch self {
        case .working: return "working"
        case .idle: return "idle"
        case .blocked: return "blocked"
        case .done: return "done"
        }
    }
}

/// Visual category for `StatusDot`, extracted pure so every running/status combination
/// stays unit-testable.
enum StatusDotStyle: Equatable {
    case stopped
    case idle
    case working
    case done
    case blocked

    static func resolve(status: AgentStatus, running: Bool?) -> StatusDotStyle {
        if running == false { return .stopped }
        switch status {
        case .blocked: return .blocked
        case .working: return .working
        case .done: return .done
        case .idle: return .idle
        }
    }
}

/// Pure WCAG contrast resolver ensuring status dots (and status text labels) stay
/// clearly legible across every dark/light theme and active accent tab chip.
enum StatusDotContrast {
    struct RGB: Equatable {
        let r: Double
        let g: Double
        let b: Double

        init(r: Double, g: Double, b: Double) {
            self.r = min(1, max(0, r))
            self.g = min(1, max(0, g))
            self.b = min(1, max(0, b))
        }

        init(hex: String) {
            let (r, g, b, _) = muxelHexComponents(hex)
            self.init(r: r, g: g, b: b)
        }

        var color: Color { Color(.sRGB, red: r, green: g, blue: b, opacity: 1) }
    }

    static func relativeLuminance(_ c: RGB) -> Double {
        func linear(_ x: Double) -> Double {
            x <= 0.04045 ? x / 12.92 : pow((x + 0.055) / 1.055, 2.4)
        }
        return 0.2126 * linear(c.r) + 0.7152 * linear(c.g) + 0.0722 * linear(c.b)
    }

    static func contrastRatio(_ a: RGB, _ b: RGB) -> Double {
        let la = relativeLuminance(a), lb = relativeLuminance(b)
        return (max(la, lb) + 0.05) / (min(la, lb) + 0.05)
    }

    /// Return a color derived from `dotHex` that achieves at least `minContrast`
    /// against `surfaceHex`. When the theme's raw color already meets the target
    /// (e.g. Catppuccin Mocha / Tokyo Night), it is returned verbatim. Otherwise it
    /// boosts chroma/brightness (on dark surfaces) or deepens luminance (on light
    /// surfaces) while preserving the hue.
    static func legibleRGB(hex dotHex: String, on surfaceHex: String, minContrast: Double = 3.2) -> RGB {
        let dot = RGB(hex: dotHex)
        let bg = RGB(hex: surfaceHex)
        if contrastRatio(dot, bg) >= minContrast { return dot }

        let white = RGB(r: 1, g: 1, b: 1)
        let black = RGB(r: 0, g: 0, b: 0)
        let lighten = contrastRatio(white, bg) >= contrastRatio(black, bg)

        var candidate = dot
        if lighten {
            // First scale channels up proportionally to boost saturation before
            // mixing toward white (keeps dark themes like Fahrenheit vivid instead
            // of washing out to gray/pastel).
            let peak = max(dot.r, max(dot.g, dot.b))
            if peak > 0.02, peak < 1 {
                let saturated = RGB(r: dot.r / peak, g: dot.g / peak, b: dot.b / peak)
                if contrastRatio(saturated, bg) >= minContrast {
                    // Binary-search scale between 1.0 and 1/peak.
                    var lo = 1.0, hi = 1.0 / peak
                    for _ in 0..<16 {
                        let mid = (lo + hi) * 0.5
                        let probe = RGB(r: dot.r * mid, g: dot.g * mid, b: dot.b * mid)
                        if contrastRatio(probe, bg) >= minContrast { hi = mid } else { lo = mid }
                    }
                    return RGB(r: dot.r * hi, g: dot.g * hi, b: dot.b * hi)
                }
                candidate = saturated
            }
        }

        let target = lighten ? white : black
        var lo = 0.0, hi = 1.0
        for _ in 0..<16 {
            let mid = (lo + hi) * 0.5
            let probe = RGB(
                r: candidate.r + (target.r - candidate.r) * mid,
                g: candidate.g + (target.g - candidate.g) * mid,
                b: candidate.b + (target.b - candidate.b) * mid
            )
            if contrastRatio(probe, bg) >= minContrast { hi = mid } else { lo = mid }
        }
        return RGB(
            r: candidate.r + (target.r - candidate.r) * hi,
            g: candidate.g + (target.g - candidate.g) * hi,
            b: candidate.b + (target.b - candidate.b) * hi
        )
    }

    static func legibleColor(hex dotHex: String, on surfaceHex: String, minContrast: Double = 3.2) -> Color {
        legibleRGB(hex: dotHex, on: surfaceHex, minContrast: minContrast).color
    }

    /// Raw hex from `theme` for a given dot style (before contrast adjustment).
    static func rawHex(for style: StatusDotStyle, running: Bool?, in theme: MuxelTheme) -> String {
        switch style {
        case .stopped: return theme.muted
        case .idle: return running == true ? theme.green : theme.muted
        case .working: return theme.yellow
        case .done: return theme.green
        case .blocked: return theme.red
        }
    }

    /// Whether the dot sits on a light/bright surface (e.g. a light theme or a bright
    /// accent chip) and should use a dark separating rim rather than a light one.
    static func prefersDarkRim(on surfaceHex: String) -> Bool {
        relativeLuminance(RGB(hex: surfaceHex)) > 0.28
    }
}

/// A small colored status dot. Reflects distinct agent states on live sessions:
/// yellow (working), red (blocked), solid green (done), hollow green ring (running idle),
/// and muted gray when no live tmux session exists. Automatically adjusts contrast and
/// adds a crisp separating rim against `surfaceHex` (defaulting to `theme.bg`) so dots
/// stay clearly visible on every dark/light theme and active accent tab chip.
struct StatusDot: View {
    @Environment(\.theme) private var theme
    let status: AgentStatus
    var running: Bool?
    /// Hex of the background surface directly behind this dot (e.g. an active tab
    /// chip's accent fill). Defaults to `theme.bg`.
    var surfaceHex: String? = nil

    private var style: StatusDotStyle { StatusDotStyle.resolve(status: status, running: running) }
    private var resolvedSurfaceHex: String { surfaceHex ?? theme.bg }

    var body: some View {
        let dotHex = StatusDotContrast.rawHex(for: style, running: running, in: theme)
        let minContrast = style == .stopped ? 2.8 : 3.4
        let dotColor = StatusDotContrast.legibleColor(hex: dotHex, on: resolvedSurfaceHex, minContrast: minContrast)
        let darkRim = StatusDotContrast.prefersDarkRim(on: resolvedSurfaceHex)
        let rimColor: Color = darkRim ? Color.black.opacity(0.45) : Color.white.opacity(style == .stopped ? 0.28 : 0.22)

        return Group {
            if style == .idle, running == true {
                Circle()
                    .fill(dotColor.opacity(darkRim ? 0.25 : 0.35))
                    .overlay(Circle().strokeBorder(dotColor, lineWidth: 1.75))
                    .overlay(Circle().strokeBorder(rimColor, lineWidth: darkRim ? 0.6 : 0))
                    .frame(width: 10, height: 10)
            } else {
                Circle()
                    .fill(dotColor)
                    .overlay(Circle().strokeBorder(rimColor, lineWidth: 0.9))
                    .frame(width: 10, height: 10)
                    .shadow(color: dotColor.opacity(glow && !darkRim ? 0.65 : 0), radius: glow && !darkRim ? 3 : 0)
            }
        }
        .accessibilityLabel(running == false ? "stopped" : status.label)
    }

    private var glow: Bool {
        switch style {
        case .working, .done, .blocked: return true
        case .stopped, .idle: return false
        }
    }
}
