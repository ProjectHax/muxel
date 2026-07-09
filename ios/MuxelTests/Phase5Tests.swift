import XCTest
@testable import muxel

/// Editor/diff viewer support: diff line classification + the remote read/diff
/// command builders.
final class Phase5Tests: XCTestCase {
    func testDiffLineClassification() {
        XCTAssertEqual(diffLineKind("+added line"), .add)
        XCTAssertEqual(diffLineKind("-removed line"), .remove)
        XCTAssertEqual(diffLineKind("+++ b/file.swift"), .meta)     // header, not add
        XCTAssertEqual(diffLineKind("--- a/file.swift"), .meta)     // header, not remove
        XCTAssertEqual(diffLineKind("@@ -1,2 +1,3 @@ func x"), .hunk)
        XCTAssertEqual(diffLineKind("diff --git a/x b/x"), .meta)
        XCTAssertEqual(diffLineKind("index abc1234..def5678 100644"), .meta)
        XCTAssertEqual(diffLineKind("# Changes in /srv/app"), .meta)
        XCTAssertEqual(diffLineKind(" unchanged context"), .context)
        XCTAssertEqual(diffLineKind("plain text"), .context)
    }

    func testReadCommandSizeGuard() {
        let cmd = RemoteFiles.readCommand(path: "/srv/app/README.md")
        XCTAssertTrue(cmd.contains("wc -c"))
        XCTAssertTrue(cmd.contains("2000000"))
        XCTAssertTrue(cmd.contains("cat"))
        XCTAssertTrue(cmd.contains("README.md"))
    }

    func testDiffCommandShape() {
        let cmd = RemoteFiles.diffCommand(dir: "/srv/app")
        XCTAssertTrue(cmd.contains("rev-parse --show-toplevel"))
        XCTAssertTrue(cmd.contains("diff HEAD --no-color"))
        XCTAssertTrue(cmd.contains("Not a git repository"))
        XCTAssertTrue(cmd.contains("head -c"))
    }
}
