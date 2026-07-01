import XCTest
@testable import muxel

/// Covers the App Lock state machine (relock policy, unlock, enable-proof) with an
/// injected evaluator — `LAContext` itself is device-only.
@MainActor
final class AppLockTests: XCTestCase {
    private var defaults: UserDefaults!
    private var suite: String!

    override func setUp() {
        super.setUp()
        suite = "muxel-applock-tests-\(UUID().uuidString)"
        defaults = UserDefaults(suiteName: suite)
    }

    override func tearDown() {
        defaults.removePersistentDomain(forName: suite)
        super.tearDown()
    }

    func testShouldRelockMatrix() {
        let t0 = Date(timeIntervalSince1970: 1_000_000)
        // Disabled → never.
        XCTAssertFalse(AppLock.shouldRelock(enabled: false, backgroundedAt: t0,
                                            now: t0.addingTimeInterval(3600)))
        // Never backgrounded → no.
        XCTAssertFalse(AppLock.shouldRelock(enabled: true, backgroundedAt: nil, now: t0))
        // Inside the 60s grace → no.
        XCTAssertFalse(AppLock.shouldRelock(enabled: true, backgroundedAt: t0,
                                            now: t0.addingTimeInterval(59)))
        // Past the grace → yes.
        XCTAssertTrue(AppLock.shouldRelock(enabled: true, backgroundedAt: t0,
                                           now: t0.addingTimeInterval(60)))
    }

    func testLaunchesLockedWhenEnabled() {
        defaults.set(true, forKey: "muxel.appLock")
        XCTAssertTrue(AppLock(defaults: defaults).isLocked)
        XCTAssertFalse(AppLock(defaults: UserDefaults(suiteName: suite + "-other")!).isLocked)
    }

    func testUnlockRequiresPassingEvaluation() async {
        defaults.set(true, forKey: "muxel.appLock")
        let lock = AppLock(defaults: defaults)

        lock.evaluate = { false }
        await lock.unlock()
        XCTAssertTrue(lock.isLocked, "a failed/canceled check stays locked")

        lock.evaluate = { true }
        await lock.unlock()
        XCTAssertFalse(lock.isLocked)
    }

    func testGraceWindowSkipsRelock() async {
        defaults.set(true, forKey: "muxel.appLock")
        let lock = AppLock(defaults: defaults)
        lock.evaluate = { true }
        await lock.unlock()
        XCTAssertFalse(lock.isLocked)

        let t0 = Date()
        lock.noteBackgrounded(now: t0)
        lock.noteActivated(now: t0.addingTimeInterval(30))
        XCTAssertFalse(lock.isLocked, "30s app-switch stays unlocked (grace)")

        lock.noteBackgrounded(now: t0)
        lock.noteActivated(now: t0.addingTimeInterval(120))
        XCTAssertTrue(lock.isLocked, "2 min away re-locks")
    }

    func testDisablingUnlocks() async {
        defaults.set(true, forKey: "muxel.appLock")
        let lock = AppLock(defaults: defaults)
        XCTAssertTrue(lock.isLocked)
        lock.isEnabled = false
        XCTAssertFalse(lock.isLocked)
        XCTAssertFalse(defaults.bool(forKey: "muxel.appLock"))
    }

    func testConfirmEnableRollsBackOnFailure() async {
        let lock = AppLock(defaults: defaults)
        lock.isEnabled = true
        lock.evaluate = { false }
        await lock.confirmEnable()
        XCTAssertFalse(lock.isEnabled, "a failed proof rolls the toggle back")

        lock.isEnabled = true
        lock.evaluate = { true }
        await lock.confirmEnable()
        XCTAssertTrue(lock.isEnabled)
    }
}
