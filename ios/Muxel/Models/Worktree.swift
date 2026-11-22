import Foundation

/// A git worktree registry entry. Port of `Worktree`
/// (`crates/muxel-core/src/worktree.rs`). Carried in `RemoteLayout` so the iOS app
/// can show worktree grouping/color; v1 MVP does not create worktrees.
struct Worktree: Codable, Equatable, Identifiable {
    var id: String
    var projectId: String
    var name: String
    var path: String
    var branch: String
    var color: Int
    var detached: Bool
    /// The branch (or commit) the worktree was made from; desktop diffs the worktree
    /// against it. Optional and omitted when nil, as in Rust, so a record iOS rewrites
    /// keeps it and one iOS creates (nil) falls back to the project's branch.
    var base: String?

    init(id: String, projectId: String, name: String, path: String, branch: String,
         color: Int, detached: Bool = false, base: String? = nil) {
        self.id = id
        self.projectId = projectId
        self.name = name
        self.path = path
        self.branch = branch
        self.color = color
        self.detached = detached
        self.base = base
    }

    private enum CodingKeys: String, CodingKey {
        case id
        case projectId = "project_id"
        case name, path, branch, color, detached, base
    }

    init(from decoder: Decoder) throws {
        let c = try decoder.container(keyedBy: CodingKeys.self)
        id = try c.decode(String.self, forKey: .id)
        projectId = try c.decode(String.self, forKey: .projectId)
        name = try c.decode(String.self, forKey: .name)
        path = try c.decode(String.self, forKey: .path)
        branch = try c.decode(String.self, forKey: .branch)
        color = try c.decode(Int.self, forKey: .color)
        detached = (try c.decodeIfPresent(Bool.self, forKey: .detached)) ?? false
        base = try c.decodeIfPresent(String.self, forKey: .base)
    }

    func encode(to encoder: Encoder) throws {
        var c = encoder.container(keyedBy: CodingKeys.self)
        try c.encode(id, forKey: .id)
        try c.encode(projectId, forKey: .projectId)
        try c.encode(name, forKey: .name)
        try c.encode(path, forKey: .path)
        try c.encode(branch, forKey: .branch)
        try c.encode(color, forKey: .color)
        try c.encode(detached, forKey: .detached)
        try c.encodeIfPresent(base, forKey: .base)
    }
}
