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
    /// Single-finger scroll gesture, the pan translation already converted to wheel
    /// ticks, and the post-lift momentum loop.
    private weak var scrollGesture: UIPanGestureRecognizer?
    private var scrollConsumedY: CGFloat = 0
    private var momentumTask: Task<Void, Never>?
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
    /// Fires (main actor) when the PTY ends **unexpectedly** — a transport drop, not a
    /// user-initiated `disconnect()`. Drives the pane's reconnect overlay + retry.
    var onConnectionLost: (() -> Void)?
    /// Fires when the user taps into this terminal — the deterministic focus hook the
    /// split UI uses to move keyboard/toolbar focus to this pane's leaf.
    var onFocusRequested: (() -> Void)?
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

    init(view: TerminalView, connection: SSHConnection, command: String, openGate: PTYOpenGate? = nil) {
        self.view = view
        self.connection = connection
        self.command = command
        self.openGate = openGate
        super.init()
        installGestures()
        // Replace SwiftTerm's stock gray accessory bar with the themed muxel row.
        // Installed before the view can ever become first responder, so no
        // reloadInputViews() dance is needed.
        view.inputAccessoryView = TerminalAccessoryRow(session: self)
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
        case .changed:
            let snapped = TerminalKeys.snappedFontSize(pinchBaseSize * g.scale)
            if snapped != view.font.pointSize {
                view.font = .monospacedSystemFont(ofSize: snapped, weight: .regular)
            }
        case .ended:
            onFontSizeCommitted?(view.font.pointSize)
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
        onFocusRequested?()
    }

    @objc private func handleScrollPan(_ g: UIPanGestureRecognizer) {
        switch g.state {
        case .began:
            momentumTask?.cancel(); momentumTask = nil
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

    /// Open the PTY at `cols`×`rows`. Idempotent (guards on `task`).
    private func launch(cols: Int, rows: Int) {
        guard task == nil else { return }
        isConnected = true
        task = Task { [weak self] in
            await self?.run(cols: cols, rows: rows)
            await self?.markDisconnected()
        }
    }

    /// Tear the session down: end input, cancel the PTY task (detaches the tmux client;
    /// the remote session lives on), and mark dead. Called on the main actor.
    func disconnect() {
        intentionallyClosed = true
        isConnected = false
        momentumTask?.cancel()
        momentumTask = nil
        resizeDebounce?.cancel()
        resizeDebounce = nil
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
            await feedText("\r\n[no live terminal in this build]\r\n")
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
                      terminalModes: .init([.ECHO: 5]))
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
            await feedText("\r\n[disconnected: \(error.localizedDescription)]\r\n")
        }
        await releaser.release()  // safety: no-op once released
    }

    @MainActor private func feed(_ bytes: ArraySlice<UInt8>) {
        lastOutputAt = Date().timeIntervalSince1970
        view.feed(byteArray: bytes)
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
        events.continuation.yield(.send(ByteBuffer(bytes: bytes)))
    }

    // MARK: TerminalViewDelegate

    func send(source: TerminalView, data: ArraySlice<UInt8>) {
        events.continuation.yield(.send(ByteBuffer(bytes: data)))

        // Accelerate held backspace. iOS repeats `deleteBackward` at a flat rate for a
        // custom key-input view (it doesn't apply the delete acceleration it gives
        // normal text fields), so holding backspace deletes slowly. On a sustained run
        // of backspace bytes, send extra backspaces straight to the PTY — ramping up the
        // longer it's held. We send raw bytes rather than calling `deleteBackward()`
        // again: re-entering it mid-edit corrupts SwiftTerm's text-input range and
        // crashes ("String index is out of bounds"). The remote deletes the chars and
        // the screen follows via echo. Single taps reset the streak.
        guard data.count == 1, let b = data.first, b == 0x7f || b == 0x08 else {
            deleteStreak = 0
            return
        }
        let now = ProcessInfo.processInfo.systemUptime
        deleteStreak = (now - lastDeleteAt < 0.2) ? deleteStreak + 1 : 0
        lastDeleteAt = now
        let extra = min(deleteStreak / 3, 5) // 0 at first, ramping to +5 (≈6× on hold)
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
    func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {}

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

    /// The live session for `instanceId`, or nil if none — recycling (dropping) a
    /// session whose PTY has died so the caller re-creates a fresh attach.
    func existing(_ instanceId: String) -> TerminalSession? {
        guard let e = entries[instanceId] else { return nil }
        if !e.session.isConnected {
            entries[instanceId] = nil
            return nil
        }
        return e.session
    }

    /// Get-or-create the session for `instanceId`, starting its PTY on first creation.
    /// The view is created at the stored font size so the deferred first-size attach
    /// opens the PTY at the right grid — no post-hoc resize.
    func session(for instanceId: String, hostId: UUID,
                 connection: SSHConnection, command: String) -> TerminalSession {
        if let s = existing(instanceId) { return s }
        let view = TerminalView(frame: .zero, font: TerminalFontPreference.font)
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
