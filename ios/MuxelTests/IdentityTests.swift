import XCTest
@testable import muxel

/// Covers the login-identity model: back-compat decoding of an older `store.json`
/// that predates the `identities` field, and the host→credential resolver shared by
/// the app-state and background-poller connection paths.
final class IdentityTests: XCTestCase {

    /// An existing `store.json` written before identities existed must still load —
    /// the custom `StoreDocument.init(from:)` defaults the missing key to `[]` rather
    /// than throwing (which would silently reset the whole document).
    func testStoreDocumentDecodesWithoutIdentities() throws {
        let json = """
        {"hosts":[{"id":"11111111-1111-1111-1111-111111111111","name":"web",\
        "hostname":"example.com","port":null,"user":"me","auth":"password",\
        "keyHasPassphrase":false,"jumpHost":null,"keepaliveSecs":null}],\
        "projects":[]}
        """
        let doc = try JSONDecoder().decode(StoreDocument.self, from: Data(json.utf8))
        XCTAssertEqual(doc.hosts.count, 1)
        XCTAssertEqual(doc.hosts.first?.name, "web")
        XCTAssertNil(doc.hosts.first?.identityId)
        XCTAssertTrue(doc.identities.isEmpty)
    }

    func testResolvedCredentialUsesIdentityWhenReferenced() {
        let id = Identity(name: "deploy", user: "deploy", auth: .key, keyHasPassphrase: true)
        var host = Host(name: "web", hostname: "example.com")
        host.user = "inline"
        host.auth = .password
        host.identityId = id.id

        let cred = host.resolvedCredential(in: [id])
        XCTAssertEqual(cred?.user, "deploy")
        XCTAssertEqual(cred?.auth, .key)
        XCTAssertEqual(cred?.keyHasPassphrase, true)
        XCTAssertEqual(cred?.secretOwner, id.id, "secret is keyed by the identity id")
    }

    func testResolvedCredentialNilWhenUnsetOrMissing() {
        var host = Host(name: "web", hostname: "example.com")
        // No reference → inline credentials (nil).
        XCTAssertNil(host.resolvedCredential(in: []))
        // References an identity that no longer exists → nil (auth layer falls back).
        host.identityId = UUID()
        XCTAssertNil(host.resolvedCredential(in: [Identity(name: "other")]))
    }
}
