import UIKit
import SwiftTerm

/// muxel-themed replacement for SwiftTerm's stock gray accessory bar:
/// scrollable key strip (`esc  ctrl  ^C  ⇥  ⇧⇥  ←  ↓  ↑  →  paste  /  -  |  ~`)
/// plus a pinned keyboard-dismiss button on the trailing edge.
///
/// - Esc/Tab/Shift-Tab/Ctrl-C/arrows/symbols send raw bytes via `TerminalSession.sendKey`;
///   arrows honor the remote's application-cursor mode (SS3 vs CSI) and auto-repeat while held.
/// - **ctrl** is a sticky modifier backed by SwiftTerm's public `controlModifier`:
///   SwiftTerm combines the next typed character itself and auto-resets, posting
///   `.terminalViewControlModifierReset` — which un-highlights the key here.
/// - **paste** calls SwiftTerm's `paste(_:)`, which honors bracketed-paste mode
///   (DECSET 2004) for tmux/agents.
/// - **dismiss** resigns first responder so the user can always reclaim the screen
///   (especially in iPhone landscape where `hideChrome` hides the nav/tab bar).
final class TerminalAccessoryRow: UIInputView, UIInputViewAudioFeedback {
    // weak: view → inputAccessoryView → session → view would otherwise cycle.
    private weak var session: TerminalSession?

    private let background = UIView()
    private let scrollView = UIScrollView()
    private let stack = UIStackView()
    private var ctrlButton: UIButton?
    private var pasteButton: UIButton?
    private var dismissButton: UIButton?
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

    /// A fixed self-sizing height. Without this, `allowsSelfSizing` derives the height
    /// from the buttons' intrinsic content (~27pt), which disagrees with the 44pt frame
    /// the keyboard placeholder was laid out against — the source of the recurring
    /// "Unable to simultaneously satisfy constraints" (`_UIKBCompatInputView`) log.
    override var intrinsicContentSize: CGSize {
        CGSize(width: UIView.noIntrinsicMetric, height: 44)
    }

    // MARK: Layout

    private func setup() {
        // An opaque backdrop over the UIInputView keyboard blur, recolored by the theme.
        background.translatesAutoresizingMaskIntoConstraints = false
        addSubview(background)

        scrollView.translatesAutoresizingMaskIntoConstraints = false
        scrollView.showsHorizontalScrollIndicator = false
        scrollView.showsVerticalScrollIndicator = false
        scrollView.alwaysBounceHorizontal = false
        scrollView.delaysContentTouches = false
        addSubview(scrollView)

        stack.axis = .horizontal
        stack.distribution = .fill
        stack.alignment = .fill
        stack.spacing = 6
        stack.translatesAutoresizingMaskIntoConstraints = false
        scrollView.addSubview(stack)

        let dismiss = makeButton(nil, systemImage: "keyboard.chevron.compact.down",
                                 accessibilityLabel: "Dismiss keyboard", minWidth: 38)
        dismiss.translatesAutoresizingMaskIntoConstraints = false
        dismiss.addAction(UIAction { [weak self] _ in
            self?.tapHaptic.impactOccurred()
            _ = self?.session?.view.resignFirstResponder()
        }, for: .touchUpInside)
        addSubview(dismiss)
        dismissButton = dismiss

        NSLayoutConstraint.activate([
            background.leadingAnchor.constraint(equalTo: leadingAnchor),
            background.trailingAnchor.constraint(equalTo: trailingAnchor),
            background.topAnchor.constraint(equalTo: topAnchor),
            background.bottomAnchor.constraint(equalTo: bottomAnchor),

            scrollView.leadingAnchor.constraint(equalTo: leadingAnchor, constant: 8),
            scrollView.trailingAnchor.constraint(equalTo: dismiss.leadingAnchor, constant: -6),
            scrollView.topAnchor.constraint(equalTo: topAnchor, constant: 6),
            scrollView.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),

            stack.leadingAnchor.constraint(equalTo: scrollView.contentLayoutGuide.leadingAnchor),
            stack.trailingAnchor.constraint(equalTo: scrollView.contentLayoutGuide.trailingAnchor),
            stack.topAnchor.constraint(equalTo: scrollView.contentLayoutGuide.topAnchor),
            stack.bottomAnchor.constraint(equalTo: scrollView.contentLayoutGuide.bottomAnchor),
            stack.heightAnchor.constraint(equalTo: scrollView.frameLayoutGuide.heightAnchor),

            dismiss.trailingAnchor.constraint(equalTo: trailingAnchor, constant: -8),
            dismiss.topAnchor.constraint(equalTo: topAnchor, constant: 6),
            dismiss.bottomAnchor.constraint(equalTo: bottomAnchor, constant: -6),
        ])

        addKey("esc", accessibilityLabel: "Escape", minWidth: 42) { [weak self] in
            self?.session?.sendKey(TerminalKeys.esc)
        }
        ctrlButton = addKey("ctrl", accessibilityLabel: "Control", minWidth: 42) { [weak self] in
            self?.toggleControl()
        }
        addKey("^C", accessibilityLabel: "Interrupt (Control C)", minWidth: 38) { [weak self] in
            self?.session?.sendKey(TerminalKeys.ctrlC)
        }
        addKey("⇥", accessibilityLabel: "Tab", minWidth: 38) { [weak self] in
            self?.session?.sendKey(TerminalKeys.tab)
        }
        addKey("⇧⇥", accessibilityLabel: "Shift Tab", minWidth: 42) { [weak self] in
            self?.session?.sendKey(TerminalKeys.shiftTab)
        }
        addArrowKey("←", .left, accessibilityLabel: "Left arrow")
        addArrowKey("↓", .down, accessibilityLabel: "Down arrow")
        addArrowKey("↑", .up, accessibilityLabel: "Up arrow")
        addArrowKey("→", .right, accessibilityLabel: "Right arrow")
        pasteButton = addKey(nil, systemImage: "doc.on.clipboard",
                             accessibilityLabel: "Paste", minWidth: 38) { [weak self] in
            self?.session?.view.paste(nil)
        }
        for (sym, label) in [("/", "Slash"), ("-", "Hyphen"), ("|", "Pipe"), ("~", "Tilde")] {
            let bytes = Array(sym.utf8)
            addKey(sym, accessibilityLabel: label, minWidth: 36) { [weak self] in
                self?.session?.sendKey(bytes)
            }
        }
        pasteboardChanged()
    }

    @discardableResult
    private func addKey(_ title: String?, systemImage: String? = nil,
                        accessibilityLabel: String, minWidth: CGFloat = 38,
                        action: @escaping () -> Void) -> UIButton {
        let button = makeButton(title, systemImage: systemImage,
                                accessibilityLabel: accessibilityLabel, minWidth: minWidth)
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
    private func addArrowKey(_ title: String, _ dir: TerminalKeys.Arrow,
                             accessibilityLabel: String) {
        let button = makeButton(title, systemImage: nil,
                                accessibilityLabel: accessibilityLabel, minWidth: 36)
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

    private func makeButton(_ title: String?, systemImage: String?,
                            accessibilityLabel: String, minWidth: CGFloat) -> UIButton {
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
        button.accessibilityLabel = accessibilityLabel
        button.layer.cornerRadius = 6
        button.layer.cornerCurve = .continuous
        button.widthAnchor.constraint(greaterThanOrEqualToConstant: minWidth).isActive = true
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
        if let dismissButton { style(dismissButton, engaged: false) }
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
