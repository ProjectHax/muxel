import XCTest
@testable import muxel

/// Covers the TOFU host-key store: first-use trust, mismatch refusal, the
/// target/jump scopes, and the OpenSSH-format fingerprint.
final class HostKeyTests: XCTestCase {
    private var defaults: UserDefaults!
    private var suite: String!

    override func setUp() {
        super.setUp()
        suite = "muxel-hostkey-tests-\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
        super.tearDown()
    }

    func testFirstUseStoresThenRevalidates() throws {
        let store = HostKeyStore(defaults: defaults)
        let host = UUID()
        XCTAssertNil(store.fingerprint(for: host))

        try store.validate(presented: "SHA256:abc", for: host) // TOFU: stores silently
        XCTAssertEqual(store.fingerprint(for: host), "SHA256:abc")
        XCTAssertNoThrow(try store.validate(presented: "SHA256:abc", for: host))
    }

    func testMismatchThrowsWithBothFingerprints() throws {
        let store = HostKeyStore(defaults: defaults)
        let host = UUID()
        try store.validate(presented: "SHA256:old", for: host)

        XCTAssertThrowsError(try store.validate(presented: "SHA256:new", for: host)) { error in
            guard case SSHError.hostKeyChanged(let expected, let got, let scope) = error else {
                return XCTFail("expected hostKeyChanged, got \(error)")
            }
            XCTAssertEqual(expected, "SHA256:old")
            XCTAssertEqual(got, "SHA256:new")
            XCTAssertEqual(scope, .target)
        }
        // The stored fingerprint is untouched until the user explicitly re-trusts.
        XCTAssertEqual(store.fingerprint(for: host), "SHA256:old")
    }

    func testAcceptPathSetFingerprintThenValidates() throws {
        let store = HostKeyStore(defaults: defaults)
        let host = UUID()
        try store.validate(presented: "SHA256:old", for: host)
        store.setFingerprint("SHA256:new", for: host) // user tapped "Trust new key"
        XCTAssertNoThrow(try store.validate(presented: "SHA256:new", for: host))
    }

    func testScopesDoNotCollide() throws {
        let store = HostKeyStore(defaults: defaults)
        let host = UUID()
        try store.validate(presented: "SHA256:target", for: host, scope: .target)
        try store.validate(presented: "SHA256:bastion", for: host, scope: .jump)

        XCTAssertEqual(store.fingerprint(for: host, scope: .target), "SHA256:target")
        XCTAssertEqual(store.fingerprint(for: host, scope: .jump), "SHA256:bastion")
        // The bastion's key showing up as the target's (or vice versa) must refuse.
        XCTAssertThrowsError(try store.validate(presented: "SHA256:bastion", for: host,
                                                scope: .target))
        XCTAssertThrowsError(try store.validate(presented: "SHA256:target", for: host,
                                                scope: .jump)) { error in
            guard case SSHError.hostKeyChanged(_, _, let scope) = error else {
                return XCTFail("expected hostKeyChanged, got \(error)")
            }
            XCTAssertEqual(scope, .jump)
        }
    }

    func testClearRemovesBothScopes() {
        let store = HostKeyStore(defaults: defaults)
        let host = UUID()
        store.setFingerprint("SHA256:t", for: host, scope: .target)
        store.setFingerprint("SHA256:j", for: host, scope: .jump)
        store.clear(for: host)
        XCTAssertNil(store.fingerprint(for: host, scope: .target))
        XCTAssertNil(store.fingerprint(for: host, scope: .jump))
    }

    func testFingerprintFormatMatchesOpenSSH() {
        // SHA256("hello") base64-encoded with '=' padding stripped, `SHA256:` prefix —
        // the exact string `ssh-keygen -lf` prints for a key blob with these bytes.
        let fp = HostKeyStore.fingerprint(ofPublicKeyBlob: Data("hello".utf8))
        XCTAssertEqual(fp, "SHA256:LPJNul+wow4m6DsqxbninhsWHlwfp0JecwQzYpOLmCQ")
        XCTAssertFalse(fp.hasSuffix("="), "OpenSSH strips base64 padding")
    }
}
