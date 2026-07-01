import XCTest
@testable import muxel

/// Covers the destructive-confirmation copy builders (counts + pluralization).
final class ConfirmationCopyTests: XCTestCase {

    func testDeleteHostPluralization() {
        let host = Host(name: "web", hostname: "example.com")
        let none = ConfirmationCopy.deleteHost(host, projectCount: 0)
        XCTAssertEqual(none.title, "Delete web?")
        XCTAssertFalse(none.message.contains("project"),
                       "no project talk when the host has none")
        XCTAssertTrue(none.message.contains("Nothing on the remote is touched."))

        let one = ConfirmationCopy.deleteHost(host, projectCount: 1)
        XCTAssertTrue(one.message.contains("its project"))

        let many = ConfirmationCopy.deleteHost(host, projectCount: 3)
        XCTAssertTrue(many.message.contains("its 3 projects"))
        XCTAssertTrue(many.message.contains("credentials"))
    }

    func testDeleteProjectsSingularAndPlural() {
        let a = RemoteProject(name: "api", hostId: UUID(), remoteRoot: "/srv/api")
        let b = RemoteProject(name: "web", hostId: UUID(), remoteRoot: "/srv/web")

        let single = ConfirmationCopy.deleteProjects([a])
        XCTAssertEqual(single.title, "Remove api?")
        XCTAssertTrue(single.message.contains("Removes it"))
        XCTAssertTrue(single.message.contains("sessions on the host keep running"))

        let multi = ConfirmationCopy.deleteProjects([a, b])
        XCTAssertEqual(multi.title, "Remove 2 projects?")
        XCTAssertTrue(multi.message.contains("Removes them"))
    }

    func testDeleteIdentityHostCounts() {
        let identity = Identity(name: "deploy")
        XCTAssertTrue(ConfirmationCopy.deleteIdentity(identity, hostCount: 0)
            .message.contains("No hosts use this login"))
        XCTAssertTrue(ConfirmationCopy.deleteIdentity(identity, hostCount: 1)
            .message.contains("1 host uses"))
        XCTAssertTrue(ConfirmationCopy.deleteIdentity(identity, hostCount: 4)
            .message.contains("4 hosts use"))
        XCTAssertEqual(ConfirmationCopy.deleteIdentity(identity, hostCount: 0).title,
                       "Delete deploy?")
    }
}
