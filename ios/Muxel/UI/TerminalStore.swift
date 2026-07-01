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
    private let events = AsyncStream<Event>.makeStream()
    private var task: Task<Void, Never>?
    /// Set once `start()` is requested; the PTY actually opens on the first real size.
    private var startRequested = false
    /// The most recent grid size SwiftTerm reported, used to open the PTY at the right
    /// size (and as the latest size for resizes once running).
    private var lastSize: (cols: Int, rows: Int)?
    /// Single-finger scroll gesture, the pan translation already converted to wheel
    /// ticks, and the post-lift momentum loop.
    private weak var scrollGesture: UIPanGestureRecognizer?
    private var scrollConsumedY: CGFloat = 0
    private var momentumTask: Task<Void, Never>?
    private let pointsPerTick: CGFloat = 18
    /// Focuses the terminal (shows the keyboard) on the first tap without waiting for
    /// SwiftTerm's double/triple-tap disambiguation.
    private weak var focusTap: UITapGestureRecognizer?
    /// Held-backspace acceleration state (see `send`).
    private var deleteStreak = 0
    private var lastDeleteAt: TimeInterval = 0

    enum Event {
        case send(ByteBuffer)
        case changeSize(cols: Int, rows: Int)
    }

    init(view: TerminalView, connection: SSHConnection, command: String) {
        self.view = view
        self.connection = connection
        self.command = command
        super.init()
        installScrollGesture()
    }

    // MARK: Single-finger fluid scroll → tmux mouse wheel

    /// A one-finger **vertical** swipe scrolls the pane's tmux scrollback by sending
    /// mouse wheel events (tmux `mouse on` is enabled on attach — see
    /// `TmuxCommands.setMouseOn`), with momentum on lift. It only begins for vertical
    /// pans (`gestureRecognizerShouldBegin`) so horizontal swipes — the edge back-swipe
    /// — pass through, and SwiftTerm's own pans are made to wait for it
    /// (`shouldBeRequiredToFailBy`), so a swipe scrolls instead of selecting.
    private func installScrollGesture() {
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
    }

    @objc private func handleFocusTap(_ g: UITapGestureRecognizer) {
        if !view.isFirstResponder { _ = view.becomeFirstResponder() }
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
        isConnected = false
        momentumTask?.cancel()
        momentumTask = nil
        events.continuation.finish()
        task?.cancel()
        task = nil
    }

    @MainActor private func markDisconnected() {
        isConnected = false
        task = nil
    }

    private func run(cols: Int, rows: Int) async {
        guard let client = try? await connection.sshClient() else {
            await feedText("\r\n[no live terminal in this build]\r\n")
            return
        }
        do {
            try await client.withPTY(
                .init(wantReply: true, term: "xterm-256color",
                      terminalCharacterWidth: cols, terminalRowHeight: rows,
                      terminalPixelWidth: 0, terminalPixelHeight: 0,
                      terminalModes: .init([.ECHO: 5]))
            ) { [command, stream = events.stream] inbound, outbound in
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
            await feedText("\r\n[disconnected: \(error.localizedDescription)]\r\n")
        }
    }

    @MainActor private func feed(_ bytes: ArraySlice<UInt8>) {
        view.feed(byteArray: bytes)
    }

    @MainActor private func feedText(_ text: String) {
        view.feed(text: text)
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
            // Already running → forward as a live resize.
            events.continuation.yield(.changeSize(cols: newCols, rows: newRows))
        }
    }

    func setTerminalTitle(source: TerminalView, title: String) {}
    func hostCurrentDirectoryUpdate(source: TerminalView, directory: String?) {}
    func scrolled(source: TerminalView, position: Double) {}
    func requestOpenLink(source: TerminalView, link: String, params: [String: String]) {}
    func bell(source: TerminalView) {}
    func clipboardCopy(source: TerminalView, content: Data) {}
    func iTermContent(source: TerminalView, content: ArraySlice<UInt8>) {}
    func rangeChanged(source: TerminalView, startY: Int, endY: Int) {}
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
    func session(for instanceId: String, hostId: UUID,
                 connection: SSHConnection, command: String) -> TerminalSession {
        if let s = existing(instanceId) { return s }
        let view = TerminalView(frame: .zero)
        let session = TerminalSession(view: view, connection: connection, command: command)
        view.terminalDelegate = session
        entries[instanceId] = Entry(hostId: hostId, session: session)
        session.start()
        return session
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

    func disconnectAll() {
        for e in entries.values { e.session.disconnect() }
        entries.removeAll()
    }
}
