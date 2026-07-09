import SwiftUI

extension View {
    /// Opt a field out of iOS Password AutoFill. muxel keeps SSH secrets in its own
    /// Keychain, not the system password manager, so the "Save Password?" prompt and
    /// strong-password overlay are just noise. Typing a field as a one-time code is the
    /// standard way to exclude it.
    ///
    /// Apply this to **both** the secure field *and* the adjacent user/username field:
    /// iOS pairs an inferred `.username` text field with any nearby secure field into a
    /// "login form" and offers to *save the pair* on dismissal — even when the secure
    /// field is already `.oneTimeCode`. Opting the username field out too breaks that
    /// pairing, which is what actually suppresses the save prompt.
    func noPasswordAutoFill() -> some View {
        textContentType(.oneTimeCode)
    }
}
