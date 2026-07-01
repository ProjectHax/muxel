import SwiftUI

/// The theme switcher: a live-previewing list of the ported muxel themes. Each row
/// renders in its *own* palette so you can see it before choosing; selecting one
/// recolors the whole app (chrome + terminal) immediately and persists the choice.
struct ThemePickerView: View {
    @EnvironmentObject var themeStore: ThemeStore
    @Environment(\.theme) private var theme
    @Environment(\.dismiss) private var dismiss

    var body: some View {
        NavigationStack {
            ScrollView {
                LazyVStack(spacing: 10) {
                    ForEach(themeStore.all) { candidate in
                        row(candidate)
                    }
                }
                .padding()
            }
            .background(theme.background.ignoresSafeArea())
            .navigationTitle("Theme")
            .navigationBarTitleDisplayMode(.inline)
            .toolbar {
                ToolbarItem(placement: .confirmationAction) {
                    Button("Done") { dismiss() }
                }
            }
        }
    }

    private func row(_ t: MuxelTheme) -> some View {
        let selected = t.id == themeStore.current.id
        return Button {
            themeStore.select(t)
        } label: {
            HStack(spacing: 12) {
                swatch(t)
                VStack(alignment: .leading, spacing: 3) {
                    Text(t.name)
                        .font(.mono(.callout, weight: .semibold))
                        .foregroundStyle(t.textColor)
                    HStack(spacing: 6) {
                        Text("❯ agent").foregroundStyle(t.accentColor)
                        Text("running").foregroundStyle(t.runningColor)
                        Text("blocked").foregroundStyle(t.blockedColor)
                    }
                    .font(.mono(.caption2))
                }
                Spacer()
                if selected {
                    Image(systemName: "checkmark.circle.fill")
                        .foregroundStyle(t.accentColor)
                }
            }
            .padding(12)
            .background(t.surfaceColor)
            .clipShape(RoundedRectangle(cornerRadius: 12, style: .continuous))
            .overlay(
                RoundedRectangle(cornerRadius: 12, style: .continuous)
                    .strokeBorder(selected ? t.accentColor : t.borderColor,
                                  lineWidth: selected ? 2 : 1)
            )
        }
        .buttonStyle(.plain)
    }

    /// A mini "pane" preview: the theme's background tile with its three status dots.
    private func swatch(_ t: MuxelTheme) -> some View {
        RoundedRectangle(cornerRadius: 8, style: .continuous)
            .fill(t.background)
            .frame(width: 56, height: 42)
            .overlay(
                RoundedRectangle(cornerRadius: 8, style: .continuous)
                    .strokeBorder(t.accentColor.opacity(0.6), lineWidth: 1)
            )
            .overlay(
                HStack(spacing: 4) {
                    Circle().fill(t.runningColor).frame(width: 7, height: 7)
                    Circle().fill(t.workingColor).frame(width: 7, height: 7)
                    Circle().fill(t.blockedColor).frame(width: 7, height: 7)
                }
            )
    }
}
