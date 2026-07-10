//! muxel-terminal — embedded terminal sessions for muxel.
//!
//! - [`TerminalSession`] owns a PTY child + the `alacritty_terminal` emulator.
//! - [`TerminalView`] is the gpui entity that renders and drives one session.

// `Arc<TerminalSession>` is shared only on the GPUI main thread (between the view
// and its element); the session is intentionally not `Send + Sync` (the PTY
// master isn't `Sync`). Same trade-off gpui-component makes for its entities.
#![allow(clippy::arc_with_non_send_sync)]

mod colors;
mod element;
mod keymap;
mod links;
mod listener;
mod search;
mod session;
mod view;

pub use colors::TerminalPalette;
pub use links::path_from_file_uri;
pub use session::{CommandSpec, PtyChunk, TerminalSession};
pub use view::{AgentStatus, OpenLink, TerminalLaunch, TerminalMouseMode, TerminalView};
