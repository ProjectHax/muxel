//! Replies to terminal queries, answered on the PTY reader thread.
//!
//! A program that queries the terminal (device attributes, colors, …) waits a
//! bounded time for the answer and then goes back to reading keystrokes, so a
//! reply that arrives late is read as *typing*. tmux, for one, forwards a
//! device-attributes reply that misses its 5s window into the pane, where an
//! agent's prompt shows `0;2501;1c`. alacritty only replies once the GPUI drain
//! has parsed the query: after a background pane's paint throttle, and
//! arbitrarily late when the UI thread stalls (a backgrounded window, every
//! remote pane reconnecting at once after a sleep). So every reply that doesn't
//! read the grid is produced here instead, by a second parser that sees the
//! bytes the moment they are read.

use crate::colors::{TerminalPalette, index_to_rgb};
use crate::listener::SharedWriter;
use alacritty_terminal::term::{Config as TermConfig, Osc52};
use alacritty_terminal::vte::ansi::{Handler, Mode, PrivateMode, Processor, Timeout};
use parking_lot::Mutex;
use std::collections::VecDeque;
use std::io::Write;
use std::sync::Arc;
use std::time::Duration;

/// Primary device attributes, as alacritty answers them: a VT102.
const PRIMARY_DA: &str = "\x1b[?6c";
/// Secondary device attributes, byte-for-byte alacritty's — its crate version
/// (0.25.1 → 2501) sits in the middle.
const SECONDARY_DA: &str = "\x1b[>0;2501;1c";
/// `CSI 5 n` operating status: OK.
const STATUS_OK: &str = "\x1b[0n";

/// The PTY's reply path, shared by the reader thread and alacritty's listener,
/// which keeps replies in the order their queries were sent.
///
/// Programs depend on that order: "query X, then primary DA" is the standard
/// probe, and DA arriving first means "X is unsupported" — X's own reply then
/// lands as typed text. Replies that read the grid (cursor position, mode
/// reports, text-area size) can only come from alacritty, so a reply the reader
/// computes while one of those is still outstanding is held, and goes out right
/// behind it.
pub(crate) struct Replies {
    writer: SharedWriter,
    order: Mutex<ReplyOrder>,
}

#[derive(Default)]
struct ReplyOrder {
    /// Grid-dependent queries the reader has seen.
    awaited: u64,
    /// Grid-dependent replies alacritty has sent.
    answered: u64,
    /// Reader replies waiting on grid replies, each with the `awaited` count
    /// that must be answered before it may go.
    held: VecDeque<(u64, String)>,
}

impl Replies {
    pub(crate) fn new(writer: SharedWriter) -> Self {
        Self {
            writer,
            order: Mutex::new(ReplyOrder::default()),
        }
    }

    /// Reader thread: send `reply` now, or behind the grid replies still owed.
    /// The write happens under the order lock, so it can't overtake a grid reply
    /// the listener is sending at the same moment.
    fn send(&self, reply: String) {
        let mut order = self.order.lock();
        if order.answered >= order.awaited {
            self.write(&reply);
        } else {
            let after = order.awaited;
            order.held.push_back((after, reply));
        }
    }

    /// Reader thread: a query only alacritty can answer has been read.
    fn await_grid_reply(&self) {
        self.order.lock().awaited += 1;
    }

    /// Listener: a reply alacritty generated while parsing. Ones the reader
    /// already sent are dropped; a grid reply goes out, followed by whatever
    /// was held behind it.
    pub(crate) fn terminal_reply(&self, reply: &str) {
        if answered_by_reader(reply) {
            return;
        }
        let mut order = self.order.lock();
        self.write(reply);
        order.answered += 1;
        let answered = order.answered;
        while order
            .held
            .front()
            .is_some_and(|(after, _)| *after <= answered)
        {
            if let Some((_, held)) = order.held.pop_front() {
                self.write(&held);
            }
        }
    }

    fn write(&self, reply: &str) {
        let mut writer = self.writer.lock();
        let _ = writer.write_all(reply.as_bytes());
        let _ = writer.flush();
    }
}

/// Whether an alacritty reply is one the reader thread sends itself: device
/// attributes (`…c`) and the operating status report (`…n`) are the only
/// alacritty replies ending in those bytes.
fn answered_by_reader(reply: &str) -> bool {
    reply.ends_with('c') || reply.ends_with('n')
}

/// Parses PTY output on the reader thread, as it's read, answering every query
/// whose reply doesn't depend on the grid.
pub(crate) struct ImmediateReplies {
    parser: Processor<Unsynchronized>,
    handler: ReplyHandler,
}

impl ImmediateReplies {
    /// `config` must be the one alacritty's `Term` runs with: its gates decide
    /// which queries get a reply at all, and both parsers must agree.
    pub(crate) fn new(
        replies: Arc<Replies>,
        palette: Arc<Mutex<TerminalPalette>>,
        config: &TermConfig,
    ) -> Self {
        Self {
            parser: Processor::new(),
            handler: ReplyHandler {
                replies,
                palette,
                kitty_keyboard: config.kitty_keyboard,
                clipboard_reads: matches!(config.osc52, Osc52::OnlyPaste | Osc52::CopyPaste),
            },
        }
    }

    pub(crate) fn advance(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.handler, bytes);
    }
}

/// Synchronized updates (DECSET 2026) hold back painting, and alacritty's
/// parser buffers everything inside one until it ends. A query in there still
/// wants its answer now, so this parser never buffers.
#[derive(Default)]
struct Unsynchronized;

impl Timeout for Unsynchronized {
    fn set_timeout(&mut self, _: Duration) {}
    fn clear_timeout(&mut self) {}
    fn pending_timeout(&self) -> bool {
        false
    }
}

/// Mirrors alacritty's reply logic query for query: what it can answer without
/// the grid it sends, and the rest it counts as owed so ordering holds.
struct ReplyHandler {
    replies: Arc<Replies>,
    palette: Arc<Mutex<TerminalPalette>>,
    kitty_keyboard: bool,
    clipboard_reads: bool,
}

impl Handler for ReplyHandler {
    fn identify_terminal(&mut self, intermediate: Option<char>) {
        match intermediate {
            None => self.replies.send(PRIMARY_DA.to_string()),
            Some('>') => self.replies.send(SECONDARY_DA.to_string()),
            _ => {}
        }
    }

    fn device_status(&mut self, arg: usize) {
        match arg {
            5 => self.replies.send(STATUS_OK.to_string()),
            // Cursor position report.
            6 => self.replies.await_grid_reply(),
            _ => {}
        }
    }

    fn report_mode(&mut self, _: Mode) {
        self.replies.await_grid_reply();
    }

    fn report_private_mode(&mut self, _: PrivateMode) {
        self.replies.await_grid_reply();
    }

    fn text_area_size_chars(&mut self) {
        self.replies.await_grid_reply();
    }

    fn report_keyboard_mode(&mut self) {
        if self.kitty_keyboard {
            self.replies.await_grid_reply();
        }
    }

    /// OSC 4;n / 10 / 11 / 12 color queries, answered from the active theme's
    /// palette so TUIs detect dark/light mode correctly. The renderer paints from
    /// this same palette (runtime `set_color` overrides are not consulted), so
    /// the answer reports exactly what's on screen.
    fn dynamic_color_sequence(&mut self, prefix: String, index: usize, terminator: &str) {
        let Some(rgb) = index_to_rgb(&self.palette.lock(), index) else {
            return;
        };
        self.replies.send(format!(
            "\x1b]{prefix};rgb:{r:02x}{r:02x}/{g:02x}{g:02x}/{b:02x}{b:02x}{terminator}",
            r = rgb.r,
            g = rgb.g,
            b = rgb.b,
        ));
    }

    /// OSC-52 read: answer with a well-formed EMPTY reply. Returning real
    /// clipboard contents would let any program that can write to this PTY's
    /// stdout — including a compromised remote over SSH — silently exfiltrate
    /// whatever the user last copied (often a password). The empty reply keeps
    /// that hardening while TUIs that probe OSC-52 support with `52;c;?` (e.g.
    /// vim autodetect) get an answer instead of hanging on a timeout.
    fn clipboard_load(&mut self, clipboard: u8, terminator: &str) {
        if self.clipboard_reads && matches!(clipboard, b'c' | b'p' | b's') {
            self.replies
                .send(format!("\x1b]52;{};{terminator}", clipboard as char));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{ImmediateReplies, Replies, SECONDARY_DA};
    use crate::colors::{TerminalPalette, index_to_rgb};
    use crate::listener::MuxelListener;
    use crate::session::term_config;
    use alacritty_terminal::event::{Event, EventListener};
    use alacritty_terminal::term::Term;
    use alacritty_terminal::term::test::TermSize;
    use alacritty_terminal::vte::ansi::Processor;
    use parking_lot::Mutex;
    use std::io::Write;
    use std::sync::Arc;

    /// A PTY writer that records everything sent to the child.
    #[derive(Clone, Default)]
    struct Sent(Arc<Mutex<Vec<u8>>>);

    impl Write for Sent {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl Sent {
        fn take(&self) -> String {
            String::from_utf8(std::mem::take(&mut *self.0.lock())).unwrap()
        }
    }

    fn palette() -> TerminalPalette {
        TerminalPalette {
            background: 0x112233,
            ..Default::default()
        }
    }

    /// The reader-thread parser plus the listener alacritty reports into, both
    /// writing to one recorded PTY — the production reply path.
    struct Pipeline {
        reader: ImmediateReplies,
        term: Term<MuxelListener>,
        parser: Processor,
        sent: Sent,
    }

    impl Pipeline {
        fn new() -> Self {
            let sent = Sent::default();
            let replies = Arc::new(Replies::new(Arc::new(Mutex::new(Box::new(sent.clone())))));
            let listener = MuxelListener {
                replies: replies.clone(),
                title: Default::default(),
                title_generation: Default::default(),
                title_changed_at: Default::default(),
                session_id_hint: Default::default(),
                bell: Default::default(),
                clipboard_store: Default::default(),
            };
            let reader =
                ImmediateReplies::new(replies, Arc::new(Mutex::new(palette())), &term_config());
            let term = Term::new(term_config(), &TermSize::new(80, 24), listener);
            Self {
                reader,
                term,
                parser: Processor::new(),
                sent,
            }
        }

        /// What the child has received after the reader thread saw `bytes`, but
        /// before the UI drained them.
        fn read(&mut self, bytes: &[u8]) -> String {
            self.reader.advance(bytes);
            self.sent.take()
        }

        /// What the child has received after the UI drain parsed `bytes`.
        fn drain(&mut self, bytes: &[u8]) -> String {
            self.parser.advance(&mut self.term, bytes);
            self.sent.take()
        }
    }

    /// Everything alacritty would reply on its own, formatted the way muxel
    /// always answered each event, in the order it raised them.
    struct Reference(Arc<Mutex<String>>);

    impl EventListener for Reference {
        fn send_event(&self, event: Event) {
            let reply = match event {
                Event::PtyWrite(text) => text,
                Event::ColorRequest(index, format) => match index_to_rgb(&palette(), index) {
                    Some(rgb) => format(rgb),
                    None => return,
                },
                Event::ClipboardLoad(_, format) => format(""),
                _ => return,
            };
            self.0.lock().push_str(&reply);
        }
    }

    fn reference_replies(bytes: &[u8]) -> String {
        let replies = Arc::new(Mutex::new(String::new()));
        let mut term = Term::new(
            term_config(),
            &TermSize::new(80, 24),
            Reference(replies.clone()),
        );
        let mut parser: Processor = Processor::new();
        parser.advance(&mut term, bytes);
        std::mem::take(&mut *replies.lock())
    }

    #[test]
    fn device_attributes_are_answered_before_the_ui_drains() {
        let mut pipeline = Pipeline::new();
        assert_eq!(pipeline.read(b"\x1b[c"), "\x1b[?6c");
        assert_eq!(pipeline.read(b"\x1b[>c"), SECONDARY_DA);
        assert_eq!(pipeline.read(b"\x1b[5n"), "\x1b[0n");
        // alacritty's own copies, once the drain reaches them, are dropped.
        assert_eq!(pipeline.drain(b"\x1b[c\x1b[>c\x1b[5n"), "");
    }

    #[test]
    fn a_query_split_across_reads_is_answered_when_it_completes() {
        let mut pipeline = Pipeline::new();
        assert_eq!(pipeline.read(b"\x1b[>"), "");
        assert_eq!(pipeline.read(b"c"), SECONDARY_DA);
        assert_eq!(pipeline.read(b"\x1b]11;"), "");
        assert_eq!(pipeline.read(b"?\x07"), "\x1b]11;rgb:1111/2222/3333\x07");
    }

    #[test]
    fn indexed_color_query_keeps_its_index_and_string_terminator() {
        let mut pipeline = Pipeline::new();
        assert_eq!(
            pipeline.read(b"\x1b]4;1;?\x1b\\"),
            "\x1b]4;1;rgb:f3f3/8b8b/a8a8\x1b\\"
        );
    }

    #[test]
    fn ordinary_osc_title_does_not_write_to_the_pty() {
        let mut pipeline = Pipeline::new();
        assert_eq!(pipeline.read(b"\x1b]0;Review changes\x07"), "");
    }

    /// "Query X, then DA1": if DA1 overtook the cursor report, the program would
    /// conclude X is unsupported and then read the late report as typing.
    #[test]
    fn a_reply_queued_behind_a_grid_reply_waits_its_turn() {
        let mut pipeline = Pipeline::new();
        let probe = b"\x1b[6n\x1b[c";
        assert_eq!(pipeline.read(probe), "");
        assert_eq!(pipeline.drain(probe), "\x1b[1;1R\x1b[?6c");
        // With nothing owed any more, the next query is answered at once again.
        assert_eq!(pipeline.read(b"\x1b[c"), "\x1b[?6c");
    }

    /// The reader and alacritty must agree on exactly which queries get a reply
    /// (an alacritty upgrade can change that, or the DA2 version), and together
    /// they must send what alacritty alone would — same bytes, same order.
    #[test]
    fn replies_match_alacritty_byte_for_byte() {
        let stream: &[u8] = concat!(
            "\x1b[c\x1b[0c\x1bZ\x1b[1c\x1b[=c\x1b[>c\x1b[>0c\x1b[>q",
            "\x1b[5n\x1b[6n\x1b[7n\x1b[c",
            "\x1b[?2026$p\x1b]11;?\x07\x1b[4$p\x1b[>c",
            "\x1b[18t\x1b[14t\x1b[?u\x1b[c",
            "\x1b]10;?\x1b\\\x1b]4;1;?;2;?\x07\x1b]52;c;?\x07\x1b]52;x;?\x07",
            "\x1b[?2026h\x1b[3;5H\x1b[6n\x1b[>c\x1b[?2026l\x1b[5n",
        )
        .as_bytes();
        let mut pipeline = Pipeline::new();
        let mut sent = String::new();
        for byte in stream {
            sent.push_str(&pipeline.read(std::slice::from_ref(byte)));
        }
        sent.push_str(&pipeline.drain(stream));
        assert_eq!(sent, reference_replies(stream));
    }
}
