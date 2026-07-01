import SwiftUI
import LocalAuthentication

/// Optional Face ID / passcode **App Lock** over the UI. Deliberately NOT Keychain
/// access control (`kSecAttrAccessControl`): the background `StatusPoller` must
/// read SSH secrets prompt-free while the phone is locked in a pocket, so secrets
/// keep `kSecAttrAccessibleAfterFirstUnlock` and this gate is presentation-only.
///
/// Policy: `.deviceOwnerAuthentication` (biometics with the device-passcode
/// fallback for free). On a device with no passcode the lock would be theater, so
/// `isAvailable` is false and the toggle is disabled with guidance — the gate
/// fails open by design.
@MainActor
final class AppLock: ObservableObject {
    @Published private(set) var isLocked: Bool

    private let defaults: UserDefaults
    private static let enabledKey = "muxel.appLock"
    private var backgroundedAt: Date?

    /// The authentication runner — injectable for tests (`LAContext` isn't).
    var evaluate: () async -> Bool = {
        let ctx = LAContext()
        return (try? await ctx.evaluatePolicy(
            .deviceOwnerAuthentication,
            localizedReason: "Unlock muxel")) ?? false
    }

    init(defaults: UserDefaults = .standard) {
        self.defaults = defaults
        // Enabled → the app launches locked.
        isLocked = defaults.bool(forKey: Self.enabledKey)
    }

    var isEnabled: Bool {
        get { defaults.bool(forKey: Self.enabledKey) }
        set {
            objectWillChange.send()
            defaults.set(newValue, forKey: Self.enabledKey)
            if !newValue { isLocked = false }
        }
    }

    /// Whether the device can enforce a lock at all (a passcode is set).
    var isAvailable: Bool {
        LAContext().canEvaluatePolicy(.deviceOwnerAuthentication, error: nil)
    }

    /// Pure relock policy: 60s of background grace, so app-switching to check
    /// something doesn't re-prompt every time.
    static func shouldRelock(enabled: Bool, backgroundedAt: Date?, now: Date) -> Bool {
        guard enabled, let backgroundedAt else { return false }
        return now.timeIntervalSince(backgroundedAt) >= 60
    }

    func noteBackgrounded(now: Date = Date()) {
        backgroundedAt = now
    }

    func noteActivated(now: Date = Date()) {
        if Self.shouldRelock(enabled: isEnabled, backgroundedAt: backgroundedAt, now: now) {
            isLocked = true
        }
        backgroundedAt = nil
    }

    /// Run the Face ID / passcode check. Stays locked on cancel/failure — the
    /// shield keeps its retry button.
    func unlock() async {
        guard isLocked else { return }
        if await evaluate() { isLocked = false }
    }

    /// Called right after enabling: run one authentication so the user proves they
    /// can pass *before* the lock can ever engage; failure rolls the toggle back.
    func confirmEnable() async {
        if !(await evaluate()) { isEnabled = false }
    }
}

/// The privacy shield shown over the app while locked (and in the app switcher
/// while App Lock is enabled): theme background, the muxel mark, and a retry
/// button — no app content leaks into snapshots.
struct AppLockShield: View {
    @Environment(\.theme) private var theme
    let locked: Bool
    let unlock: () -> Void

    var body: some View {
        ZStack {
            theme.background.ignoresSafeArea()
            GridBackground().opacity(0.5)
            VStack(spacing: 14) {
                Image("MuxelMark")
                    .resizable()
                    .scaledToFit()
                    .frame(width: 64, height: 64)
                PromptLabel(text: "locked")
                if locked {
                    Button(action: unlock) {
                        Label("Unlock", systemImage: "faceid")
                            .font(.mono(.footnote, weight: .semibold))
                    }
                    .buttonStyle(.borderedProminent)
                }
            }
        }
    }
}
