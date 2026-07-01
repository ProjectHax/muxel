import SwiftUI
import SwiftTerm

/// Thin SwiftUI wrapper that mounts a `TerminalSession`'s SwiftTerm view. The session
/// (PTY bridge + emulator state) is owned by `TerminalStore`, NOT by this view, so the
/// terminal stays connected across navigation: when the pane leaves the screen this
/// representable is dismantled, but the session keeps running in the store, and when
/// the pane returns we re-mount the same view (scrollback intact). Teardown is the
/// store's job (Close instance / host delete / app quit) — `dismantleUIView`
/// deliberately does nothing.
struct LiveTerminalView: UIViewRepresentable {
    let session: TerminalSession
    /// The active theme — applied to the terminal's bg/fg/cursor/ANSI palette, and
    /// re-applied via `updateUIView` when the user switches themes.
    var theme: MuxelTheme

    func makeUIView(context: Context) -> TerminalView {
        // The same UIView may be re-mounted after navigation; detach from any stale
        // superview so UIKit can re-parent it without asserting.
        session.view.removeFromSuperview()
        theme.apply(to: session.view)
        return session.view
    }

    func updateUIView(_ uiView: TerminalView, context: Context) {
        theme.apply(to: uiView)
    }
}
