import XCTest
@testable import muxel

/// Mirrors worktree.rs naming tests + covers the iOS remote-create command builder.
final class WorktreeTests: XCTestCase {
    private let nilId = "00000000-0000-0000-0000-000000000000"

    // MARK: naming (port of worktree.rs)

    func testSlugSanitizes() {
        XCTAssertEqual(WorktreeNaming.slug("My Repo!"), "My-Repo")
        XCTAssertEqual(WorktreeNaming.slug("///"), "repo")
    }

    func testBranchAndDirUseIdPrefix() {
        XCTAssertEqual(WorktreeNaming.branchName(instanceId: nilId), "muxel/00000000")
        XCTAssertEqual(WorktreeNaming.dirName(repoName: "My Repo", instanceId: nilId), "My-Repo_00000000")
    }

    func testWorktreePathJoinsBase() {
        XCTAssertEqual(
            WorktreeNaming.worktreePath(base: "/data/worktrees", repoName: "repo", instanceId: nilId),
            "/data/worktrees/repo_00000000")
    }

    func testRandomNameIsAdjectiveHyphenNoun() {
        for _ in 0..<20 {
            let parts = WorktreeNaming.randomName().split(separator: "-")
            XCTAssertEqual(parts.count, 2)
        }
    }

    func testNextColorSkipsUsedIgnoresDetachedAndWraps() {
        let p = "proj"
        func wt(_ color: Int, detached: Bool = false, project: String = "proj") -> Worktree {
            Worktree(id: UUID().uuidString, projectId: project, name: "n", path: "/p",
                     branch: "b", color: color, detached: detached)
        }
        XCTAssertEqual(WorktreeNaming.nextColor(worktrees: [wt(0), wt(1)], projectId: p), 2)
        XCTAssertEqual(WorktreeNaming.nextColor(worktrees: [wt(0, detached: true)], projectId: p), 0)
        XCTAssertEqual(WorktreeNaming.nextColor(worktrees: (0..<8).map { wt($0) }, projectId: p), 0)
        XCTAssertEqual(WorktreeNaming.nextColor(worktrees: [wt(0, project: "other")], projectId: p), 0)
    }

    // MARK: remote create command + result parsing

    func testCreateCommandShape() {
        let cmd = WorktreeService.createCommand(root: "/srv/app", dirName: "app_1a2b3c4d",
                                                branch: "muxel/1a2b3c4d")
        XCTAssertTrue(cmd.contains("rev-parse --is-inside-work-tree"))
        XCTAssertTrue(cmd.contains("worktree add -b"))
        XCTAssertTrue(cmd.contains("XDG_DATA_HOME"))
        XCTAssertTrue(cmd.contains("MUXEL_WT_OK"))
        XCTAssertTrue(cmd.contains("muxel/1a2b3c4d"))
        // Best-effort env trust so mise/direnv configs load in the new worktree path.
        XCTAssertTrue(cmd.contains("mise trust"))
        XCTAssertTrue(cmd.contains("direnv allow"))
    }

    func testParseResultOK() {
        let path = "/home/u/.local/share/muxel/worktrees/app_1a2b3c4d"
        guard case let .success(p) = WorktreeService.parseResult("MUXEL_WT_OK \(path)") else {
            return XCTFail("expected success")
        }
        XCTAssertEqual(p, path)
    }

    func testParseResultErr() {
        guard case let .failure(a) = WorktreeService.parseResult("MUXEL_WT_ERR not a git repository") else {
            return XCTFail()
        }
        XCTAssertEqual(a, .notGitRepo)
        guard case let .failure(b) = WorktreeService.parseResult("MUXEL_WT_ERR fatal: branch exists") else {
            return XCTFail()
        }
        XCTAssertEqual(b, .git("fatal: branch exists"))
    }

    func testParseResultGarbage() {
        guard case .failure = WorktreeService.parseResult("random noise") else {
            return XCTFail("garbage should not parse as success")
        }
    }
}
