import XCTest
@testable import muxel

/// Covers the ProxyJump-style `[user@]host[:port]` parser behind jump-host support.
final class JumpHostTests: XCTestCase {

    func testBareHost() {
        XCTAssertEqual(JumpHostSpec.parse("bastion"),
                       JumpHostSpec(user: nil, host: "bastion", port: 22))
    }

    func testUserAtHost() {
        XCTAssertEqual(JumpHostSpec.parse("ops@bastion.example.com"),
                       JumpHostSpec(user: "ops", host: "bastion.example.com", port: 22))
    }

    func testHostWithPort() {
        XCTAssertEqual(JumpHostSpec.parse("bastion:2222"),
                       JumpHostSpec(user: nil, host: "bastion", port: 2222))
    }

    func testUserHostPort() {
        XCTAssertEqual(JumpHostSpec.parse("ops@bastion:2222"),
                       JumpHostSpec(user: "ops", host: "bastion", port: 2222))
    }

    func testWhitespaceTrimmed() {
        XCTAssertEqual(JumpHostSpec.parse("  bastion  "),
                       JumpHostSpec(user: nil, host: "bastion", port: 22))
    }

    func testGarbageIsNil() {
        XCTAssertNil(JumpHostSpec.parse(""))
        XCTAssertNil(JumpHostSpec.parse("   "))
        XCTAssertNil(JumpHostSpec.parse("@bastion"), "empty user")
        XCTAssertNil(JumpHostSpec.parse("bastion:notaport"), "non-numeric port")
        XCTAssertNil(JumpHostSpec.parse("bastion:0"), "out-of-range port")
        XCTAssertNil(JumpHostSpec.parse("bastion:70000"), "out-of-range port")
        XCTAssertNil(JumpHostSpec.parse("two words"), "embedded whitespace")
        XCTAssertNil(JumpHostSpec.parse("a@"), "empty host")
    }

    func testJumpChainsRejected() {
        // Comma chains (`-J a,b`) aren't supported on iOS — refuse rather than
        // silently using the first hop.
        XCTAssertNil(JumpHostSpec.parse("first,second"))
        XCTAssertNil(JumpHostSpec.parse("u@a:22,b"))
    }

    // MARK: Bastion credential resolution

    func testNoJumpHostMeansNoJumpCredential() {
        let host = Host(name: "web", hostname: "example.com")
        XCTAssertNil(host.resolvedJumpCredential(in: []))
    }

    func testJumpIdentityWins() {
        let jumpId = Identity(name: "bastion", user: "ops", auth: .key, keyHasPassphrase: true)
        var host = Host(name: "web", hostname: "example.com")
        host.user = "me"
        host.jumpHost = "bastion.example.com"
        host.jumpIdentityId = jumpId.id

        let cred = host.resolvedJumpCredential(in: [jumpId])
        XCTAssertEqual(cred?.user, "ops")
        XCTAssertEqual(cred?.auth, .key)
        XCTAssertEqual(cred?.secretOwner, jumpId.id)
    }

    func testSameAsHostFallsBackToHostCredential() {
        // Host uses an identity → the bastion shares it.
        let identity = Identity(name: "deploy", user: "deploy", auth: .password)
        var host = Host(name: "web", hostname: "example.com")
        host.jumpHost = "bastion"
        host.identityId = identity.id
        XCTAssertEqual(host.resolvedJumpCredential(in: [identity])?.secretOwner, identity.id)

        // Host uses inline credentials → the bastion reads the host's own slot.
        var inline = Host(name: "db", hostname: "db.example.com")
        inline.user = "me"
        inline.auth = .key
        inline.jumpHost = "bastion"
        let cred = inline.resolvedJumpCredential(in: [])
        XCTAssertEqual(cred?.secretOwner, inline.id)
        XCTAssertEqual(cred?.user, "me")
        XCTAssertEqual(cred?.auth, .key)
    }

    /// A `store.json` written before `jumpIdentityId` existed must still decode
    /// (synthesized Codable with an optional field).
    func testHostDecodesWithoutJumpIdentityId() throws {
        let json = """
        {"id":"11111111-1111-1111-1111-111111111111","name":"web",\
        "hostname":"example.com","port":null,"user":"me","auth":"password",\
        "keyHasPassphrase":false,"jumpHost":"bastion","keepaliveSecs":null}
        """
        let host = try JSONDecoder().decode(Host.self, from: Data(json.utf8))
        XCTAssertNil(host.jumpIdentityId)
        XCTAssertEqual(host.jumpHost, "bastion")
    }
}
