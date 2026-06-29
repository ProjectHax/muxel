import Foundation

/// A muxel project's pane layout tree. Port of `PaneNode` / `LeafData`
/// (`crates/muxel-core/src/pane.rs`). Tagged by `"kind"`: `"leaf"` | `"split"`.
///
/// Instance ids are lowercase hyphenated UUID strings (Rust's serde output), kept
/// as `String` so re-serialization is byte-identical and doesn't trip desktop's
/// `content_key` change detection.
indirect enum PaneNode: Equatable {
    case leaf(tabs: [String], active: Int)
    case split(direction: SplitDirection, sizes: [Double], children: [PaneNode])
}

enum SplitDirection: String, Codable, Equatable {
    case horizontal
    case vertical
}

extension PaneNode: Codable {
    private enum CodingKeys: String, CodingKey {
        case kind, tabs, active, instance, direction, sizes, children
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        switch try c.decode(String.self, forKey: .kind) {
        case "leaf":
            if let tabs = try c.decodeIfPresent([String].self, forKey: .tabs), !tabs.isEmpty {
                let active = (try c.decodeIfPresent(Int.self, forKey: .active)) ?? 0
                self = .leaf(tabs: tabs, active: min(max(active, 0), tabs.count - 1))
            } else if let inst = try c.decodeIfPresent(String.self, forKey: .instance) {
                // Legacy single-tab leaf: {"kind":"leaf","instance":"<uuid>"}.
                self = .leaf(tabs: [inst], active: 0)
            } else {
                throw DecodingError.dataCorruptedError(
                    forKey: .tabs, in: c, debugDescription: "leaf has no tabs")
            }
        case "split":
            self = .split(
                direction: try c.decode(SplitDirection.self, forKey: .direction),
                sizes: try c.decode([Double].self, forKey: .sizes),
                children: try c.decode([PaneNode].self, forKey: .children)
            )
        case let other:
            throw DecodingError.dataCorruptedError(
                forKey: .kind, in: c, debugDescription: "unknown pane kind \(other)")
        }
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        switch self {
        case let .leaf(tabs, active):
            try c.encode("leaf", forKey: .kind)
            try c.encode(tabs, forKey: .tabs)
            try c.encode(active, forKey: .active)
        case let .split(direction, sizes, children):
            try c.encode("split", forKey: .kind)
            try c.encode(direction, forKey: .direction)
            try c.encode(sizes, forKey: .sizes)
            try c.encode(children, forKey: .children)
        }
    }
}

extension PaneNode {
    /// All instance ids referenced by this subtree, left-to-right (tabs in order).
    var allTabs: [String] {
        switch self {
        case let .leaf(tabs, _): return tabs
        case let .split(_, _, children): return children.flatMap(\.allTabs)
        }
    }

    /// The active leaf's tabs + active index — what the MVP renders.
    var activeLeaf: (tabs: [String], active: Int)? {
        switch self {
        case let .leaf(tabs, active):
            return (tabs, active)
        case let .split(_, _, children):
            // First leaf, depth-first. (Full split rendering is a later phase.)
            for child in children {
                if let leaf = child.activeLeaf { return leaf }
            }
            return nil
        }
    }
}
