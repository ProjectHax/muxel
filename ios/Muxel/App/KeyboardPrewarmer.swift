import UIKit

/// Pre-loads the iOS keyboard once, so the first time the user taps a terminal it
/// appears immediately instead of waiting ~1s for the system to cold-start the
/// keyboard process. Briefly makes an off-screen text field first responder and
/// resigns it in the same run loop (no visible keyboard).
enum KeyboardPrewarmer {
    private static var done = false

    @MainActor
    static func warmOnce() {
        guard !done else { return }
        done = true
        DispatchQueue.main.async {
            guard let window = UIApplication.shared.connectedScenes
                .compactMap({ $0 as? UIWindowScene })
                .flatMap({ $0.windows })
                .first(where: { $0.isKeyWindow })
            else { return }
            let field = UITextField(frame: CGRect(x: -10, y: -10, width: 1, height: 1))
            field.alpha = 0 // hidden views can't become first responder; alpha 0 can
            window.addSubview(field)
            field.becomeFirstResponder()
            field.resignFirstResponder()
            field.removeFromSuperview()
        }
    }
}
