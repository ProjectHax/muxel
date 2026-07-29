//! `alacritty_terminal` event listener: handles title/bell, queues OSC-52
//! copies, answers color queries, and writes PTY responses (cursor-position
//! reports, query replies) back to the child.

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::term::ClipboardType;
use parking_lot::Mutex;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;

/// Shared, thread-safe handle to the PTY's input side.
pub(crate) type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Forwards terminal events to shared state. Lives inside the `Term`, so its
/// `send_event` runs wherever the VTE processor is advanced (the GPUI thread) —
/// and while the `Term` mutex is held, so it must never call back into the
/// session; it only touches its own shared state and the PTY writer.
pub(crate) struct MuxelListener {
    pub writer: SharedWriter,
    pub title: Arc<Mutex<Option<String>>>,
    /// Monotonic sequence for OSC title changes, including ResetTitle. Consumers
    /// use it to distinguish a repeated title event from an unchanged snapshot.
    pub title_generation: Arc<AtomicU64>,
    pub title_changed_at: Arc<Mutex<Option<Instant>>>,
    /// Latest UUID-shaped OSC title published by the child. Agent UIs may replace
    /// it with a human title immediately or switch sessions in-place.
    pub session_id_hint: Arc<Mutex<Option<String>>>,
    pub bell: Arc<AtomicBool>,
    /// OSC-52 copies from the child, drained by the view onto the system
    /// clipboard (a clipboard write needs a gpui context this thread lacks).
    pub clipboard_store: Arc<Mutex<Vec<(ClipboardType, String)>>>,
}

fn uuid_in_title(title: &str) -> Option<&str> {
    title
        .split(|ch: char| {
            ch.is_ascii_whitespace() || matches!(ch, '|' | ':' | '(' | ')' | '[' | ']')
        })
        .map(str::trim)
        .find(|part| uuid::Uuid::parse_str(part).is_ok())
}

impl MuxelListener {
    fn write_reply(&self, reply: &str) {
        let mut writer = self.writer.lock();
        let _ = writer.write_all(reply.as_bytes());
        let _ = writer.flush();
    }
}

impl EventListener for MuxelListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                let mut hint = self.session_id_hint.lock();
                if let Some(id) = uuid_in_title(&title) {
                    *hint = Some(id.to_string());
                }
                *self.title.lock() = Some(title);
                *self.title_changed_at.lock() = Some(Instant::now());
                self.title_generation.fetch_add(1, Ordering::Relaxed);
            }
            Event::ResetTitle => {
                *self.title.lock() = None;
                *self.title_changed_at.lock() = Some(Instant::now());
                self.title_generation.fetch_add(1, Ordering::Relaxed);
            }
            Event::Bell => self.bell.store(true, Ordering::Relaxed),
            // Apps query the terminal (cursor position, device attributes, …);
            // the reply must go back to the PTY's stdin.
            Event::PtyWrite(text) => self.write_reply(&text),
            // OSC-52 copy: alacritty hands over the already-base64-decoded text;
            // queue it for the view to land on the system clipboard.
            Event::ClipboardStore(ty, text) => self.clipboard_store.lock().push((ty, text)),
            // OSC-52 read: answer with a well-formed EMPTY reply. Returning real
            // clipboard contents would let any program that can write to this
            // PTY's stdout — including a compromised remote over SSH — silently
            // exfiltrate whatever the user last copied (often a password). The
            // empty reply keeps that hardening while TUIs that probe OSC-52
            // support with `52;c;?` (e.g. vim autodetect) get an answer instead
            // of hanging on a timeout.
            Event::ClipboardLoad(_, format) => {
                let reply = format("");
                self.write_reply(&reply);
            }
            // Color queries are answered synchronously on the PTY reader thread.
            // Waiting for this listener means waiting for the GPUI drain task,
            // which is late enough for a TUI to treat the reply as typed text.
            Event::ColorRequest(_, _) => {}
            // `Wakeup` and `ChildExit` are emitted only by alacritty's own
            // EventLoop, which muxel doesn't run: repaints are driven by the
            // view's drain task, and exit (with its code) comes from the PTY
            // reader's EOF. The remaining events (cursor blink, pointer shape,
            // text-area pixel size for CSI 14t) are intentionally unhandled.
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_title_hint_follows_an_in_place_session_switch() {
        let hint = Arc::new(Mutex::new(None));
        let listener = MuxelListener {
            writer: Arc::new(Mutex::new(Box::new(Vec::<u8>::new()))),
            title: Arc::new(Mutex::new(None)),
            title_generation: Arc::new(AtomicU64::new(0)),
            title_changed_at: Arc::new(Mutex::new(None)),
            session_id_hint: hint.clone(),
            bell: Arc::new(AtomicBool::new(false)),
            clipboard_store: Arc::new(Mutex::new(Vec::new())),
        };
        let first = "019f95d7-db31-7db0-904d-9e08330e0000";
        let resumed = "019f95d7-db31-7db0-904d-9e08330e0001";

        listener.send_event(Event::Title(format!("{first} | Starting ⠏")));
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 1);
        assert_eq!(hint.lock().as_deref(), Some(first));
        listener.send_event(Event::Title("Review changes".to_string()));
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 2);
        assert_eq!(hint.lock().as_deref(), Some(first));

        listener.send_event(Event::Title(resumed.to_string()));
        assert_eq!(hint.lock().as_deref(), Some(resumed));
        listener.send_event(Event::ResetTitle);
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 4);
        assert_eq!(*listener.title.lock(), None);
    }
}
