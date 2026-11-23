import ActivityKit
import SwiftUI
import WidgetKit

/// Catppuccin Mocha, self-contained: the widget extension can't import the app's
/// `MuxelTheme` (that lives in the `muxel` module). Matches `muxel.svg`.
private enum Brand {
    static let accent = Color(.sRGB, red: 0x89 / 255, green: 0xb4 / 255, blue: 0xfa / 255)
    static let attention = Color(.sRGB, red: 0xf3 / 255, green: 0x8b / 255, blue: 0xa8 / 255)
    static let working = Color(.sRGB, red: 0xf9 / 255, green: 0xe2 / 255, blue: 0xaf / 255)
    static let done = Color(.sRGB, red: 0xa6 / 255, green: 0xe3 / 255, blue: 0xa1 / 255)
    static let idle = Color(.sRGB, red: 0x6c / 255, green: 0x70 / 255, blue: 0x86 / 255)
    static let text = Color(.sRGB, red: 0xcd / 255, green: 0xd6 / 255, blue: 0xf4 / 255)

    static func color(_ s: MuxelActivityAttributes.InstanceState) -> Color {
        switch s {
        case .needsInput: return attention
        case .finished: return done
        case .working: return working
        case .idle: return idle
        }
    }
}

/// The muxel status bar: a Live Activity listing every agent instance and its state
/// on the Lock Screen and in the Dynamic Island while the app is minimized. Agents
/// waiting for you ("needs input") are surfaced first.
struct MuxelLiveActivity: Widget {
    var body: some WidgetConfiguration {
        ActivityConfiguration(for: MuxelActivityAttributes.self) { ctx in
            LockScreenView(state: ctx.state)
                .activityBackgroundTint(Color.black.opacity(0.55))
                .activitySystemActionForegroundColor(Brand.accent)
        } dynamicIsland: { ctx in
            DynamicIsland {
                DynamicIslandExpandedRegion(.leading) {
                    CountBadge(n: ctx.state.needsInputCount, color: Brand.attention,
                               systemImage: "bell.fill")
                }
                DynamicIslandExpandedRegion(.trailing) {
                    CountBadge(n: ctx.state.workingCount, color: Brand.working,
                               systemImage: "bolt.horizontal.fill")
                }
                DynamicIslandExpandedRegion(.center) {
                    Text("muxel").font(.caption2.weight(.semibold)).foregroundStyle(Brand.accent)
                }
                DynamicIslandExpandedRegion(.bottom) {
                    let groups = MuxelActivityAttributes.groupByProject(ctx.state.instances)
                    ProjectGroupList(groups: Array(groups.prefix(3)),
                                     extra: max(0, groups.count - 3))
                }
            } compactLeading: {
                Image(systemName: "terminal.fill").foregroundStyle(Brand.accent)
            } compactTrailing: {
                if ctx.state.needsInputCount > 0 {
                    Text("\(ctx.state.needsInputCount)").foregroundStyle(Brand.attention).bold()
                } else {
                    Text("\(ctx.state.workingCount)").foregroundStyle(Brand.working)
                }
            } minimal: {
                Image(systemName: ctx.state.needsInputCount > 0 ? "bell.fill" : "terminal.fill")
                    .foregroundStyle(ctx.state.needsInputCount > 0 ? Brand.attention : Brand.accent)
            }
            .keylineTint(Brand.accent)
        }
    }
}

/// Lock Screen / banner presentation — one line per project with multiple status dots
/// (one per agent) and the project's attention / last-agent status. Sized to stay safely
/// under iOS's 160 pt Live Activity height cap so the top `❯ muxel` header never clips.
private struct LockScreenView: View {
    let state: MuxelActivityAttributes.ContentState
    private static let cap = 4

    private var groups: [MuxelActivityAttributes.ProjectGroup] {
        MuxelActivityAttributes.groupByProject(state.instances)
    }

    var body: some View {
        let allGroups = groups
        let shown = Array(allGroups.prefix(Self.cap))
        let extraProjects = max(0, allGroups.count - shown.count)
        VStack(alignment: .leading, spacing: 4) {
            HStack {
                HStack(spacing: 5) {
                    Text("❯").foregroundStyle(Brand.accent)
                    Text("muxel").foregroundStyle(Brand.accent)
                }
                .font(.caption.weight(.bold))
                Spacer()
                Text(state.updatedAt, style: .relative)
                    .font(.caption2).foregroundStyle(Brand.idle)
                    .lineLimit(1)
            }
            .fixedSize(horizontal: false, vertical: true)
            summaryLine
            ForEach(shown) { ProjectGroupRowView(group: $0) }
            if extraProjects > 0 {
                Text("+\(extraProjects) more project\(extraProjects == 1 ? "" : "s")")
                    .font(.caption2).foregroundStyle(Brand.idle)
            }
        }
        .padding(.horizontal, 14)
        .padding(.vertical, 10)
    }

    @ViewBuilder private var summaryLine: some View {
        HStack(spacing: 10) {
            if state.needsInputCount > 0 {
                Label("\(state.needsInputCount) need input", systemImage: "bell.fill")
                    .foregroundStyle(Brand.attention)
            }
            if state.workingCount > 0 {
                Label("\(state.workingCount) working", systemImage: "bolt.horizontal.fill")
                    .foregroundStyle(Brand.working)
            }
            if state.finishedCount > 0 {
                Label("\(state.finishedCount) finished", systemImage: "checkmark")
                    .foregroundStyle(Brand.done)
            }
            if state.needsInputCount == 0 && state.workingCount == 0 && state.finishedCount == 0 {
                Text("all idle").foregroundStyle(Brand.idle)
            }
        }
        .font(.caption2.weight(.semibold))
        .lineLimit(1)
        .minimumScaleFactor(0.85)
    }
}

/// A compact list of grouped project rows (Dynamic Island bottom region).
private struct ProjectGroupList: View {
    let groups: [MuxelActivityAttributes.ProjectGroup]
    let extra: Int

    var body: some View {
        VStack(alignment: .leading, spacing: 2) {
            ForEach(groups) { ProjectGroupRowView(group: $0) }
            if extra > 0 {
                Text("+\(extra) more").font(.caption2).foregroundStyle(Brand.idle)
            }
        }
    }
}

/// One project line: multiple status dots (one per agent), project name, and the
/// highest-priority attention / last-agent status on the trailing edge.
private struct ProjectGroupRowView: View {
    let group: MuxelActivityAttributes.ProjectGroup
    private static let maxDots = 5

    var body: some View {
        let shownDots = Array(group.states.prefix(Self.maxDots))
        let extraDots = max(0, group.states.count - shownDots.count)
        HStack(spacing: 6) {
            HStack(spacing: 3) {
                ForEach(Array(shownDots.enumerated()), id: \.offset) { _, s in
                    if s == .idle {
                        Circle()
                            .strokeBorder(Brand.done.opacity(0.85), lineWidth: 1.3)
                            .frame(width: 7, height: 7)
                    } else {
                        Circle()
                            .fill(Brand.color(s))
                            .frame(width: 7, height: 7)
                    }
                }
                if extraDots > 0 {
                    Text("+\(extraDots)")
                        .font(.system(size: 9, weight: .semibold, design: .monospaced))
                        .foregroundStyle(Brand.idle)
                }
            }
            .fixedSize(horizontal: true, vertical: false)
            Text(group.project)
                .font(.caption2.weight(group.highlightState == .needsInput ? .semibold : .medium))
                .foregroundStyle(group.highlightState == .needsInput ? Brand.attention : Brand.text)
                .lineLimit(1)
            Spacer(minLength: 4)
            Text(group.summaryText)
                .font(.caption2.weight(group.highlightState == .needsInput ? .semibold : .regular))
                .foregroundStyle(Brand.color(group.highlightState))
                .lineLimit(1)
                .truncationMode(.tail)
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
