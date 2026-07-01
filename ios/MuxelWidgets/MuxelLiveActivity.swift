import ActivityKit
import SwiftUI
import WidgetKit

/// Catppuccin Mocha, self-contained: the widget extension can't import the app's
/// `MuxelTheme` (that lives in the `muxel` module). Matches `muxel.svg`.
private enum Brand {
    static let accent = Color(.sRGB, red: 0x89 / 255, green: 0xb4 / 255, blue: 0xfa / 255)
    static let attention = Color(.sRGB, red: 0xf3 / 255, green: 0x8b / 255, blue: 0xa8 / 255)
    static let working = Color(.sRGB, red: 0xf9 / 255, green: 0xe2 / 255, blue: 0xaf / 255)
    static let running = Color(.sRGB, red: 0xa6 / 255, green: 0xe3 / 255, blue: 0xa1 / 255)
    static let idle = Color(.sRGB, red: 0x6c / 255, green: 0x70 / 255, blue: 0x86 / 255)
    static let text = Color(.sRGB, red: 0xcd / 255, green: 0xd6 / 255, blue: 0xf4 / 255)

    static func color(_ s: MuxelActivityAttributes.InstanceState) -> Color {
        switch s {
        case .attention: return attention
        case .working: return running
        case .idle: return idle
        }
    }
}

/// The muxel status bar: a Live Activity listing every agent instance and its state
/// on the Lock Screen and in the Dynamic Island while the app is minimized.
struct MuxelLiveActivity: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: MuxelActivityAttributes.self) { ctx in
            LockScreenView(state: ctx.state)
                .activityBackgroundTint(Color.black.opacity(0.55))
                .activitySystemActionForegroundColor(Brand.accent)
        } dynamicIsland: { ctx in
            DynamicIsland {
                DynamicIslandExpandedRegion(.leading) {
                    CountBadge(n: ctx.state.workingCount, color: Brand.running,
                               systemImage: "bolt.horizontal.fill")
                }
                DynamicIslandExpandedRegion(.trailing) {
                    CountBadge(n: ctx.state.attentionCount, color: Brand.attention,
                               systemImage: "bell.fill")
                }
                DynamicIslandExpandedRegion(.center) {
                    Text("muxel").font(.caption2.weight(.semibold)).foregroundStyle(Brand.accent)
                }
                DynamicIslandExpandedRegion(.bottom) {
                    InstanceList(rows: Array(ctx.state.instances.prefix(4)),
                                 extra: ctx.state.instanceCount - min(ctx.state.instances.count, 4))
                }
            } compactLeading: {
                Image(systemName: "terminal.fill").foregroundStyle(Brand.accent)
            } compactTrailing: {
                if ctx.state.attentionCount > 0 {
                    Text("\(ctx.state.attentionCount)").foregroundStyle(Brand.attention).bold()
                } else {
                    Text("\(ctx.state.workingCount)").foregroundStyle(Brand.running)
                }
            } minimal: {
                Image(systemName: ctx.state.attentionCount > 0 ? "bell.fill" : "terminal.fill")
                    .foregroundStyle(ctx.state.attentionCount > 0 ? Brand.attention : Brand.accent)
            }
            .keylineTint(Brand.accent)
        }
    }
}

/// Lock Screen / banner presentation — a per-instance list.
private struct LockScreenView: View {
    let state: MuxelActivityAttributes.ContentState
    private static let cap = 6

    var body: some View {
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                HStack(spacing: 5) {
                    Text("❯").foregroundStyle(Brand.accent)
                    Text("muxel").foregroundStyle(Brand.accent)
                }
                .font(.caption.weight(.bold))
                Spacer()
                Text(state.updatedAt, style: .relative)
                    .font(.caption2).foregroundStyle(Brand.idle)
            }
            summaryLine
            ForEach(Array(state.instances.prefix(Self.cap))) { InstanceRowView(row: $0) }
            if extra > 0 {
                Text("+\(extra) more").font(.caption2).foregroundStyle(Brand.idle)
            }
        }
        .padding(12)
    }

    private var extra: Int { state.instanceCount - min(state.instances.count, Self.cap) }

    @ViewBuilder private var summaryLine: some View {
        HStack(spacing: 10) {
            if state.attentionCount > 0 {
                Label("\(state.attentionCount) need attention", systemImage: "bell.fill")
                    .foregroundStyle(Brand.attention)
            }
            if state.workingCount > 0 {
                Label("\(state.workingCount) working", systemImage: "bolt.horizontal.fill")
                    .foregroundStyle(Brand.running)
            }
            if state.attentionCount == 0 && state.workingCount == 0 {
                Text("all idle").foregroundStyle(Brand.idle)
            }
        }
        .font(.caption2.weight(.semibold))
    }
}

/// A compact list of instance rows (Dynamic Island bottom region).
private struct InstanceList: View {
    let rows: [MuxelActivityAttributes.InstanceRow]
    let extra: Int

    var body: some View {
        VStack(alignment: .leading, spacing: 3) {
            ForEach(rows) { InstanceRowView(row: $0) }
            if extra > 0 {
                Text("+\(extra) more").font(.caption2).foregroundStyle(Brand.idle)
            }
        }
    }
}

/// One agent instance: a state dot, its name, and (muted) its project.
private struct InstanceRowView: View {
    let row: MuxelActivityAttributes.InstanceRow

    var body: some View {
        HStack(spacing: 7) {
            Circle().fill(Brand.color(row.state)).frame(width: 7, height: 7)
            Text(row.name)
                .font(.caption2.weight(row.needsAttention ? .semibold : .regular))
                .foregroundStyle(row.needsAttention ? Brand.attention : Brand.text)
                .lineLimit(1)
            Text(row.project)
                .font(.caption2)
                .foregroundStyle(Brand.idle)
                .lineLimit(1)
            Spacer(minLength: 2)
        }
    }
}

private struct CountBadge: View {
    let n: Int
    let color: Color
    let systemImage: String

    var body: some View {
        Label("\(n)", systemImage: systemImage)
            .font(.caption.weight(.semibold))
            .foregroundStyle(n > 0 ? color : Brand.idle)
    }
}
