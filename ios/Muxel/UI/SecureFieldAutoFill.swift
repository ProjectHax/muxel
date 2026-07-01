import SwiftUI

extension View {
    /// Stop iOS Password AutoFill from offering to *save* what's typed into a
    /// `SecureField`. muxel keeps SSH secrets in its own Keychain, not the system
    /// password manager, so the "Save Password?" prompt and strong-password overlay
    /// are just noise. Marking the field as a one-time code is the standard way to
    /// opt a secure field out of credential saving.
    func noPasswordAutoFill() -> some View {
        textContentType(.oneTimeCode)
    }
}
