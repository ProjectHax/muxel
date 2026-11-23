import Foundation
import UIKit
import SwiftTerm
import Citadel
import NIOCore

/// One live SSH **PTY** terminal, owned by `TerminalStore` (NOT by the SwiftUI view).
/// It bridges PTY bytes ↔ SwiftTerm exactly like the old `LiveTerminalView.Coordinator`,
/// but its lifetime is app-controlled: it keeps running as you navigate away and back,
/// and only tears down on `disconnect()` (Close instance / host delete / app quit).
///
/// The `view` is created and handed in by the store on the main actor; `feed`/`feedText`
/// stay `@MainActor` (UIView access), while `run()`/`withPTY` run off-main like before.
final class TerminalSession: NSObject, TerminalViewDelegate, UIGestureRecognizerDelegate {
    let view: TerminalView
    /// True for headless background status monitors (`tmux attach-session -r`).
    /// Never mounted in a UIView and never sends size nudges or input.
    let isReadOnly: Bool
    /// True while the PTY task is live; flips false when the channel ends or
    /// `disconnect()` is called, so the store can recycle a dead session (e.g. after
    /// an iOS background TCP drop) and re-attach fresh.
    private(set) var isConnected = false

    private let connection: SSHConnection
    private let command: String
    /// Bounds concurrent PTY channel opens on this host (nil = unbounded). Held only
    /// through channel establishment, not the session's lifetime.
    private let openGate: PTYOpenGate?
    private let events = AsyncStream<Event>.makeStream()
    private var task: Task<Void, Never>?
    /// Set once `start()` is requested; the PTY actually opens on the first real size.
    private var startRequested = false
    /// The most recent grid size SwiftTerm reported, used to open the PTY at the right
    /// size (and as the latest size for resizes once running).
    private var lastSize: (cols: Int, rows: Int)?
    /// Debounces the PTY window-change: a pane resize (dragging a split divider) re-frames
    /// the view every frame, and sending a window-change per frame floods SSH + reflows
    /// tmux each time. We coalesce to one send once the size settles.
    private var resizeDebounce: Task<Void, Never>?
    /// Nudges the remote PTY size on attach/re-focus so tmux always fits the phone's
    /// viewport (even when desktop was previously the active client).
    private var syncSizeTask: Task<Void, Never>?
    private var didInitialSizeSync = false
    /// Single-finger scroll gesture, the pan translation already converted to wheel
    /// ticks, and the post-lift momentum loop.
    private weak var scrollGesture: UIPanGestureRecognizer?
    private var scrollConsumedY: CGFloat = 0
    private var momentumTask: Task<Void, Never>?
    private var scrollToBottomTask: Task<Void, Never>?
    private let pointsPerTick: CGFloat = 18
    /// Focuses the terminal (shows the keyboard) on the first tap without waiting for
    /// SwiftTerm's double/triple-tap disambiguation.
    private weak var focusTap: UITapGestureRecognizer?
    /// Two-finger pinch → live font zoom (see `handlePinch`).
    private weak var pinchGesture: UIPinchGestureRecognizer?
    private var pinchBaseSize: CGFloat = 0
    /// Fires with the final snapped size when a pinch ends; the store persists it
    /// and re-fonts every other live terminal.
    var onFontSizeCommitted: ((CGFloat) -> Void)?
    /// Fires with the live snapped font size during a pinch (nil when the gesture
    /// ends) so the pane can show a brief point-size HUD pill.
    var onFontSizePreview: ((CGFloat?) -> Void)?
    /// Fires (main actor) when the user scrolls up into tmux scrollback (true) or
    /// returns to the live bottom (false), driving the floating jump-to-bottom pill.
    var onScrolledBackChanged: ((Bool) -> Void)?
    /// Fires (main actor) when the user taps or starts typing while scrolled up in
    /// tmux scrollback, so the pane exits tmux copy-mode and keystrokes land in the
    /// agent's input box instead of being eaten by copy-mode.
    var onExitCopyModeRequested: (() -> Void)?
    /// Net upward scroll ticks sent since the last return to bottom / key input.
    private var scrollUpTicks = 0
    /// Fires (main actor) when the PTY ends **unexpectedly** — a transport drop, not a
    /// user-initiated `disconnect()`. Drives the pane's reconnect overlay + retry.
    var onConnectionLost: (() -> Void)?
    /// Fires when the user taps into this terminal — the deterministic focus hook the
    /// split UI uses to move keyboard/toolbar focus to this pane's leaf.
    var onFocusRequested: (() -> Void)?
    /// Fires (main actor, debounced) after the first PTY bytes arrive so background
    /// monitors can re-classify live status immediately once the screen paints.
    var onFirstOutput: (() -> Void)?
    private var didNotifyFirstOutput = false
    private var firstOutputNotifyTask: Task<Void, Never>?
    /// Buffers pre-tmux login shell banner / prompt bytes until tmux enters alternate
    /// screen (`\e[?1049h`) so the `export PATH=…` prelude never flashes on screen.
    private var awaitingTmuxScreen = true
    private var preTmuxBuffer: [UInt8] = []
    private var preTmuxFallbackTask: Task<Void, Never>?
    /// Set by `disconnect()` so the terminal `run()` completing after a deliberate
    /// teardown (Close pane / host edit / quit) doesn't read as a drop.
    private var intentionallyClosed = false
    /// Held-backspace acceleration state (see `send`).
    private var deleteStreak = 0
    private var lastDeleteAt: TimeInterval = 0
    /// Bell-haptic throttle (some TUIs ring BEL per keystroke).
    private var lastBellAt: TimeInterval = 0
    /// Unix time of the most recent PTY output — the live-status oracle. 0 until the
    /// first byte arrives (the `has_output` gate for startup injection + classify).
    private(set) var lastOutputAt: TimeInterval = 0

    /// Seconds since the last PTY output (the `idle_for` equivalent from live output,
    /// tighter than tmux `window_activity`). `.greatestFiniteMagnitude` before any.
    var idleFor: TimeInterval {
        lastOutputAt == 0 ? .greatestFiniteMagnitude : max(0, Date().timeIntervalSince1970 - lastOutputAt)
    }
    /// Whether the PTY has produced any output yet (startup-injection readiness).
    var hasOutput: Bool { lastOutputAt > 0 }
    /// Whether the pane rang the bell within the last poll interval (~3s).
    var recentBell: Bool { lastBellAt > 0 && Date().timeIntervalSince1970 - lastBellAt < 3 }

    enum Event {
        case send(ByteBuffer)
        case changeSize(cols: Int, rows: Int)
    }

    init(view: TerminalView, connection: SSHConnection, command: String,
         openGate: PTYOpenGate? = nil, isReadOnly: Bool = false) {
        self.view = view
        self.connection = connection
        self.command = command
        self.openGate = openGate
        self.isReadOnly = isReadOnly
        super.init()
        if !isReadOnly {
            installGestures()
            // Replace SwiftTerm's stock gray accessory bar with the themed muxel row.
            // Installed before the view can ever become first responder, so no
            // reloadInputViews() dance is needed.
            view.inputAccessoryView = TerminalAccessoryRow(session: self)
        }
    }

    // MARK: Single-finger fluid scroll → tmux mouse wheel

    /// A one-finger **vertical** swipe scrolls the pane's tmux scrollback by sending
    /// mouse wheel events (tmux `mouse on` is enabled on attach — see
    /// `TmuxCommands.setMouseOn`), with momentum on lift. It only begins for vertical
    /// pans (`gestureRecognizerShouldBegin`) so horizontal swipes — the edge back-swipe
    /// — pass through, and SwiftTerm's own pans are made to wait for it
    /// (`shouldBeRequiredToFailBy`), so a swipe scrolls instead of selecting.
    private func installGestures() {
        let g = UIPanGestureRecognizer(target: self, action: #selector(handleScrollPan(_:)))
        g.minimumNumberOfTouches = 1
        g.maximumNumberOfTouches = 1
        g.delaysTouchesEnded = false // don't hold touch-end back from SwiftTerm's taps
        g.delegate = self
        view.addGestureRecognizer(g)
        scrollGesture = g

        // Bring up the keyboard on the first tap. SwiftTerm's own single-tap is delayed
        // because it requires the double/triple-tap gestures to fail first; this fires
        // immediately and coexists with them, so focusing the terminal feels instant.
        let tap = UITapGestureRecognizer(target: self, action: #selector(handleFocusTap(_:)))
        tap.delegate = self
        view.addGestureRecognizer(tap)
        focusTap = tap

        // Two-finger pinch zooms the font. SwiftTerm ships no pinch of its own; the
        // 1-finger scroll pan can't see two touches, so they never co-fire (the
        // finger-added-mid-pan case is cancelled explicitly in handlePinch).
        let pinch = UIPinchGestureRecognizer(target: self, action: #selector(handlePinch(_:)))
        pinch.delegate = self
        view.addGestureRecognizer(pinch)
        pinchGesture = pinch
    }

    /// Live font zoom: snap to integer point sizes (bounding re-rasterization and
    /// the PTY window-change storm to one per point step) between 9 and 24. The
    /// font setter drives SwiftTerm's `resetFont → resize → sizeChanged`, which
    /// lands in the existing `.changeSize` → SSH window-change path — tmux reflows
    /// exactly as it does on rotation.
    @objc private func handlePinch(_ g: UIPinchGestureRecognizer) {
        switch g.state {
        case .began:
            momentumTask?.cancel()
            momentumTask = nil
            // Cancel an in-flight scroll pan so zoom and scroll never fight.
            scrollGesture?.isEnabled = false
            scrollGesture?.isEnabled = true
            pinchBaseSize = view.font.pointSize
            onFontSizePreview?(pinchBaseSize)
        case .changed:
            let snapped = TerminalKeys.snappedFontSize(pinchBaseSize * g.scale)
            if snapped != view.font.pointSize {
                view.font = .monospacedSystemFont(ofSize: snapped, weight: .regular)
            }
            onFontSizePreview?(snapped)
        case .ended:
            onFontSizeCommitted?(view.font.pointSize)
            onFontSizePreview?(nil)
        case .cancelled, .failed:
            onFontSizePreview?(nil)
        default:
            break
        }
    }

    /// Adopt a font size committed on another terminal (or restored at creation).
    @MainActor func applyFont(size: CGFloat) {
        guard view.font.pointSize != size else { return }
        view.font = .monospacedSystemFont(ofSize: size, weight: .regular)
    }

    @objc private func handleFocusTap(_ g: UITapGestureRecognizer) {
        if !view.isFirstResponder { _ = view.becomeFirstResponder() }
        if scrollUpTicks > 0 {
            resetScrollbackIndicator()
            onExitCopyModeRequested?()
        }
        onFocusRequested?()
    }

    @objc private func handleScrollPan(_ g: UIPanGestureRecognizer) {
        switch g.state {
        case .began:
            momentumTask?.cancel(); momentumTask = nil
            scrollToBottomTask?.cancel(); scrollToBottomTask = nil
            scrollConsumedY = 0
        case .changed:
            emitTicks(forTranslation: g.translation(in: view).y)
        case .ended:
            startMomentum(velocity: g.velocity(in: view).y)
        default:
            break
        }
    }

    /// Emit one wheel tick per `pointsPerTick` of finger travel not yet consumed.
    /// iOS-natural direction: dragging down reveals older output (tmux scroll up).
    private func emitTicks(forTranslation y: CGFloat) {
        while y - scrollConsumedY >= pointsPerTick { sendWheel(up: true);  scrollConsumedY += pointsPerTick }
        while y - scrollConsumedY <= -pointsPerTick { sendWheel(up: false); scrollConsumedY -= pointsPerTick }
    }

    /// After lift, keep scrolling with a decaying velocity so a flick coasts to a stop.
    private func startMomentum(velocity: CGFloat) {
        guard abs(velocity) > 250 else { return } // ignore slow releases
        momentumTask = Task { @MainActor in
            var v = velocity
            var carry: CGFloat = 0
            while !Task.isCancelled, abs(v) > 80 {
                carry += v / 60 // distance covered in one ~1/60s frame
                while carry >= pointsPerTick { sendWheel(up: true);  carry -= pointsPerTick }
                while carry <= -pointsPerTick { sendWheel(up: false); carry += pointsPerTick }
                v *= 0.94 // deceleration
                try? await Task.sleep(nanoseconds: 16_000_000)
            }
        }
    }

    /// Send one SGR mouse wheel event. tmux (mouse on) treats button 64 as wheel-up
    /// (into scrollback / copy mode) and 65 as wheel-down. Position is the grid centre,
    /// which lands inside the pane for any single-pane session.
    private func sendWheel(up: Bool) {
        let cols = lastSize?.cols ?? 80
        let rows = lastSize?.rows ?? 24
        let col = max(1, cols / 2)
        let row = max(1, rows / 2)
        let seq = "\u{1b}[<\(up ? 64 : 65);\(col);\(row)M"
        events.continuation.yield(.send(ByteBuffer(string: seq)))
        let wasScrolled = scrollUpTicks > 0
        if up {
            scrollUpTicks += 1
        } else {
            scrollUpTicks = max(0, scrollUpTicks - 1)
        }
        let nowScrolled = scrollUpTicks > 0
        if nowScrolled != wasScrolled { onScrolledBackChanged?(nowScrolled) }
    }

    /// Clear scroll momentum and reset the scrolled-back indicator (called when the
    /// user taps the Jump to bottom pill or sends keyboard input).
    @MainActor func resetScrollbackIndicator() {
        momentumTask?.cancel()
        momentumTask = nil
        if scrollUpTicks > 0 {
            scrollUpTicks = 0
            onScrolledBackChanged?(false)
        }
    }

    /// Jump back to the live bottom for mouse-aware TUIs (Grok, OpenCode, etc.) that
    /// consume tmux wheel events directly instead of entering tmux copy-mode: cancels
    /// momentum, resets the indicator, and emits paced SGR wheel-down chunks (so TUI
    /// event loops don't drop or coalesce a single huge write) all the way to the bottom.
    @MainActor func scrollToBottom() {
        let ticks = scrollUpTicks
        resetScrollbackIndicator()
        scrollToBottomTask?.cancel()
        scrollToBottomTask = nil
        let total = TerminalKeys.scrollToBottomTicks(forScrollUpTicks: ticks)
        guard total > 0 else { return }
        let cols = lastSize?.cols ?? 80
        let rows = lastSize?.rows ?? 24
        let col = max(1, cols / 2)
        let row = max(1, rows / 2)
        let one = "\u{1b}[<65;\(col);\(row)M"
        let batchSize = 20
        scrollToBottomTask = Task { @MainActor [weak self] in
            var remaining = total
            while remaining > 0, !Task.isCancelled, let self {
                let count = min(remaining, batchSize)
                self.events.continuation.yield(.send(ByteBuffer(string: String(repeating: one, count: count))))
                remaining -= count
                if remaining > 0 {
                    try? await Task.sleep(nanoseconds: 15_000_000)
                }
            }
        }
    }

    /// Only begin for predominantly-vertical pans, so horizontal swipes (the edge
    /// back-swipe) and taps fall through to the navigation / SwiftTerm.
    func gestureRecognizerShouldBegin(_ gestureRecognizer: UIGestureRecognizer) -> Bool {
        guard gestureRecognizer === scrollGesture,
              let pan = gestureRecognizer as? UIPanGestureRecognizer else { return true }
        let v = pan.velocity(in: view)
        return abs(v.y) > abs(v.x)
    }

    /// SwiftTerm's pan/selection gestures wait for our scroll to fail, so a vertical
    /// swipe scrolls rather than selecting. The screen-edge back-swipe is left alone.
    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer,
                           shouldBeRequiredToFailBy other: UIGestureRecognizer) -> Bool {
        guard gestureRecognizer === scrollGesture, other is UIPanGestureRecognizer else { return false }
        return !(other is UIScreenEdgePanGestureRecognizer)
    }

    /// The focus tap coexists with SwiftTerm's tap/selection gestures (it only calls
    /// `becomeFirstResponder`), so it never blocks them.
    func gestureRecognizer(_ gestureRecognizer: UIGestureRecognizer,
                           shouldRecognizeSimultaneouslyWith other: UIGestureRecognizer) -> Bool {
        gestureRecognizer === focusTap || other === focusTap
    }

    /// Request the PTY attach. The PTY is opened at SwiftTerm's real grid size, so we
    /// wait for the first `sizeChanged` (the view isn't laid out yet when the store
    /// calls this); if a size is already known, open immediately. Opening at the wrong
    /// size and resizing afterwards garbled full-width TUI output. Called on the main
    /// actor.
    func start() {
        guard task == nil else { return }
        startRequested = true
        isConnected = true // pending until the PTY opens; keeps the store from recycling it
        if let s = lastSize {
            launch(cols: s.cols, rows: s.rows)
        }
    }

    /// Start a headless background monitor without waiting for UIView layout.
    @MainActor func startHeadless(cols: Int = 120, rows: Int = 40) {
        guard task == nil else { return }
        startRequested = true
        lastSize = (cols, rows)
        launch(cols: cols, rows: rows)
    }

    /// Open the PTY at `cols`×`rows`. Idempotent (guards on `task`).
    private func launch(cols: Int, rows: Int) {
        guard task == nil else { return }
        isConnected = true
        task = Task { [weak self] in
            await self?.run(cols: cols, rows: rows)
            self?.markDisconnected()
        }
    }

    /// Tear the session down: end input, cancel the PTY task (detaches the tmux client;
    /// the remote session lives on), and mark dead. Called on the main actor.
    func disconnect() {
        intentionallyClosed = true
        isConnected = false
        momentumTask?.cancel()
        momentumTask = nil
        scrollToBottomTask?.cancel()
        scrollToBottomTask = nil
        resizeDebounce?.cancel()
        resizeDebounce = nil
        syncSizeTask?.cancel()
        syncSizeTask = nil
        firstOutputNotifyTask?.cancel()
        firstOutputNotifyTask = nil
        preTmuxFallbackTask?.cancel()
        preTmuxFallbackTask = nil
        preTmuxBuffer.removeAll()
        events.continuation.finish()
        task?.cancel()
        task = nil
    }

    @MainActor private func markDisconnected() {
        isConnected = false
        task = nil
        // A drop (not a user-initiated teardown) → tell the pane to reconnect.
        if !intentionallyClosed { onConnectionLost?() }
    }

    private func run(cols: Int, rows: Int) async {
        guard let client = try? await connection.sshClient() else {
            feedText("\r\n[no live terminal in this build]\r\n")
            return
        }
        // Bound concurrent channel opens: acquire before opening, release once the
        // channel is established (top of the withPTY body) or if the open fails.
        await openGate?.acquire()
        let releaser = GateReleaser(gate: openGate)
        do {
            try await client.withPTY(
                .init(wantReply: true, term: "xterm-256color",
                      terminalCharacterWidth: cols, terminalRowHeight: rows,
                      terminalPixelWidth: 0, terminalPixelHeight: 0,
                      // ECHO off on the bootstrap shell so the `export PATH=…; exec tmux`
                      // command is never echoed; tmux sets raw mode and creates a normal
                      // echoing inner PTY for the agent/shell pane.
                      terminalModes: .init([.ECHO: 0]))
            ) { [command, stream = events.stream, releaser] inbound, outbound in
                await releaser.release()  // channel established
                // Replace the PTY's login shell with tmux (attach / new-session -A).
                try await outbound.write(ByteBuffer(string: command + "\n"))
                await withThrowingTaskGroup(of: Void.self) { group in
                    group.addTask {
                        for try await chunk in inbound {
                            switch chunk {
                            case .stdout(var buf), .stderr(var buf):
                                if let bytes = buf.readBytes(length: buf.readableBytes) {
                                    await self.feed(bytes[...])
                                }
                            }
                        }
                    }
                    group.addTask {
                        for await event in stream {
                            switch event {
                            case let .send(buf):
                                try? await outbound.write(buf)
                            case let .changeSize(cols, rows):
                                try? await outbound.changeSize(cols: cols, rows: rows,
                                                               pixelWidth: 0, pixelHeight: 0)
                            }
                        }
                    }
                }
            }
        } catch {
            await releaser.release()  // release if the open itself failed
            feedText("\r\n[disconnected: \(error.localizedDescription)]\r\n")
        }
        await releaser.release()  // safety: no-op once released
    }

    @MainActor private func feed(_ bytes: ArraySlice<UInt8>) {
        if awaitingTmuxScreen {
            preTmuxBuffer.append(contentsOf: bytes)
            if let offset = TerminalKeys.tmuxAttachScreenOffset(in: preTmuxBuffer) {
                awaitingTmuxScreen = false
                preTmuxFallbackTask?.cancel()
                preTmuxFallbackTask = nil
                let slice = preTmuxBuffer[offset...]
                preTmuxBuffer.removeAll()
                deliverToTerminal(slice)
                return
            }
            // Fallback: if tmux doesn't take over within 1.8s (e.g. error message or
            // missing tmux), flush buffered output so errors are never hidden.
            if preTmuxFallbackTask == nil {
                preTmuxFallbackTask = Task { @MainActor [weak self] in
                    try? await Task.sleep(nanoseconds: 1_800_000_000)
                    guard let self, !Task.isCancelled, self.awaitingTmuxScreen else { return }
                    self.awaitingTmuxScreen = false
                    self.preTmuxFallbackTask = nil
                    let buffered = self.preTmuxBuffer
                    self.preTmuxBuffer.removeAll()
                    if !buffered.isEmpty {
                        self.deliverToTerminal(buffered[...])
                    }
                }
            }
            return
        }
        deliverToTerminal(bytes)
    }

    @MainActor private func deliverToTerminal(_ bytes: ArraySlice<UInt8>) {
        lastOutputAt = Date().timeIntervalSince1970
        view.feed(byteArray: bytes)
        if !isReadOnly, !didInitialSizeSync {
            didInitialSizeSync = true
            syncRemoteSize(delayMs: 80)
        }
        if !didNotifyFirstOutput, let notify = onFirstOutput {
            didNotifyFirstOutput = true
            firstOutputNotifyTask = Task { @MainActor [weak self] in
                // Re-classify twice: once quickly (~600ms) and once after full screen
                // settle (~1500ms) so live status updates immediately.
                try? await Task.sleep(nanoseconds: 600_000_000)
                guard let self, !Task.isCancelled, self.isConnected else { return }
                notify()
                try? await Task.sleep(nanoseconds: 900_000_000)
                guard !Task.isCancelled, self.isConnected else { return }
                notify()
            }
        }
    }

    /// Force the remote tmux client to receive a `SIGWINCH` and resize the window to
    /// fit this device's exact `(cols, rows)` without needing the user to tap first.
    /// Nudges `cols - 1` → `cols` because Linux/macOS kernels suppress `SIGWINCH`
    /// when `TIOCSWINSZ` sees identical dimensions.
    @MainActor func syncRemoteSize(delayMs: UInt64 = 0) {
        guard isConnected else { return }
        syncSizeTask?.cancel()
        syncSizeTask = Task { @MainActor [weak self] in
            if delayMs > 0 {
                try? await Task.sleep(nanoseconds: delayMs * 1_000_000)
            }
            guard let self, !Task.isCancelled, self.isConnected else { return }
            let size = self.lastSize ?? {
                let t = self.view.getTerminal()
                return (t.cols, t.rows)
            }()
            guard size.cols > 1, size.rows > 0 else { return }
            self.events.continuation.yield(.changeSize(cols: size.cols - 1, rows: size.rows))
            try? await Task.sleep(nanoseconds: 60_000_000)
            guard !Task.isCancelled, self.isConnected else { return }
            let finalSize = self.lastSize ?? size
            guard finalSize.cols > 0, finalSize.rows > 0 else { return }
            self.events.continuation.yield(.changeSize(cols: finalSize.cols, rows: finalSize.rows))
        }
    }

    @MainActor private func feedText(_ text: String) {
        view.feed(text: text)
    }

    /// The visible grid as plain text — the marker-scan input, the iOS analogue of
    /// desktop's `visible_text()` (`session.rs`). Reads only the on-screen rows
    /// (`0..<rows`, resolved through the display offset by SwiftTerm), never
    /// scrollback, so it's cheap enough to run every poll. Main actor (SwiftTerm state).
    @MainActor func visibleText() -> String {
        let terminal = view.getTerminal()
        var lines: [String] = []
        lines.reserveCapacity(terminal.rows)
        for row in 0..<terminal.rows {
            if let line = terminal.getLine(row: row) {
                lines.append(line.translateToString(trimRight: true))
            }
        }
        return lines.joined(separator: "\n")
    }

    /// Accessory-key path: raw bytes straight to the PTY, bypassing the delegate
    /// `send()` heuristics (and resetting the held-backspace streak, so a key tap
    /// can never extend a delete run).
    @MainActor func sendKey(_ bytes: [UInt8]) {
        deleteStreak = 0
        view.unmarkText()
        if scrollUpTicks > 0 {
            resetScrollbackIndicator()
            onExitCopyModeRequested?()
        }
        events.continuation.yield(.send(ByteBuffer(bytes: bytes)))
    }

    // MARK: TerminalViewDelegate

    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        if scrollUpTicks > 0 {
            resetScrollbackIndicator()
            onExitCopyModeRequested?()
        }
        events.continuation.yield(.send(ByteBuffer(bytes: data)))

        // Accelerate held backspace. iOS repeats `deleteBackward` at a flat rate for a
        // custom key-input view (it doesn't apply the delete acceleration it gives
        // normal text fields), so holding backspace deletes slowly. On a sustained run
        // of backspace bytes (30ms–200ms apart), send extra backspaces straight to the
        // PTY — ramping up the longer it's held. Rapid bursts (<30ms, e.g. UIKit's
        // `replace(_:withText:)` deleting a word) are never accelerated so we never
        // over-delete and overwrite earlier prompt text.
        guard data.count == 1, let b = data.first, b == 0x7f || b == 0x08 else {
            deleteStreak = 0
            return
        }
        let now = ProcessInfo.processInfo.systemUptime
        let (nextStreak, extra) = TerminalKeys.acceleratedBackspaces(
            interval: now - lastDeleteAt, streak: deleteStreak)
        deleteStreak = nextStreak
        lastDeleteAt = now
        for _ in 0..<extra {
            events.continuation.yield(.send(ByteBuffer(bytes: [b])))
        }
    }

    func sizeChanged(source: TerminalView, newCols: Int, newRows: Int) {
        guard newCols > 0, newRows > 0 else { return }
        lastSize = (newCols, newRows)
        if startRequested, task == nil {
            // The view is laid out + in a window now (first real size), so switch on
            // SwiftTerm's Metal GPU renderer: it rasterizes the current grid each frame
            // instead of the default CoreGraphics path that composites row "stripes"
            // and leaves stale glyphs stacked on redraw (the overlapping-text glitch).
            // Best-effort — falls back to CoreGraphics if Metal is unavailable.
            try? source.setUseMetal(true)
            // Keep the default `.perRowPersistent` buffering — `.perFrameAggregated`
            // was a smoothness tweak but its per-frame glyph aggregation shifted the
            // cursor/cells by one; the per-row default renders the cursor correctly.
            source.metalBufferingMode = .perRowPersistent
            // …then open the PTY at the real size.
            launch(cols: newCols, rows: newRows)
        } else {
            // Already running → forward as a live resize, debounced so a divider drag
            // (many size changes) sends one window-change once it settles, not per frame.
            syncSizeTask?.cancel()
            syncSizeTask = nil
            resizeDebounce?.cancel()
            resizeDebounce = Task { @MainActor [weak self] in
                try? await Task.sleep(nanoseconds: 150_000_000)
                guard let self, !Task.isCancelled else { return }
                self.events.continuation.yield(.changeSize(cols: newCols, rows: newRows))
            }
        }
    }

    func setTerminalTitle(source: TerminalView, title: String) {}
    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
    func scrolled(source: TerminalView, position: Double) {}
    func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {
        guard let url = TerminalKeys.openableURL(link) else { return }
        UIApplication.shared.open(url)
    }

    /// The remote rang the bell — a warning haptic, throttled so a BEL-per-keystroke
    /// TUI can't buzz continuously.
    func bell(source: TerminalView) {
        let now = ProcessInfo.processInfo.systemUptime
        guard now - lastBellAt > 1.5 else { return }
        lastBellAt = now
        UINotificationFeedbackGenerator().notificationOccurred(.warning)
    }

    /// OSC-52 copy from the remote (tmux `set-clipboard on` forwards it — enabled
    /// on attach next to `mouse on`). SwiftTerm hands over the already-base64-decoded
    /// bytes; land them in the system pasteboard.
    func clipboardCopy(source: TerminalView, content: Data) {
        guard let text = String(data: content, encoding: .utf8), !text.isEmpty else { return }
        UIPasteboard.general.string = text
    }

    func iTermContent(source: TerminalView, content: ArraySlice<UInt8>) {}
    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
}

/// The persisted terminal font size — pinch-to-zoom sets it, every terminal (live
/// and future) adopts it. Same UserDefaults convention as `"muxel.theme.id"`.
enum TerminalFontPreference {
    static let key = "muxel.terminal.fontSize"

    /// SwiftTerm's default is 12pt; stored values are clamped to the same 9…24
    /// range the pinch enforces.
    static var size: CGFloat {
        get {
            let stored = UserDefaults.standard.double(forKey: key)
            return stored > 0 ? TerminalKeys.snappedFontSize(stored) : 12
        }
        set { UserDefaults.standard.set(Double(newValue), forKey: key) }
    }

    static var font: UIFont { .monospacedSystemFont(ofSize: size, weight: .regular) }
}

/// App-owned cache of live `TerminalSession`s, keyed by instance id, so a pane's SSH
/// PTY survives navigation (backing out of a project, switching tabs) — the iOS
/// analogue of desktop muxel owning terminals in `MuxelApp.terminals`. Sessions are
/// torn down only by `disconnect(_:)` (Close instance), `disconnect(forHost:)` (host
/// delete), or process exit (app fully quit). Lives on `AppState`.
@MainActor
final class TerminalStore {
    private struct Entry {
        let hostId: UUID
        let session: TerminalSession
    }
    private var entries: [String: Entry] = [:]
    /// One PTY-open gate per host, so cold-starting a multi-pane split layout doesn't
    /// fire N unbounded channel opens at once.
    private var gates: [UUID: PTYOpenGate] = [:]
    /// Fired (main actor) with an instance id when its session drops unexpectedly.
    /// `AppState` wires this to `deadPanes` so the pane shows a reconnect overlay.
    var onSessionDied: ((String) -> Void)?

    private func gate(for hostId: UUID) -> PTYOpenGate {
        if let g = gates[hostId] { return g }
        let g = PTYOpenGate(limit: 2)
        gates[hostId] = g
        return g
    }

    /// Whether any of `instanceIds` currently holds a live connected PTY session
    /// (actively streaming & marker-monitored in the background).
    func hasLiveSession(for instanceIds: [String]) -> Bool {
        instanceIds.contains { entries[$0]?.session.isConnected == true }
    }

    /// Whether the instance's live session has produced any PTY output yet — the
    /// readiness oracle for startup injection. False if no session is attached.
    func hasOutput(for instanceId: String) -> Bool {
        entries[instanceId]?.session.hasOutput ?? false
    }

    /// A connected pane's live-screen snapshot for marker classification, or nil if no
    /// session is attached for `instanceId`. Reading the SwiftTerm grid is cheap; it's
    /// what gives an attached pane real working/blocked status (unlike the tmux-vars
    /// poll, which can only see exit/bell/activity).
    func liveScreen(for instanceId: String) -> LiveScreen? {
        guard let e = entries[instanceId], e.session.isConnected else { return nil }
        return LiveScreen(text: e.session.visibleText(),
                          idle: e.session.idleFor,
                          bell: e.session.recentBell)
    }

    /// Max concurrent read-only background monitor channels per host. OpenSSH defaults
    /// to `MaxSessions 10` per TCP connection, so capping at 6 leaves ample headroom for
    /// serialized exec polls and interactive panes.
    static let maxBackgroundMonitorsPerHost = 6

    /// The live **interactive** session for `instanceId`, or nil if none — recycling a
    /// session whose PTY has died so the caller re-creates a fresh attach. Read-only
    /// background monitors return nil here so opening a tab upgrades cleanly to an
    /// interactive session.
    func existing(_ instanceId: String) -> TerminalSession? {
        guard let e = entries[instanceId] else { return nil }
        if !e.session.isConnected {
            entries[instanceId] = nil
            return nil
        }
        if e.session.isReadOnly { return nil }
        return e.session
    }

    /// Start a headless read-only monitor (`tmux attach-session -r`) for `instanceId`
    /// if it isn't already attached (interactive or read-only) and the host is within
    /// `maxBackgroundMonitorsPerHost`. When `evictIdleIfNeeded` is true and the host is
    /// already at its monitor cap, evicts the longest-idle (>60s) read-only monitor to
    /// make room for a newly active agent. Returns true if a new monitor was started.
    @discardableResult
    func startBackgroundMonitor(
        for instanceId: String,
        hostId: UUID,
        connection: SSHConnection,
        command: String,
        evictIdleIfNeeded: Bool = false,
        onReady: @escaping () -> Void
    ) -> Bool {
        if let e = entries[instanceId] {
            if e.session.isConnected { return false }
            entries[instanceId] = nil
        }
        let activeMonitors = entries.filter {
            $0.value.hostId == hostId && $0.value.session.isReadOnly && $0.value.session.isConnected
        }
        if activeMonitors.count >= Self.maxBackgroundMonitorsPerHost {
            guard evictIdleIfNeeded,
                  let victim = activeMonitors
                    .filter({ $0.value.session.idleFor > 60 })
                    .max(by: { $0.value.session.idleFor < $1.value.session.idleFor })
            else { return false }
            victim.value.session.disconnect()
            entries[victim.key] = nil
        }

        let view = MuxelTerminalView(frame: CGRect(x: 0, y: 0, width: 960, height: 640),
                                     font: TerminalFontPreference.font)
        let session = TerminalSession(view: view, connection: connection, command: command,
                                      openGate: gate(for: hostId), isReadOnly: true)
        view.terminalDelegate = session
        session.onFirstOutput = onReady
        entries[instanceId] = Entry(hostId: hostId, session: session)
        session.startHeadless(cols: 120, rows: 40)
        return true
    }

    /// Attach a headless read-only monitor (`tmux -u attach-session -r`) for
    /// `instanceId` and wait until its initial tmux screen paints (or times out),
    /// returning the live `LiveScreen` snapshot. Leaves the session in `entries` so
    /// the caller can either keep it running (for working/watched projects) or call
    /// `disconnectReadOnly(_:)` (for idle unwatched projects).
    func probeLiveScreen(
        for instanceId: String,
        hostId: UUID,
        connection: SSHConnection,
        command: String,
        timeoutSecs: TimeInterval = 2.4,
        onReady: @escaping () -> Void
    ) async -> LiveScreen? {
        if let e = entries[instanceId], e.session.isConnected, e.session.hasOutput {
            return liveScreen(for: instanceId)
        }
        let session: TerminalSession
        if let e = entries[instanceId], e.session.isConnected {
            session = e.session
        } else {
            let view = MuxelTerminalView(frame: CGRect(x: 0, y: 0, width: 960, height: 640),
                                         font: TerminalFontPreference.font)
            let created = TerminalSession(view: view, connection: connection, command: command,
                                          openGate: gate(for: hostId), isReadOnly: true)
            view.terminalDelegate = created
            created.onFirstOutput = onReady
            entries[instanceId] = Entry(hostId: hostId, session: created)
            created.startHeadless(cols: 120, rows: 40)
            session = created
        }

        let start = Date()
        var firstOutputAt: Date? = session.hasOutput ? start : nil
        while session.isConnected && Date().timeIntervalSince(start) < timeoutSecs {
            if session.hasOutput {
                if firstOutputAt == nil { firstOutputAt = Date() }
                let sinceFirst = Date().timeIntervalSince(firstOutputAt!)
                // Wait at least 0.75s after the first byte (login shell + tmux attach)
                // and let output settle (~0.35s quiet or 1.4s total after first output).
                if sinceFirst >= 1.4 || (sinceFirst >= 0.75 && session.idleFor >= 0.35) {
                    break
                }
            }
            try? await Task.sleep(nanoseconds: 100_000_000)
        }
        guard session.isConnected, session.hasOutput else { return nil }
        return liveScreen(for: instanceId)
    }

    /// Probe multiple running tmux sessions on a single host over **one** SSH PTY
    /// channel (reusing the single pooled `SSHConnection`). Already-live sessions in
    /// `entries` are returned immediately with zero network traffic; remaining targets
    /// are snapshotted sequentially (~150ms each) inside one PTY and decoded offscreen
    /// with a pure SwiftTerm `Terminal` (no `UIView` allocation, no `capture-pane`).
    func batchProbeScreens(
        targets: [(id: String, session: String, socket: String?, idle: TimeInterval, bell: Bool)],
        connection: SSHConnection
    ) async -> [String: LiveScreen] {
        var results: [String: LiveScreen] = [:]
        var toProbe: [(id: String, session: String, socket: String?, idle: TimeInterval, bell: Bool)] = []
        for t in targets {
            if let existing = liveScreen(for: t.id), !existing.text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                results[t.id] = existing
            } else {
                toProbe.append(t)
            }
        }
        guard !toProbe.isEmpty else { return results }
        let probed = await Self.batchProbeScreensOffscreen(targets: toProbe, connection: connection)
        for (id, screen) in probed { results[id] = screen }
        return results
    }

    /// Offscreen single-PTY batched read-only screen probe callable from any actor
    /// (including `StatusPoller` background refreshes). Never creates UIViews or runs
    /// `capture-pane`.
    nonisolated static func batchProbeScreensOffscreen(
        targets: [(id: String, session: String, socket: String?, idle: TimeInterval, bell: Bool)],
        connection: SSHConnection
    ) async -> [String: LiveScreen] {
        guard !targets.isEmpty else { return [:] }
        guard let client = try? await connection.sshClient() else { return [:] }

        let nonce = UUID().uuidString.replacingOccurrences(of: "-", with: "").prefix(8).lowercased()
        let cmd = TmuxCommands.batchProbeReadOnlyPTYCommand(
            targets: targets.map { ($0.session, $0.socket) },
            nonce: nonce)
        let doneMarker = Array("__MUXEL_PD_\(nonce)__".utf8)
        let maxDurationNs = UInt64(max(4.0, Double(targets.count) * 1.2) * 1_000_000_000)

        let captured: [UInt8] = await withTaskGroup(of: [UInt8].self) { group in
            group.addTask {
                var buffer: [UInt8] = []
                do {
                    try await client.withPTY(
                        .init(wantReply: true, term: "xterm-256color",
                              terminalCharacterWidth: 120, terminalRowHeight: 40,
                              terminalPixelWidth: 0, terminalPixelHeight: 0,
                              terminalModes: .init([.ECHO: 0]))
                    ) { inbound, outbound in
                        try await outbound.write(ByteBuffer(string: cmd + "\n"))
                        for try await chunk in inbound {
                            if Task.isCancelled { break }
                            switch chunk {
                            case .stdout(var buf), .stderr(var buf):
                                if let bytes = buf.readBytes(length: buf.readableBytes) {
                                    buffer.append(contentsOf: bytes)
                                    if Self.containsMarker(doneMarker, in: buffer) {
                                        return
                                    }
                                }
                            }
                        }
                    }
                } catch {
                    // Best-effort: return whatever slices already arrived before drop/cancel.
                }
                return buffer
            }
            group.addTask {
                try? await Task.sleep(nanoseconds: maxDurationNs)
                return []
            }
            let first = await group.next() ?? []
            group.cancelAll()
            return first
        }

        var results: [String: LiveScreen] = [:]
        let slices = Self.parseBatchProbeOutput(captured, count: targets.count, nonce: nonce)
        for (idx, target) in targets.enumerated() {
            guard let raw = slices[idx], !raw.isEmpty,
                  // Require a real tmux alt-screen paint (`\e[?1049h` / `\e[?47h`); a
                  // bare `[detached (from session …)]` line with no screen paint would
                  // otherwise look like a marker-free screen and falsely trigger `.done`.
                  TerminalKeys.tmuxAttachScreenOffset(in: raw) != nil else { continue }
            let text = Self.renderScreenText(from: raw, cols: 120, rows: 40)
            if !text.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty {
                results[target.id] = LiveScreen(text: text, idle: target.idle, bell: target.bell)
            }
        }
        return results
    }

    /// Pure parser for `batchProbeReadOnlyPTYCommand` output: extracts each target's
    /// raw PTY byte slice between `__MUXEL_PB_<nonce>_<i>__` and `__MUXEL_PE_<nonce>_<i>__`.
    nonisolated static func parseBatchProbeOutput(
        _ bytes: [UInt8], count: Int, nonce: String
    ) -> [Int: [UInt8]] {
        var out: [Int: [UInt8]] = [:]
        var searchFrom = 0
        for idx in 0..<count {
            let begin = Array("__MUXEL_PB_\(nonce)_\(idx)__".utf8)
            let end = Array("__MUXEL_PE_\(nonce)_\(idx)__".utf8)
            guard let bPos = indexOf(begin, in: bytes, from: searchFrom) else { continue }
            let payloadStart = bPos + begin.count
            guard let ePos = indexOf(end, in: bytes, from: payloadStart) else { continue }
            out[idx] = Array(bytes[payloadStart..<ePos])
            searchFrom = ePos + end.count
        }
        return out
    }

    /// Render a single probed tmux byte slice into plain visible screen text using an
    /// offscreen SwiftTerm `Terminal` (zero UIView/Metal overhead).
    nonisolated static func renderScreenText(from bytes: [UInt8], cols: Int = 120, rows: Int = 40) -> String {
        let delegate = SilentTerminalDelegate()
        let terminal = Terminal(delegate: delegate, options: TerminalOptions(cols: cols, rows: rows))
        let slice = TerminalKeys.tmuxAltScreenSlice(in: bytes)
        terminal.feed(buffer: slice)
        var lines: [String] = []
        lines.reserveCapacity(terminal.rows)
        for row in 0..<terminal.rows {
            if let line = terminal.getLine(row: row) {
                lines.append(line.translateToString(trimRight: true))
            }
        }
        return lines.joined(separator: "\n")
    }

    private nonisolated static func containsMarker(_ marker: [UInt8], in bytes: [UInt8]) -> Bool {
        let start = max(0, bytes.count - marker.count - 128)
        return indexOf(marker, in: bytes, from: start) != nil
    }

    private nonisolated static func indexOf(_ needle: [UInt8], in haystack: [UInt8], from start: Int) -> Int? {
        guard !needle.isEmpty, haystack.count >= start + needle.count else { return nil }
        let limit = haystack.count - needle.count
        let first = needle[0]
        for i in start...limit where haystack[i] == first {
            var matched = true
            for j in 1..<needle.count where haystack[i + j] != needle[j] {
                matched = false
                break
            }
            if matched { return i }
        }
        return nil
    }

    /// Disconnect `instanceId` only if it is a background read-only monitor (never
    /// disconnects a real interactive terminal the user opened).
    func disconnectReadOnly(_ instanceId: String) {
        guard let e = entries[instanceId], e.session.isReadOnly else { return }
        e.session.disconnect()
        entries[instanceId] = nil
    }

    /// Number of currently connected read-only background monitors on `hostId`.
    func activeReadOnlyMonitorCount(on hostId: UUID) -> Int {
        entries.values.filter {
            $0.hostId == hostId && $0.session.isReadOnly && $0.session.isConnected
        }.count
    }

    /// Get-or-create the interactive session for `instanceId`, starting its PTY on first
    /// creation. Upgrades (disconnects) any existing read-only background monitor first.
    func session(for instanceId: String, hostId: UUID,
                 connection: SSHConnection, command: String) -> TerminalSession {
        if let s = existing(instanceId) { return s }
        if let e = entries[instanceId], e.session.isReadOnly {
            e.session.disconnect()
            entries[instanceId] = nil
        }
        let view = MuxelTerminalView(frame: .zero, font: TerminalFontPreference.font)
        let session = TerminalSession(view: view, connection: connection, command: command,
                                      openGate: gate(for: hostId))
        view.terminalDelegate = session
        session.onFontSizeCommitted = { [weak self] size in
            TerminalFontPreference.size = size
            self?.applyFontSize(size, except: instanceId)
        }
        session.onConnectionLost = { [weak self] in self?.onSessionDied?(instanceId) }
        entries[instanceId] = Entry(hostId: hostId, session: session)
        session.start()
        return session
    }

    /// Persist a new terminal font size (from Settings) and apply it to every live pane.
    func setFontSize(_ size: CGFloat) {
        let snapped = TerminalKeys.snappedFontSize(size)
        TerminalFontPreference.size = snapped
        applyFontSize(snapped)
    }

    /// Re-font every other live terminal after a pinch commits (each one's font
    /// setter drives its own sizeChanged → SSH window-change).
    func applyFontSize(_ size: CGFloat, except instanceId: String? = nil) {
        for (id, e) in entries where id != instanceId {
            e.session.applyFont(size: size)
        }
    }

    func disconnect(_ instanceId: String) {
        entries[instanceId]?.session.disconnect()
        entries[instanceId] = nil
    }

    func disconnect(forHost hostId: UUID) {
        for (id, e) in entries where e.hostId == hostId {
            e.session.disconnect()
            entries[id] = nil
        }
    }
}

/// Minimal no-op delegate for offscreen SwiftTerm `Terminal` screen-snapshot decoding.
private final class SilentTerminalDelegate: TerminalDelegate {
    func showCursor(source: Terminal) {}
    func hideCursor(source: Terminal) {}
    func send(source: Terminal, data: ArraySlice<UInt8>) {}
}

/// TerminalView configured for reliable iOS & Deskflow agent input:
/// - Disables SwiftTerm's mouse reporting so taps/drags never send stray clicks into
///   tmux/TUI panels that steal focus from the agent's text box or trigger copy-mode
///   (vertical scrolling still works via `TerminalSession`'s SGR wheel gesture, and
///   OSC-8 links / long-press selection stay intact).
/// - Disables iOS inline predictions / math completions / smart replacements so UIKit
///   doesn't call `replace(_:withText:)` against stale local storage and overwrite
///   prompt text.
private final class MuxelTerminalView: TerminalView {
    override init(frame: CGRect, font: UIFont?) {
        super.init(frame: frame, font: font)
        configureTerminalInputTraits()
    }

    required init?(coder: NSCoder) {
        super.init(coder: coder)
        configureTerminalInputTraits()
    }

    @available(iOS 17.0, *)
    var inlinePredictionType: UITextInlinePredictionType {
        get { .no }
        set {}
    }

    @available(iOS 18.0, *)
    var mathExpressionCompletionType: UITextMathExpressionCompletionType {
        get { .no }
        set {}
    }

    private func configureTerminalInputTraits() {
        allowMouseReporting = false
        // Pixel-aligned box-drawing and half/shade block glyphs (for smooth RGB
        // block gradients and TUI borders) plus full bright ANSI colors.
        customBlockGlyphs = true
        useBrightColors = true
        autocapitalizationType = .none
        autocorrectionType = .no
        spellCheckingType = .no
        smartQuotesType = .no
        smartDashesType = .no
        smartInsertDeleteType = .no
    }
}

/// A counting semaphore bounding concurrent PTY channel opens per host. Same
/// continuation-queue shape as `CitadelSSHConnection`'s command slot.
actor PTYOpenGate {
    private let limit: Int
    private var active = 0
    private var waiters: [CheckedContinuation<Void, Never>] = []

    init(limit: Int) { self.limit = limit }

    func acquire() async {
        if active < limit {
            active += 1
            return
        }
        await withCheckedContinuation { waiters.append($0) }
    }

    func release() {
        if let next = waiters.first {
            waiters.removeFirst()
            next.resume()  // hand the slot straight to a waiter (active unchanged)
        } else {
            active = max(0, active - 1)
        }
    }
}

/// One-shot gate release, so both the withPTY body (channel established) and the
/// open-failure catch can call `release()` without double-releasing.
actor GateReleaser {
    private let gate: PTYOpenGate?
    private var done = false
    init(gate: PTYOpenGate?) { self.gate = gate }
    func release() async {
        guard !done else { return }
        done = true
        await gate?.release()
    }
}
