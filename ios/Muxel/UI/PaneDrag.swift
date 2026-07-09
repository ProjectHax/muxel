import SwiftUI

/// Shared coordinate space for the iPad pane drag: chip drag points and leaf frames
/// are all measured in this space so hit-testing lines up.
let paneDragSpace = "muxel.paneDrag"

/// One leaf's live frame + tabs, collected via a preference so the drag can hit-test
/// which pane the finger is over.
struct LeafFrameInfo: Equatable {
    let anchor: String       // the leaf's first tab id (stable target for mutations)
    let tabs: [String]
    let rect: CGRect
}

struct LeafFramesKey: PreferenceKey {
    static var defaultValue: [LeafFrameInfo] = []
    static func reduce(value: inout [LeafFrameInfo], nextValue: () -> [LeafFrameInfo]) {
        value += nextValue()
    }
}

/// Where a drop lands within a pane: center → move into that pane's tabs; an edge →
/// split out on that side.
enum PaneDropZone: Equatable {
    case tabs
    case split(SplitDirection, before: Bool)

    /// Height of the tab strip at the top of each pane — dropping here joins as a tab.
    static let stripHeight: CGFloat = 44

    /// Classify a point within `rect` (desktop model): the top tab-strip band or the
    /// broad center → join as a tab; the outer quarter of a body edge → split there.
    static func classify(point: CGPoint, in rect: CGRect) -> PaneDropZone {
        guard rect.width > 1, rect.height > 1 else { return .tabs }
        // Dropping on the tab strip merges the agent into this pane's tabs.
        if point.y - rect.minY < stripHeight { return .tabs }
        let fx = (point.x - rect.minX) / rect.width
        let fy = (point.y - rect.minY) / rect.height
        let left = fx, right = 1 - fx, top = fy, bottom = 1 - fy
        let nearest = min(left, right, top, bottom)
        let edge = 0.25
        if nearest > edge { return .tabs }
        if nearest == left { return .split(.horizontal, before: true) }
        if nearest == right { return .split(.horizontal, before: false) }
        if nearest == top { return .split(.vertical, before: true) }
        return .split(.vertical, before: false)
    }

    /// The region within `rect` a drop in this zone will occupy — for the live preview.
    func previewRect(in rect: CGRect) -> CGRect {
        switch self {
        case .tabs:
            return rect
        case let .split(.horizontal, before):
            return CGRect(x: before ? rect.minX : rect.midX, y: rect.minY,
                          width: rect.width / 2, height: rect.height)
        case let .split(.vertical, before):
            return CGRect(x: rect.minX, y: before ? rect.minY : rect.midY,
                          width: rect.width, height: rect.height / 2)
        }
    }
}

/// Owns the live state of an in-progress chip drag (which agent, where the finger is,
/// which pane it's over). A reference type so chips/leaves/the preview all observe the
/// same instance without recreating the render context on every move.
@MainActor
final class PaneDragCoordinator: ObservableObject {
    @Published var draggingId: String?
    @Published var point: CGPoint = .zero
    /// The leaf anchor currently under the finger (drives the target highlight).
    @Published var targetAnchor: String?
    /// Latest leaf frames (not published — read at drop time; updated via preference).
    var frames: [LeafFrameInfo] = []

    func begin(_ id: String, at point: CGPoint) {
        draggingId = id
        self.point = point
        updateTarget()
    }

    func update(to point: CGPoint) {
        self.point = point
        updateTarget()
    }

    /// The pane + zone the finger is currently over (nil if over nothing) — drives the
    /// live drop preview and the final drop.
    var currentDrop: (leaf: LeafFrameInfo, zone: PaneDropZone)? {
        guard draggingId != nil, let leaf = leaf(under: point) else { return nil }
        return (leaf, PaneDropZone.classify(point: point, in: leaf.rect))
    }

    /// Resolve the drop and reset. No `defer`/early-return here: a `defer` that resets
    /// `self` on a `guard`-fail path trips the optimizer's "invalid reuse after
    /// initialization failure" on some Swift versions.
    func end() -> (leaf: LeafFrameInfo, zone: PaneDropZone)? {
        let result = currentDrop
        draggingId = nil
        targetAnchor = nil
        return result
    }

    private func updateTarget() {
        targetAnchor = leaf(under: point)?.anchor
    }

    private func leaf(under point: CGPoint) -> LeafFrameInfo? {
        frames.first { $0.rect.contains(point) }
    }
}
