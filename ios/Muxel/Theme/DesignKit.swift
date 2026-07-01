import SwiftUI

/// muxel's terminal-flavored SwiftUI building blocks: a mono chrome font, "pane"
/// cards (echoing the tiled panes in the brand mark), prompt-caret headers/labels,
/// themed form sections, a shared centered empty/error state, and a faint
/// terminal-grid texture.

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

    /// Fill the screen edge-to-edge with the active theme background.
    func muxelBackground() -> some View {
        modifier(MuxelBackgroundModifier())
    }

    /// Theme a sheet's `Form`/`List`: hide the system grouped background, fill with
    /// the theme background, and tint controls with the theme accent. Pair the
    /// sections inside with `MuxelSection` for themed rows.
    func muxelSheet() -> some View {
        modifier(MuxelSheetModifier())
    }
}

private struct MuxelBackgroundModifier: ViewModifier {
    @Environment(\.theme) private var theme
    func body(content: Content) -> some View {
        content.background(theme.background.ignoresSafeArea())
    }
}

private struct MuxelSheetModifier: ViewModifier {
    @Environment(\.theme) private var theme
    func body(content: Content) -> some View {
        content
            .scrollContentBackground(.hidden)
            .muxelBackground()
            .tint(theme.accentColor)
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

/// A mono prompt-echo line: an accent caret + muted text, e.g. `❯ no panes yet`.
/// Unlike `PromptHeader` it keeps the text's case (a message, not a section title).
struct PromptLabel: View {
    @Environment(\.theme) private var theme
    let text: String
    var style: Font.TextStyle = .callout

    var body: some View {
        HStack(spacing: 6) {
            Text("❯")
                .font(.mono(style, weight: .bold))
                .foregroundStyle(theme.accentColor)
            Text(text)
                .font(.mono(style))
                .foregroundStyle(theme.mutedColor)
        }
    }
}

/// A `Form`/`List` section in the muxel visual language: a `❯ HEADER` prompt header
/// and theme-surface rows (`listRowBackground` must be applied per section, which is
/// why this wrapper exists instead of hand-styling every sheet).
struct MuxelSection<Content: View, Footer: View>: View {
    @Environment(\.theme) private var theme
    private let header: String?
    private let content: Content
    private let footer: Footer

    init(_ header: String? = nil,
         @ViewBuilder content: () -> Content,
         @ViewBuilder footer: () -> Footer) {
        self.header = header
        self.content = content()
        self.footer = footer()
    }

    var body: some View {
        Section {
            content
                .listRowBackground(theme.surfaceColor)
                .foregroundStyle(theme.textColor)
        } header: {
            if let header { PromptHeader(text: header) }
        } footer: {
            footer
                .font(.mono(.footnote))
                .foregroundStyle(theme.mutedColor)
        }
    }
}

extension MuxelSection where Footer == EmptyView {
    init(_ header: String? = nil, @ViewBuilder content: () -> Content) {
        self.init(header, content: content) { EmptyView() }
    }
}

/// The shared centered empty / error / loading state: an icon or spinner, a mono
/// title (optionally as a `❯` prompt line), an optional muted message, optional
/// grid texture, and an actions slot (retry / launch buttons). One component so
/// every screen's states match.
struct CenteredState<Actions: View>: View {
    @Environment(\.theme) private var theme
    private let icon: String?
    private let iconColor: Color?
    private let spinner: Bool
    private let title: String
    private let prompt: Bool
    private let message: String?
    private let showsGrid: Bool
    private let actions: Actions

    init(icon: String? = nil,
         iconColor: Color? = nil,
         spinner: Bool = false,
         title: String,
         prompt: Bool = false,
         message: String? = nil,
         showsGrid: Bool = false,
         @ViewBuilder actions: () -> Actions) {
        self.icon = icon
        self.iconColor = iconColor
        self.spinner = spinner
        self.title = title
        self.prompt = prompt
        self.message = message
        self.showsGrid = showsGrid
        self.actions = actions()
    }

    var body: some View {
        ZStack {
            if showsGrid { GridBackground().opacity(0.5) }
            VStack(spacing: 10) {
                if spinner {
                    ProgressView()
                        .tint(theme.mutedColor)
                } else if let icon {
                    Image(systemName: icon)
                        .font(.system(size: 34, weight: .light))
                        .foregroundStyle(iconColor ?? theme.mutedColor)
                }
                if prompt {
                    PromptLabel(text: title)
                } else {
                    Text(title)
                        .font(.mono(.callout, weight: .semibold))
                        .foregroundStyle(theme.textColor)
                }
                if let message {
                    Text(message)
                        .font(.mono(.caption))
                        .foregroundStyle(theme.mutedColor)
                        .multilineTextAlignment(.center)
                }
                actions
            }
            .padding(24)
        }
        .frame(maxWidth: .infinity, maxHeight: .infinity)
    }
}

extension CenteredState where Actions == EmptyView {
    init(icon: String? = nil,
         iconColor: Color? = nil,
         spinner: Bool = false,
         title: String,
         prompt: Bool = false,
         message: String? = nil,
         showsGrid: Bool = false) {
        self.init(icon: icon, iconColor: iconColor, spinner: spinner, title: title,
                  prompt: prompt, message: message, showsGrid: showsGrid) { EmptyView() }
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
