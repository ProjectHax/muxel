import SwiftUI

/// muxel's terminal-flavored SwiftUI building blocks: a mono chrome font, "pane"
/// cards (echoing the tiled panes in the brand mark), a prompt-caret section header,
/// and a faint terminal-grid texture for empty states.

extension Font {
    /// The app's mono chrome font (SF Mono) — used for terminal-flavored labels.
    static func mono(_ style: Font.TextStyle = .body, weight: Font.Weight = .regular) -> Font {
        .system(style, design: .monospaced).weight(weight)
    }
}

extension View {
    /// Style content as a terminal "pane": surface fill, a subtle (or accent, when
    /// active) border, and rounded corners — echoes the panes in the muxel mark.
    func paneCard(_ theme: MuxelTheme, active: Bool = false, radius: CGFloat = 10) -> some View {
        background(active ? theme.accentColor.opacity(0.14) : theme.surfaceColor)
            .clipShape(RoundedRectangle(cornerRadius: radius, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: radius, style: .continuous)
                    .strokeBorder(
                        active ? theme.accentColor.opacity(0.9) : theme.borderColor,
                        lineWidth: active ? 1.3 : 1)
            )
    }

    /// Fill the screen edge-to-edge with the theme background.
    func muxelBackground(_ theme: MuxelTheme) -> some View {
        background(theme.background.ignoresSafeArea())
    }
}

/// A mono section header with a prompt caret, e.g. `❯ HOSTS`.
struct PromptHeader: View {
    @Environment(\.theme) private var theme
    let text: String
    var body: some View {
        HStack(spacing: 6) {
            Text("❯")
                .font(.mono(.caption, weight: .bold))
                .foregroundStyle(theme.accentColor)
            Text(text.uppercased())
                .font(.mono(.caption, weight: .semibold))
                .tracking(1.5)
                .foregroundStyle(theme.mutedColor)
        }
    }
}

/// A faint terminal-grid texture — used behind empty states for a bit of character.
struct GridBackground: View {
    @Environment(\.theme) private var theme
    var spacing: CGFloat = 22
    var body: some View {
        Canvas { ctx, size in
            var path = Path()
            var x: CGFloat = 0
            while x <= size.width {
                path.move(to: CGPoint(x: x, y: 0))
                path.addLine(to: CGPoint(x: x, y: size.height))
                x += spacing
            }
            var y: CGFloat = 0
            while y <= size.height {
                path.move(to: CGPoint(x: 0, y: y))
                path.addLine(to: CGPoint(x: size.width, y: y))
                y += spacing
            }
            ctx.stroke(path, with: .color(theme.borderColor.opacity(0.35)), lineWidth: 0.5)
        }
        .allowsHitTesting(false)
    }
}
