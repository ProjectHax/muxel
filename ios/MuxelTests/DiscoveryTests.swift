import XCTest
@testable import muxel

/// Tests for the pure project-discovery logic: the remote `find` command we build
/// and the parsing of its marker-path output into project roots.
final class DiscoveryTests: XCTestCase {

    func testFindCommandScansHomeForMarkersAndPrunes() {
        let cmd = ProjectDiscovery.findCommand()
        // Scans $HOME (shell-expanded), bounds depth, and looks for the marker path.
        XCTAssertTrue(cmd.hasPrefix("find \"$HOME\" -maxdepth 7 "))
        XCTAssertTrue(cmd.contains("-type f -path '*/.muxel/workspace.json' -print"))
        // Prunes heavy dirs so it stays quick.
        XCTAssertTrue(cmd.contains("-name 'node_modules'"))
        XCTAssertTrue(cmd.contains("-name '.git'"))
        XCTAssertTrue(cmd.contains("-prune -o"))
        // Errors (permission denied on unreadable dirs) are swallowed.
        XCTAssertTrue(cmd.hasSuffix("2>/dev/null"))
    }

    func testParseStripsMarkerDedupesAndSorts() {
        let output = "/home/dev/web/.muxel/workspace.json\n"
            + "/srv/app/.muxel/workspace.json\n"
            + "/home/dev/web/.muxel/workspace.json\n"   // duplicate
            + "not-a-marker-line\n"
            + "\n"                                       // blank
        let found = ProjectDiscovery.parse(output)
        XCTAssertEqual(found.map(\.remoteRoot), ["/home/dev/web", "/srv/app"])
        XCTAssertEqual(found.map(\.name), ["web", "app"])
    }

    func testParseEmptyOutputYieldsNothing() {
        XCTAssertTrue(ProjectDiscovery.parse("").isEmpty)
        XCTAssertTrue(ProjectDiscovery.parse("\n  \n").isEmpty)
    }
}
