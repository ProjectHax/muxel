import XCTest
@testable import muxel

/// Covers the host editor's save/test gate, especially the auth-switch-on-edit
/// edge case (a stored secret in the other slot must not count as usable).
final class HostEditorLogicTests: XCTestCase {

    func testAddRequiresNameHostnameAndSecret() {
        // Missing secret → no.
        XCTAssertFalse(HostEditorLogic.canSave(
            name: "web", hostname: "example.com", usingIdentity: false,
            auth: .password, existingAuth: nil, hasPassword: false, hasKey: false))
        // Password present → yes.
        XCTAssertTrue(HostEditorLogic.canSave(
            name: "web", hostname: "example.com", usingIdentity: false,
            auth: .password, existingAuth: nil, hasPassword: true, hasKey: false))
        // Key auth needs a key, not a password.
        XCTAssertFalse(HostEditorLogic.canSave(
            name: "web", hostname: "example.com", usingIdentity: false,
            auth: .key, existingAuth: nil, hasPassword: true, hasKey: false))
        XCTAssertTrue(HostEditorLogic.canSave(
            name: "web", hostname: "example.com", usingIdentity: false,
            auth: .key, existingAuth: nil, hasPassword: false, hasKey: true))
        // Missing name/hostname → no, even with a secret.
        XCTAssertFalse(HostEditorLogic.canSave(
            name: "", hostname: "example.com", usingIdentity: false,
            auth: .password, existingAuth: nil, hasPassword: true, hasKey: false))
        XCTAssertFalse(HostEditorLogic.canSave(
            name: "web", hostname: "", usingIdentity: false,
            auth: .password, existingAuth: nil, hasPassword: true, hasKey: false))
    }

    func testIdentityNeedsNoInlineSecret() {
        XCTAssertTrue(HostEditorLogic.canSave(
            name: "web", hostname: "example.com", usingIdentity: true,
            auth: .password, existingAuth: nil, hasPassword: false, hasKey: false))
    }

    func testEditKeepsStoredSecretForSameAuth() {
        // Edit, same method, nothing re-entered → the stored secret still applies.
        XCTAssertTrue(HostEditorLogic.canSave(
            name: "web", hostname: "example.com", usingIdentity: false,
            auth: .password, existingAuth: .password, hasPassword: false, hasKey: false))
    }

    func testEditAuthSwitchRequiresNewSecret() {
        // Password → key without importing a key: the stored password doesn't apply.
        XCTAssertFalse(HostEditorLogic.canSave(
            name: "web", hostname: "example.com", usingIdentity: false,
            auth: .key, existingAuth: .password, hasPassword: false, hasKey: false))
        XCTAssertTrue(HostEditorLogic.canSave(
            name: "web", hostname: "example.com", usingIdentity: false,
            auth: .key, existingAuth: .password, hasPassword: false, hasKey: true))
    }

    func testCanTestNeedsNoName() {
        XCTAssertTrue(HostEditorLogic.canTest(
            hostname: "example.com", usingIdentity: false,
            auth: .password, existingAuth: nil, hasPassword: true, hasKey: false))
        XCTAssertFalse(HostEditorLogic.canTest(
            hostname: "", usingIdentity: false,
            auth: .password, existingAuth: nil, hasPassword: true, hasKey: false))
        XCTAssertFalse(HostEditorLogic.canTest(
            hostname: "example.com", usingIdentity: false,
            auth: .password, existingAuth: nil, hasPassword: false, hasKey: false))
    }
}
