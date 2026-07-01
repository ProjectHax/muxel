import XCTest
@testable import muxel

/// Covers the `StoreDocument` reference queries behind the delete-confirmation copy.
final class StoreQueryTests: XCTestCase {

    func testHostsUsingIdentity() {
        let identity = Identity(name: "deploy")
        let other = Identity(name: "admin")
        var a = Host(name: "a", hostname: "a.example.com")
        a.identityId = identity.id
        var b = Host(name: "b", hostname: "b.example.com")
        b.identityId = other.id
        let c = Host(name: "c", hostname: "c.example.com") // inline credentials

        var doc = StoreDocument()
        doc.identities = [identity, other]
        doc.hosts = [a, b, c]

        XCTAssertEqual(doc.hosts(using: identity).map(\.name), ["a"])
        XCTAssertEqual(doc.hosts(using: other).map(\.name), ["b"])
        XCTAssertTrue(doc.hosts(using: Identity(name: "unused")).isEmpty)
    }

    func testProjectsUnderHost() {
        let host = Host(name: "web", hostname: "example.com")
        let stranger = Host(name: "db", hostname: "db.example.com")
        var doc = StoreDocument()
        doc.hosts = [host, stranger]
        doc.projects = [
            RemoteProject(name: "api", hostId: host.id, remoteRoot: "/srv/api"),
            RemoteProject(name: "site", hostId: host.id, remoteRoot: "/srv/site"),
            RemoteProject(name: "etl", hostId: stranger.id, remoteRoot: "/srv/etl"),
        ]

        XCTAssertEqual(doc.projects(under: host).map(\.name), ["api", "site"])
        XCTAssertEqual(doc.projects(under: stranger).map(\.name), ["etl"])
    }
}
