import UIKit
import SwiftTerm

/// muxel-themed replacement for SwiftTerm's stock gray accessory bar:
/// `esc  ctrl  ⇥  ←  ↓  ↑  →  paste` in the app's mono font over theme colors.
///
/// - Esc/Tab/arrows send raw bytes via `TerminalSession.sendKey`; arrows honor the
///   remote's application-cursor mode (SS3 vs CSI) and auto-repeat while held.
/// - **ctrl** is a sticky modifier backed by SwiftTerm's public `controlModifier`:
///   SwiftTerm combines the next typed character itself and auto-resets, posting
///   `.terminalViewControlModifierReset` — which un-highlights the key here.
/// - **paste** calls SwiftTerm's `paste(_:)`, which honors bracketed-paste mode
///   (DECSET 2004) for tmux/agents.
final class TerminalAccessoryRow: UIInputView, UIInputViewAudioFeedback {
    // weak: view → inputAccessoryView → session → view would otherwise cycle.
    private weak var session: TerminalSession?

    private let background = UIView()
    private let stack = UIStackView()
    private var ctrlButton: UIButton?
    private var pasteButton: UIButton?
    private var plainButtons: [UIButton] = []
    private var repeatTask: Task<Void, Never>?

    private let tapHaptic = UIImpactFeedbackGenerator(style: .light)
    private let ctrlHaptic = UIImpactFeedbackGenerator(style: .medium)

    private var keyBackground = UIColor.darkGray
    private var keyText = UIColor.white
    private var accent = UIColor.systemBlue
    private var accentText = UIColor.black
    private var appliedThemeId: String?

    var enableInputClicksWhenVisible: Bool { true }

    init(session: TerminalSession) {
        self.session = session
        super.init(frame: CGRect(x: 0, y: 0, width: UIScreen.main.bounds.width, height: 44),
                   inputViewStyle: .keyboard)
        allowsSelfSizing = true
        autoresizingMask = .flexibleWidth
        setup()
        NotificationCenter.default.addObserver(
            self, selector: #selector(controlModifierDidReset(_:)),
            name: .terminalViewControlModifierReset, object: nil)
        NotificationCenter.default.addObserver(
            self, selector: #selector(pasteboardChanged),
            name: UIPasteboard.changedNotification, object: nil)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) { fatalError("init(coder:) is not supported") }

    // MARK: Layout

    private func setup() {
        // An opaque backdrop over the UIInputView keyboard blur, recolored by the theme.
        background.translatesAutoresizingMaskIntoConstraints = false
        addSubview(background)

        stack.axis = .horizontal
        stack.distribution = .fillEqually
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        addSubview(stack)

        NSLayoutConstraint.activate([
            background.leadingAnchor.constraint(equalTo: leadingAnchor),
            background.trailingAnchor.constraint(equalTo: trailingAnchor),
            background.topAnchor.constraint(equalTo: topAnchor),
            background.bottomAnchor.constraint(equalTo: bottomAnchor),
            stack.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8),
            stack.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            stack.topAnchor.constraint(equalTo: topAnchor, constant: 6),
            stack.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
        ])

        addKey("esc") { [weak self] in self?.session?.sendKey(TerminalKeys.esc) }
        ctrlButton = addKey("ctrl") { [weak self] in self?.toggleControl() }
        addKey("⇥") { [weak self] in self?.session?.sendKey(TerminalKeys.tab) }
        addArrowKey("←", .left)
        addArrowKey("↓", .down)
        addArrowKey("↑", .up)
        addArrowKey("→", .right)
        pasteButton = addKey(nil, systemImage: "doc.on.clipboard") { [weak self] in
            self?.session?.view.paste(nil)
        }
        pasteboardChanged()
    }

    @discardableResult
    private func addKey(_ title: String?, systemImage: String? = nil,
                        action: @escaping () -> Void) -> UIButton {
        let button = makeButton(title, systemImage: systemImage)
        button.addAction(UIAction { [weak self] _ in
            self?.tapHaptic.impactOccurred()
            action()
        }, for: .touchUpInside)
        if title != nil { plainButtons.append(button) }
        stack.addArrangedSubview(button)
        return button
    }

    /// Arrows repeat while held (600ms initial delay, then 100ms — the stock bar's
    /// cadence). The repeat sends raw bytes via `sendKey`, which can never trip the
    /// held-backspace heuristic.
    private func addArrowKey(_ title: String, _ dir: TerminalKeys.Arrow) {
        let button = makeButton(title, systemImage: nil)
        button.addAction(UIAction { [weak self] _ in
            guard let self else { return }
            self.tapHaptic.impactOccurred()
            self.sendArrow(dir)
            self.repeatTask?.cancel()
            self.repeatTask = Task { @MainActor [weak self] in
                try? await Task.sleep(for: .milliseconds(600))
                while !Task.isCancelled {
                    self?.sendArrow(dir)
                    try? await Task.sleep(for: .milliseconds(100))
                }
            }
        }, for: .touchDown)
        for event in [UIControl.Event.touchUpInside, .touchUpOutside, .touchCancel] {
            button.addAction(UIAction { [weak self] _ in
                self?.repeatTask?.cancel()
                self?.repeatTask = nil
            }, for: event)
        }
        plainButtons.append(button)
        stack.addArrangedSubview(button)
    }

    private func makeButton(_ title: String?, systemImage: String?) -> UIButton {
        let button = UIButton(type: .system)
        if let title {
            button.setTitle(title, for: .normal)
            button.titleLabel?.font = .monospacedSystemFont(ofSize: 13, weight: .medium)
        }
        if let systemImage {
            button.setImage(UIImage(systemName: systemImage), for: .normal)
            button.setPreferredSymbolConfiguration(UIImage.SymbolConfiguration(pointSize: 13),
                                                   forImageIn: .normal)
        }
        button.layer.cornerRadius = 6
        button.layer.cornerCurve = .continuous
        return button
    }

    // MARK: Actions

    private func sendArrow(_ dir: TerminalKeys.Arrow) {
        guard let session else { return }
        let appCursor = session.view.getTerminal().applicationCursor
        session.sendKey(TerminalKeys.arrow(dir, applicationCursor: appCursor))
    }

    /// Sticky Ctrl: SwiftTerm's soft-keyboard input path combines the next typed
    /// character with `controlModifier` and resets it (posting the notification
    /// observed below). A second tap toggles it off manually.
    private func toggleControl() {
        guard let view = session?.view else { return }
        view.controlModifier.toggle()
        if view.controlModifier { ctrlHaptic.impactOccurred() }
        styleCtrl(engaged: view.controlModifier)
    }

    @objc private func controlModifierDidReset(_ note: Notification) {
        // Only our terminal's resets matter (object is the posting TerminalView).
        guard note.object as? TerminalView === session?.view else { return }
        styleCtrl(engaged: false)
    }

    @objc private func pasteboardChanged() {
        pasteButton?.isEnabled = UIPasteboard.general.hasStrings
        pasteButton?.alpha = UIPasteboard.general.hasStrings ? 1 : 0.4
    }

    // MARK: Theme

    /// Recolor the bar + keys; idempotent per theme (cheap to call from every
    /// `updateUIView` theme re-apply).
    func apply(theme: MuxelTheme) {
        guard theme.id != appliedThemeId else { return }
        appliedThemeId = theme.id
        background.backgroundColor = UIColor(muxelHex: theme.bg)
        keyBackground = UIColor(muxelHex: theme.surfaceAlt)
        keyText = UIColor(muxelHex: theme.fg)
        accent = UIColor(muxelHex: theme.accent)
        accentText = UIColor(muxelHex: theme.bg)
        for button in plainButtons { style(button, engaged: false) }
        if let pasteButton { style(pasteButton, engaged: false) }
        styleCtrl(engaged: session?.view.controlModifier ?? false)
    }

    private func styleCtrl(engaged: Bool) {
        guard let ctrlButton else { return }
        style(ctrlButton, engaged: engaged)
    }

    private func style(_ button: UIButton, engaged: Bool) {
        button.backgroundColor = engaged ? accent : keyBackground
        button.tintColor = engaged ? accentText : keyText
        button.setTitleColor(engaged ? accentText : keyText, for: .normal)
    }
}
