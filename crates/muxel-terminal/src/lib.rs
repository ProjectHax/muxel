//! muxel-terminal — embedded terminal sessions for muxel.
//!
//! - [`TerminalSession`] owns a PTY child + the `alacritty_terminal` emulator.
//! - [`TerminalView`] is the gpui entity that renders and drives one session.

// `Arc<TerminalSession>` is shared only on the GPUI main thread (between the view
// and its element); the session is intentionally not `Send + Sync` (the PTY
// master isn't `Sync`). Same trade-off gpui-component makes for its entities.
#![allow(clippy::arc_with_non_send_sync)]

mod colors;
mod control_input;
mod control_paste;
mod element;
mod keymap;
mod links;
mod listener;
mod pane_job;
mod present_flag;
#[cfg(unix)]
mod process_tree_unix;
mod profile;
mod profile_idle;
pub mod profile_log;
mod program_status;
mod replies;
mod search;
mod session;
mod startup;
mod view;

pub use colors::TerminalPalette;
pub use control_input::{
    ControlInput, ControlWrite, ControlWriteError, TmuxInputRoute, TmuxRunner,
};
pub use links::{FileLinkTarget, file_target_from_uri, file_uri, path_from_file_uri};
pub use pane_job::{HitStatus, MemoryLimitHit};
pub use present_flag::{mark_present_needed, take_present_needed};
pub use profile::startup_event;
pub use program_status::ProgramStatusKind;
pub use session::{
    CommandSpec, LaunchBinding, PTY_OUTPUT_QUEUE_BYTE_CAPACITY, PTY_OUTPUT_QUEUE_CHUNK_CAPACITY,
    PtyChunk, PtyOutputQueueStats, PtyReceiver, TerminalSession, TitleSource,
    literal_program_for_spawn, muse_config_supported,
};
pub use view::{
    AgentStatus, DisplayEpoch, LocalPaste, LocalPasteHandler, OpenLink, OpenLinkBackground,
    PathPasteErrorHandler, PromptSubmission, PromptSubmissionState, TerminalFocusObserver,
    TerminalFocusProfile, TerminalLaunch, TerminalMouseMode, TerminalNotifyCause,
    TerminalRuntimeSnapshot, TerminalStatusCoverage, TerminalStatusReason, TerminalView,
    TextPasteErrorHandler, clean_agent_title, pane_grid, paste_clipboard_into_session,
};
