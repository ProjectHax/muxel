import XCTest
@testable import muxel

/// Covers the in-form connection test: inline secrets are staged under the fixed
/// scratch owner and cleaned up; failures come back inline (not as alerts).
@MainActor
final class DraftConnectionTests: XCTestCase {

    func testInlineDraftUsesScratchOwnerAndCleansUp() async {
        let state = TestFixtures.makeState()
        var captured: ResolvedCredential?
        state.connectionFactory = { _, credentials in
            captured = credentials.target
            return MockSSHConnection()
        }

        var draft = Host(name: "new", hostname: "example.com")
        draft.user = "me"
        draft.auth = .password
        let result = await state.testConnection(draft: draft, identityId: nil,
                                                password: "hunter2", keyData: nil,
                                                passphrase: nil)

        XCTAssertTrue(result.ok)
        XCTAssertEqual(captured?.secretOwner, AppState.scratchSecretOwner,
                       "inline secrets are staged under the scratch owner, never the host id")
        XCTAssertNil(Keychain.password(for: AppState.scratchSecretOwner),
                     "the staged secret is deleted after the attempt")
        XCTAssertNil(Keychain.password(for: draft.id),
                     "nothing is stored under the (unsaved) host id")
        XCTAssertNil(state.notice, "the in-form test returns inline, never raising a banner")
    }

    func testEditModeKeepingStoredSecretPassesNoCredential() async {
        let state = TestFixtures.makeState()
        var captured: ResolvedCredential? = ResolvedCredential(
            user: "sentinel", auth: .password, keyHasPassphrase: false, secretOwner: UUID())
        state.connectionFactory = { _, credentials in
            captured = credentials.target
            return MockSSHConnection()
        }

        // Editing without re-entering a secret: the connection must fall back to
        // the host's own Keychain slot (credential == nil).
        let existing = Host(name: "web", hostname: "example.com")
        let result = await state.testConnection(draft: existing, identityId: nil,
                                                password: nil, keyData: nil,
                                                passphrase: nil)
        XCTAssertTrue(result.ok)
        XCTAssertNil(captured)
    }

    func testIdentityDraftResolvesTheIdentity() async {
        let state = TestFixtures.makeState()
        let identity = Identity(name: "deploy", user: "deploy", auth: .password)
        state.doc.identities = [identity]

        var captured: ResolvedCredential?
        state.connectionFactory = { _, credentials in
            captured = credentials.target
            return MockSSHConnection()
        }

        let draft = Host(name: "web", hostname: "example.com")
        _ = await state.testConnection(draft: draft, identityId: identity.id,
                                       password: nil, keyData: nil, passphrase: nil)
        XCTAssertEqual(captured?.secretOwner, identity.id)
        XCTAssertEqual(captured?.user, "deploy")
    }

    func testFailureComesBackInline() async {
        let state = TestFixtures.makeState()
        state.connectionFactory = { _, _ in ThrowingSSHConnection() }

        var draft = Host(name: "new", hostname: "example.com")
        draft.auth = .password
        let result = await state.testConnection(draft: draft, identityId: nil,
                                                password: "pw", keyData: nil,
                                                passphrase: nil)
        XCTAssertFalse(result.ok)
        XCTAssertTrue(result.message.contains("unreachable"))
        XCTAssertNil(Keychain.password(for: AppState.scratchSecretOwner),
                     "cleanup also runs on failure")
    }
}
