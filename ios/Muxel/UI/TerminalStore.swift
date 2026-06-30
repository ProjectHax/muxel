import Foundation
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
final class TerminalSession: NSObject, TerminalViewDelegate {
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

    enum Event {
        case send(ByteBuffer)
        case changeSize(cols: Int, rows: Int)
    }

    init(view: TerminalView, connection: SSHConnection, command: String) {
        self.view = view
        self.connection = connection
        self.command = command
        super.init()
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
            // Aggregate all visible rows into one GPU buffer per frame: smoother for
            // the full-screen TUI agents (claude) that repaint most of the screen each
            // frame, vs the per-row default tuned for few-rows-change interactive use.
            source.metalBufferingMode = .perFrameAggregated
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
