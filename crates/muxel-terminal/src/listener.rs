//! `alacritty_terminal` event listener: handles title/bell, queues OSC-52
//! copies, and writes the replies only the emulator can give (cursor-position
//! and mode reports) back to the child. Every other query is answered on the
//! PTY reader thread — see `replies`.

use crate::replies::Replies;
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
    pub replies: Arc<Replies>,
    pub title: Arc<Mutex<Option<String>>>,
    /// Monotonic sequence for OSC title changes, including ResetTitle. Consumers
    /// use it to distinguish a repeated title event from an unchanged snapshot.
    pub title_generation: Arc<AtomicU64>,
    pub title_changed_at: Arc<Mutex<Option<Instant>>>,
    /// Latest OSC title whose thread field is a full UUID or Codex's validated
    /// 32-character `thread-id` abbreviation. The app resolves and validates the
    /// provider's full title contract before adopting it.
    pub session_id_hint: Arc<Mutex<Option<String>>>,
    pub bell: Arc<AtomicBool>,
    /// OSC-52 copies from the child, drained by the view onto the system
    /// clipboard (a clipboard write needs a gpui context this thread lacks).
    pub clipboard_store: Arc<Mutex<Vec<(ClipboardType, String)>>>,
}

fn session_thread_field(title: &str) -> Option<&str> {
    let title = title.trim();
    let title = title
        .strip_prefix("[ ! ] Action Required | ")
        .or_else(|| title.strip_prefix("[ . ] Action Required | "))
        .unwrap_or(title);
    let thread = title
        .split_once(" | ")
        .map_or(title, |(thread, _)| thread.trim());
    if uuid::Uuid::parse_str(thread).is_ok() {
        return Some(thread);
    }
    let prefix = thread.strip_suffix("...")?;
    (prefix.len() == 29 && uuid::Uuid::parse_str(&format!("{prefix}0000000")).is_ok())
        .then_some(thread)
}

impl EventListener for MuxelListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => {
                let mut hint = self.session_id_hint.lock();
                if session_thread_field(&title).is_some() {
                    *hint = Some(title.clone());
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
            // the reply must go back to the PTY's stdin. Those the reader thread
            // already answered are dropped here, the rest kept in query order.
            Event::PtyWrite(text) => self.replies.terminal_reply(&text),
            // OSC-52 copy: alacritty hands over the already-base64-decoded text;
            // queue it for the view to land on the system clipboard.
            Event::ClipboardStore(ty, text) => self.clipboard_store.lock().push((ty, text)),
            // OSC-52 reads and color queries are answered synchronously on the
            // PTY reader thread. Waiting for this listener means waiting for the
            // GPUI drain task, which is late enough for a TUI to treat the reply
            // as typed text.
            Event::ClipboardLoad(_, _) | Event::ColorRequest(_, _) => {}
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
    fn renamed_codex_title_retains_the_immutable_resume_hint() {
        let hint = Arc::new(Mutex::new(None));
        let listener = MuxelListener {
            replies: Arc::new(Replies::new(Arc::new(Mutex::new(Box::new(
                Vec::<u8>::new(),
            ))))),
            title: Arc::new(Mutex::new(None)),
            title_generation: Arc::new(AtomicU64::new(0)),
            title_changed_at: Arc::new(Mutex::new(None)),
            session_id_hint: hint.clone(),
            bell: Arc::new(AtomicBool::new(false)),
            clipboard_store: Arc::new(Mutex::new(Vec::new())),
        };
        let first = "019f95d7-db31-7db0-904d-9e08330e0000";
        let resumed = "019f95d7-db31-7db0-904d-9e08330e0001";

        let first_frame = format!("{}... | Starting ⠹", &first[..29]);
        listener.send_event(Event::Title(first_frame.clone()));
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 1);
        assert_eq!(hint.lock().as_deref(), Some(first_frame.as_str()));
        listener.send_event(Event::Title("Renamed conversation | Ready".to_string()));
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 2);
        assert_eq!(hint.lock().as_deref(), Some(first_frame.as_str()));

        listener.send_event(Event::Title(format!("Review {resumed} | Ready ·")));
        assert_eq!(hint.lock().as_deref(), Some(first_frame.as_str()));

        listener.send_event(Event::Title(resumed.to_string()));
        assert_eq!(hint.lock().as_deref(), Some(resumed));
        listener.send_event(Event::ResetTitle);
        assert_eq!(listener.title_generation.load(Ordering::Relaxed), 5);
        assert_eq!(*listener.title.lock(), None);
    }
}
