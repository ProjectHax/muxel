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

    func testParseEndpointSplitsUserHostAndPort() {
        let full = HostEditorLogic.parseEndpoint("alice@dev.example.com:2222")
        XCTAssertEqual(full?.user, "alice")
        XCTAssertEqual(full?.hostname, "dev.example.com")
        XCTAssertEqual(full?.port, 2222)

        let sshPrefix = HostEditorLogic.parseEndpoint("ssh bob@10.0.0.5")
        XCTAssertEqual(sshPrefix?.user, "bob")
        XCTAssertEqual(sshPrefix?.hostname, "10.0.0.5")
        XCTAssertNil(sshPrefix?.port)

        let hostPort = HostEditorLogic.parseEndpoint("box.local:2200")
        XCTAssertNil(hostPort?.user)
        XCTAssertEqual(hostPort?.hostname, "box.local")
        XCTAssertEqual(hostPort?.port, 2200)

        // Plain hostnames, IPv6 literals, and invalid ports are left untouched.
        XCTAssertNil(HostEditorLogic.parseEndpoint("example.com"))
        XCTAssertNil(HostEditorLogic.parseEndpoint("fe80::1"))
        XCTAssertNil(HostEditorLogic.parseEndpoint("host:99999"))
        XCTAssertNil(HostEditorLogic.parseEndpoint("@host"))
    }

    func testSaveDisabledReasonExplainsMissingField() {
        XCTAssertNotNil(HostEditorLogic.saveDisabledReason(
            name: "", hostname: "h", jumpValid: true, usingIdentity: true,
            auth: .password, existingAuth: nil, hasPassword: false, hasKey: false))
        XCTAssertNotNil(HostEditorLogic.saveDisabledReason(
            name: "n", hostname: "", jumpValid: true, usingIdentity: true,
            auth: .password, existingAuth: nil, hasPassword: false, hasKey: false))
        XCTAssertNotNil(HostEditorLogic.saveDisabledReason(
            name: "n", hostname: "h", jumpValid: false, usingIdentity: true,
            auth: .password, existingAuth: nil, hasPassword: false, hasKey: false))
        XCTAssertNotNil(HostEditorLogic.saveDisabledReason(
            name: "n", hostname: "h", jumpValid: true, usingIdentity: false,
            auth: .key, existingAuth: nil, hasPassword: false, hasKey: false))
        XCTAssertNil(HostEditorLogic.saveDisabledReason(
            name: "n", hostname: "h", jumpValid: true, usingIdentity: true,
            auth: .password, existingAuth: nil, hasPassword: false, hasKey: false))
    }


    func testTypingAnEndpointOnlySplitsItOnceComplete() {
        // Each keystroke adds one character, which is never split on its own:
        // `host:2` already parses, and splitting it would scramble the port.
        var typed = ""
        for ch in "myhost:2222" {
            let next = typed + String(ch)
            XCTAssertFalse(HostEditorLogic.isBulkInsert(from: typed, to: next))
            typed = next
        }
        // Pasting (or autofill) adds the whole endpoint at once.
        XCTAssertTrue(HostEditorLogic.isBulkInsert(from: "", to: "root@box:2200"))
        XCTAssertFalse(HostEditorLogic.isBulkInsert(from: "root@box:2200", to: "box"))

        // Leaving the field (or saving) splits the finished text.
        let fields = HostEditorLogic.applyEndpoint(typed, name: "", user: "", port: "", usingIdentity: false)
        XCTAssertEqual(fields?.hostname, "myhost")
        XCTAssertEqual(fields?.port, "2222")
        XCTAssertEqual(fields?.name, "myhost")
        XCTAssertEqual(fields?.user, "")
        XCTAssertNil(HostEditorLogic.applyEndpoint("myhost", name: "", user: "", port: "", usingIdentity: false))
    }

    func testEndpointInHostnameReplacesUserAndPortButKeepsName() {
        let fields = HostEditorLogic.applyEndpoint(
            "admin@box:2200", name: "Build box", user: "root", port: "22", usingIdentity: false)
        XCTAssertEqual(fields?.name, "Build box")
        XCTAssertEqual(fields?.hostname, "box")
        XCTAssertEqual(fields?.user, "admin")
        XCTAssertEqual(fields?.port, "2200")
        // A shared identity supplies the user, so the typed one isn't taken.
        let identity = HostEditorLogic.applyEndpoint(
            "admin@box", name: "", user: "", port: "22", usingIdentity: true)
        XCTAssertEqual(identity?.user, "")
        XCTAssertEqual(identity?.port, "22")
    }
}
