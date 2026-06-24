//! `alacritty_terminal` event listener: handles title/bell and writes PTY
//! responses (e.g. cursor-position reports) back to the child.

use alacritty_terminal::event::{Event, EventListener};
use parking_lot::Mutex;
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

/// Shared, thread-safe handle to the PTY's input side.
pub(crate) type SharedWriter = Arc<Mutex<Box<dyn Write + Send>>>;

/// Forwards terminal events to shared state. Lives inside the `Term`, so its
/// `send_event` runs wherever the VTE processor is advanced (the GPUI thread).
pub(crate) struct MuxelListener {
    pub writer: SharedWriter,
    pub title: Arc<Mutex<Option<String>>>,
    pub bell: Arc<AtomicBool>,
}

impl EventListener for MuxelListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::Title(title) => *self.title.lock() = Some(title),
            Event::ResetTitle => *self.title.lock() = None,
            Event::Bell => self.bell.store(true, Ordering::Relaxed),
            // Apps query the terminal (cursor position, device attributes, …);
            // the reply must go back to the PTY's stdin.
            Event::PtyWrite(text) => {
                let mut writer = self.writer.lock();
                let _ = writer.write_all(text.as_bytes());
                let _ = writer.flush();
            }
            // Wakeup, clipboard, color queries, child-exit, etc. are ignored for now.
            _ => {}
        }
    }
}
