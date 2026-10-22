//! A single terminal session: a PTY child process plus the `alacritty_terminal`
//! emulator state that interprets its output.
//!
//! Threading model: a dedicated OS thread blocks reading the PTY and ships
//! bytes over an async channel. The GPUI thread drains that channel and feeds
//! the bytes through the VTE `Processor` into the `Term` (see `process_output`),
//! so the `Term` is only ever touched from the GPUI thread.

use crate::colors::TerminalPalette;
use crate::listener::{EvidenceRead, LifecycleEvidence, MuxelListener, SharedWriter};
use crate::profile;
use crate::replies::{ImmediateReplies, Replies};
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{
    ClipboardType, Config as TermConfig, Osc52, Term, TermDamage, TermMode,
};
use alacritty_terminal::vte::ansi::Processor;
use anyhow::{Context as _, Result};
use muxel_core::path_paste::{PastePathError, PasteShell};
use parking_lot::{Condvar, Mutex};
#[cfg(unix)]
use portable_pty::Child;
use portable_pty::{ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};
use uuid::Uuid;

type SharedChildKiller = Arc<Mutex<Box<dyn ChildKiller + Send + Sync>>>;

/// Identity and timestamp attached to one PTY read while profiling is enabled.
#[derive(Clone, Copy, Debug)]
pub(crate) struct PtyReadTiming {
    pub(crate) sequence: u64,
    pub(crate) read_at_us: u64,
}

/// An item produced by the PTY reader thread.
pub enum PtyChunk {
    /// Raw bytes read from the PTY.
    Output(Vec<u8>),
    /// The child exited (or the PTY closed).
    Exit {
        /// Exit code, when the OS reported one within the harvest window.
        ///
        /// Beware: `portable_pty` reports a *signalled* child as `code = 1`
        /// (`ExitStatus::code()` is `None` for a signal, and it falls back to
        /// 1), so this alone cannot distinguish `exit(1)` from "killed". Read
        /// it together with `signal`.
        code: Option<i32>,
        /// The signal name that killed the child (`"Hangup"`, `"Killed"`, …),
        /// when it died on one. This is the field that says *who* ended the
        /// process: a `Hangup` means its PTY master closed, while `Killed`
        /// means something sent SIGKILL.
        signal: Option<String>,
        /// Set when the session ended on a PTY read *error* rather than a clean
        /// EOF — the child may not have exited at all (kept for diagnostics).
        read_error: Option<String>,
    },
}

/// Maximum terminal output retained between the PTY reader and its consumer.
///
/// This matches the UI drain budget; a drain can exceed it by one read. Once
/// a pane reaches the limit, its reader blocks and lets PTY backpressure reach
/// the child.
pub const PTY_OUTPUT_QUEUE_BYTE_CAPACITY: usize = 256 * 1024;

/// Maximum number of output/exit envelopes retained for one terminal.
pub const PTY_OUTPUT_QUEUE_CHUNK_CAPACITY: usize = 64;

const PTY_READ_BUFFER_BYTES: usize = 64 * 1024;
const _: () = assert!(PTY_READ_BUFFER_BYTES <= PTY_OUTPUT_QUEUE_BYTE_CAPACITY);

/// A point-in-time view of one terminal's output queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct PtyOutputQueueStats {
    /// Output bytes admitted by the reader but not yet received by the consumer.
    pub queued_bytes: usize,
    /// Output or exit envelopes admitted but not yet received.
    pub queued_chunks: usize,
    /// Largest observed value of [`Self::queued_bytes`].
    pub high_water_bytes: usize,
    /// Largest observed value of [`Self::queued_chunks`].
    pub high_water_chunks: usize,
}

/// Largest occupancy a queue reached since the profiler last sampled it.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct OutputQueuePeak {
    pub bytes: usize,
    pub chunks: usize,
}

#[derive(Default)]
struct OutputQueueCounters {
    queued_bytes: usize,
    queued_chunks: usize,
    high_water_bytes: usize,
    high_water_chunks: usize,
    /// Peak occupancy since the last [`OutputQueueState::take_profile_peak`],
    /// recorded at admission so a prompt receive cannot hide it.
    profile_peak: OutputQueuePeak,
    receiver_closed: bool,
    /// The owning session was dropped: the reader must never wait for space
    /// again (see [`OutputQueueState::close_session`]).
    session_closed: bool,
    /// The Unix reader's poller, notified when the receiver or the session
    /// closes to wake a reader parked in its wait while the PTY is silent.
    #[cfg(unix)]
    reader_wake: Option<Arc<polling::Poller>>,
}

/// Wake the Unix reader once its stream has closed (the caller has just marked
/// it so, under the same lock, so the woken reader sees it).
///
/// This is a poller notification rather than closing a pipe on purpose. macOS
/// has no `pipe2`, so a pipe is created first and only then marked
/// close-on-exec, and a process another thread spawns in between inherits it.
/// muxel spawns from many threads (git, ssh, other panes' shells), and a child
/// holding the write end would keep this reader asleep after its pane closed,
/// until that unrelated child exited. A notification is no descriptor at all on
/// macOS (kqueue's `EVFILT_USER`) and an eventfd created close-on-exec in one
/// step on Linux.
#[cfg(unix)]
fn wake_reader(counters: &mut OutputQueueCounters) {
    if let Some(poller) = counters.reader_wake.take() {
        let _ = poller.notify();
    }
}

struct OutputQueueState {
    counters: Mutex<OutputQueueCounters>,
    space_available: Condvar,
    byte_capacity: usize,
    chunk_capacity: usize,
    /// Kept here because the session, its receiver and its reader already
    /// share this state for teardown.
    #[cfg(unix)]
    child_hangup: ChildHangup,
    #[cfg(all(test, unix))]
    force_child_exit: AtomicBool,
}

/// Muxel's one SIGHUP to a Unix child. [`TerminalSession::kill`], the
/// session's Drop and the reader (once its stream ends) each ask for it; the
/// first sends it and the others skip it. A program still saving after a
/// SIGHUP may treat a second one as an order to quit at once, or may have
/// restored the default action while it saves, so a repeat would cut that
/// work short. The reader's SIGKILL deadline counts from the claim, which is
/// stamped just before that SIGHUP is sent.
#[cfg(unix)]
#[derive(Default)]
struct ChildHangup {
    sent_at: Mutex<Option<Instant>>,
}

#[cfg(unix)]
impl ChildHangup {
    /// True only for the first caller, which must then send the SIGHUP.
    fn claim(&self) -> bool {
        let mut sent_at = self.sent_at.lock();
        if sent_at.is_some() {
            return false;
        }
        *sent_at = Some(Instant::now());
        true
    }

    fn sent_at(&self) -> Option<Instant> {
        *self.sent_at.lock()
    }
}

impl OutputQueueState {
    fn has_room(&self, counters: &OutputQueueCounters, bytes: usize) -> bool {
        let bytes_fit = if bytes > self.byte_capacity {
            // An oversized read enters only an empty queue.
            counters.queued_bytes == 0
        } else {
            counters.queued_bytes + bytes <= self.byte_capacity
        };
        bytes_fit && counters.queued_chunks < self.chunk_capacity
    }

    /// Wait for room, then account for `bytes`. False when the receiver is
    /// gone, or when the session is gone and the queue is full: after the
    /// session drops, the stream ends rather than wait on a consumer that may
    /// never drain while the platform PTY closes.
    fn admit(&self, bytes: usize) -> bool {
        let mut counters = self.counters.lock();
        loop {
            if counters.receiver_closed {
                return false;
            }
            if self.has_room(&counters, bytes) {
                break;
            }
            if counters.session_closed {
                return false;
            }
            self.space_available.wait(&mut counters);
        }
        counters.queued_bytes += bytes;
        counters.queued_chunks += 1;
        counters.high_water_bytes = counters.high_water_bytes.max(counters.queued_bytes);
        counters.high_water_chunks = counters.high_water_chunks.max(counters.queued_chunks);
        counters.profile_peak.bytes = counters.profile_peak.bytes.max(counters.queued_bytes);
        counters.profile_peak.chunks = counters.profile_peak.chunks.max(counters.queued_chunks);
        true
    }

    fn release(&self, bytes: usize) {
        let mut counters = self.counters.lock();
        if counters.receiver_closed {
            return;
        }
        counters.queued_bytes = counters
            .queued_bytes
            .checked_sub(bytes)
            .expect("released more PTY output bytes than the queue retained");
        counters.queued_chunks = counters
            .queued_chunks
            .checked_sub(1)
            .expect("released an event from an empty PTY output queue");
        self.space_available.notify_one();
    }

    fn close_receiver(&self) {
        let mut counters = self.counters.lock();
        counters.receiver_closed = true;
        counters.queued_bytes = 0;
        counters.queued_chunks = 0;
        #[cfg(unix)]
        wake_reader(&mut counters);
        self.space_available.notify_all();
    }

    /// Called when the owning session drops, before it closes the platform
    /// PTY. Before Windows 11 24H2, `ClosePseudoConsole` waits until its
    /// pending output is drained, so a reader parked here on a receiver that
    /// outlives the session (and may never drain) would stall the caller.
    /// Waking it lets the reader end the stream and drain the PTY itself.
    /// On Unix it also wakes a reader parked in `poll`: a descendant that
    /// ignores SIGHUP can hold the PTY open and silent, so no EOF would come.
    fn close_session(&self) {
        let mut counters = self.counters.lock();
        counters.session_closed = true;
        #[cfg(unix)]
        wake_reader(&mut counters);
        self.space_available.notify_all();
    }

    #[cfg(all(test, unix))]
    fn force_child_exit_for_test(&self) {
        self.force_child_exit.store(true, Ordering::Release);
        self.close_session();
    }

    /// Hand the queue the reader's poller, to notify when the stream closes. A
    /// stream already closed is notified at once, so the reader's first wait
    /// returns.
    #[cfg(unix)]
    fn install_reader_wake(&self, poller: Arc<polling::Poller>) {
        let mut counters = self.counters.lock();
        if counters.receiver_closed || counters.session_closed {
            let _ = poller.notify();
        } else {
            counters.reader_wake = Some(poller);
        }
    }

    /// The reader has stopped waiting, so its poller has no further use; a
    /// receiver kept after the stream ends must not keep it alive.
    #[cfg(unix)]
    fn release_reader_wake(&self) {
        self.counters.lock().reader_wake = None;
    }

    /// Whether the receiver or the session has gone, ending the stream.
    #[cfg(unix)]
    fn is_closed(&self) -> bool {
        let counters = self.counters.lock();
        counters.receiver_closed || counters.session_closed
    }

    fn take_profile_peak(&self) -> OutputQueuePeak {
        let mut counters = self.counters.lock();
        let current = OutputQueuePeak {
            bytes: counters.queued_bytes,
            chunks: counters.queued_chunks,
        };
        std::mem::replace(&mut counters.profile_peak, current)
    }

    fn stats(&self) -> PtyOutputQueueStats {
        let counters = self.counters.lock();
        PtyOutputQueueStats {
            queued_bytes: counters.queued_bytes,
            queued_chunks: counters.queued_chunks,
            high_water_bytes: counters.high_water_bytes,
            high_water_chunks: counters.high_water_chunks,
        }
    }
}

/// Resolve the monitor-thread spawn result. A failed spawn must release a
/// reader blocked on queue admission before closing the Windows pseudoconsole.
#[cfg(windows)]
fn monitor_spawn_result<T>(
    result: std::io::Result<T>,
    output_queue: &OutputQueueState,
    cleanup: impl FnOnce(),
) -> std::io::Result<T> {
    match result {
        Ok(handle) => Ok(handle),
        Err(error) => {
            output_queue.close_session();
            cleanup();
            Err(error)
        }
    }
}

pub(crate) trait OutputQueueItem {
    fn output_bytes(&self) -> usize;
}

impl OutputQueueItem for PtyChunk {
    fn output_bytes(&self) -> usize {
        match self {
            Self::Output(bytes) => bytes.len(),
            Self::Exit { .. } => 0,
        }
    }
}

struct OutputQueueSender<T> {
    tx: async_channel::Sender<T>,
    state: Arc<OutputQueueState>,
}

impl<T: OutputQueueItem> OutputQueueSender<T> {
    fn send_blocking(&self, item: T) -> bool {
        let bytes = item.output_bytes();
        if !self.state.admit(bytes) {
            return false;
        }
        if self.tx.send_blocking(item).is_err() {
            self.state.release(bytes);
            return false;
        }
        true
    }
}

struct OutputQueueReceiverInner<T> {
    rx: async_channel::Receiver<T>,
    state: Arc<OutputQueueState>,
}

impl<T> Drop for OutputQueueReceiverInner<T> {
    fn drop(&mut self) {
        self.rx.close();
        self.state.close_receiver();
    }
}

pub(crate) struct OutputQueueReceiver<T> {
    inner: Arc<OutputQueueReceiverInner<T>>,
}

impl<T> Clone for OutputQueueReceiver<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: OutputQueueItem> OutputQueueReceiver<T> {
    pub(crate) async fn recv(&self) -> Result<T, async_channel::RecvError> {
        let item = self.inner.rx.recv().await?;
        self.inner.state.release(item.output_bytes());
        Ok(item)
    }

    pub(crate) fn recv_blocking(&self) -> Result<T, async_channel::RecvError> {
        let item = self.inner.rx.recv_blocking()?;
        self.inner.state.release(item.output_bytes());
        Ok(item)
    }

    pub(crate) fn try_recv(&self) -> Result<T, async_channel::TryRecvError> {
        let item = self.inner.rx.try_recv()?;
        self.inner.state.release(item.output_bytes());
        Ok(item)
    }

    pub(crate) fn stats(&self) -> PtyOutputQueueStats {
        self.inner.state.stats()
    }

    /// Peak occupancy since the previous call, for the opt-in profiler.
    pub(crate) fn take_profile_peak(&self) -> OutputQueuePeak {
        self.inner.state.take_profile_peak()
    }
}

fn output_queue<T>(
    byte_capacity: usize,
    chunk_capacity: usize,
) -> (OutputQueueSender<T>, OutputQueueReceiver<T>) {
    assert!(byte_capacity > 0);
    assert!(chunk_capacity > 0);
    let (tx, rx) = async_channel::unbounded();
    let state = Arc::new(OutputQueueState {
        counters: Mutex::new(OutputQueueCounters::default()),
        space_available: Condvar::new(),
        byte_capacity,
        chunk_capacity,
        #[cfg(unix)]
        child_hangup: ChildHangup::default(),
        #[cfg(all(test, unix))]
        force_child_exit: AtomicBool::new(false),
    });
    (
        OutputQueueSender {
            tx,
            state: state.clone(),
        },
        OutputQueueReceiver {
            inner: Arc::new(OutputQueueReceiverInner { rx, state }),
        },
    )
}

/// Receiver for a terminal's bounded output stream.
///
/// Clones compete for events from the same queue; they do not broadcast them.
/// Dropping the last clone ends delivery and wakes queue admission. Unix also
/// wakes a blocked reader; Windows can remain blocked until output or PTY close.
///
/// While the session lives and a receiver keeps draining, every terminal byte
/// arrives, in order, with Exit last. Only teardown loses output: dropping the
/// last receiver ends delivery even while the session lives, and dropping the
/// session ends it as described below.
///
/// A receiver may outlive its [`TerminalSession`]. Events already queued stay
/// receivable, but once the session is gone the reader never waits: not for
/// queue space, and on Unix not for more PTY output. On Unix it stops reading
/// at once, discarding output it has not read even when the queue has room;
/// on Windows it reads until the pseudoconsole closes, delivering while the
/// queue has room. Output that does not fit ends the stream without Exit;
/// otherwise Exit follows if the queue has room for it. Either way the
/// receiver then reports `Closed`.
///
/// On Unix muxel sends the child one SIGHUP, from [`TerminalSession::kill`],
/// the session's Drop or the reader once teardown stops it, whichever comes
/// first. If the reader was still reading when teardown stopped it, it
/// SIGKILLs a child still running five seconds (`CHILD_HANGUP_GRACE`) after
/// that SIGHUP was claimed; the stream does not wait for that. A child that
/// had already closed its PTY handles, so the reader saw EOF or a read error
/// first, is not force-killed, as before. Muxel cannot promise the child those
/// five seconds: the kernel sends its own hangup when the last PTY master
/// closes, which is outside muxel's control.
pub struct PtyReceiver {
    inner: OutputQueueReceiver<PtyChunk>,
}

impl Clone for PtyReceiver {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl std::fmt::Debug for PtyReceiver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PtyReceiver")
            .field("stats", &self.stats())
            .finish_non_exhaustive()
    }
}

impl PtyReceiver {
    /// Wait asynchronously for the next output or exit event.
    pub async fn recv(&self) -> Result<PtyChunk, async_channel::RecvError> {
        self.inner.recv().await
    }

    /// Wait synchronously for the next output or exit event.
    pub fn recv_blocking(&self) -> Result<PtyChunk, async_channel::RecvError> {
        self.inner.recv_blocking()
    }

    /// Receive the next event immediately, if one is ready.
    pub fn try_recv(&self) -> Result<PtyChunk, async_channel::TryRecvError> {
        self.inner.try_recv()
    }

    /// Return current and high-water queue occupancy.
    pub fn stats(&self) -> PtyOutputQueueStats {
        self.inner.stats()
    }
}

/// Internal reader envelope. It preserves the exact public [`PtyChunk`] shape
/// while carrying when each read arrived, and opt-in profiling timing, from the
/// PTY reader to the GPUI drain path.
pub(crate) enum ProfiledPtyChunk {
    Output {
        bytes: Vec<u8>,
        timing: Option<PtyReadTiming>,
        /// When the reader received these bytes. Lifecycle evidence they carry
        /// is stamped with this, not with when the drain parses them.
        read_at: SystemTime,
    },
    Exit {
        code: Option<i32>,
        signal: Option<String>,
        read_error: Option<String>,
    },
}

/// PTY output drained for one parse, with when the reader received each chunk.
#[derive(Default)]
pub(crate) struct PtyOutput {
    bytes: Vec<u8>,
    /// For each chunk in order: where it ends in `bytes`, and when it was read.
    reads: Vec<(usize, SystemTime)>,
}

impl PtyOutput {
    pub(crate) fn push(&mut self, bytes: &[u8], read_at: SystemTime) {
        self.bytes.extend_from_slice(bytes);
        self.reads.push((self.bytes.len(), read_at));
    }

    pub(crate) fn len(&self) -> usize {
        self.bytes.len()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    fn chunks(&self) -> impl Iterator<Item = (&[u8], SystemTime)> {
        let mut start = 0;
        self.reads.iter().map(move |&(end, read_at)| {
            let chunk = &self.bytes[start..end];
            start = end;
            (chunk, read_at)
        })
    }
}

impl OutputQueueItem for ProfiledPtyChunk {
    fn output_bytes(&self) -> usize {
        match self {
            Self::Output { bytes, .. } => bytes.len(),
            Self::Exit { .. } => 0,
        }
    }
}

pub(crate) type ProfiledPtyReceiver = OutputQueueReceiver<ProfiledPtyChunk>;

enum PtyChunkSender {
    Public(OutputQueueSender<PtyChunk>),
    Profiled(OutputQueueSender<ProfiledPtyChunk>),
}

impl PtyChunkSender {
    fn queue_state(&self) -> Arc<OutputQueueState> {
        match self {
            Self::Public(tx) => tx.state.clone(),
            Self::Profiled(tx) => tx.state.clone(),
        }
    }

    fn carries_timing(&self) -> bool {
        matches!(self, Self::Profiled(_))
    }

    fn send_output(
        &self,
        bytes: Vec<u8>,
        timing: Option<PtyReadTiming>,
        read_at: SystemTime,
    ) -> bool {
        match self {
            Self::Public(tx) => tx.send_blocking(PtyChunk::Output(bytes)),
            Self::Profiled(tx) => tx.send_blocking(ProfiledPtyChunk::Output {
                bytes,
                timing,
                read_at,
            }),
        }
    }

    fn send_exit(&self, code: Option<i32>, signal: Option<String>, read_error: Option<String>) {
        match self {
            Self::Public(tx) => {
                tx.send_blocking(PtyChunk::Exit {
                    code,
                    signal,
                    read_error,
                });
            }
            Self::Profiled(tx) => {
                tx.send_blocking(ProfiledPtyChunk::Exit {
                    code,
                    signal,
                    read_error,
                });
            }
        }
    }
}

/// What muxel knows at spawn about the provider conversation a launch opens.
/// A scheduled loop owns the conversation its first submitted turn went to
/// only when that conversation was known before the turn (see
/// [`TerminalSession::first_submission_binding`]).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum LaunchBinding {
    /// The provider names its conversation once it runs, as a fresh Codex
    /// launch mints its own thread and publishes it in its title. The first
    /// UUID title begins the launch's first binding.
    #[default]
    Unknown,
    /// The launch opens a known conversation: one muxel pinned with a session
    /// flag (`--session-id`, `--resume`; its id here), or the one a program
    /// that reports no id of its own is in. The launch begins in that binding,
    /// and its first UUID title names it unless it names another pinned id.
    Known(Option<String>),
}

/// Reads a pane's title from outside its PTY. A tmux client forwards none of
/// the titles its pane's program sets unless `set-titles` is on, which muxel
/// leaves to the user, so a tmux-wrapped pane's title is read through tmux.
#[derive(Clone)]
pub struct TitleSource(Arc<dyn Fn() -> Option<String> + Send + Sync>);

impl TitleSource {
    /// `read` blocks (it may run a process); it is only ever called from a
    /// background thread, and must bound its own time.
    pub fn new(read: impl Fn() -> Option<String> + Send + Sync + 'static) -> Self {
        Self(Arc::new(read))
    }

    pub fn read(&self) -> Option<String> {
        (self.0)()
    }
}

impl std::fmt::Debug for TitleSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TitleSource(..)")
    }
}

/// What to run in a terminal.
#[derive(Clone, Debug)]
pub struct CommandSpec {
    pub program: String,
    /// Syntax for file arguments at launch, retained across transport wrappers.
    pub paste_shell: PasteShell,
    /// Already resolved literal target. Never repeat PATH/PATHEXT lookup or use
    /// a batch interpreter at the final spawn boundary.
    pub literal_argv: bool,
    /// False for outside-created panes: a failed approved program must surface
    /// its launch error instead of installing a writable default shell.
    pub allow_shell_fallback: bool,
    /// Logical agent program when `program` is a transport/wrapper such as ssh
    /// or tmux. Lifecycle title parsing keys off this, not the local child name.
    pub status_program: Option<String>,
    pub args: Vec<String>,
    pub cwd: Option<String>,
    pub env: Vec<(String, String)>,
    /// Variables the child must not have, whether muxel's own environment or
    /// `env` has them.
    pub env_remove: Vec<String>,
    /// Text to type into the terminal shortly after start (system-prompt
    /// "type-in" injection). Handled by the view, not the session.
    pub startup_input: Option<String>,
    /// Runtime lease for the first task created through outside control.
    pub startup_turn: Option<muxel_core::control::StartupTurnOwner>,
    /// Where that task's startup reads the provider's title when the PTY can't
    /// see it (a tmux-wrapped pane). Unused by any other automation.
    pub title_source: Option<TitleSource>,
    /// Shift+Tab presses to send at startup before typing (runner auto mode).
    pub auto_mode_presses: u8,
    /// Press Enter to submit after typing `startup_input` (false = leave it in
    /// the input unsubmitted).
    pub submit: bool,
    /// On-screen strings that mean the agent is actively working (e.g. its
    /// spinner footer). Empty → fall back to the output-activity heuristic.
    pub working_markers: Vec<String>,
    /// On-screen strings that mean the agent is blocked on the user (e.g. a
    /// permission/approval prompt). Empty → no marker-based blocked detection.
    pub blocked_markers: Vec<String>,
    /// Fixed delay (ms) after the agent's first output before startup automation
    /// types into it. 0 = auto (wait for output to go quiet instead).
    pub startup_delay_ms: u32,
    /// The provider conversation this launch opens, if muxel knows it.
    pub binding: LaunchBinding,
    /// Whether the command actually resumes an existing provider conversation.
    pub launch_resumed: bool,
}

impl CommandSpec {
    /// Run the user's default shell. On Windows that's PowerShell (`cmd.exe` is
    /// available as a separate preset); elsewhere it's `$SHELL`.
    pub fn shell() -> Self {
        #[cfg(windows)]
        let program = "powershell.exe".to_string();
        #[cfg(not(windows))]
        let program = std::env::var("SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
        Self {
            paste_shell: PasteShell::for_program(Some(&program), PasteShell::default()),
            program,
            literal_argv: false,
            status_program: None,
            allow_shell_fallback: true,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
            startup_input: None,
            auto_mode_presses: 0,
            startup_turn: None,
            title_source: None,
            submit: true,
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            binding: LaunchBinding::Unknown,
            launch_resumed: false,
        }
    }

    /// Run an arbitrary program with arguments.
    pub fn program(program: impl Into<String>, args: Vec<String>) -> Self {
        let program = program.into();
        Self {
            paste_shell: PasteShell::for_program(Some(&program), PasteShell::default()),
            status_program: Some(program.clone()),
            program,
            literal_argv: false,
            args,
            allow_shell_fallback: true,
            cwd: None,
            env: Vec::new(),
            env_remove: Vec::new(),
            startup_input: None,
            auto_mode_presses: 0,
            startup_turn: None,
            title_source: None,
            submit: true,
            working_markers: Vec::new(),
            blocked_markers: Vec::new(),
            startup_delay_ms: 0,
            binding: LaunchBinding::Unknown,
            launch_resumed: false,
        }
    }

    pub fn literal(program: impl Into<String>, args: Vec<String>) -> Self {
        Self {
            literal_argv: true,
            submit: false,
            allow_shell_fallback: false,
            ..Self::program(program, args)
        }
    }

    fn program_for_spawn(&self) -> Result<String> {
        if self.literal_argv {
            validate_literal_program(&self.program)?;
            Ok(self.program.clone())
        } else {
            Ok(resolve_program_for_spawn(&self.program))
        }
    }

    /// Set the working/blocked status markers.
    pub fn with_markers(mut self, working: Vec<String>, blocked: Vec<String>) -> Self {
        self.working_markers = working;
        self.blocked_markers = blocked;
        self
    }

    pub fn with_status_program(mut self, program: Option<String>) -> Self {
        self.status_program = program;
        self
    }

    pub fn with_paste_shell(mut self, shell: PasteShell) -> Self {
        self.paste_shell = shell;
        self
    }

    pub fn with_cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn with_startup_input(mut self, input: impl Into<String>) -> Self {
        self.startup_input = Some(input.into());
        self
    }

    pub fn with_auto_mode(mut self, presses: u8) -> Self {
        self.auto_mode_presses = presses;
        self
    }

    pub fn with_submit(mut self, submit: bool) -> Self {
        self.submit = submit;
        self
    }

    pub fn with_startup_delay(mut self, ms: u32) -> Self {
        self.startup_delay_ms = ms;
        self
    }

    pub fn with_binding(mut self, binding: LaunchBinding) -> Self {
        self.binding = binding;
        self
    }

    pub fn with_launch_resumed(mut self, resumed: bool) -> Self {
        self.launch_resumed = resumed;
        self
    }

    pub fn with_title_source(mut self, source: TitleSource) -> Self {
        self.title_source = Some(source);
        self
    }
}

/// How long the requested size must hold steady before we actually resize. A
/// pane close / divider drag produces a burst of size changes; coalescing them
/// into one resize avoids repeated SIGWINCHes that make TUIs redraw (and can
/// leave duplicated static output, e.g. a reprinted banner).
const RESIZE_SETTLE: Duration = Duration::from_millis(60);

/// Sentinel for `mouse_pressed_button` meaning "no button press is outstanding".
const NO_MOUSE_PRESS: u8 = u8::MAX;

/// `Write` adapter that queues bytes to the dedicated PTY writer thread.
/// `write` never blocks (unbounded channel); byte order is preserved. Errors
/// once input closes or the worker exits, mirroring a broken pipe. Profiling
/// timestamps are omitted entirely when the profiler is off.
struct QueuedWrite {
    bytes: Vec<u8>,
    queued_at: Option<Instant>,
}

fn queue_delay_excluding_profiler(
    queued_at: Instant,
    dequeued_at: Instant,
    profiler_interval: Option<(Instant, Instant)>,
) -> Duration {
    let raw = dequeued_at.saturating_duration_since(queued_at);
    let Some((profile_started, profile_finished)) = profiler_interval else {
        return raw;
    };
    let overlap_started = queued_at.max(profile_started);
    let overlap_finished = dequeued_at.min(profile_finished);
    raw.saturating_sub(overlap_finished.saturating_duration_since(overlap_started))
}

struct ChannelWriter {
    tx: std::sync::mpsc::Sender<Option<QueuedWrite>>,
    profile: bool,
    closed: Arc<AtomicBool>,
}

impl Write for ChannelWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.closed.load(Ordering::Acquire) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "pty child exited",
            ));
        }
        self.tx
            .send(Some(QueuedWrite {
                bytes: buf.to_vec(),
                queued_at: self.profile.then(Instant::now),
            }))
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::BrokenPipe, "pty writer gone"))?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Closing ConPTY must happen off the UI thread while its reader is draining.
/// Taking ownership under the lock, then dropping outside it, also prevents
/// a resize from waiting on ClosePseudoConsole's output flush.
struct SessionMaster {
    inner: Mutex<Option<Box<dyn MasterPty + Send>>>,
}

impl SessionMaster {
    #[cfg(windows)]
    fn close(&self) {
        let master = self.inner.lock().take();
        drop(master);
    }
}

#[cfg(not(windows))]
type SessionChild = Box<dyn portable_pty::Child + Send + Sync>;

#[cfg(windows)]
struct SessionChild {
    exit: std::sync::mpsc::Receiver<std::io::Result<portable_pty::ExitStatus>>,
    killer: SharedChildKiller,
}

#[cfg(windows)]
impl SessionChild {
    fn kill(&mut self) -> std::io::Result<()> {
        self.killer.lock().kill()
    }

    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        match self.exit.try_recv() {
            Ok(result) => result.map(Some),
            Err(std::sync::mpsc::TryRecvError::Empty) => Ok(None),
            Err(std::sync::mpsc::TryRecvError::Disconnected) => Err(std::io::Error::other(
                "PTY child monitor ended without an exit status",
            )),
        }
    }
}

/// Debounce state for resizing: the last-applied size, the size currently being
/// requested, and when that request first appeared.
struct ResizeState {
    applied: (u16, u16),
    target: (u16, u16),
    since: Instant,
}

fn draft_after_input(previous: bool, submission_pending: bool, has_bytes: bool) -> bool {
    if !has_bytes {
        previous
    } else {
        !submission_pending
    }
}

/// The input word (see `TerminalSession::input_state`) after one non-empty
/// write. The revision always advances. Typed input latches the draft, or
/// clears it when it carries a submission (`Some(true)`); a mouse or wheel
/// report (`None`) is input but not text, so the draft stays as it was.
fn next_input_state(state: u64, submitted: Option<bool>) -> u64 {
    let revision = (state >> 1).wrapping_add(1) & (u64::MAX >> 1);
    let previous_draft = state & 1 != 0;
    let draft = submitted.map_or(previous_draft, |submitted| {
        draft_after_input(previous_draft, submitted, true)
    });
    (revision << 1) | u64::from(draft)
}

/// A clickable link the pointer is ctrl-hovering: a span of columns on one
/// buffer line (negative = history, so the underline scrolls with the content)
/// plus the URI a click would open.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct HoveredLink {
    /// Buffer line (alacritty `Line` coordinate; negative = scrollback).
    pub line: i32,
    /// Column span `[start, end)` on that line.
    pub start: usize,
    pub end: usize,
    /// What a click opens: an `http(s)://` URL or a `file://` URI.
    pub url: String,
}

/// Last known pointer position inside the terminal grid (for re-hit-testing when
/// Ctrl/Cmd is pressed without a mouse move).
#[derive(Clone, Copy, Debug)]
pub(crate) struct PointerHit {
    /// Position relative to the terminal origin, in pixels.
    pub local_x: f32,
    pub local_y: f32,
    pub cell_width: f32,
    pub line_height: f32,
    pub cols: u16,
    pub rows: u16,
}

pub struct TerminalSession {
    pub id: Uuid,
    launch_resumed: bool,
    term: Arc<Mutex<Term<MuxelListener>>>,
    processor: Mutex<Processor>,
    writer: SharedWriter,
    output_queue: Arc<OutputQueueState>,
    // `MasterPty` is Send but not Sync. Access is rare (settled resize and a
    // Unix foreground-process query), so a mutex lets a fully spawned session
    // move from the launch worker to the UI thread without weakening safety.
    master: Arc<SessionMaster>,
    /// On Windows a separate waiter observes process exit and closes ConPTY;
    /// on Unix the reader harvests the child status after EOF.
    killer: SharedChildKiller,
    input_closed: Arc<AtomicBool>,
    input_stop: std::sync::mpsc::Sender<Option<QueuedWrite>>,
    /// The PTY child's pid (the shell/agent), captured at spawn. Compared against
    /// the terminal's foreground process group to tell whether the child is idle
    /// at its prompt vs. running a foreground command (see `is_idle_foreground`).
    /// Only read on Unix (`tcgetpgrp`); kept on all platforms for spawn symmetry.
    #[cfg_attr(not(unix), allow(dead_code))]
    child_pid: Option<u32>,
    title: Arc<Mutex<Option<String>>>,
    evidence: Arc<Mutex<LifecycleEvidence>>,
    title_generation: Arc<AtomicU64>,
    title_changed_at: Arc<Mutex<Option<Instant>>>,
    session_id_hint: Arc<Mutex<Option<String>>>,
    bell: Arc<AtomicBool>,
    /// OSC-52 copies parsed from output, pending pickup by the view (which owns
    /// the gpui context a clipboard write needs).
    clipboard_store: Arc<Mutex<Vec<(ClipboardType, String)>>>,
    /// The palette color queries are answered from (see `replies`); kept
    /// current with the app theme via [`Self::set_palette`].
    palette: Arc<Mutex<TerminalPalette>>,
    /// True while a left-drag text selection started in this terminal.
    selecting: AtomicBool,
    /// The mouse button (0/1/2) whose *press* we last forwarded to a mouse-
    /// reporting app, or `NO_MOUSE_PRESS` for none. Lets the release always be
    /// sent (even if the pointer left the pane or Shift is now held) so the app
    /// isn't stranded with a phantom held button.
    mouse_pressed_button: AtomicU8,
    /// Sub-line scroll-wheel remainder, carried across wheel events.
    scroll_accum: Mutex<f32>,
    /// While dragging the scrollbar thumb: the grab offset within the thumb.
    scrollbar_drag: Mutex<Option<f32>>,
    /// Resize debounce state (skips redundant resizes + coalesces bursts).
    resize: Mutex<ResizeState>,
    /// Active search needle (lowercased not required — matching is ASCII-insensitive).
    /// Empty = no search; the element highlights matches of this each paint.
    search: Mutex<Vec<char>>,
    /// Remote handling for file/image uploads. See [`crate::LocalPaste`].
    local_paste: Mutex<Option<crate::LocalPasteHandler>>,
    path_paste_error: Mutex<Option<crate::PathPasteErrorHandler>>,
    paste_shell: PasteShell,
    /// The local working directory the child was spawned in — the base for
    /// resolving relative file paths on ctrl+click. Remote panes run `ssh`
    /// locally with no cwd set, so their relative paths stay unresolvable.
    cwd: Option<std::path::PathBuf>,
    /// The link span under a ctrl+hover, if any; the element paints an underline
    /// over it and shows a pointing-hand cursor (mirrors the `search` pattern).
    hovered_link: Mutex<Option<HoveredLink>>,
    /// Latest pointer position over this terminal (for Ctrl-down re-hit-test).
    pointer_hit: Mutex<Option<PointerHit>>,
    /// When output was last processed (for idle/status detection).
    last_output: Mutex<Instant>,
    /// Whether the child has produced any output yet (vs. still starting up).
    output_seen: AtomicBool,
    /// Whether this terminal currently has keyboard focus (UI thread + drain).
    focused: AtomicBool,
    /// Set once Muxel deliberately submits a turn with Enter, for this whole
    /// launch. Startup title activity alone is not evidence that a turn
    /// completed. The runtime reducer uses the submission recorded in
    /// `evidence` instead, which counts only within its provider binding.
    turn_submitted: AtomicBool,
    /// When the user last sent this terminal input themselves (see
    /// [`Self::mark_user_input`]), as opposed to text muxel typed on its own.
    last_user_input: Mutex<Option<Instant>>,
    submission_pending: AtomicBool,
    /// The input revision and a conservative draft bit in one atomic word, so
    /// a reader never sees one write's revision paired with another write's
    /// draft bit. The low bit is the draft flag; the remaining bits are the
    /// revision, advanced for each input write (see [`Self::input_snapshot`]).
    input_state: AtomicU64,
    /// Coalesced wakeup for the view's runtime reducer. Capacity one makes a
    /// burst of keys/resizes one reconciliation request instead of an event log.
    runtime_trigger: async_channel::Sender<()>,
    runtime_events: async_channel::Receiver<()>,
    /// Bumped whenever pixels that depend on the grid/selection/scroll/search
    /// would change. [`crate::element`] skips the full cell walk when this matches
    /// the last painted generation (draw-list replay).
    content_gen: AtomicU64,
    /// Last built draw list for this session (main-thread only via the GPUI paint
    /// path). Invalid when [`Self::content_gen`] advances or paint metrics change.
    paint_list: Mutex<Option<PaintDrawList>>,
    /// Accumulated grid damage since the last paint (from alacritty `TermDamage`).
    pending_damage: Mutex<ContentDamage>,
    /// Deadline through which PTY output is treated as an interactive response
    /// to recent input. A TUI usually redraws rather than literally echoing the
    /// typed byte, so correlating one input with the next output batch is wrong
    /// when agent output is already queued.
    interactive_until: Mutex<Option<Instant>>,
    _reader: JoinHandle<()>,
    #[cfg(all(test, windows))]
    _monitor: JoinHandle<()>,
}

/// Viewport damage since the last paint, derived from alacritty's damage tracker.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) enum ContentDamage {
    /// Entire viewport must be rebuilt (scroll, resize, clear, alt-screen, …).
    #[default]
    Full,
    /// Only these **visual** line indices (0 = top of viewport) changed.
    Partial(Vec<i32>),
}

impl ContentDamage {
    pub(crate) fn merge(&mut self, other: ContentDamage) {
        match (&mut *self, other) {
            (ContentDamage::Full, _) => {}
            (_, ContentDamage::Full) => *self = ContentDamage::Full,
            (ContentDamage::Partial(a), ContentDamage::Partial(b)) => {
                for line in b {
                    if !a.contains(&line) {
                        a.push(line);
                    }
                }
                a.sort_unstable();
            }
        }
    }

    /// Whether a partial rebuild is worthwhile (not almost-full).
    pub(crate) fn prefer_partial_rebuild(&self, screen_lines: usize) -> Option<&[i32]> {
        match self {
            ContentDamage::Full => None,
            ContentDamage::Partial(lines)
                if !lines.is_empty() && lines.len() * 2 < screen_lines.max(1) =>
            {
                Some(lines.as_slice())
            }
            ContentDamage::Partial(_) => None,
        }
    }
}

/// Layout metrics that must match for a draw-list replay to be valid.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct PaintMetrics {
    pub cell_w: f32,
    pub line_h: f32,
    pub font_size: f32,
    pub cols: u16,
    pub rows: u16,
    pub bg: [f32; 4],
}

/// One text run ready to paint. After the first full paint, [`Self::shaped`]
/// holds the gpui layout so sibling-pane replays skip `shape_line`.
#[derive(Clone, Debug)]
pub(crate) struct CachedRun {
    pub start_line: i32,
    pub start_col: i32,
    pub text: String,
    pub bold: bool,
    pub italic: bool,
    pub color: [f32; 4],
    pub underline: bool,
    pub wavy: bool,
    pub strike: bool,
    /// Populated during paint (needs the window text system). Replay uses this
    /// and skips re-shaping.
    pub shaped: Option<gpui::ShapedLine>,
}

impl PaintMetrics {
    /// Whether shaped glyph layouts built under these metrics are reusable under
    /// `other`: same font geometry. Grid size / background may differ — shaping
    /// doesn't depend on them.
    pub(crate) fn same_font(&self, other: &Self) -> bool {
        self.cell_w == other.cell_w
            && self.line_h == other.line_h
            && self.font_size == other.font_size
    }
}

/// Batched background / selection / search rect.
#[derive(Clone, Copy, Debug)]
pub(crate) struct CachedRect {
    pub line: i32,
    pub start_col: i32,
    pub num_cells: usize,
    pub color: [f32; 4],
}

/// Full-grid draw list produced by a full paint walk; reused on subsequent paints
/// while [`TerminalSession::content_gen`] is unchanged.
#[derive(Clone, Debug)]
pub(crate) struct PaintDrawList {
    pub content_gen: u64,
    pub metrics: PaintMetrics,
    pub runs: Vec<CachedRun>,
    pub bg_rects: Vec<CachedRect>,
    pub sel_rects: Vec<CachedRect>,
    pub search_rects: Vec<CachedRect>,
}

impl TerminalSession {
    pub fn launch_resumed(&self) -> bool {
        self.launch_resumed
    }

    /// Spawn a PTY running `spec` at the given initial grid size. Returns the
    /// session plus the receiver the UI drains for output/exit events.
    pub fn spawn(spec: CommandSpec, cols: u16, rows: u16) -> Result<(Arc<Self>, PtyReceiver)> {
        let (tx, rx) = output_queue(
            PTY_OUTPUT_QUEUE_BYTE_CAPACITY,
            PTY_OUTPUT_QUEUE_CHUNK_CAPACITY,
        );
        let session = Self::spawn_inner(spec, cols, rows, None, PtyChunkSender::Public(tx))?;
        Ok((session, PtyReceiver { inner: rx }))
    }

    /// Spawn with phase timings attributed to one Muxel pane.
    pub fn spawn_profiled(
        spec: CommandSpec,
        cols: u16,
        rows: u16,
        instance_id: uuid::Uuid,
    ) -> Result<(Arc<Self>, PtyReceiver)> {
        let (tx, rx) = output_queue(
            PTY_OUTPUT_QUEUE_BYTE_CAPACITY,
            PTY_OUTPUT_QUEUE_CHUNK_CAPACITY,
        );
        let session = Self::spawn_inner(
            spec,
            cols,
            rows,
            Some(instance_id),
            PtyChunkSender::Public(tx),
        )?;
        Ok((session, PtyReceiver { inner: rx }))
    }

    pub(crate) fn spawn_internal(
        spec: CommandSpec,
        cols: u16,
        rows: u16,
        instance_id: Option<uuid::Uuid>,
    ) -> Result<(Arc<Self>, ProfiledPtyReceiver)> {
        let (tx, rx) = output_queue(
            PTY_OUTPUT_QUEUE_BYTE_CAPACITY,
            PTY_OUTPUT_QUEUE_CHUNK_CAPACITY,
        );
        let session =
            Self::spawn_inner(spec, cols, rows, instance_id, PtyChunkSender::Profiled(tx))?;
        Ok((session, rx))
    }

    fn spawn_inner(
        spec: CommandSpec,
        cols: u16,
        rows: u16,
        instance_id: Option<uuid::Uuid>,
        tx: PtyChunkSender,
    ) -> Result<Arc<Self>> {
        let cols = cols.max(1);
        let rows = rows.max(1);

        if instance_id.is_some() && crate::profile::is_enabled() {
            crate::profile::start();
        }

        let program_name = spec.program.clone();
        let pty_system = native_pty_system();
        let phase = std::time::Instant::now();
        let pair = pty_system
            .openpty(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            })
            .context("open pty")?;
        if let Some(instance_id) = instance_id {
            crate::profile::startup_event(
                instance_id,
                &program_name,
                "openpty",
                phase.elapsed(),
                0,
            );
        }

        // On Windows, resolve bare names like `codex` to a real CreateProcess
        // target (`codex.cmd` / `codex.exe`). npm installs an extension-less
        // `#!/bin/sh` shim *before* the `.cmd` wrapper; portable-pty's search
        // returns the shim first and CreateProcessW fails with
        // ERROR_BAD_EXE_FORMAT (193) "%1 is not a valid Win32 application".
        let phase = std::time::Instant::now();
        let program = spec.program_for_spawn()?;
        if let Some(instance_id) = instance_id {
            crate::profile::startup_event(
                instance_id,
                &program_name,
                "program-resolution",
                phase.elapsed(),
                0,
            );
        }
        let mut builder = command_builder_for_spawn(&program, &spec.args);
        // Remembered for resolving relative file paths on ctrl+click.
        let cwd = spec.cwd.as_ref().map(std::path::PathBuf::from);
        if let Some(cwd) = &spec.cwd {
            builder.cwd(cwd);
        }
        // When muxel itself runs from an AppImage, its process environment
        // carries the AppImage runtime's leakage — APPDIR/APPIMAGE/ARGV0/OWD, a
        // `MAKE` pointing back at the AppImage, and AppImage-mount entries in
        // PATH/LD_LIBRARY_PATH. Strip it so spawned shells/agents/build tools get
        // a clean system environment (otherwise e.g. cmake caches `$(MAKE)` as the
        // AppImage and `make` relaunches muxel instead of building).
        sanitize_appimage_env(&mut builder);
        ensure_utf8_locale(&mut builder);
        builder.env("TERM", "xterm-256color");
        builder.env("COLORTERM", "truecolor");
        // Last, so anything the instance/preset sets wins over muxel's defaults.
        apply_spec_env(&mut builder, &spec);
        // These are private transport slots, not preset-overridable child env.
        // Reapply them after preset env so a collision cannot replace the batch
        // program or argv that Muxel resolved.
        apply_windows_batch_runner_env(&mut builder, &program, &spec.args);

        let phase = std::time::Instant::now();
        let child = pair.slave.spawn_command(builder).context("spawn command")?;
        if let Some(instance_id) = instance_id {
            crate::profile::startup_event(
                instance_id,
                &program_name,
                "child-spawn",
                phase.elapsed(),
                0,
            );
        }
        let setup_started = std::time::Instant::now();
        let child_pid = child.process_id();
        let killer = Arc::new(Mutex::new(child.clone_killer()));
        let reader = pair.master.try_clone_reader().context("clone pty reader")?;
        let reader_wake = ReaderWake::new(&*pair.master, &tx.queue_state())?;
        // PTY writes go through a dedicated thread — NEVER synchronously from
        // the caller. When a busy agent stops draining stdin (e.g. an Ink TUI
        // deep in its render debounce under key-repeat), conhost stops reading
        // the ConPTY input pipe and `write_all` BLOCKS until the agent catches
        // up — which it only does once input pauses. Written directly from the
        // UI thread, that stall froze the entire window for seconds mid-hold
        // (no draws, no presents, no input processing), unfreezing exactly at
        // key release. The writer thread absorbs the stall; callers (key
        // handler, mouse reports, the VTE listener's query replies) just queue
        // bytes. Windows Terminal threads its PTY input for the same reason.
        let mut pipe_writer = pair.master.take_writer().context("take pty writer")?;
        let profile_writes = instance_id.filter(|_| crate::profile::is_enabled());
        let (write_tx, write_rx) = std::sync::mpsc::channel::<Option<QueuedWrite>>();
        let input_closed = Arc::new(AtomicBool::new(false));
        let worker_closed = input_closed.clone();
        let writer_handle = std::thread::Builder::new()
            .name("muxel-pty-writer".to_string())
            .spawn(move || {
                // Exits when every sender is gone (session dropped) or the
                // pipe breaks (child gone).
                let mut prior_profiler_interval = None;
                while let Ok(Some(write)) = write_rx.recv() {
                    if worker_closed.load(Ordering::Acquire) {
                        break;
                    }
                    let queue_delay = write.queued_at.map(|queued_at| {
                        queue_delay_excluding_profiler(
                            queued_at,
                            Instant::now(),
                            prior_profiler_interval,
                        )
                    });
                    let write_started = write.queued_at.map(|_| Instant::now());
                    let result = pipe_writer.write_all(&write.bytes);
                    if result.is_ok() {
                        let _ = pipe_writer.flush();
                    }
                    if let (Some(instance_id), Some(queue_delay), Some(write_started)) =
                        (profile_writes, queue_delay, write_started)
                    {
                        let profile_started = Instant::now();
                        crate::profile::pty_write(
                            instance_id,
                            write.bytes.len(),
                            queue_delay,
                            write_started.elapsed(),
                        );
                        prior_profiler_interval = Some((profile_started, Instant::now()));
                    }
                    if result.is_err() {
                        break;
                    }
                }
            })
            .context("spawn pty writer thread")?;
        let writer: SharedWriter = Arc::new(Mutex::new(Box::new(ChannelWriter {
            tx: write_tx.clone(),
            profile: profile_writes.is_some(),
            closed: input_closed.clone(),
        })));
        // The Windows slave shares ConPTY ownership with the master. Release
        // it before the monitor can close the console after a fast child exit.
        drop(pair.slave);
        let master = Arc::new(SessionMaster {
            inner: Mutex::new(Some(pair.master)),
        });

        #[cfg(windows)]
        let (exit_tx, exit_rx) = std::sync::mpsc::channel();
        #[cfg(windows)]
        let session_child = SessionChild {
            exit: exit_rx,
            killer: killer.clone(),
        };
        #[cfg(not(windows))]
        let session_child = child;

        let palette = Arc::new(Mutex::new(TerminalPalette::default()));
        let term_config = term_config();
        let replies = Arc::new(Replies::new(writer.clone()));
        let immediate_replies =
            ImmediateReplies::new(replies.clone(), palette.clone(), &term_config);
        let output_queue = tx.queue_state();
        let profile_reads =
            tx.carries_timing() && instance_id.is_some() && crate::profile::is_enabled();
        let reader_handle = std::thread::Builder::new()
            .name("muxel-pty-reader".to_string())
            .spawn({
                let input_closed = input_closed.clone();
                let input_stop = write_tx.clone();
                move || {
                    read_loop(
                        reader,
                        reader_wake,
                        session_child,
                        tx,
                        immediate_replies,
                        profile_reads,
                        InputShutdown {
                            closed: input_closed,
                            stop: input_stop,
                        },
                    )
                }
            })
            .context("spawn reader thread")?;

        #[cfg(windows)]
        let monitor_handle = {
            let monitor_master = master.clone();
            let monitor_closed = input_closed.clone();
            let monitor_stop = write_tx.clone();
            let monitor = std::thread::Builder::new()
                .name("muxel-pty-child".to_string())
                .spawn(move || {
                    let mut child = child;
                    let status = child.wait();
                    monitor_closed.store(true, Ordering::Release);
                    let _ = monitor_stop.send(None);
                    // Publish before close: the reader emits Exit only after
                    // draining all output produced before the child died.
                    let _ = exit_tx.send(status);
                    cancel_finished_child_input(&writer_handle);
                    monitor_master.close();
                    let _ = writer_handle.join();
                });
            monitor_spawn_result(monitor, &output_queue, || {
                let _ = killer.lock().kill();
                input_closed.store(true, Ordering::Release);
                let _ = write_tx.send(None);
                master.close();
            })
            .context("spawn PTY child monitor")?
        };
        #[cfg(all(windows, not(test)))]
        drop(monitor_handle);
        #[cfg(not(windows))]
        drop(writer_handle);

        let title = Arc::new(Mutex::new(None));
        let evidence = Arc::new(Mutex::new(LifecycleEvidence::for_launch(&spec.binding)));
        let title_generation = Arc::new(AtomicU64::new(0));
        let title_changed_at = Arc::new(Mutex::new(None));
        let session_id_hint = Arc::new(Mutex::new(None));
        let bell = Arc::new(AtomicBool::new(false));
        let clipboard_store = Arc::new(Mutex::new(Vec::new()));
        let listener = MuxelListener {
            replies,
            title: title.clone(),
            evidence: evidence.clone(),
            title_generation: title_generation.clone(),
            title_changed_at: title_changed_at.clone(),
            session_id_hint: session_id_hint.clone(),
            bell: bell.clone(),
            clipboard_store: clipboard_store.clone(),
        };

        let term = Term::new(
            term_config,
            &TermSize::new(cols as usize, rows as usize),
            listener,
        );

        let (runtime_trigger, runtime_events) = async_channel::bounded(1);

        let session = Arc::new(Self {
            id: Uuid::new_v4(),
            launch_resumed: spec.launch_resumed,
            term: Arc::new(Mutex::new(term)),
            processor: Mutex::new(Processor::new()),
            writer,
            master,
            output_queue,
            killer,
            input_closed,
            input_stop: write_tx,
            child_pid,
            title,
            evidence,
            title_generation,
            title_changed_at,
            session_id_hint,
            bell,
            clipboard_store,
            palette,
            selecting: AtomicBool::new(false),
            mouse_pressed_button: AtomicU8::new(NO_MOUSE_PRESS),
            scroll_accum: Mutex::new(0.0),
            scrollbar_drag: Mutex::new(None),
            resize: Mutex::new(ResizeState {
                applied: (cols, rows),
                target: (cols, rows),
                since: Instant::now(),
            }),
            search: Mutex::new(Vec::new()),
            local_paste: Mutex::new(None),
            path_paste_error: Mutex::new(None),
            paste_shell: spec.paste_shell,
            cwd,
            hovered_link: Mutex::new(None),
            pointer_hit: Mutex::new(None),
            last_output: Mutex::new(Instant::now()),
            output_seen: AtomicBool::new(false),
            focused: AtomicBool::new(false),
            turn_submitted: AtomicBool::new(false),
            last_user_input: Mutex::new(None),
            submission_pending: AtomicBool::new(false),
            input_state: AtomicU64::new(0),
            runtime_trigger,
            runtime_events,
            content_gen: AtomicU64::new(1),
            paint_list: Mutex::new(None),
            pending_damage: Mutex::new(ContentDamage::Full),
            interactive_until: Mutex::new(None),
            _reader: reader_handle,
            #[cfg(all(test, windows))]
            _monitor: monitor_handle,
        });

        if let Some(instance_id) = instance_id {
            crate::profile::startup_event(
                instance_id,
                &program_name,
                "pty-setup",
                setup_started.elapsed(),
                0,
            );
        }

        Ok(session)
    }

    /// Feed PTY output through the VTE parser into the terminal grid. Evidence
    /// it raises (a title, a bell) is stamped as read now; the view's drain uses
    /// [`Self::process_drained`], which stamps it with when it was read.
    pub(crate) fn process_output(&self, data: &[u8]) {
        self.parse([(data, SystemTime::now())]);
    }

    /// Feed a drained batch through the parser chunk by chunk, so the evidence
    /// each chunk raises is stamped with when the reader received it. A batch
    /// can wait in the queue and the drain's coalescing delay, so its parse
    /// time can fall after a provider switch its bytes preceded.
    pub(crate) fn process_drained(&self, output: &PtyOutput) {
        self.parse(output.chunks());
    }

    /// Collects alacritty damage for the next paint (partial row rebuild) and
    /// for paint-priority scheduling (recent interaction vs stream).
    fn parse<'a>(&self, chunks: impl IntoIterator<Item = (&'a [u8], SystemTime)>) {
        let mut term = self.term.lock();
        let mut processor = self.processor.lock();
        let mut parsed = false;
        for (bytes, read_at) in chunks {
            if bytes.is_empty() {
                continue;
            }
            self.evidence.lock().begin_read(read_at);
            processor.advance(&mut *term, bytes);
            parsed = true;
        }
        // Synchronized updates (DECSET 2026): vte BUFFERS everything between
        // BSU/ESU instead of applying it, and expiring a stuck window is the
        // embedder's job (alacritty services this in its event loop; vte's
        // deadline is 150ms). Without this check, an app that opens a window
        // and never closes it would freeze the grid until it next goes idle.
        // Checking here bounds the freeze to deadline + one batch gap. What it
        // applies was read by the last chunk at the latest, and is stamped so.
        if processor
            .sync_timeout()
            .sync_timeout()
            .is_some_and(|d| Instant::now() >= d)
        {
            processor.stop_sync(&mut *term);
            profile::sync_expired();
        }
        self.evidence.lock().end_read();
        if parsed {
            let screen = term.screen_lines() as i32;
            let dmg = match term.damage() {
                TermDamage::Full => ContentDamage::Full,
                TermDamage::Partial(iter) => {
                    // alacritty's iterator yields viewport-oriented line indices
                    // when display_offset is applied; clamp to the visible grid.
                    let mut lines: Vec<i32> = iter
                        .map(|b| b.line as i32)
                        .filter(|&l| l >= 0 && l < screen)
                        .collect();
                    lines.sort_unstable();
                    lines.dedup();
                    if lines.is_empty() {
                        ContentDamage::Full
                    } else {
                        ContentDamage::Partial(lines)
                    }
                }
            };
            term.reset_damage();
            // Do not call bump_content() here — it marks Full damage and would
            // erase the partial lines we just collected.
            self.pending_damage.lock().merge(dmg);
            self.content_gen.fetch_add(1, Ordering::Relaxed);
        }
        *self.last_output.lock() = Instant::now();
        self.output_seen.store(true, Ordering::Relaxed);
    }

    /// Take accumulated damage for the next paint.
    pub(crate) fn take_pending_damage(&self) -> ContentDamage {
        std::mem::replace(
            &mut *self.pending_damage.lock(),
            ContentDamage::Partial(Vec::new()),
        )
    }

    /// Peek damage without clearing (tests). Unix-only with its caller module.
    #[cfg(all(test, unix))]
    pub(crate) fn pending_damage_for_test(&self) -> ContentDamage {
        self.pending_damage.lock().clone()
    }

    #[cfg(all(test, unix))]
    pub(crate) fn clear_pending_damage_for_test(&self) {
        *self.pending_damage.lock() = ContentDamage::Partial(Vec::new());
        // Don't touch content_gen — tests only care about damage merge.
    }

    /// Keep terminal output responsive briefly after user input. This is an
    /// interaction window, not echo detection: full-screen TUIs redraw in
    /// response to keys and streaming output may already be queued.
    fn mark_interactive(&self) {
        const INTERACTION_WINDOW: Duration = Duration::from_millis(75);
        *self.interactive_until.lock() = Some(Instant::now() + INTERACTION_WINDOW);
    }

    /// Whether output should use the interactive paint cadence.
    pub(crate) fn is_interactive(&self) -> bool {
        self.interactive_until
            .lock()
            .is_some_and(|deadline| Instant::now() < deadline)
    }

    /// Current content generation — advances on any grid/selection/scroll/search
    /// change that should invalidate the cached draw list.
    pub(crate) fn content_generation(&self) -> u64 {
        self.content_gen.load(Ordering::Relaxed)
    }

    /// Invalidate the paint draw-list cache (grid-facing content changed).
    pub(crate) fn bump_content(&self) {
        self.content_gen.fetch_add(1, Ordering::Relaxed);
        // Selection / search / scroll paths bump without going through
        // process_output's damage collector — treat as full viewport.
        self.pending_damage.lock().merge(ContentDamage::Full);
    }

    /// Store a freshly built draw list after a full paint walk.
    pub(crate) fn store_paint_list(&self, list: PaintDrawList) {
        *self.paint_list.lock() = Some(list);
    }

    /// Take the previous draw list (for shape retention across content_gen bumps).
    pub(crate) fn take_paint_list(&self) -> Option<PaintDrawList> {
        self.paint_list.lock().take()
    }

    /// Run `f` with the cached draw list when it matches `content_gen` + `metrics`.
    /// Returns `true` if the cache hit (so the caller can skip a full rebuild).
    pub(crate) fn with_paint_list_if_valid(
        &self,
        content_gen: u64,
        metrics: PaintMetrics,
        f: impl FnOnce(&PaintDrawList),
    ) -> bool {
        let guard = self.paint_list.lock();
        if let Some(list) = guard
            .as_ref()
            .filter(|l| l.content_gen == content_gen && l.metrics == metrics)
        {
            f(list);
            true
        } else {
            false
        }
    }

    /// Whether the child has produced any output yet.
    pub fn has_output(&self) -> bool {
        self.output_seen.load(Ordering::Relaxed)
    }

    /// Return current and high-water reader-to-consumer queue occupancy.
    pub fn output_queue_stats(&self) -> PtyOutputQueueStats {
        self.output_queue.stats()
    }

    /// Write bytes to the PTY (user input, pastes, key sequences). Any input
    /// jumps the viewport back to the bottom, so typing while scrolled up in the
    /// history snaps you to the live prompt.
    pub fn write_input(&self, data: &[u8]) {
        let _ = self.try_write_input(data);
    }

    /// [`Self::write_input`], saying whether the bytes could be handed to the
    /// PTY. An error means the terminal's writer is gone (its program exited and
    /// a write has already failed); `Ok` only means the bytes were queued.
    pub fn try_write_input(&self, data: &[u8]) -> std::io::Result<()> {
        self.scroll_input_to_bottom();
        if !data.is_empty() {
            let submitted_enter = self.submission_pending.swap(false, Ordering::AcqRel);
            self.advance_input(Some(submitted_enter));
            if submitted_enter {
                self.trigger_runtime();
            }
            // A TUI reaction will follow; prioritize output during this short
            // interaction window even if stream output is already queued.
            self.mark_interactive();
        }
        self.try_write_raw(data)
    }

    /// Admit a complete submission (Enter, or text ending in Enter) and commit
    /// its lifecycle facts together. Failed admission leaves the draft and
    /// submission evidence unchanged, with no marker for a later key to consume.
    /// Success means queued bytes, not confirmation that the child consumed them.
    pub fn try_submit_input(&self, data: &[u8]) -> std::io::Result<()> {
        self.scroll_input_to_bottom();
        let mut writer = self.writer.lock();
        // The child can react as soon as write_all publishes bytes. Keep the
        // input time before any resume hook it causes, but commit no facts
        // until writer admission succeeds.
        let submitted_at = SystemTime::now();
        self.submission_pending.store(false, Ordering::Release);
        writer.write_all(data)?;
        writer.flush()?;
        if !data.is_empty() {
            self.evidence.lock().record_submission(submitted_at);
            self.turn_submitted.store(true, Ordering::Relaxed);
            self.advance_input(Some(true));
            self.mark_interactive();
            self.trigger_runtime();
        }
        Ok(())
    }

    fn scroll_input_to_bottom(&self) {
        // Skip scroll work when already pinned to the live row — every key
        // otherwise takes the term mutex for a no-op scroll under load.
        {
            let mut term = self.term.lock();
            if term.grid().display_offset() != 0 {
                term.scroll_display(Scroll::Bottom);
                self.pending_damage.lock().merge(ContentDamage::Full);
                self.bump_content();
            }
        }
    }

    /// Input revision for keys, pastes, and pointer reports sent by this terminal.
    /// Input from another client of the same tmux session is not counted.
    pub fn input_rev(&self) -> u64 {
        self.input_snapshot().0
    }

    /// [`Self::write_input`] for input the user produced themselves — a key typed
    /// into the pane — so it counts as someone being at this machine.
    pub fn write_user_input(&self, data: &[u8]) {
        self.mark_user_input();
        self.write_input(data);
    }

    /// Note that the user just sent this terminal input themselves: a key, a
    /// paste, a drop, or a click or scroll the program receives. Runners, snippets,
    /// auto-continue and outside control type on their own and never mark it.
    pub fn mark_user_input(&self) {
        *self.last_user_input.lock() = Some(Instant::now());
    }

    /// Send pastes that name this machine's files — a clipboard image, copied or
    /// dropped files — to `handler` rather than directly into the PTY, for panes
    /// running on another host that cannot read this machine's files/clipboard.
    pub fn set_local_paste_handler(&self, handler: Option<crate::LocalPasteHandler>) {
        *self.local_paste.lock() = handler;
    }

    pub(crate) fn local_paste_handler(&self) -> Option<crate::LocalPasteHandler> {
        self.local_paste.lock().clone()
    }

    /// Only local refusals cross into the app; valid files/images keep their
    /// synchronous input order relative to the next key.
    pub fn set_path_paste_error_handler(&self, handler: Option<crate::PathPasteErrorHandler>) {
        *self.path_paste_error.lock() = handler;
    }

    pub(crate) fn path_paste_error_handler(&self) -> Option<crate::PathPasteErrorHandler> {
        self.path_paste_error.lock().clone()
    }

    /// When the user last sent this terminal input themselves, if ever.
    pub fn last_user_input(&self) -> Option<Instant> {
        *self.last_user_input.lock()
    }

    fn advance_input(&self, submitted: Option<bool>) {
        let _ = self
            .input_state
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |state| {
                Some(next_input_state(state, submitted))
            });
    }

    /// Announce that the next input write submits a turn. Call it just before
    /// writing the Enter (or the text ending in one).
    pub fn mark_turn_submitted(&self) {
        self.evidence.lock().record_submission(SystemTime::now());
        self.turn_submitted.store(true, Ordering::Relaxed);
        self.submission_pending.store(true, Ordering::Release);
    }

    /// Whether any turn was submitted in this launch, in any conversation.
    pub fn has_submitted_turn(&self) -> bool {
        self.turn_submitted.load(Ordering::Relaxed)
    }

    /// Conversation observed before this launch's first submitted turn. None
    /// means no turn was submitted or its recipient was unknown. A later
    /// identity or submission never establishes or transfers that ownership.
    pub fn first_submission_binding(&self) -> Option<u64> {
        self.evidence.lock().first_submission_binding()
    }

    /// The input revision and draft bit. Ordinary non-empty input writes (keys,
    /// pastes, mouse and wheel reports) advance the revision before writer
    /// admission; fallible submissions commit it after successful admission.
    /// Focus reports and the terminal's own replies to the program don't count.
    /// The draft bit is set
    /// by typed input and cleared by a submission.
    pub fn input_snapshot(&self) -> (u64, bool) {
        let state = self.input_state.load(Ordering::Relaxed);
        (state >> 1, state & 1 != 0)
    }

    #[cfg(test)]
    pub(crate) fn runtime_evidence(&self, cursor: Option<u64>) -> EvidenceRead {
        self.runtime_evidence_at(cursor, SystemTime::now())
    }

    /// Lifecycle evidence for the runtime reducer at wall-clock time `now`:
    /// title edges after `cursor` (`None`: from the current binding's start)
    /// and whether a turn was submitted in the current binding.
    pub(crate) fn runtime_evidence_at(&self, cursor: Option<u64>, now: SystemTime) -> EvidenceRead {
        self.evidence.lock().read_at(cursor, now)
    }

    /// Coalesced lifecycle invalidations for the runtime reducer. Receivers
    /// share this queue; a consumer must reconcile the latest session facts.
    pub fn runtime_events(&self) -> async_channel::Receiver<()> {
        self.runtime_events.clone()
    }

    fn trigger_runtime(&self) {
        let _ = self.runtime_trigger.try_send(());
    }

    /// Paste text into the PTY, honoring bracketed-paste mode (so the program
    /// receives it as a paste, not as typed keystrokes). Shared by the keyboard
    /// shortcut and the mouse copy/paste modes.
    pub fn paste(&self, text: &str) {
        let _ = self.try_paste(text);
    }

    /// Paste text and report admission to the PTY writer. Success does not
    /// confirm that the child consumed the text.
    pub fn try_paste(&self, text: &str) -> std::io::Result<()> {
        let payload = if self.is_bracketed_paste() {
            format!("\x1b[200~{}\x1b[201~", text.replace('\x1b', ""))
        } else {
            text.replace("\r\n", "\r").replace('\n', "\r")
        };
        self.try_write_input(payload.as_bytes())
    }

    /// Paste filesystem paths into the PTY (drag-drop / clipboard files).
    ///
    /// Validate the complete batch before writing. Refused filenames never
    /// produce partial input; callers show the error to the user.
    pub fn paste_paths(&self, paths: &[std::path::PathBuf]) -> Result<(), PastePathError> {
        let paths = paths
            .iter()
            .map(|path| path.to_str().ok_or(PastePathError::NonUnicode))
            .collect::<Result<Vec<_>, _>>()?;
        self.paste_path_strings(&paths)
    }

    /// Remote uploads use the same launch target and validation as local files.
    pub fn paste_path_strings(&self, paths: &[&str]) -> Result<(), PastePathError> {
        let text = muxel_core::path_paste::format_paths(self.paste_shell, paths)?;
        if !text.is_empty() {
            self.paste(&text);
        }
        Ok(())
    }

    /// Send a mouse or wheel report. It reaches the program as input, so it
    /// advances the input revision, but it isn't text: the draft bit and the
    /// scroll position stay as they were.
    fn write_pointer(&self, data: &[u8]) {
        if !data.is_empty() {
            self.advance_input(None);
        }
        self.write_raw(data);
    }

    /// Write bytes to the PTY without touching the scroll position (used for
    /// non-input writes like focus reports).
    fn write_raw(&self, data: &[u8]) {
        let _ = self.try_write_raw(data);
    }

    fn try_write_raw(&self, data: &[u8]) -> std::io::Result<()> {
        let mut writer = self.writer.lock();
        writer.write_all(data)?;
        writer.flush()
    }

    /// Scroll the viewport by a mouse-wheel delta. `delta_y` is the pixel delta
    /// (positive = up / into scrollback); sub-line remainders are accumulated
    /// across events for smooth trackpad scrolling. `col`/`row` are the 0-based
    /// cell under the pointer, used only when the wheel is forwarded as a mouse
    /// report.
    ///
    /// The notches are dispatched the same three ways every terminal uses: if the
    /// app enabled mouse reporting (opencode, grok, vim-with-mouse) the wheel is
    /// forwarded as a mouse event so the app scrolls itself; on the alternate
    /// screen with alternate-scroll and no mouse reporting (e.g. plain `less`) it
    /// is emulated with cursor-key presses; otherwise it moves through our own
    /// scrollback locally.
    ///
    /// Returns whether anything changed (so the caller can request a repaint).
    pub fn scroll_wheel(&self, delta_y: f32, line_height: f32, col: usize, row: usize) -> bool {
        if line_height <= 0.0 {
            return false;
        }
        let lines = {
            let mut acc = self.scroll_accum.lock();
            *acc += delta_y;
            let lines = (*acc / line_height) as i32;
            if lines != 0 {
                *acc -= lines as f32 * line_height;
            }
            lines
        };
        if lines == 0 {
            return false;
        }
        // Only a person turns the wheel, whichever way it is dispatched below.
        self.mark_user_input();

        // Decide what the wheel means, reading every relevant mode under one lock.
        enum Wheel {
            Scrolled,
            MouseReport { sgr: bool },
            Arrows { app_cursor: bool },
        }
        let action = {
            let mut term = self.term.lock();
            let mode = term.mode();
            if mode.intersects(TermMode::MOUSE_MODE) {
                Wheel::MouseReport {
                    sgr: mode.contains(TermMode::SGR_MOUSE),
                }
            } else if mode.contains(TermMode::ALT_SCREEN)
                && mode.contains(TermMode::ALTERNATE_SCROLL)
            {
                Wheel::Arrows {
                    app_cursor: mode.contains(TermMode::APP_CURSOR),
                }
            } else {
                term.scroll_display(Scroll::Delta(lines));
                Wheel::Scrolled
            }
        };

        let count = lines.unsigned_abs().min(100) as usize;
        match action {
            Wheel::Scrolled => {
                self.bump_content();
            }
            Wheel::MouseReport { sgr } => {
                let mut buf = Vec::with_capacity(count * 16);
                for _ in 0..count {
                    push_wheel_report(&mut buf, lines > 0, col, row, sgr);
                }
                self.write_pointer(&buf);
            }
            Wheel::Arrows { app_cursor } => {
                let seq: &[u8] = match (lines > 0, app_cursor) {
                    (true, true) => b"\x1bOA", // scroll up → Up arrow
                    (true, false) => b"\x1b[A",
                    (false, true) => b"\x1bOB", // scroll down → Down arrow
                    (false, false) => b"\x1b[B",
                };
                for _ in 0..count {
                    self.write_pointer(seq);
                }
            }
        }
        true
    }

    /// The grid size currently applied to the PTY, `(cols, rows)`.
    ///
    /// Worth persisting: a pane that respawns at the size it last had opens its
    /// program *already* at the right size, so nothing has to resize after the first
    /// paint. That matters most for a `tmux attach`, where the session's program has
    /// long since drawn its UI — it repaints only when something prompts it to, so a
    /// late resize leaves the first frame visibly mis-spaced until the user types.
    pub fn size(&self) -> (u16, u16) {
        self.resize.lock().applied
    }

    /// Resize the PTY and emulator grid, **debounced**: the requested size must
    /// hold steady for [`RESIZE_SETTLE`] before it's applied, so a burst of
    /// changes from a pane close / divider drag collapses into one resize (one
    /// SIGWINCH) instead of many. Returns `true` while a resize is still
    /// settling — the caller should schedule another frame for *this* view
    /// (`request_animation_frame`, not `window.refresh`) so the pending resize
    /// lands without invalidating every cached terminal.
    #[must_use]
    pub fn resize(&self, cols: u16, rows: u16) -> bool {
        let cols = cols.max(1);
        let rows = rows.max(1);
        let mut st = self.resize.lock();
        if st.applied == (cols, rows) {
            return false; // already at this size
        }
        if st.target != (cols, rows) {
            // New target: start (or restart) the settle window.
            st.target = (cols, rows);
            st.since = Instant::now();
            return true;
        }
        if st.since.elapsed() < RESIZE_SETTLE {
            return true; // same target, still settling
        }
        // Settled — apply to the PTY (SIGWINCH) and the grid together.
        if let Some(master) = self.master.inner.lock().as_ref() {
            let _ = master.resize(PtySize {
                rows,
                cols,
                pixel_width: 0,
                pixel_height: 0,
            });
        }
        self.term
            .lock()
            .resize(TermSize::new(cols as usize, rows as usize));
        st.applied = (cols, rows);
        self.bump_content();
        self.trigger_runtime();
        false
    }

    /// Clear the scrollback history (keeps the current screen) and snap the view
    /// to the bottom.
    pub fn clear_scrollback(&self) {
        let mut term = self.term.lock();
        term.grid_mut().clear_history();
        term.scroll_display(Scroll::Bottom);
        drop(term);
        self.bump_content();
    }

    /// Set the search needle the element highlights (empty string clears it).
    pub fn set_search(&self, needle: &str) {
        let chars: Vec<char> = needle.chars().collect();
        let mut cur = self.search.lock();
        if *cur == chars {
            return;
        }
        *cur = chars;
        drop(cur);
        self.bump_content();
    }

    /// The current search needle (chars), for the element to highlight.
    pub(crate) fn search_needle(&self) -> Vec<char> {
        self.search.lock().clone()
    }

    /// The local working directory the child was spawned in, if any.
    pub fn cwd(&self) -> Option<&std::path::Path> {
        self.cwd.as_deref()
    }

    /// The ctrl-hovered link span, if any (painted as an underline).
    pub(crate) fn hovered_link(&self) -> Option<HoveredLink> {
        self.hovered_link.lock().clone()
    }

    /// Replace the hovered-link state; returns whether it actually changed (so
    /// callers only repaint on transitions, not every mouse move).
    pub(crate) fn set_hovered_link(&self, link: Option<HoveredLink>) -> bool {
        let mut cur = self.hovered_link.lock();
        if *cur == link {
            return false;
        }
        *cur = link;
        // Link underline is painted live outside the draw-list cache — no gen bump.
        true
    }

    /// Remember the last pointer position over the grid (updated on every move).
    pub(crate) fn set_pointer_hit(&self, hit: Option<PointerHit>) {
        *self.pointer_hit.lock() = hit;
    }

    pub(crate) fn pointer_hit(&self) -> Option<PointerHit> {
        *self.pointer_hit.lock()
    }

    /// Buffer-line indices (negative = history) containing `needle`
    /// (case-insensitive), oldest to newest. Coordinates match the element's
    /// `grid[Line(n)]`, so [`Self::scroll_to_line`] brings one into view.
    pub fn search_match_lines(&self, needle: &str) -> Vec<i32> {
        use alacritty_terminal::index::{Column, Line, Point as GridPoint};
        let needle: Vec<char> = needle.chars().collect();
        if needle.is_empty() {
            return Vec::new();
        }
        let term = self.term.lock();
        let grid = term.grid();
        let cols = grid.columns();
        let screen = grid.screen_lines() as i32;
        let hist = grid.history_size() as i32;
        let mut out = Vec::new();
        for line in -hist..screen {
            let chars: Vec<char> = (0..cols)
                .map(|c| {
                    grid[GridPoint {
                        line: Line(line),
                        column: Column(c),
                    }]
                    .c
                })
                .collect();
            if crate::search::line_contains(&chars, &needle) {
                out.push(line);
            }
        }
        out
    }

    /// Scroll so buffer `line` (negative = history) is at/near the top of view.
    pub fn scroll_to_line(&self, line: i32) {
        self.set_display_offset((-line).max(0) as usize);
    }

    /// Live `(history_size, display_offset, screen_lines)` — used to lay out the
    /// scrollbar.
    pub fn grid_metrics(&self) -> (usize, usize, usize) {
        let term = self.term.lock();
        let grid = term.grid();
        (
            grid.history_size(),
            grid.display_offset(),
            grid.screen_lines(),
        )
    }

    /// Scroll so exactly `target` history lines sit above the viewport (clamped
    /// to the available history). Drives the draggable scrollbar.
    pub fn set_display_offset(&self, target: usize) {
        let mut term = self.term.lock();
        let target = target.min(term.grid().history_size());
        let cur = term.grid().display_offset();
        let delta = target as i32 - cur as i32;
        if delta != 0 {
            term.scroll_display(Scroll::Delta(delta));
            drop(term);
            self.bump_content();
        }
    }

    pub(crate) fn scrollbar_drag_start(&self, grab: f32) {
        *self.scrollbar_drag.lock() = Some(grab);
    }
    pub(crate) fn scrollbar_drag_end(&self) {
        *self.scrollbar_drag.lock() = None;
    }
    /// The grab offset within the thumb while a scrollbar drag is in progress.
    pub(crate) fn scrollbar_grab(&self) -> Option<f32> {
        *self.scrollbar_drag.lock()
    }

    /// Read the terminal grid for rendering.
    pub(crate) fn with_term<R>(&self, f: impl FnOnce(&Term<MuxelListener>) -> R) -> R {
        let term = self.term.lock();
        f(&term)
    }

    /// Whether the program currently wants its cursor drawn (DECTCEM, `CSI ?25h`
    /// / `?25l`). A TUI hides it while it redraws and shows it again after, so
    /// this flips constantly mid-frame — see the cursor-hide defer in `view.rs`.
    pub(crate) fn cursor_visible(&self) -> bool {
        self.with_term(|term| term.mode().contains(TermMode::SHOW_CURSOR))
    }

    /// Cursor position metadata for the profiler. Terminal contents stay out of
    /// profiler state and logs.
    pub(crate) fn cursor_position(&self) -> (usize, i32) {
        self.with_term(|term| {
            let cursor = term.grid().cursor.point;
            (cursor.column.0, cursor.line.0)
        })
    }

    /// The visible screen as text (one row per line, newline-separated). Used for
    /// marker-based agent-status detection (e.g. scanning for "esc to interrupt").
    pub(crate) fn visible_text(&self) -> String {
        self.startup_screen().text
    }

    /// Capture input geometry and modes with the same grid generation.
    pub(crate) fn startup_screen(&self) -> crate::startup::Screen {
        use alacritty_terminal::index::{Column, Line, Point as GridPoint};
        self.with_term(|term| {
            let grid = term.grid();
            let rows = grid.screen_lines();
            let cols = grid.columns();
            let mut s = String::with_capacity(rows * (cols + 1));
            for row in 0..rows {
                for col in 0..cols {
                    s.push(
                        grid[GridPoint {
                            line: Line(row as i32),
                            column: Column(col),
                        }]
                        .c,
                    );
                }
                s.push('\n');
            }
            let cursor = grid.cursor.point;
            crate::startup::Screen {
                text: s,
                cursor_column: cursor.column.0,
                cursor_row: cursor.line.0,
                cursor_visible: term.mode().contains(TermMode::SHOW_CURSOR),
                bracketed_paste: term.mode().contains(TermMode::BRACKETED_PASTE),
            }
        })
    }

    /// The last `max_lines` rows of the buffer — scrollback as well as screen — as
    /// text, with soft-wrapped rows rejoined into the lines they were written as.
    /// Used by read-aloud, whose reply has often scrolled off the top of the screen.
    pub(crate) fn recent_text(&self, max_lines: usize) -> String {
        use alacritty_terminal::index::{Column, Line};
        use alacritty_terminal::term::cell::Flags;
        self.with_term(|term| {
            let grid = term.grid();
            let cols = grid.columns();
            let screen = grid.screen_lines() as i32;
            let first = (screen - max_lines as i32).max(-(grid.history_size() as i32));
            let mut out = String::new();
            for line in first..screen {
                let row = &grid[Line(line)];
                let mut text = String::with_capacity(cols);
                for col in 0..cols {
                    let cell = &row[Column(col)];
                    // The second half of a wide glyph is a placeholder, not a space.
                    if cell
                        .flags
                        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
                    {
                        continue;
                    }
                    text.push(cell.c);
                    if let Some(combining) = cell.zerowidth() {
                        text.extend(combining);
                    }
                }
                let wrapped = cols > 0 && row[Column(cols - 1)].flags.contains(Flags::WRAPLINE);
                if wrapped {
                    out.push_str(&text);
                } else {
                    out.push_str(text.trim_end());
                    out.push('\n');
                }
            }
            out
        })
    }

    /// Mutate the terminal (e.g. to update the text selection).
    pub(crate) fn with_term_mut<R>(&self, f: impl FnOnce(&mut Term<MuxelListener>) -> R) -> R {
        let mut term = self.term.lock();
        let out = f(&mut term);
        drop(term);
        // Selection / other visual mutations go through here.
        self.bump_content();
        out
    }

    /// The currently-selected text, if any.
    pub fn selection_to_string(&self) -> Option<String> {
        self.term.lock().selection_to_string()
    }

    /// Clear any active text selection. Returns whether there was one (so the
    /// caller can skip a repaint when nothing visually changed).
    pub fn clear_selection(&self) -> bool {
        let mut term = self.term.lock();
        let had = term.selection.is_some();
        term.selection = None;
        drop(term);
        if had {
            self.bump_content();
        }
        had
    }

    pub(crate) fn start_selecting(&self) {
        self.selecting.store(true, Ordering::Relaxed);
    }
    pub(crate) fn stop_selecting(&self) {
        self.selecting.store(false, Ordering::Relaxed);
    }
    pub(crate) fn is_selecting(&self) -> bool {
        self.selecting.load(Ordering::Relaxed)
    }

    /// Whether the app has enabled DECCKM (application cursor keys).
    pub fn is_app_cursor_mode(&self) -> bool {
        self.term.lock().mode().contains(TermMode::APP_CURSOR)
    }

    /// Whether the app has enabled bracketed paste mode.
    pub fn is_bracketed_paste(&self) -> bool {
        self.term.lock().mode().contains(TermMode::BRACKETED_PASTE)
    }

    /// Report a focus change to the PTY (CSI I / CSI O), but only if the running
    /// program requested focus reporting (DECSET 1004). Lets agents like Claude
    /// know whether their pane is the one the user is looking at.
    pub fn report_focus(&self, focused: bool) {
        self.focused.store(focused, Ordering::Relaxed);
        if self.term.lock().mode().contains(TermMode::FOCUS_IN_OUT) {
            // Raw write: a focus report must not yank the viewport to the bottom
            // (e.g. clicking a scrolled-up pane to read its history).
            self.write_raw(if focused { b"\x1b[I" } else { b"\x1b[O" });
        }
    }

    /// Whether the UI currently treats this terminal as focused (for paint/drain budget).
    pub fn is_focused(&self) -> bool {
        self.focused.load(Ordering::Relaxed)
    }

    /// The most recent OSC title, if any.
    pub fn title(&self) -> Option<String> {
        self.title.lock().clone()
    }

    /// Feed PTY bytes through the production parser from the developer title
    /// probe. Product code should let `TerminalView` drain output instead.
    #[doc(hidden)]
    pub fn process_probe_output(&self, data: &[u8]) {
        self.process_output(data);
    }

    /// Latest OSC title plus its change generation. Unlike [`Self::title`], this
    /// lets lifecycle consumers observe a ResetTitle or a repeated title event.
    pub fn title_snapshot(&self) -> (u64, Option<String>) {
        (
            self.title_generation.load(Ordering::Relaxed),
            self.title.lock().clone(),
        )
    }

    /// Age of the latest title event. Title-derived lifecycle claims must expire
    /// rather than treating a cached spinner frame as permanent Working.
    pub fn title_age(&self) -> Option<Duration> {
        self.title_changed_at.lock().as_ref().map(Instant::elapsed)
    }

    /// The running TUI switched provider conversations at some instant in
    /// `[from, to]` (one instant when the report carries its own time), as
    /// reported out of band: a session hook's record or a registry row, read on
    /// a later tick. Its clock can't be ordered exactly against PTY output, so
    /// a new binding begins with an ambiguity window around the switch. The
    /// current title stays as level state unless it was read before the switch
    /// could have happened (the next OSC title repopulates it), and the bell is
    /// cleared. Output read inside the window forms no completion edge and
    /// rings no bell; the reducer waits for evidence read after it.
    pub fn provider_switched(&self, from: SystemTime, to: SystemTime) {
        self.provider_switched_at(from, to, SystemTime::now());
    }

    pub(crate) fn provider_switched_at(
        &self,
        from: SystemTime,
        to: SystemTime,
        learned_at: SystemTime,
    ) {
        let evidence = &mut *self.evidence.lock();
        let switch = evidence.switch_binding(from, to, learned_at);
        if switch.bell {
            self.bell.store(false, Ordering::Relaxed);
        }
        if switch.title && self.title.lock().take().is_some() {
            self.title_generation.fetch_add(1, Ordering::Relaxed);
            *self.title_changed_at.lock() = Some(Instant::now());
        }
        // The reducer's screen facts reset with the binding; a new content
        // generation makes it recapture them.
        self.bump_content();
        self.trigger_runtime();
    }

    /// Latest provider-owned OSC session hint retained across ordinary title
    /// changes. Muxel resolves and validates it before adopting the identity.
    pub fn session_id_hint(&self) -> Option<String> {
        self.session_id_hint.lock().clone()
    }

    /// Revision of the provider conversation bound to this live PTY launch.
    pub fn binding_revision(&self) -> u64 {
        self.evidence.lock().binding_rev()
    }

    /// PID of the process attached directly to this PTY. On Windows agent
    /// launchers may insert a PowerShell/cmd wrapper; provider processes remain
    /// descendants of this PID.
    pub fn child_pid(&self) -> Option<u32> {
        self.child_pid
    }

    /// Drain the OSC-52 copies parsed since the last call. The view lands them
    /// on the system clipboard (this crate has no gpui context of its own here).
    pub(crate) fn take_clipboard_stores(&self) -> Vec<(ClipboardType, String)> {
        std::mem::take(&mut *self.clipboard_store.lock())
    }

    /// Replace the palette color queries are answered from — pushed by the view
    /// whenever the app theme (re)applies, so answers track what's painted.
    pub(crate) fn set_palette(&self, palette: TerminalPalette) {
        *self.palette.lock() = palette;
        self.bump_content();
    }

    /// Consume the "bell rang" edge.
    pub fn take_bell(&self) -> bool {
        self.bell.swap(false, Ordering::Relaxed)
    }

    /// Whether the bell has rung (non-consuming).
    pub fn has_bell(&self) -> bool {
        self.bell.load(Ordering::Relaxed)
    }

    /// Clear the bell (e.g. once the user focuses the pane).
    pub fn clear_bell(&self) {
        self.bell.store(false, Ordering::Relaxed);
        self.trigger_runtime();
    }

    /// Time since output was last processed (for idle detection).
    pub fn idle_for(&self) -> Duration {
        self.last_output.lock().elapsed()
    }

    /// Whether the child is sitting idle at its prompt with no foreground command
    /// running — i.e. the terminal's foreground process group *is* the child
    /// itself. A shell that's running `vim`/`make`/etc. puts that command in a new
    /// foreground group, so this returns `false`. Used to skip the close
    /// confirmation for an untouched shell pane.
    ///
    /// `false` when it can't be determined — no foreground group, an unknown child
    /// pid, or a platform without `tcgetpgrp` (Windows) — so callers stay safe and
    /// confirm as usual.
    pub fn is_idle_foreground(&self) -> bool {
        #[cfg(unix)]
        {
            match (
                self.master
                    .inner
                    .lock()
                    .as_ref()
                    .and_then(|master| master.process_group_leader()),
                self.child_pid,
            ) {
                (Some(fg), Some(pid)) => fg == pid as libc::pid_t,
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    /// Kill the child process. On Unix this sends SIGHUP, at most once per
    /// child: a later `kill`, the session's Drop and the reader skip theirs.
    pub fn kill(&self) {
        #[cfg(unix)]
        if self.output_queue.child_hangup.claim() {
            let _ = self.killer.lock().kill();
        }
        #[cfg(not(unix))]
        let _ = self.killer.lock().kill();
    }

    /// Whether the child enabled any mouse-reporting mode (clicks / drag / motion).
    /// When true, left-clicks should be forwarded as mouse reports instead of
    /// starting a local text selection (unless Shift is held).
    pub fn mouse_reporting(&self) -> bool {
        self.term.lock().mode().intersects(TermMode::MOUSE_MODE)
    }

    /// Whether motion events should be reported (any-event or cell-motion / drag).
    pub fn mouse_motion_reporting(&self) -> bool {
        self.term
            .lock()
            .mode()
            .intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG)
    }

    /// Send a mouse button press/release at 0-based cell (`col`, `row`).
    /// `button`: 0 left, 1 middle, 2 right.
    #[allow(clippy::too_many_arguments)]
    pub fn report_mouse_button(
        &self,
        col: usize,
        row: usize,
        button: u8,
        pressed: bool,
        shift: bool,
        alt: bool,
        control: bool,
    ) {
        let sgr = self.term.lock().mode().contains(TermMode::SGR_MOUSE);
        let mods = (if shift { 4 } else { 0 })
            + (if alt { 8 } else { 0 })
            + (if control { 16 } else { 0 });
        let mut buf = Vec::with_capacity(24);
        push_mouse_report(&mut buf, button + mods, col, row, pressed, sgr);
        self.mark_user_input();
        self.write_pointer(&buf);
        // Remember an outstanding press so the matching release is guaranteed.
        self.mouse_pressed_button.store(
            if pressed { button } else { NO_MOUSE_PRESS },
            Ordering::Relaxed,
        );
    }

    /// The button (0/1/2) whose press was forwarded and not yet released, if any.
    /// The mouse-up handler uses this to always send the release.
    pub fn mouse_press_pending(&self) -> Option<u8> {
        match self.mouse_pressed_button.load(Ordering::Relaxed) {
            NO_MOUSE_PRESS => None,
            b => Some(b),
        }
    }

    /// Send a mouse motion event. `button` is the pressed button (0/1/2) or `None`
    /// for motion with no button. Only emitted when the app asked for drag/motion
    /// reports. Button-held motion uses code 32+button (SGR motion encoding).
    #[allow(clippy::too_many_arguments)]
    pub fn report_mouse_motion(
        &self,
        col: usize,
        row: usize,
        button: Option<u8>,
        shift: bool,
        alt: bool,
        control: bool,
    ) {
        let (sgr, base) = {
            let term = self.term.lock();
            let mode = term.mode();
            if !mode.intersects(TermMode::MOUSE_MOTION | TermMode::MOUSE_DRAG) {
                return;
            }
            // Drag mode only reports motion while a button is held.
            if mode.contains(TermMode::MOUSE_DRAG)
                && !mode.contains(TermMode::MOUSE_MOTION)
                && button.is_none()
            {
                return;
            }
            let sgr = mode.contains(TermMode::SGR_MOUSE);
            let base = match button {
                Some(b) => 32 + b, // button-held motion
                None => 35,        // motion with no button
            };
            (sgr, base)
        };
        let mods = (if shift { 4 } else { 0 })
            + (if alt { 8 } else { 0 })
            + (if control { 16 } else { 0 });
        let mut buf = Vec::with_capacity(24);
        // Motion is always "press" encoding in SGR (`M`).
        push_mouse_report(&mut buf, base + mods, col, row, true, sgr);
        self.mark_user_input();
        self.write_pointer(&buf);
    }
}

/// Append one mouse-wheel report — button 64 (scroll up) / 65 (scroll down) — at
/// the 0-based cell (`col`, `row`), in SGR (1006) or legacy X10 encoding. Wheel
/// events are press-only (no release), so the SGR form always ends in `M`.
fn push_wheel_report(buf: &mut Vec<u8>, up: bool, col: usize, row: usize, sgr: bool) {
    let cb = if up { 64 } else { 65 };
    push_mouse_report(buf, cb, col, row, true, sgr);
}

/// Encode one mouse event at 0-based (`col`, `row`) with SGR or X10 encoding.
fn push_mouse_report(
    buf: &mut Vec<u8>,
    button: u8,
    col: usize,
    row: usize,
    pressed: bool,
    sgr: bool,
) {
    if sgr {
        // ESC [ < Cb ; Cx ; Cy M/m   (1-based coordinates)
        let c = if pressed { 'M' } else { 'm' };
        buf.extend_from_slice(format!("\x1b[<{};{};{}{c}", button, col + 1, row + 1).as_bytes());
    } else {
        // ESC [ M  Cb+32  Cx+32  Cy+32   (1-based coords, classic 223-cell ceiling)
        // Release uses button 3 in normal encoding.
        let cb = if pressed { button } else { 3 };
        let enc = |v: usize| -> u8 { ((v + 1).min(223) + 32) as u8 };
        buf.extend_from_slice(&[0x1b, b'[', b'M', cb.saturating_add(32), enc(col), enc(row)]);
    }
}

/// Resolve a literal launch without permitting a batch interpreter to reparse
/// argv. Windows npm Codex wrappers resolve to their packaged native executable.
pub fn literal_program_for_spawn(program: &str) -> anyhow::Result<String> {
    #[cfg(windows)]
    let path = std::path::Path::new(program);
    #[cfg(windows)]
    let resolved = if path.is_file()
        && path
            .extension()
            .and_then(|ext| ext.to_str())
            .is_some_and(|ext| ext.eq_ignore_ascii_case("exe") || ext.eq_ignore_ascii_case("com"))
    {
        program.to_string()
    } else {
        resolve_program_for_spawn(program)
    };
    #[cfg(not(windows))]
    let resolved = resolve_program_for_spawn(program);
    #[cfg(windows)]
    let resolved = {
        // Resolution can return a relative PATH entry. Persist an absolute target
        // so neither the PTY cwd nor a later PATH change can choose another file.
        let path = std::path::absolute(&resolved)?;
        path.to_string_lossy().into_owned()
    };
    validate_literal_program(&resolved)?;
    Ok(resolved)
}

fn validate_literal_program(program: &str) -> Result<()> {
    #[cfg(windows)]
    {
        anyhow::ensure!(
            !is_windows_batch_program(program),
            "literal argv cannot run Windows batch script '{program}'; choose its native executable or use --preset"
        );
        let path = std::path::Path::new(program);
        anyhow::ensure!(
            path.is_absolute() && path.is_file(),
            "literal executable '{program}' must be an existing absolute native executable"
        );
    }
    #[cfg(not(windows))]
    let _ = program;
    Ok(())
}

/// Resolve `program` to a path CreateProcess can launch on Windows.
///
/// portable-pty's PATH search prefers an extension-less file over PATHEXT
/// (`.exe`/`.cmd`). npm puts a Unix `#!/bin/sh` shim at that name, so agents
/// like Codex fail with os error 193 ("%1 is not a valid Win32 application").
/// Prefer PATHEXT matches and skip shebang scripts; leave non-Windows alone.
fn resolve_program_for_spawn(program: &str) -> String {
    #[cfg(not(windows))]
    {
        program.to_string()
    }
    #[cfg(windows)]
    {
        resolve_program_for_spawn_windows(program)
    }
}

fn command_builder_for_spawn(program: &str, args: &[String]) -> CommandBuilder {
    #[cfg(not(windows))]
    {
        let mut builder = CommandBuilder::new(program);
        for arg in args {
            builder.arg(arg);
        }
        builder
    }
    #[cfg(windows)]
    {
        if !is_windows_batch_program(program) {
            let mut builder = CommandBuilder::new(program);
            for arg in args {
                builder.arg(arg);
            }
            return builder;
        }

        // CreateProcess cannot execute batch files, and cmd.exe reparses a
        // reconstructed command string (`%*`, quotes, and metacharacters).
        // Keep the public PATH target, but move each original argv token
        // through child-only environment slots. Batch files still require a
        // final cmd.exe parse, so reject metacharacters that cmd can reinterpret
        // instead of silently launching a different command.
        const BATCH_RUNNER: &str = concat!(
            "$program = $env:MUXEL_BATCH_PROGRAM; ",
            "$argv = @(for ($i = 0; $i -lt [int]$env:MUXEL_BATCH_ARG_COUNT; $i++) { ",
            "$value = [Environment]::GetEnvironmentVariable(('MUXEL_BATCH_ARG_' + $i)); ",
            "if ($null -eq $value) { '' } else { $value } }); ",
            "$unsafe = [char[]]'\"%&|<>^' + [char[]]([char]13,[char]10); ",
            "if ($argv | Where-Object { $_.IndexOfAny($unsafe) -ge 0 }) { ",
            "[Console]::Error.WriteLine('muxel: batch-file arguments contain characters cmd.exe can reinterpret'); exit 2 }; ",
            "Remove-Item Env:MUXEL_BATCH_PROGRAM,Env:MUXEL_BATCH_ARG_COUNT -ErrorAction SilentlyContinue; ",
            "for ($i = 0; $i -lt $argv.Count; $i++) { ",
            "Remove-Item ('Env:MUXEL_BATCH_ARG_' + $i) -ErrorAction SilentlyContinue }; ",
            "$invoke_argv = @($argv | ForEach-Object { ",
            "if ($_.Length -eq 0) { '\"\"' } else { $_ } }); ",
            "& $program @invoke_argv; ",
            "if ($null -eq $LASTEXITCODE) { exit 1 }; exit $LASTEXITCODE"
        );
        let encoded_runner = {
            use base64::Engine;
            let bytes = BATCH_RUNNER
                .encode_utf16()
                .flat_map(u16::to_le_bytes)
                .collect::<Vec<_>>();
            base64::engine::general_purpose::STANDARD.encode(bytes)
        };
        let mut builder = CommandBuilder::new("powershell.exe");
        builder.arg("-NoLogo");
        builder.arg("-NoProfile");
        builder.arg("-NonInteractive");
        builder.arg("-EncodedCommand");
        builder.arg(encoded_runner);
        apply_windows_batch_runner_env(&mut builder, program, args);
        builder
    }
}

#[cfg(windows)]
fn is_windows_batch_program(program: &str) -> bool {
    std::path::Path::new(program)
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat"))
}

fn apply_windows_batch_runner_env(builder: &mut CommandBuilder, program: &str, args: &[String]) {
    #[cfg(not(windows))]
    let _ = (builder, program, args);
    #[cfg(windows)]
    if is_windows_batch_program(program) {
        builder.env("MUXEL_BATCH_PROGRAM", program);
        builder.env("MUXEL_BATCH_ARG_COUNT", args.len().to_string());
        for (index, arg) in args.iter().enumerate() {
            builder.env(format!("MUXEL_BATCH_ARG_{index}"), arg);
        }
    }
}

#[cfg(windows)]
fn resolve_program_for_spawn_windows(program: &str) -> String {
    use std::path::{Path, PathBuf};

    fn prefer_packaged_codex(path: PathBuf) -> PathBuf {
        let is_codex_batch = path
            .file_stem()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.eq_ignore_ascii_case("codex"))
            && path
                .extension()
                .and_then(|ext| ext.to_str())
                .is_some_and(|ext| {
                    ext.eq_ignore_ascii_case("cmd") || ext.eq_ignore_ascii_case("bat")
                });
        if !is_codex_batch {
            return path;
        }
        let Some(npm_bin) = path.parent() else {
            return path;
        };
        let (package, target) = if cfg!(target_arch = "aarch64") {
            ("codex-win32-arm64", "aarch64-pc-windows-msvc")
        } else {
            ("codex-win32-x64", "x86_64-pc-windows-msvc")
        };
        let root = npm_bin.join("node_modules").join("@openai").join("codex");
        [
            root.join("node_modules")
                .join("@openai")
                .join(package)
                .join("vendor")
                .join(target)
                .join("bin")
                .join("codex.exe"),
            root.join("vendor")
                .join(target)
                .join("bin")
                .join("codex.exe"),
        ]
        .into_iter()
        .find(|candidate| candidate.is_file())
        .unwrap_or(path)
    }

    fn is_shebang(path: &Path) -> bool {
        use std::io::Read;
        let mut f = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => return false,
        };
        let mut buf = [0u8; 2];
        matches!(f.read(&mut buf), Ok(2) if &buf == b"#!")
    }

    fn is_spawnable(path: &Path) -> bool {
        if !path.is_file() {
            return false;
        }
        // CreateProcess runs .exe/.com and host scripts for .cmd/.bat.
        // Extension-less npm shims are `#!/bin/sh` and are not.
        match path.extension().and_then(|e| e.to_str()).map(|e| {
            e.eq_ignore_ascii_case("exe")
                || e.eq_ignore_ascii_case("com")
                || e.eq_ignore_ascii_case("cmd")
                || e.eq_ignore_ascii_case("bat")
        }) {
            Some(true) => true,
            _ => !is_shebang(path),
        }
    }

    fn pathexts() -> Vec<String> {
        std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string())
            .split(';')
            .filter(|e| !e.is_empty())
            .map(|e| e.trim_start_matches('.').to_string())
            .collect()
    }

    fn try_with_exts(base: PathBuf, exts: &[String]) -> Option<PathBuf> {
        for ext in exts {
            let mut candidate = base.as_os_str().to_owned();
            candidate.push(".");
            candidate.push(ext);
            let cand = PathBuf::from(candidate);
            if is_spawnable(&cand) {
                return Some(prefer_packaged_codex(cand));
            }
        }
        if is_spawnable(&base) {
            return Some(prefer_packaged_codex(base));
        }
        None
    }

    let path = Path::new(program);
    // Absolute or relative path with a separator: prefer a spawnable sibling.
    if program.contains('\\') || program.contains('/') {
        let exts = pathexts();
        if let Some(p) = try_with_exts(path.to_path_buf(), &exts) {
            return p.to_string_lossy().into_owned();
        }
        return program.to_string();
    }

    // Bare name: search PATH, PATHEXT before bare (opposite of portable-pty).
    let exts = pathexts();
    let search_native_codex = program.eq_ignore_ascii_case("codex");
    let mut codex_batch_fallback = None;
    if let Some(paths) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&paths) {
            if let Some(p) = try_with_exts(dir.join(program), &exts) {
                if search_native_codex && is_windows_batch_program(&p.to_string_lossy()) {
                    codex_batch_fallback.get_or_insert(p);
                    continue;
                }
                return p.to_string_lossy().into_owned();
            }
        }
    }
    if let Some(path) = codex_batch_fallback {
        return path.to_string_lossy().into_owned();
    }
    program.to_string()
}

/// Give the child a UTF-8 locale when it would otherwise inherit none.
///
/// A GUI app gets no locale from the OS session — on macOS `launchctl getenv LANG`
/// is empty — so children of a Finder-launched muxel see no `LANG`/`LC_ALL`/
/// `LC_CTYPE` and fall back to ASCII. tmux is the visible casualty: its client
/// rewrites every non-ASCII cell as `_`. Only fills a gap; a locale the user has
/// actually set is never touched. See `muxel_core::locale`.
///
/// Unix only: Windows has no locale environment variables (the console is driven
/// by code pages), and tmux doesn't run there.
#[cfg(unix)]
fn ensure_utf8_locale(builder: &mut CommandBuilder) {
    let var = |k: &str| std::env::var(k).ok();
    if muxel_core::locale::needs_utf8_locale(
        var("LC_ALL").as_deref(),
        var("LC_CTYPE").as_deref(),
        var("LANG").as_deref(),
    ) {
        builder.env("LANG", muxel_core::locale::FALLBACK_UTF8_LOCALE);
    }
}

#[cfg(not(unix))]
fn ensure_utf8_locale(_builder: &mut CommandBuilder) {}

/// `spec`'s variables on top of what `builder` already has (muxel's own
/// environment and its defaults), then without `spec.env_remove`, which covers
/// both.
fn apply_spec_env(builder: &mut CommandBuilder, spec: &CommandSpec) {
    for (k, v) in &spec.env {
        builder.env(k, v);
    }
    for k in &spec.env_remove {
        builder.env_remove(k);
    }
}

/// Strip an AppImage runtime's environment leakage from a child command, so a
/// shell/agent muxel spawns gets a clean system environment. No-op unless muxel
/// itself is running from an AppImage (`$APPIMAGE` set).
fn sanitize_appimage_env(builder: &mut CommandBuilder) {
    let Some(appimage) = std::env::var_os("APPIMAGE") else {
        return;
    };
    let appdir = std::env::var_os("APPDIR").unwrap_or_default();
    let vars: Vec<_> = std::env::vars_os().collect();
    let (drop, overrides) = appimage_env_fixups(&vars, &appimage, &appdir);
    for k in drop {
        builder.env_remove(&k);
    }
    for (k, v) in overrides {
        builder.env(&k, &v);
    }
}

/// Pure half of [`sanitize_appimage_env`]: given the current environment plus the
/// AppImage's binary path and mount dir, return the env keys to DROP and the
/// `(key, cleaned-value)` pairs to OVERRIDE.
///
/// - `APPDIR`/`APPIMAGE`/`ARGV0`/`OWD` (the runtime markers) are dropped.
/// - colon-separated search paths have their AppImage-mount (`$APPDIR/…`) entries
///   stripped (dropped if nothing's left).
/// - any other variable whose value is the AppImage binary or points into the
///   mount (e.g. a poisoned `MAKE`) is dropped.
/// - a selected profile root is authoritative application context, not runtime
///   leakage, even when it shares the mount's path prefix.
fn appimage_env_fixups(
    vars: &[(OsString, OsString)],
    appimage: &OsStr,
    appdir: &OsStr,
) -> (Vec<OsString>, Vec<(OsString, OsString)>) {
    const MARKERS: [&str; 4] = ["APPDIR", "APPIMAGE", "ARGV0", "OWD"];
    const PATH_LISTS: [&str; 5] = [
        "PATH",
        "LD_LIBRARY_PATH",
        "PYTHONPATH",
        "PERLLIB",
        "XDG_DATA_DIRS",
    ];
    let in_mount =
        |value: &[u8]| !appdir.is_empty() && value.starts_with(appdir.as_encoded_bytes());
    let mut drop = Vec::new();
    let mut overrides = Vec::new();
    for (k, v) in vars {
        let profile_root = k.to_str().is_some_and(|key| {
            if cfg!(windows) {
                key.eq_ignore_ascii_case("MUXEL_USER_DATA_DIR")
            } else {
                key == "MUXEL_USER_DATA_DIR"
            }
        });
        if profile_root && !v.is_empty() {
            continue;
        }
        if k.to_str().is_some_and(|key| MARKERS.contains(&key)) {
            drop.push(k.clone());
        } else if k.to_str().is_some_and(|key| PATH_LISTS.contains(&key)) {
            let kept: Vec<&[u8]> = v
                .as_encoded_bytes()
                .split(|byte| *byte == b':')
                .filter(|entry| !entry.is_empty() && !in_mount(entry))
                .collect();
            let orig = v
                .as_encoded_bytes()
                .split(|byte| *byte == b':')
                .filter(|entry| !entry.is_empty())
                .count();
            if kept.len() != orig {
                if kept.is_empty() {
                    drop.push(k.clone());
                } else {
                    let mut cleaned = OsString::new();
                    for (index, entry) in kept.into_iter().enumerate() {
                        if index > 0 {
                            cleaned.push(":");
                        }
                        // SAFETY: these bytes came from this process's OsStr
                        // encoding and were split only at ASCII ':'. ASCII is
                        // self-synchronizing in that encoding, so each segment
                        // remains a valid encoded OsStr, including non-Unicode.
                        cleaned.push(unsafe { OsStr::from_encoded_bytes_unchecked(entry) });
                    }
                    overrides.push((k.clone(), cleaned));
                }
            }
        } else if v == appimage || in_mount(v.as_encoded_bytes()) {
            drop.push(k.clone());
        }
    }
    (drop, overrides)
}

impl Drop for TerminalSession {
    fn drop(&mut self) {
        self.input_closed.store(true, Ordering::Release);
        let _ = self.input_stop.send(None);
        // Release a reader parked on a full output queue before the `master`
        // field drops and closes the platform PTY; a receiver that outlives
        // the session must not hold that close hostage.
        self.output_queue.close_session();
        // Best-effort: kill the child so the reader thread sees EOF and exits.
        // On Unix, skip it if muxel already sent this child its SIGHUP.
        #[cfg(unix)]
        if self.output_queue.child_hangup.claim() {
            let _ = self.killer.lock().kill();
        }
        #[cfg(not(unix))]
        let _ = self.killer.lock().kill();
    }
}

/// The emulator config every session's `Term` runs with. The reader thread's
/// reply parser is built from it too, so both agree on which queries are
/// answered at all.
pub(crate) fn term_config() -> TermConfig {
    // Allow OSC-52 *reads* through — they get an empty reply (see `replies`)
    // instead of alacritty's default silent deny, so probing TUIs don't hang.
    TermConfig {
        osc52: Osc52::CopyPaste,
        ..TermConfig::default()
    }
}

#[cfg(windows)]
fn cancel_finished_child_input(writer: &JoinHandle<()>) {
    use std::os::windows::io::AsRawHandle as _;
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::IO::CancelSynchronousIo;

    // Check repeatedly because a writer admitted just before process exit can
    // enter WriteFile after the first cancellation. Never join on the UI thread.
    for _ in 0..20 {
        if writer.is_finished() {
            break;
        }
        let _ = unsafe { CancelSynchronousIo(HANDLE(writer.as_raw_handle() as isize)) };
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Lets teardown wake a PTY reader parked on a silent child.
///
/// On Unix the reader waits on the PTY with a poller the output queue also
/// holds; dropping the last receiver or the session notifies it (see
/// [`wake_reader`]), which wakes the wait with no PTY bytes. Without it, a descendant
/// that ignores SIGHUP can keep the slave open and silent after its pane
/// closes, pinning the reader thread, its PTY descriptors and the unreaped
/// child until the descendant writes or exits. The wait goes through the
/// `polling` crate (epoll on Linux, kqueue on macOS) rather than `poll(2)`,
/// which macOS does not support on devices such as PTYs. Windows needs no
/// wake: dropping the session closes the pseudoconsole, which ends the
/// reader's pipe.
struct ReaderWake {
    #[cfg(unix)]
    pty: std::os::fd::OwnedFd,
    #[cfg(unix)]
    queue: Arc<OutputQueueState>,
    #[cfg(unix)]
    poller: Arc<polling::Poller>,
}

/// Pause between attempts when the wait itself fails on a Unix PTY reader.
#[cfg(unix)]
const READER_POLL_RETRY: Duration = Duration::from_millis(50);

/// Poller key for the reader's PTY.
#[cfg(unix)]
const READER_PTY_KEY: usize = 0;

impl ReaderWake {
    #[cfg(unix)]
    fn new(master: &dyn MasterPty, queue: &Arc<OutputQueueState>) -> Result<Self> {
        use polling::{Event, PollMode, Poller};
        let fd = master.as_raw_fd().context("pty master has no descriptor")?;
        // SAFETY: `master` owns `fd` and outlives this call. The clone is a
        // separate descriptor for the same PTY, owned by the reader thread.
        let pty = unsafe { std::os::fd::BorrowedFd::borrow_raw(fd) }
            .try_clone_to_owned()
            .context("clone pty descriptor for polling")?;
        let poller = Arc::new(Poller::new().context("create pty reader poller")?);
        // SAFETY: the PTY descriptor lives in this struct, whose Drop deletes
        // it from the poller before it closes. Level mode reports the PTY on
        // every wait while it has output, with no re-arming.
        unsafe { poller.add_with_mode(&pty, Event::readable(READER_PTY_KEY), PollMode::Level) }
            .context("watch pty descriptor")?;
        queue.install_reader_wake(poller.clone());
        Ok(Self {
            pty,
            queue: queue.clone(),
            poller,
        })
    }

    #[cfg(not(unix))]
    fn new(_master: &dyn MasterPty, _queue: &OutputQueueState) -> Result<Self> {
        Ok(Self {})
    }

    /// Wait until the PTY has output, EOF, or an error to read. False once
    /// the last receiver or the session has dropped, even with output pending.
    ///
    /// The queue's notification (see [`wake_reader`]) wakes the wait with no
    /// events, so every wake checks whether the stream has closed. Falling
    /// through to the read would block where teardown cannot wake it, so a
    /// failed wait is retried after a short pause, checking between attempts
    /// whether the stream has closed.
    #[cfg(unix)]
    fn wait_for_output(&self) -> bool {
        let mut events = polling::Events::new();
        let mut warned = false;
        loop {
            if self.queue.is_closed() {
                return false;
            }
            events.clear();
            match self.poller.wait(&mut events, None) {
                Ok(_) => {
                    if self.queue.is_closed() {
                        return false;
                    }
                    if events.iter().any(|event| event.key == READER_PTY_KEY) {
                        return true;
                    }
                    // A notification that raced a reopen, or a spurious wake:
                    // check again and wait.
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(error) => {
                    if self.queue.is_closed() {
                        return false;
                    }
                    if !warned {
                        log::warn!("pty reader wait failed (retrying): {error}");
                        warned = true;
                    }
                    std::thread::sleep(READER_POLL_RETRY);
                }
            }
        }
    }

    #[cfg(not(unix))]
    fn wait_for_output(&self) -> bool {
        true
    }
}

#[cfg(unix)]
impl Drop for ReaderWake {
    fn drop(&mut self) {
        let _ = self.poller.delete(&self.pty);
    }
}

/// How long the reader waits, from the claim of muxel's one SIGHUP to a Unix
/// child (see [`ChildHangup`]), before SIGKILLing a child still running. It
/// applies only when teardown stops a reader that is still reading; a child
/// that already closed its PTY handles, so the reader saw EOF or a read error,
/// is not force-killed. The grace allows a SIGHUP handler time to save before
/// muxel escalates. It cannot guarantee that time: the kernel sends its own
/// hangup when the last PTY master closes.
#[cfg(unix)]
const CHILD_HANGUP_GRACE: Duration = Duration::from_secs(5);

/// The reader's Unix child, hung up with a SIGKILL deadline.
///
/// portable-pty's `Child::kill` sends SIGHUP but escalates to SIGKILL after
/// about 200 ms, cutting off a HUP handler that needs longer. This sends both
/// signals itself with libc, to the child's own pid, and only while
/// `try_wait` reports the child unreaped: until this thread reaps it, the pid
/// cannot belong to another process. The SIGHUP goes through the queue's
/// [`ChildHangup`], so the child gets it once, from whoever asks first.
#[cfg(unix)]
struct HangupChild {
    child: Box<dyn Child + Send + Sync>,
    queue: Arc<OutputQueueState>,
    hung_up: bool,
}

#[cfg(unix)]
impl HangupChild {
    fn new(child: Box<dyn Child + Send + Sync>, queue: Arc<OutputQueueState>) -> Self {
        Self {
            child,
            queue,
            hung_up: false,
        }
    }

    fn try_wait(&mut self) -> std::io::Result<Option<portable_pty::ExitStatus>> {
        self.child.try_wait()
    }

    /// Hang the child up: send SIGHUP unless muxel already has, and arm the
    /// SIGKILL deadline.
    fn hang_up(&mut self) {
        if !self.hung_up {
            self.hung_up = true;
            if self.queue.child_hangup.claim() {
                self.signal(libc::SIGHUP);
            }
        }
    }

    /// After a hangup, wait until [`CHILD_HANGUP_GRACE`] has passed since
    /// muxel's SIGHUP was claimed, then SIGKILL a child still running. The
    /// caller still reaps it. Without a `hang_up` (the reader ended on EOF or
    /// a read error) it arms nothing.
    fn finish_hangup(&mut self) {
        #[cfg(test)]
        if self.queue.force_child_exit.load(Ordering::Acquire) {
            self.signal(libc::SIGKILL);
            return;
        }
        if !self.hung_up {
            return;
        }
        let Some(sent_at) = self.queue.child_hangup.sent_at() else {
            return;
        };
        let deadline = sent_at + CHILD_HANGUP_GRACE;
        while Instant::now() < deadline {
            #[cfg(test)]
            if self.queue.force_child_exit.load(Ordering::Acquire) {
                self.signal(libc::SIGKILL);
                return;
            }
            match self.child.try_wait() {
                Ok(None) => std::thread::sleep(Duration::from_millis(50)),
                _ => return,
            }
        }
        self.signal(libc::SIGKILL);
    }

    fn signal(&mut self, signal: libc::c_int) {
        if !matches!(self.child.try_wait(), Ok(None)) {
            return;
        }
        let Some(pid) = self.child.process_id() else {
            return;
        };
        // SAFETY: `kill` takes plain integers; the pid is our unreaped child.
        unsafe {
            libc::kill(pid as libc::pid_t, signal);
        }
    }
}

/// The writer's existing closed flag and stop channel, handed to the reader
/// together so ending its stream stops input before reaping the child.
struct InputShutdown {
    closed: Arc<AtomicBool>,
    stop: std::sync::mpsc::Sender<Option<QueuedWrite>>,
}

/// Drain PTY output before publishing Exit. Windows gets child status from a
/// separate waiter: a live ConPTY can keep its output pipe open after every
/// client process exits, so waiting for EOF before waiting for the child hangs.
/// Unix owns its child and wakes on queue teardown, then reaps it after EOF.
fn read_loop(
    mut reader: Box<dyn Read + Send>,
    wake: ReaderWake,
    #[cfg_attr(unix, allow(unused_mut))] mut child: SessionChild,
    tx: PtyChunkSender,
    mut immediate_replies: ImmediateReplies,
    profile_reads: bool,
    input_shutdown: InputShutdown,
) {
    #[cfg(unix)]
    let mut child = HangupChild::new(child, tx.queue_state());
    let mut buf = [0u8; PTY_READ_BUFFER_BYTES];
    // Only a clean EOF or a real error ends the session. EINTR is a signal
    // interruption, not an exit — retrying it keeps a healthy pane from being
    // torn down. Any other error is recorded so the app can log/show it.
    let mut read_error: Option<String> = None;
    let mut ui_gone = false;
    let mut read_sequence = 0u64;
    loop {
        if !wake.wait_for_output() {
            // The last receiver or the session dropped. Nothing will render
            // more output, and a silent descendant may hold the PTY open with
            // no EOF to come, so stop reading now. Ending the stream does not
            // end the child: it gets muxel's SIGHUP here unless `kill` or the
            // session's Drop already sent it, and SIGKILL only once
            // CHILD_HANGUP_GRACE has passed since then, after the PTY is
            // released below. Exit is still offered: a receiver kept past its
            // session sees the stream end instead of waiting on it. (Only Unix
            // gets here.)
            #[cfg(unix)]
            child.hang_up();
            #[cfg(not(unix))]
            let _ = child.kill();
            break;
        }
        match reader.read(&mut buf) {
            Ok(0) => break, // EOF → every slave fd closed; child is (probably) gone
            Ok(n) => {
                let read_at = SystemTime::now();
                if ui_gone {
                    // Keep the PTY output pipe moving while owned teardown kills
                    // the child and closes ConPTY. ClosePseudoConsole may wait
                    // on an undrained pipe on Windows; discarded bytes need no
                    // allocation, parsing, timing, or UI delivery.
                    continue;
                }
                let timing = profile_reads.then(|| {
                    read_sequence = read_sequence.wrapping_add(1).max(1);
                    profile::pty_read(n, read_sequence)
                });
                // Before the bytes reach the UI: replies that wait on the UI's
                // drain can miss the asker's window and be read as typing.
                immediate_replies.advance(&buf[..n]);
                if !tx.send_output(buf[..n].to_vec(), timing, read_at) {
                    // The stream has ended: the receiver dropped, or the
                    // session dropped while the receiver was behind. Stop the
                    // child, then keep draining on Windows until EOF so ConPTY
                    // teardown cannot block on a full output pipe. Unix PTYs
                    // must release the reader immediately: a HUP-ignoring
                    // descendant can otherwise hold the slave open forever.
                    // There the child is hung up with a SIGKILL deadline, as
                    // on the wake path above.
                    ui_gone = true;
                    #[cfg(unix)]
                    {
                        child.hang_up();
                        break;
                    }
                    #[cfg(not(unix))]
                    let _ = child.kill();
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => {
                if !ui_gone {
                    log::warn!("pty read failed (treating as session end): {e}");
                    read_error = Some(format!("{:?}: {e}", e.kind()));
                }
                break;
            }
        }
    }
    input_shutdown.closed.store(true, Ordering::Release);
    let _ = input_shutdown.stop.send(None);
    // The reader's master-fd dups are no longer needed; free them before the
    // (potentially long) reap below so closed sessions don't hold PTYs open.
    drop(reader);
    #[cfg(unix)]
    {
        drop(wake);
        tx.queue_state().release_reader_wake();
        // The immediate-reply writer keeps the PTY writer thread, and its master
        // descriptor, alive after the session drops. Release it before the
        // SIGKILL deadline can hold this thread for seconds.
        drop(immediate_replies);
    }
    // Fast bounded poll for the exit status, so a prompt exit reports its code
    // with the Exit event. None after the window means the code is unknown *so
    // far* (e.g. the PTY closed before the process finished dying).
    let mut code = None;
    let mut signal = None;
    for _ in 0..20 {
        match child.try_wait() {
            Ok(Some(status)) => {
                // A signalled child has no exit code of its own; portable_pty
                // substitutes 1. Keep the signal name so the app can tell a
                // real `exit(1)` from a SIGHUP/SIGKILL — without it every
                // killed pane is indistinguishable from a crashed one.
                code = Some(status.exit_code() as i32);
                signal = status.signal().map(str::to_string);
                break;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(25)),
            Err(_) => break,
        }
    }
    if !ui_gone {
        tx.send_exit(code, signal.clone(), read_error);
    }
    // End the stream now, so the receiver reports `Closed` after its last
    // event even while the reap below is still waiting.
    drop(tx);
    // A hung-up child still running after the window above is SIGKILLed once
    // CHILD_HANGUP_GRACE has passed since muxel's SIGHUP; this thread holds no
    // PTY descriptor or stream end while it waits.
    #[cfg(unix)]
    if code.is_none() {
        child.finish_hangup();
    }
    // Keep waiting until the child is actually reaped. A child that outlives
    // its PTY (daemonized, or slow to die after a kill) parks this thread at a
    // 1s cadence instead of leaving a permanent zombie when it finally exits;
    // the thread costs nothing while sleeping and dies with the process.
    if code.is_none() {
        while let Ok(None) = child.try_wait() {
            #[cfg(all(test, unix))]
            if child.queue.force_child_exit.load(Ordering::Acquire) {
                child.signal(libc::SIGKILL);
            }
            std::thread::sleep(Duration::from_secs(1));
        }
    }
}

// Real-PTY tests are Unix-only, like the `tests` module below. A Windows
// fixture that exits on the first keystroke (`cmd.exe /C "pause"`) deadlocks
// the suite: once the child is gone, further writes to the ConPTY never
// return, so `stream_then_type_stress` hung until the 6 h CI timeout. The
// pure paint-priority logic is covered cross-platform in `view.rs` tests.
#[cfg(all(test, unix))]
mod content_damage_tests {
    use super::{CommandSpec, ContentDamage, TerminalSession};
    use std::time::{Duration, Instant};

    fn spawn_quiet() -> std::sync::Arc<TerminalSession> {
        let spec = CommandSpec::program("/bin/cat", vec![]);
        TerminalSession::spawn(spec, 80, 24).expect("spawn").0
    }

    /// Printable output records damage; interaction priority survives queued
    /// stream batches until its deadline.
    #[test]
    fn process_output_records_damage_during_interaction() {
        let session = spawn_quiet();
        let _ = session.take_pending_damage();
        session.clear_pending_damage_for_test();

        session.write_input(b"x");
        assert!(session.is_interactive());
        session.process_output(b"hello");
        assert!(session.is_interactive());

        match session.pending_damage_for_test() {
            ContentDamage::Full => {}
            ContentDamage::Partial(lines) => {
                assert!(!lines.is_empty());
                assert!(lines.iter().all(|&l| (0..24).contains(&l)));
            }
        }
        session.kill();
    }

    /// Stream + type: output batches do not consume recent-input priority.
    #[test]
    fn stream_then_type_stress() {
        let session = spawn_quiet();
        let _ = session.take_pending_damage();
        session.clear_pending_damage_for_test();

        // Agent stream: big ANSI-ish dumps outside an interaction window.
        let chunk = b"\x1b[32mline of agent output that is fairly long\x1b[0m\r\n".repeat(40);
        for _ in 0..20 {
            session.process_output(&chunk);
            assert!(!session.is_interactive());
        }
        // Damage should be Full or a non-empty Partial after scroll/stream.
        match session.take_pending_damage() {
            ContentDamage::Full => {}
            ContentDamage::Partial(lines) => assert!(!lines.is_empty()),
        }

        // Every nearby output batch remains interactive instead of the first
        // queued stream batch consuming a one-shot flag.
        for _ in 0..30 {
            session.write_input(b"x");
            session.process_output(b"x");
            assert!(session.is_interactive());
            session.process_output(&chunk[..200.min(chunk.len())]);
            assert!(session.is_interactive());
        }
        *session.interactive_until.lock() = Some(Instant::now() - Duration::from_millis(1));
        assert!(!session.is_interactive());
        session.kill();
    }

    /// Open descriptors in this process. `/dev/fd` on macOS, `/proc/self/fd` on
    /// Linux; both list one entry per open descriptor.
    fn open_fds() -> usize {
        let dir = if cfg!(target_os = "linux") {
            "/proc/self/fd"
        } else {
            "/dev/fd"
        };
        std::fs::read_dir(dir).map(|d| d.count()).unwrap_or(0)
    }

    /// Poll until the process is holding at most `limit` descriptors, or
    /// `budget` runs out; returns the last count seen.
    fn settle_fds(limit: usize, budget: Duration) -> usize {
        let deadline = Instant::now() + budget;
        let mut open = open_fds();
        while open > limit && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            open = open_fds();
        }
        open
    }

    /// Descriptor use must not grow with the number of spawned-and-dropped
    /// sessions.
    ///
    /// A live session costs five or six: the PTY master, the reader thread's
    /// dup, its polling dup and poller (on Linux with the poller's eventfd),
    /// and the writer thread's dup. The thread-owned ones are only released when
    /// those threads exit, which happens because dropping the session kills the child
    /// (reader sees EOF) and drops the writer channel's `Sender`. Anything that
    /// keeps an `Arc<TerminalSession>` alive past its pane therefore leaks
    /// descriptors until the process hits EMFILE — which is exactly what a
    /// window-lifetime focus subscription capturing the session used to do (see
    /// `TerminalView::new_with_focus_observer`).
    ///
    /// Deliberately one session at a time, settling between rounds: PTYs are a
    /// system-wide resource (`kern.tty.ptmx_max` is 511 on macOS) that the rest
    /// of this parallel suite is also drawing on, so a batch of live PTYs would
    /// starve its neighbours. `SLACK` absorbs their descriptors instead; a leak
    /// holds 2 per round and so can never settle back under it. The per-round
    /// budget is short (threads unwind in milliseconds) so a leaking build fails
    /// promptly; the generous one is spent once, on the final reading.
    #[test]
    fn dropped_sessions_do_not_accumulate_descriptors() {
        const ROUNDS: usize = 12;
        const SLACK: usize = 12;
        const PER_ROUND: Duration = Duration::from_secs(2);
        const FINAL: Duration = Duration::from_secs(15);

        let baseline = open_fds();
        for _ in 0..ROUNDS {
            drop(spawn_quiet());
            settle_fds(baseline + SLACK, PER_ROUND);
        }
        let open = settle_fds(baseline + SLACK, FINAL);
        assert!(
            open <= baseline + SLACK,
            "{ROUNDS} spawned-and-dropped sessions leaked descriptors: \
             {baseline} open before, {open} after"
        );
    }
}

#[cfg(test)]
mod wheel_report_tests {
    use super::{
        CommandSpec, PtyChunk, TerminalSession, appimage_env_fixups, apply_spec_env,
        push_mouse_report, push_wheel_report,
    };
    use std::time::{Duration, Instant};

    #[test]
    fn a_removed_variable_is_gone_whether_inherited_or_set() {
        let pair = |k: &str, v: &str| (k.to_string(), v.to_string());
        let mut spec = CommandSpec::program("x", Vec::new());
        spec.env = vec![pair("MUXEL_TEST_SET", "set"), pair("MUXEL_TEST_KEEP", "1")];
        spec.env_remove = vec!["MUXEL_TEST_SET".into(), "PATH".into()];
        // A builder starts from this process's environment, which has PATH.
        let mut builder = portable_pty::CommandBuilder::new("x");
        assert!(builder.get_env("PATH").is_some());
        apply_spec_env(&mut builder, &spec);
        assert_eq!(builder.get_env("PATH"), None);
        assert_eq!(builder.get_env("MUXEL_TEST_SET"), None);
        assert_eq!(builder.get_env("MUXEL_TEST_KEEP"), Some("1".as_ref()));
    }

    #[test]
    fn appimage_env_fixups_strip_leakage_keep_the_rest() {
        let appimage = "/home/u/Apps/muxel-linux-x86_64.AppImage";
        let appdir = "/tmp/.mount_muxelXYZ";
        let s = |k: &str, v: &str| (k.into(), v.into());
        let vars = vec![
            s("APPIMAGE", appimage),
            s("APPDIR", appdir),
            s("ARGV0", appimage),
            s("OWD", "/home/u"),
            s("MAKE", appimage), // poisoned scalar
            s("PATH", &format!("{appdir}/usr/bin:/usr/bin:/bin")), // partly poisoned
            s("LD_LIBRARY_PATH", &format!("{appdir}/usr/lib")), // wholly poisoned
            s("HOME", "/home/u"), // keep
            s("EDITOR", "vim"),  // keep
            s("muxel_user_data_dir", &format!("{appdir}/profile")),
        ];
        let (drop, overrides) = appimage_env_fixups(&vars, appimage.as_ref(), appdir.as_ref());

        for k in [
            "APPIMAGE",
            "APPDIR",
            "ARGV0",
            "OWD",
            "MAKE",
            "LD_LIBRARY_PATH",
        ] {
            assert!(drop.contains(&k.into()), "should drop {k}");
        }
        // PATH keeps only the system entries.
        assert_eq!(
            overrides
                .iter()
                .find(|(k, _)| k == "PATH")
                .map(|(_, v)| v.as_os_str()),
            Some("/usr/bin:/bin".as_ref())
        );
        // Untouched, legitimate vars stay.
        for k in ["HOME", "EDITOR"] {
            assert!(!drop.contains(&k.into()));
            assert!(!overrides.iter().any(|(o, _)| o == k));
        }
        // This spelling is the authoritative root only on Windows.
        assert_eq!(drop.contains(&"muxel_user_data_dir".into()), !cfg!(windows));
    }

    #[cfg(any(windows, unix))]
    fn non_unicode_os_string() -> std::ffi::OsString {
        #[cfg(windows)]
        {
            use std::os::windows::ffi::OsStringExt as _;
            std::ffi::OsString::from_wide(&[b'x' as u16, 0xd800])
        }
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt as _;
            std::ffi::OsString::from_vec(vec![b'x', 0xff])
        }
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn appimage_fixups_preserve_non_unicode_keys_values_and_path_segments() {
        use std::ffi::OsString;
        let raw = non_unicode_os_string();
        let mut mount = OsString::from("/tmp/.mount_");
        mount.push(&raw);
        let mut poisoned = mount.clone();
        poisoned.push("/bin");
        let mut kept = OsString::from("/outside/");
        kept.push(&raw);
        let mut path = poisoned.clone();
        path.push(":");
        path.push(&kept);
        path.push(":/bin");
        let mut expected_path = kept.clone();
        expected_path.push(":/bin");
        let image = OsString::from("/opt/muxel.AppImage");
        let vars = vec![
            ("APPIMAGE".into(), image.clone()),
            ("APPDIR".into(), mount.clone()),
            ("PATH".into(), path),
            ("MAKE".into(), poisoned),
            ("MUXEL_USER_DATA_DIR".into(), kept.clone()),
            (raw.clone(), raw.clone()),
        ];
        let mut builder = portable_pty::CommandBuilder::new("unused");
        for (key, value) in &vars {
            builder.env(key, value);
        }
        let (drop, overrides) = appimage_env_fixups(&vars, &image, &mount);
        for key in drop {
            builder.env_remove(key);
        }
        for (key, value) in overrides {
            builder.env(key, value);
        }
        for key in ["APPIMAGE", "APPDIR", "MAKE"] {
            assert!(builder.get_env(key).is_none());
        }
        assert_eq!(builder.get_env("PATH"), Some(expected_path.as_os_str()));
        assert_eq!(
            builder.get_env("MUXEL_USER_DATA_DIR"),
            Some(kept.as_os_str())
        );
        assert_eq!(builder.get_env(&raw), Some(raw.as_os_str()));
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn appimage_selected_roots_survive_actual_pty_construction() {
        let temp = std::env::temp_dir();
        let scratch = temp.join(format!("muxel-appimage-profile-{}", uuid::Uuid::new_v4()));
        assert_eq!(scratch.parent(), Some(temp.as_path()));
        std::fs::create_dir(&scratch).unwrap();
        let mount = scratch.join("muxel-app");
        std::fs::create_dir(&mount).unwrap();
        let keys = if cfg!(windows) {
            vec![
                "MUXEL_USER_DATA_DIR",
                "muxel_user_data_dir",
                "MuXeL_UsEr_DaTa_DiR",
            ]
        } else {
            vec!["MUXEL_USER_DATA_DIR"]
        };
        let mut failures = Vec::new();
        for case in [
            "outside",
            "inside",
            "prefix-sibling",
            "non-unicode",
            "unselected",
        ] {
            let root = appimage_test_root(&scratch, case);
            if let Some(root) = &root {
                assert!(root.is_absolute());
                if case == "non-unicode" {
                    assert!(root.to_str().is_none());
                } else {
                    std::fs::create_dir_all(root).unwrap();
                    // Both inside and prefix-sibling roots are valid writable
                    // fixture directories, not an assumption about mount modes.
                    std::fs::write(root.join("writable-profile-marker"), "fixture").unwrap();
                }
            }
            for key in &keys {
                let mut command = std::process::Command::new(std::env::current_exe().unwrap());
                command
                    .args([
                        "--exact",
                        "session::wheel_report_tests::appimage_profile_root_child_probe",
                        "--nocapture",
                    ])
                    .env("MUXEL_APPIMAGE_PROBE_STAGE", "launch")
                    .env("MUXEL_APPIMAGE_TEST_SCRATCH", &scratch)
                    .env("MUXEL_APPIMAGE_TEST_CASE", case)
                    .env_remove("MUXEL_USER_DATA_DIR")
                    .env("APPIMAGE", scratch.join("muxel.AppImage"))
                    .env("APPDIR", &mount);
                if let Some(root) = &root {
                    command.env(key, root);
                }
                let output = command.output().unwrap();
                println!(
                    "APPIMAGE_PROFILE_CASE={case} KEY={key} PASSED={}",
                    output.status.success()
                );
                if !output.status.success() {
                    failures.push(format!(
                        "case={case}, key={key}: {}{}",
                        String::from_utf8_lossy(&output.stdout),
                        String::from_utf8_lossy(&output.stderr)
                    ));
                }
            }
        }
        assert_eq!(scratch.parent(), Some(temp.as_path()));
        std::fs::remove_dir_all(&scratch).unwrap();
        assert!(!scratch.exists());
        println!("APPIMAGE_PROFILE_FIXTURE_CLEANED={}", scratch.display());
        assert!(failures.is_empty(), "{}", failures.join("\n"));
    }

    #[cfg(any(windows, unix))]
    fn appimage_test_root(scratch: &std::path::Path, case: &str) -> Option<std::path::PathBuf> {
        match case {
            "outside" => Some(scratch.join("outside-profile")),
            "inside" => Some(scratch.join("muxel-app").join("profile")),
            "prefix-sibling" => Some(scratch.join("muxel-app-profile")),
            "non-unicode" => Some(scratch.join(non_unicode_os_string())),
            "unselected" => None,
            _ => panic!("unknown AppImage fixture case"),
        }
    }

    #[cfg(any(windows, unix))]
    #[test]
    fn appimage_profile_root_child_probe() {
        let Ok(stage) = std::env::var("MUXEL_APPIMAGE_PROBE_STAGE") else {
            return;
        };
        let scratch = std::env::var_os("MUXEL_APPIMAGE_TEST_SCRATCH").unwrap();
        let case = std::env::var("MUXEL_APPIMAGE_TEST_CASE").unwrap();
        let root = appimage_test_root(std::path::Path::new(&scratch), &case)
            .map(std::path::PathBuf::into_os_string);
        assert_eq!(
            std::env::var_os("MUXEL_USER_DATA_DIR"),
            root,
            "exact selected root must survive {stage}: {case}"
        );
        if stage == "observe" {
            assert!(std::env::var_os("APPIMAGE").is_none());
            assert!(std::env::var_os("APPDIR").is_none());
            println!("APPIMAGE_PROFILE_ROOT_CHILD_OK");
            return;
        }
        let mut spec = CommandSpec::program(
            std::env::current_exe().unwrap().to_str().unwrap(),
            vec![
                "--exact".into(),
                "session::wheel_report_tests::appimage_profile_root_child_probe".into(),
                "--nocapture".into(),
            ],
        );
        spec.env
            .push(("MUXEL_APPIMAGE_PROBE_STAGE".into(), "observe".into()));
        let (_session, rx) = TerminalSession::spawn(spec, 100, 30).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        let mut output = Vec::new();
        loop {
            assert!(
                Instant::now() < deadline,
                "child timed out: {}",
                String::from_utf8_lossy(&output)
            );
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => output.extend(bytes),
                Ok(PtyChunk::Exit {
                    code, read_error, ..
                }) => {
                    assert_eq!(code, Some(0), "{}", String::from_utf8_lossy(&output));
                    assert!(read_error.is_none());
                    assert!(
                        String::from_utf8_lossy(&output).contains("APPIMAGE_PROFILE_ROOT_CHILD_OK")
                    );
                    break;
                }
                Err(_) => std::thread::sleep(Duration::from_millis(20)),
            }
        }
    }

    #[test]
    fn sgr_encoding() {
        let mut b = Vec::new();
        push_wheel_report(&mut b, true, 0, 0, true);
        assert_eq!(b, b"\x1b[<64;1;1M"); // wheel up at top-left cell
        b.clear();
        push_wheel_report(&mut b, false, 4, 9, true);
        assert_eq!(b, b"\x1b[<65;5;10M"); // wheel down at col 5, row 10 (1-based)
    }

    #[test]
    fn legacy_encoding() {
        let mut b = Vec::new();
        push_wheel_report(&mut b, true, 0, 0, false);
        // ESC [ M, button 64+32=96, then (col+1)+32 and (row+1)+32.
        assert_eq!(b, &[0x1b, b'[', b'M', 96, 33, 33]);
        b.clear();
        push_wheel_report(&mut b, false, 1, 2, false);
        assert_eq!(b, &[0x1b, b'[', b'M', 97, 34, 35]);
    }

    #[test]
    fn sgr_button_press_release() {
        let mut b = Vec::new();
        push_mouse_report(&mut b, 0, 2, 3, true, true);
        assert_eq!(b, b"\x1b[<0;3;4M");
        b.clear();
        push_mouse_report(&mut b, 0, 2, 3, false, true);
        assert_eq!(b, b"\x1b[<0;3;4m");
    }

    #[test]
    fn legacy_button_release_uses_code_3() {
        let mut b = Vec::new();
        push_mouse_report(&mut b, 0, 0, 0, false, false);
        // ESC [ M, release button 3+32=35, cell (1,1) → 33,33
        assert_eq!(b, &[0x1b, b'[', b'M', 35, 33, 33]);
    }
}

#[cfg(test)]
mod profiler_transport_tests {
    use super::*;

    #[test]
    fn public_output_variant_keeps_tuple_shape() {
        let output = PtyChunk::Output(vec![1, 2, 3]);
        match output {
            PtyChunk::Output(bytes) => assert_eq!(bytes, [1, 2, 3]),
            PtyChunk::Exit { .. } => panic!("expected output"),
        }
    }

    #[test]
    fn private_output_envelope_keeps_reader_timing_with_bytes() {
        let (tx, rx) = output_queue(16, 1);
        let sender = PtyChunkSender::Profiled(tx);
        let timing = PtyReadTiming {
            sequence: 7,
            read_at_us: 11,
        };
        let read_at = SystemTime::UNIX_EPOCH + Duration::from_secs(5);
        assert!(sender.send_output(vec![4, 5], Some(timing), read_at));
        match rx.try_recv().expect("profiled output") {
            ProfiledPtyChunk::Output {
                bytes,
                timing: Some(received),
                read_at: received_at,
            } => {
                assert_eq!(bytes, [4, 5]);
                assert_eq!(received.sequence, timing.sequence);
                assert_eq!(received.read_at_us, timing.read_at_us);
                assert_eq!(received_at, read_at);
            }
            _ => panic!("expected timed output"),
        }
    }

    #[test]
    fn output_queue_blocks_at_byte_limit_and_preserves_order() {
        let (tx, rx) = output_queue::<PtyChunk>(5, 2);
        assert!(tx.send_blocking(PtyChunk::Output(vec![1, 2, 3])));
        assert!(tx.send_blocking(PtyChunk::Output(vec![4, 5])));
        assert_eq!(
            rx.stats(),
            PtyOutputQueueStats {
                queued_bytes: 5,
                queued_chunks: 2,
                high_water_bytes: 5,
                high_water_chunks: 2,
            }
        );

        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let sender = std::thread::spawn(move || {
            started_tx.send(()).expect("report sender start");
            assert!(tx.send_blocking(PtyChunk::Output(vec![6])));
            assert!(tx.send_blocking(PtyChunk::Exit {
                code: Some(0),
                signal: None,
                read_error: None,
            }));
            done_tx.send(()).expect("report sender completion");
        });
        started_rx.recv().expect("sender started");
        assert!(
            done_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "sender passed a full queue"
        );

        let mut output = Vec::new();
        loop {
            match rx.recv_blocking().expect("queued event") {
                PtyChunk::Output(bytes) => output.extend(bytes),
                PtyChunk::Exit { code, .. } => {
                    assert_eq!(code, Some(0));
                    break;
                }
            }
        }
        done_rx
            .recv_timeout(Duration::from_secs(1))
            .expect("sender should resume as the receiver drains");
        sender.join().expect("sender thread");
        assert_eq!(output, [1, 2, 3, 4, 5, 6]);
        let stats = rx.stats();
        assert_eq!(stats.queued_bytes, 0);
        assert_eq!(stats.queued_chunks, 0);
        assert_eq!(stats.high_water_bytes, 5);
        assert_eq!(stats.high_water_chunks, 2);
    }

    #[test]
    fn fragmented_stream_is_byte_exact_through_repeated_backpressure() {
        let expected: Vec<u8> = (0..128 * 1024).map(|index| (index % 251) as u8).collect();
        let producer_bytes = expected.clone();
        let (tx, rx) = output_queue::<PtyChunk>(1024, 8);
        let producer = std::thread::spawn(move || {
            let fragment_sizes = [1, 2, 7, 63, 511, 1024];
            let mut offset = 0;
            let mut fragment = 0;
            while offset < producer_bytes.len() {
                let end = (offset + fragment_sizes[fragment % fragment_sizes.len()])
                    .min(producer_bytes.len());
                assert!(tx.send_blocking(PtyChunk::Output(producer_bytes[offset..end].to_vec())));
                offset = end;
                fragment += 1;
            }
            assert!(tx.send_blocking(PtyChunk::Exit {
                code: Some(0),
                signal: None,
                read_error: None,
            }));
        });

        let mut actual = Vec::with_capacity(expected.len());
        while let PtyChunk::Output(bytes) = rx.recv_blocking().expect("fragmented event") {
            actual.extend(bytes);
        }
        producer.join().expect("fragment producer");
        assert_eq!(actual, expected);
        assert!(rx.stats().high_water_bytes <= 1024);
        assert!(rx.stats().high_water_chunks <= 8);
    }

    #[test]
    fn dropping_receiver_cancels_a_blocked_sender() {
        let (tx, rx) = output_queue::<PtyChunk>(4, 1);
        let state = tx.state.clone();
        assert!(tx.send_blocking(PtyChunk::Output(vec![1, 2, 3, 4])));
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let sender = std::thread::spawn(move || {
            started_tx.send(()).expect("report sender start");
            done_tx
                .send(tx.send_blocking(PtyChunk::Output(vec![5])))
                .expect("report sender completion");
        });

        started_rx.recv().expect("sender started");
        drop(rx);
        assert!(
            !done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("receiver drop should wake the sender")
        );
        sender.join().expect("sender thread");
        let stats = state.stats();
        assert_eq!(stats.queued_bytes, 0);
        assert_eq!(stats.queued_chunks, 0);
    }

    #[test]
    fn closing_the_session_ends_a_blocked_send_without_the_receiver() {
        let (tx, rx) = output_queue::<PtyChunk>(4, 1);
        let state = tx.state.clone();
        assert!(tx.send_blocking(PtyChunk::Output(vec![1, 2, 3, 4])));
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let sender = std::thread::spawn(move || {
            started_tx.send(()).expect("report sender start");
            done_tx
                .send(tx.send_blocking(PtyChunk::Output(vec![5])))
                .expect("report sender completion");
            tx
        });

        started_rx.recv().expect("sender started");
        assert!(
            done_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "sender passed a full queue"
        );
        state.close_session();
        assert!(
            !done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("session close should wake the sender")
        );
        let tx = sender.join().expect("sender thread");

        // What was queued stays receivable, and a send that fits is still
        // admitted: the stream continues while the consumer keeps up.
        assert!(matches!(
            rx.try_recv(),
            Ok(PtyChunk::Output(bytes)) if bytes == [1, 2, 3, 4]
        ));
        assert!(tx.send_blocking(PtyChunk::Exit {
            code: Some(0),
            signal: None,
            read_error: None,
        }));
        assert!(matches!(rx.try_recv(), Ok(PtyChunk::Exit { .. })));
    }

    #[cfg(windows)]
    #[test]
    fn monitor_spawn_failure_closes_queue_before_console_cleanup() {
        let (tx, rx) = output_queue::<PtyChunk>(4, 1);
        let state = tx.state.clone();
        assert!(tx.send_blocking(PtyChunk::Output(vec![1, 2, 3, 4])));

        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let sender = std::thread::spawn(move || {
            started_tx.send(()).expect("report sender start");
            let sent = tx.send_blocking(PtyChunk::Output(vec![5]));
            done_tx.send(sent).expect("report sender completion");
        });
        started_rx.recv().expect("sender started");
        assert!(
            done_rx.recv_timeout(Duration::from_millis(20)).is_err(),
            "sender must be blocked by the full queue"
        );

        let mut close_saw_reader_finished = false;
        let result = monitor_spawn_result::<()>(
            Err(std::io::Error::other("injected monitor spawn failure")),
            &state,
            || {
                // Model ClosePseudoConsole waiting for the reader to drain.
                close_saw_reader_finished =
                    matches!(done_rx.recv_timeout(Duration::from_secs(1)), Ok(false));
            },
        );

        // Always release and join before assertions, so a broken ordering
        // reports a test failure without leaving the sender parked.
        state.close_session();
        sender.join().expect("blocked sender must be joined");

        let error = result.expect_err("injected spawn failure must be returned");
        assert_eq!(error.to_string(), "injected monitor spawn failure");
        assert!(
            close_saw_reader_finished,
            "queue shutdown must unblock the reader before console close"
        );
        assert!(matches!(
            rx.try_recv(),
            Ok(PtyChunk::Output(bytes)) if bytes == [1, 2, 3, 4]
        ));
    }

    #[cfg(windows)]
    #[test]
    fn successful_monitor_spawn_does_not_close_queue_or_run_cleanup() {
        let (tx, rx) = output_queue::<PtyChunk>(4, 1);
        let state = tx.state.clone();
        let cleanup_called = std::sync::atomic::AtomicBool::new(false);
        let result = monitor_spawn_result(Ok(7), &state, || {
            cleanup_called.store(true, Ordering::Release);
        });
        assert_eq!(result.unwrap(), 7);
        assert!(!cleanup_called.load(Ordering::Acquire));
        assert!(!state.counters.lock().session_closed);
        assert!(tx.send_blocking(PtyChunk::Output(vec![1])));
        assert!(matches!(rx.try_recv(), Ok(PtyChunk::Output(bytes)) if bytes == [1]));
    }

    #[test]
    fn profile_peak_survives_a_prompt_receive() {
        let (tx, rx) = output_queue::<PtyChunk>(16, 4);
        assert!(tx.send_blocking(PtyChunk::Output(vec![0; 5])));
        assert!(tx.send_blocking(PtyChunk::Output(vec![0; 3])));
        // The drain receives before it samples, as the view's drain loop does.
        assert!(matches!(rx.try_recv(), Ok(PtyChunk::Output(bytes)) if bytes.len() == 5));
        assert!(matches!(rx.try_recv(), Ok(PtyChunk::Output(bytes)) if bytes.len() == 3));
        assert_eq!(
            rx.take_profile_peak(),
            OutputQueuePeak {
                bytes: 8,
                chunks: 2
            }
        );

        // Each sample restarts from the occupancy at the time it was taken.
        assert_eq!(rx.take_profile_peak(), OutputQueuePeak::default());
        assert!(tx.send_blocking(PtyChunk::Output(vec![0; 2])));
        let one_queued = OutputQueuePeak {
            bytes: 2,
            chunks: 1,
        };
        assert_eq!(rx.take_profile_peak(), one_queued);
        assert_eq!(rx.take_profile_peak(), one_queued);
    }

    #[test]
    fn oversized_item_waits_for_an_empty_queue_instead_of_panicking() {
        let (tx, rx) = output_queue::<PtyChunk>(4, 2);
        assert!(tx.send_blocking(PtyChunk::Output(vec![1])));
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let sender = std::thread::spawn(move || {
            started_tx.send(()).expect("report sender start");
            done_tx
                .send(tx.send_blocking(PtyChunk::Output(vec![2; 5])))
                .expect("report oversized send");
        });
        started_rx.recv().expect("sender started");
        assert!(done_rx.recv_timeout(Duration::from_millis(20)).is_err());
        assert!(matches!(
            rx.recv_blocking(),
            Ok(PtyChunk::Output(bytes)) if bytes == [1]
        ));
        assert!(
            done_rx
                .recv_timeout(Duration::from_secs(1))
                .expect("oversized item should enter an empty queue")
        );
        sender.join().expect("sender thread");
        assert_eq!(rx.stats().queued_bytes, 5);
    }

    #[test]
    fn fifty_pane_model_has_a_fixed_aggregate_bound() {
        const PANES: usize = 50;
        let mut queues = Vec::with_capacity(PANES);
        for _ in 0..PANES {
            let (tx, rx) = output_queue::<PtyChunk>(
                PTY_OUTPUT_QUEUE_BYTE_CAPACITY,
                PTY_OUTPUT_QUEUE_CHUNK_CAPACITY,
            );
            for _ in 0..4 {
                assert!(tx.send_blocking(PtyChunk::Output(vec![0; 64 * 1024])));
            }
            queues.push((tx, rx));
        }

        let queued_bytes: usize = queues.iter().map(|(_, rx)| rx.stats().queued_bytes).sum();
        assert_eq!(queued_bytes, PANES * PTY_OUTPUT_QUEUE_BYTE_CAPACITY);
        assert!(queues.iter().all(|(_, rx)| {
            let stats = rx.stats();
            stats.queued_bytes == PTY_OUTPUT_QUEUE_BYTE_CAPACITY
                && stats.queued_chunks <= PTY_OUTPUT_QUEUE_CHUNK_CAPACITY
        }));
    }

    #[test]
    fn writer_queue_delay_excludes_prior_profiler_work() {
        let base = Instant::now();
        let profile_started = base + Duration::from_millis(10);
        let profile_finished = base + Duration::from_millis(15);
        let dequeued = base + Duration::from_millis(20);

        assert_eq!(
            queue_delay_excluding_profiler(
                base,
                dequeued,
                Some((profile_started, profile_finished)),
            ),
            Duration::from_millis(15)
        );
        assert_eq!(
            queue_delay_excluding_profiler(
                base + Duration::from_millis(12),
                dequeued,
                Some((profile_started, profile_finished)),
            ),
            Duration::from_millis(5)
        );
    }
}

#[cfg(all(test, windows))]
mod windows_child_exit_tests {
    use super::{CommandSpec, PtyChunk, RESIZE_SETTLE, TerminalSession};
    use std::io::Write;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    fn spawn(script: &str) -> (Arc<TerminalSession>, super::PtyReceiver) {
        let powershell = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("System32/WindowsPowerShell/v1.0/powershell.exe");
        TerminalSession::spawn(
            CommandSpec::program(
                powershell.to_string_lossy(),
                vec!["-NoProfile".into(), "-Command".into(), script.into()],
            ),
            80,
            24,
        )
        .expect("spawn isolated PowerShell fixture")
    }

    fn wait_ready(session: &TerminalSession, rx: &super::PtyReceiver) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    session.process_output(&bytes);
                    if session.visible_text().contains("FIXTURE_READY") {
                        return;
                    }
                }
                Ok(PtyChunk::Exit { .. }) => panic!("fixture exited before becoming ready"),
                Err(async_channel::TryRecvError::Closed) => panic!("fixture reader closed"),
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        panic!("fixture never became ready");
    }

    #[test]
    fn refused_second_file_writes_no_partial_input_to_the_pty() {
        let (session, rx) = spawn(
            "[Console]::WriteLine('FIXTURE_READY'); $line=[Console]::ReadLine(); [Console]::WriteLine('RECORDED:'+ $line); exit 0",
        );
        wait_ready(&session, &rx);
        let paths = [
            std::path::PathBuf::from("C:\\safe.txt"),
            std::path::PathBuf::from("C:\\tmp\\bad\nname.txt"),
        ];
        assert_eq!(
            session.paste_paths(&paths),
            Err(muxel_core::path_paste::PastePathError::InvalidPath)
        );
        use std::os::windows::ffi::OsStringExt;
        let non_unicode = [
            std::path::PathBuf::from("C:\\safe.txt"),
            std::path::PathBuf::from(std::ffi::OsString::from_wide(&[0xd800])),
        ];
        assert_eq!(
            session.paste_paths(&non_unicode),
            Err(muxel_core::path_paste::PastePathError::NonUnicode)
        );
        session.write_input(b"SENTINEL\r");
        let (code, output) = wait_exit(&session, &rx);
        assert_eq!(code, Some(0));
        let output = String::from_utf8_lossy(&output);
        assert!(output.contains("RECORDED:SENTINEL"), "{output}");
        assert!(!output.contains("safe.txt"), "{output}");
    }

    #[test]
    fn valid_file_paste_precedes_the_next_key_in_the_pty() {
        let (session, rx) = spawn(
            "[Console]::WriteLine('FIXTURE_READY'); $line=[Console]::ReadLine(); [Console]::WriteLine('RECORDED:'+ $line); exit 0",
        );
        wait_ready(&session, &rx);
        session
            .paste_paths(&[std::path::PathBuf::from("C:\\tmp\\report (1).pdf")])
            .unwrap();
        session.write_input(b"SENTINEL\r");
        let (code, output) = wait_exit(&session, &rx);
        assert_eq!(code, Some(0));
        let output = String::from_utf8_lossy(&output);
        assert!(
            output.contains("RECORDED: 'C:\\tmp\\report (1).pdf' SENTINEL"),
            "{output}"
        );
    }

    fn wait_exit(session: &TerminalSession, rx: &super::PtyReceiver) -> (Option<i32>, Vec<u8>) {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    // Feed the parser as the GUI does, including terminal replies.
                    session.process_output(&bytes);
                    output.extend_from_slice(&bytes);
                }
                Ok(PtyChunk::Exit {
                    code,
                    signal,
                    read_error,
                }) => {
                    assert_eq!(signal, None);
                    assert_eq!(read_error, None);
                    assert!(
                        session
                            .input_closed
                            .load(std::sync::atomic::Ordering::Acquire)
                    );
                    assert!(session.writer.lock().write_all(b"AFTER_EXIT").is_err());
                    return (code, output);
                }
                Err(async_channel::TryRecvError::Closed) => panic!("reader closed without Exit"),
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5));
                }
            }
        }
        panic!("child exit was not reported while its terminal session stayed alive");
    }

    #[test]
    fn natural_exit_is_reported_while_session_is_still_alive() {
        for code in [0, 7] {
            let (session, rx) = spawn(&format!(
                "[Console]::WriteLine('FINAL_OUTPUT'); exit {code}"
            ));
            let (reported, output) = wait_exit(&session, &rx);
            assert_eq!(reported, Some(code));
            assert!(output.windows(12).any(|bytes| bytes == b"FINAL_OUTPUT"));
            assert!(!matches!(
                rx.try_recv(),
                Ok(PtyChunk::Output(_)) | Ok(PtyChunk::Exit { .. })
            ));
        }
    }

    #[test]
    fn startup_and_exit_need_no_ui_parser_or_cursor_reply() {
        let (session, rx) = spawn("[Console]::WriteLine('FINAL_OUTPUT'); exit 7");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        let mut reported = false;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => output.extend_from_slice(&bytes),
                Ok(PtyChunk::Exit {
                    code, read_error, ..
                }) => {
                    assert_eq!(code, Some(7));
                    assert_eq!(read_error, None);
                    reported = true;
                    break;
                }
                Err(async_channel::TryRecvError::Closed) => panic!("reader closed without Exit"),
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
            }
        }
        assert!(reported, "startup must not wait for a UI cursor reply");
        assert!(output.windows(12).any(|bytes| bytes == b"FINAL_OUTPUT"));
        assert!(!output.windows(4).any(|bytes| bytes == b"\x1b[6n"));
        let deadline = Instant::now() + Duration::from_secs(2);
        while !session._reader.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            session._reader.is_finished(),
            "exit must finish the reader, not just its event"
        );
        while !session._monitor.is_finished() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(
            session._monitor.is_finished(),
            "child monitor and writer did not finish"
        );
    }

    #[test]
    fn killed_before_any_ui_drain_finishes_without_a_cursor_reply() {
        let (session, rx) = spawn("Start-Sleep -Seconds 60");
        session.kill();
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut exited = false;
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    assert!(!bytes.windows(4).any(|bytes| bytes == b"\x1b[6n"));
                }
                Ok(PtyChunk::Exit { code, .. }) => {
                    assert!(code.is_some());
                    exited = true;
                    break;
                }
                Err(async_channel::TryRecvError::Closed) => panic!("closed without Exit"),
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(5))
                }
            }
        }
        assert!(exited);
        while (!session._reader.is_finished() || !session._monitor.is_finished())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(session._reader.is_finished());
        assert!(session._monitor.is_finished());
    }

    #[test]
    fn receiver_drop_stops_a_streaming_child_before_the_session_drops() {
        let (session, rx) = spawn(
            "[Console]::WriteLine('FIXTURE_READY'); while ($true) { [Console]::Write(('X' * 4096)) }",
        );
        wait_ready(&session, &rx);
        drop(rx);

        let deadline = Instant::now() + Duration::from_secs(10);
        while (!session._reader.is_finished() || !session._monitor.is_finished())
            && Instant::now() < deadline
        {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(session._reader.is_finished());
        assert!(session._monitor.is_finished());
        assert!(
            session
                .input_closed
                .load(std::sync::atomic::Ordering::Acquire)
        );
    }

    #[test]
    fn output_is_drained_before_exit_after_a_large_final_write() {
        let (session, rx) =
            spawn("[Console]::Write(('X' * 262144)); [Console]::WriteLine('FINAL_OUTPUT'); exit 7");
        let (code, output) = wait_exit(&session, &rx);
        assert_eq!(code, Some(7));
        assert!(
            output.len() > 65536,
            "fixture must span more than one reader batch"
        );
        assert!(output.windows(12).any(|bytes| bytes == b"FINAL_OUTPUT"));
    }

    #[test]
    fn killed_child_reports_exit_even_with_blocked_input() {
        let (session, rx) = spawn("[Console]::WriteLine('FIXTURE_READY'); Start-Sleep -Seconds 60");
        wait_ready(&session, &rx);
        session
            .writer
            .lock()
            .write_all(&vec![b'x'; 2 * 1024 * 1024])
            .unwrap();
        // The child deliberately never reads stdin: this is much larger than
        // its pipe buffer and lets the worker enter a blocking WriteFile.
        std::thread::sleep(Duration::from_millis(100));
        session.kill();
        let (code, _) = wait_exit(&session, &rx);
        assert!(code.is_some());
    }

    #[test]
    fn a_live_child_still_receives_input_and_resizes() {
        let (session, rx) = spawn(
            "[Console]::WriteLine('FIXTURE_READY'); $null = [Console]::ReadLine(); [Console]::WriteLine(('WIDTH_' + [Console]::WindowWidth)); exit 0",
        );
        wait_ready(&session, &rx);
        assert!(session.resize(101, 24));
        std::thread::sleep(RESIZE_SETTLE + Duration::from_millis(10));
        assert!(!session.resize(101, 24));
        session.write_input(b"finish\r");
        let (code, output) = wait_exit(&session, &rx);
        assert_eq!(code, Some(0));
        assert!(output.windows(9).any(|bytes| bytes == b"WIDTH_101"));
    }
}

#[cfg(test)]
mod bounded_output_pty_tests {
    use super::*;
    use std::io::Write;

    const FIXTURE_ENV: &str = "MUXEL_PTY_OUTPUT_FIXTURE_CHILD";
    const START_MARKER: &[u8] = b"<MUXEL-PTY-OUTPUT-START>";
    const END_MARKER: &[u8] = b"<MUXEL-PTY-OUTPUT-END>";
    const RECORD_PREFIX: &[u8] = b"MUXEL_REC:";

    fn fixture_payload() -> Vec<u8> {
        let motif = b"ascii|\xc2\xb5|\xe4\xb8\xad|0123456789abcdef0123456789abcdef|\r\n";
        let mut payload = Vec::with_capacity(384 * 1024);
        let mut sequence = 0u32;
        while payload.len() < 384 * 1024 {
            payload.extend_from_slice(format!("MUXEL_REC:{sequence:08x}|").as_bytes());
            payload.extend_from_slice(motif);
            sequence = sequence.wrapping_add(1);
        }
        payload
    }

    fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    fn record_sequences(output: &[u8]) -> Vec<u32> {
        let mut sequences = Vec::new();
        let mut offset = 0;
        while let Some(relative) = find_bytes(&output[offset..], RECORD_PREFIX) {
            let record = offset + relative + RECORD_PREFIX.len();
            let Some(hex) = output.get(record..record + 8) else {
                break;
            };
            if output.get(record + 8) == Some(&b'|') && hex.iter().all(u8::is_ascii_hexdigit) {
                let hex = std::str::from_utf8(hex).expect("ASCII fixture sequence");
                sequences.push(u32::from_str_radix(hex, 16).expect("hex fixture sequence"));
            }
            offset = record + 8;
        }
        sequences
    }

    fn spawn_fixture_to_start() -> (Arc<TerminalSession>, PtyReceiver, Vec<u8>) {
        let test_exe = std::env::current_exe().expect("resolve current test executable");
        let mut spec = CommandSpec::program(
            test_exe.to_string_lossy(),
            vec![
                "bounded_output_pty_tests::real_pty_fixture_child".to_string(),
                "--nocapture".to_string(),
                "--test-threads=1".to_string(),
            ],
        );
        spec.env.push((FIXTURE_ENV.to_string(), "1".to_string()));
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn PTY fixture");

        // ConPTY asks for the cursor position while attaching some console
        // programs. Complete that handshake, then stop draining as soon as the
        // fixture marker arrives so the payload itself can saturate the queue.
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        while find_bytes(&output, START_MARKER).is_none() {
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    if bytes.windows(4).any(|window| window == b"\x1b[6n") {
                        session.write_input(b"\x1b[1;1R");
                    }
                    output.extend(bytes);
                }
                Ok(PtyChunk::Exit { read_error, .. }) => {
                    panic!("fixture exited before its start marker: {read_error:?}")
                }
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "fixture did not reach its start marker"
                    );
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("fixture transport closed before its start marker")
                }
            }
        }
        (session, rx, output)
    }

    fn wait_for_saturation(rx: &PtyReceiver) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let stats = rx.stats();
            if stats.queued_chunks == PTY_OUTPUT_QUEUE_CHUNK_CAPACITY
                || stats.queued_bytes
                    >= PTY_OUTPUT_QUEUE_BYTE_CAPACITY.saturating_sub(PTY_READ_BUFFER_BYTES)
            {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "fixture did not saturate bounded transport: {stats:?}"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The parent test starts this exact test binary inside a real platform
    /// PTY. Normal test discovery takes the no-op branch.
    #[test]
    fn real_pty_fixture_child() {
        if std::env::var_os(FIXTURE_ENV).is_none() {
            return;
        }

        let payload = fixture_payload();
        let mut stdout = std::io::stdout().lock();
        stdout.write_all(START_MARKER).expect("write start marker");
        // Some older ConPTY hosts can inject repaint sequences between writes.
        // Keep each record token in one write so decoration cannot split the
        // sequence number, while deliberately fragmenting early record bodies
        // to retain coverage for arbitrary UTF-8/output chunk boundaries.
        const RECORD_TOKEN_BYTES: usize = RECORD_PREFIX.len() + 8 + 1;
        const FRAGMENTED_RECORDS: usize = 32;
        let fragment_sizes = [1, 2, 7, 63];
        let mut offset = 0;
        for record in payload
            .split_inclusive(|byte| *byte == b'\n')
            .take(FRAGMENTED_RECORDS)
        {
            assert!(record.len() >= RECORD_TOKEN_BYTES);
            stdout
                .write_all(&record[..RECORD_TOKEN_BYTES])
                .expect("write atomic fixture token");
            let mut body_offset = RECORD_TOKEN_BYTES;
            let mut fragment = 0;
            while body_offset < record.len() {
                let end = (body_offset + fragment_sizes[fragment % fragment_sizes.len()])
                    .min(record.len());
                stdout
                    .write_all(&record[body_offset..end])
                    .expect("write fixture body fragment");
                body_offset = end;
                fragment += 1;
            }
            offset += record.len();
        }
        stdout
            .write_all(&payload[offset..])
            .expect("write fixture payload tail");
        stdout.write_all(END_MARKER).expect("write end marker");
        stdout.flush().expect("flush fixture output");
    }

    #[test]
    fn real_pty_saturation_preserves_records_and_exit_order() {
        let (session, rx, mut transport_output) = spawn_fixture_to_start();
        // Only the Windows branch below takes the session out early.
        #[cfg_attr(not(windows), allow(unused_mut))]
        let mut session = Some(session);
        wait_for_saturation(&rx);

        let saturated = rx.stats();
        assert!(saturated.queued_bytes <= PTY_OUTPUT_QUEUE_BYTE_CAPACITY);
        assert!(saturated.queued_chunks <= PTY_OUTPUT_QUEUE_CHUNK_CAPACITY);

        let receive_deadline = Instant::now() + Duration::from_secs(20);
        let mut saw_exit = false;
        let mut teardown_requested = false;
        while Instant::now() < receive_deadline {
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    if bytes.windows(4).any(|window| window == b"\x1b[6n")
                        && let Some(session) = &session
                    {
                        session.write_input(b"\x1b[1;1R");
                    }
                    transport_output.extend(bytes);
                    if !teardown_requested && find_bytes(&transport_output, END_MARKER).is_some() {
                        // ConPTY may retain the console host after the test body
                        // has finished. Owned teardown must still cancel it and
                        // deliver Exit after every preceding output byte.
                        #[cfg(windows)]
                        drop(session.take());
                        teardown_requested = true;
                    }
                }
                Ok(PtyChunk::Exit { .. }) => {
                    saw_exit = true;
                    break;
                }
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(async_channel::TryRecvError::Closed) => break,
            }
        }
        assert!(
            saw_exit,
            "fixture exit was not delivered after its output: bytes={}, end_marker={}, stats={:?}",
            transport_output.len(),
            find_bytes(&transport_output, END_MARKER).is_some(),
            rx.stats()
        );

        let payload_start = find_bytes(&transport_output, START_MARKER)
            .map(|index| index + START_MARKER.len())
            .expect("start marker in PTY output");
        let payload_end = find_bytes(&transport_output[payload_start..], END_MARKER)
            .map(|index| payload_start + index)
            .expect("end marker in PTY output");
        let expected_sequences = record_sequences(&fixture_payload());
        let actual_sequences = record_sequences(&transport_output[payload_start..payload_end]);
        assert_eq!(
            actual_sequences, expected_sequences,
            "PTY transport lost, duplicated, or reordered fixture records"
        );

        let drained = rx.stats();
        assert_eq!(drained.queued_bytes, 0);
        assert_eq!(drained.queued_chunks, 0);
        assert!(drained.high_water_bytes <= PTY_OUTPUT_QUEUE_BYTE_CAPACITY);
        assert!(drained.high_water_chunks <= PTY_OUTPUT_QUEUE_CHUNK_CAPACITY);
    }

    #[test]
    fn saturated_real_pty_teardown_returns_promptly() {
        let (session, rx, _output) = spawn_fixture_to_start();
        wait_for_saturation(&rx);

        let started = Instant::now();
        drop(rx);
        drop(session);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "dropping a saturated PTY blocked its owner"
        );
    }

    /// Whether the reader thread has finished and dropped its sender.
    fn wait_for_reader_release(rx: &PtyReceiver, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        while rx.inner.inner.rx.sender_count() > 0 {
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        true
    }

    #[cfg(windows)]
    fn process_is_running(pid: u32) -> bool {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn OpenProcess(access: u32, inherit: i32, pid: u32) -> *mut std::ffi::c_void;
            fn WaitForSingleObject(handle: *mut std::ffi::c_void, millis: u32) -> u32;
            fn CloseHandle(handle: *mut std::ffi::c_void) -> i32;
        }
        const SYNCHRONIZE: u32 = 0x0010_0000;
        const WAIT_TIMEOUT: u32 = 0x0000_0102;

        // SAFETY: OpenProcess takes plain values and returns null on failure.
        let handle = unsafe { OpenProcess(SYNCHRONIZE, 0, pid) };
        if handle.is_null() {
            return false;
        }
        // SAFETY: `handle` is a live process handle opened with SYNCHRONIZE
        // above, and it is closed exactly once here.
        unsafe {
            let running = WaitForSingleObject(handle, 0) == WAIT_TIMEOUT;
            CloseHandle(handle);
            running
        }
    }

    /// Before Windows 11 24H2, `ClosePseudoConsole` waits until the console's
    /// pending output is drained; from 24H2 on it returns at once, so timing
    /// alone proves nothing on a current host. Dropping the session first must
    /// release a reader parked on a full queue whose receiver is still held
    /// and never drained, so that the reader drains the PTY to EOF itself.
    #[test]
    fn saturated_real_pty_session_first_teardown_releases_reader() {
        let (session, rx, _output) = spawn_fixture_to_start();
        wait_for_saturation(&rx);
        // The queue holds at most 256 KiB of the fixture's 384 KiB payload, so
        // the fixture is still blocked writing the rest into the console.
        #[cfg(windows)]
        assert!(
            process_is_running(session.child_pid.expect("fixture pid")),
            "the fixture finished before teardown, leaving no output pending"
        );

        let (done_tx, done_rx) = std::sync::mpsc::sync_channel(1);
        let teardown = std::thread::spawn(move || {
            let started = Instant::now();
            drop(session);
            let elapsed = started.elapsed();
            let _ = done_tx.send(elapsed);
        });
        let elapsed = done_rx.recv_timeout(Duration::from_secs(2));
        let released = wait_for_reader_release(&rx, Duration::from_secs(5));
        let held = rx.stats();
        // Also release a failed run cleanly: receiver closure wakes the
        // backpressured reader, allowing the teardown thread to finish.
        drop(rx);
        teardown.join().expect("session teardown thread panicked");
        let elapsed = elapsed.expect("dropping the session first blocked on a saturated PTY");
        assert!(
            elapsed < Duration::from_secs(2),
            "dropping a saturated PTY session blocked before its receiver"
        );
        assert!(
            released,
            "the reader stayed parked on its full queue after the session dropped: {held:?}"
        );
    }

    #[cfg(windows)]
    fn process_handle_count() -> u32 {
        #[link(name = "kernel32")]
        unsafe extern "system" {
            fn GetCurrentProcess() -> *mut std::ffi::c_void;
            fn GetProcessHandleCount(process: *mut std::ffi::c_void, handle_count: *mut u32)
            -> i32;
        }

        let mut count = 0;
        // SAFETY: GetCurrentProcess returns the current process pseudo-handle;
        // `count` remains live and writable for the duration of the call.
        let succeeded = unsafe { GetProcessHandleCount(GetCurrentProcess(), &mut count) };
        assert_ne!(succeeded, 0, "GetProcessHandleCount failed");
        count
    }

    #[cfg(windows)]
    #[test]
    #[ignore = "controlled resource-plateau trial; run alone with --test-threads=1"]
    fn repeated_real_pty_saturation_plateaus_process_handles() {
        const ROUNDS: usize = 12;
        const HANDLE_SLACK: u32 = 4;

        // Warm process-global ConPTY/test machinery before taking the baseline.
        real_pty_saturation_preserves_records_and_exit_order();
        saturated_real_pty_teardown_returns_promptly();
        saturated_real_pty_session_first_teardown_releases_reader();
        let baseline = process_handle_count();
        for _ in 0..ROUNDS {
            real_pty_saturation_preserves_records_and_exit_order();
            saturated_real_pty_teardown_returns_promptly();
            saturated_real_pty_session_first_teardown_releases_reader();
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut handles = process_handle_count();
        while handles > baseline + HANDLE_SLACK && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(25));
            handles = process_handle_count();
        }
        assert!(
            handles <= baseline + HANDLE_SLACK,
            "{ROUNDS} real-PTY saturation/teardown rounds retained process handles: baseline={baseline}, final={handles}"
        );
        eprintln!("real-PTY handle plateau: rounds={ROUNDS}, baseline={baseline}, final={handles}");
    }
}

// Windows PATH resolution for npm shims (CreateProcess cannot run #!/bin/sh).
#[cfg(all(test, windows))]
mod windows_spawn_resolve {
    use super::{
        CommandSpec, PtyChunk, TerminalSession, literal_program_for_spawn,
        resolve_program_for_spawn_windows,
    };
    use std::io::Write;
    use std::sync::Mutex;
    use std::time::{Duration, Instant};

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn literal_launch_refuses_batch_scripts_before_spawning() {
        for extension in ["cmd", "bat"] {
            let path = std::env::temp_dir().join(format!(
                "muxel-literal-{}.{}",
                uuid::Uuid::new_v4(),
                extension
            ));
            std::fs::write(&path, "@echo off\r\n").unwrap();
            let error = literal_program_for_spawn(path.to_str().unwrap())
                .unwrap_err()
                .to_string();
            assert!(error.contains(path.to_str().unwrap()), "{error}");
            assert!(
                error.contains("native executable") && error.contains("--preset"),
                "{error}"
            );
            let spec = CommandSpec::literal(path.display().to_string(), vec![]);
            assert!(
                spec.program_for_spawn().is_err(),
                "final boundary allowed batch target"
            );
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn literal_executable_is_resolved_once_even_with_a_batch_sidecar() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("muxel-native-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("tool.exe");
        std::fs::write(&exe, b"native fixture").unwrap();
        std::fs::write(dir.join("tool.exe.cmd"), b"@echo off\r\n").unwrap();
        let resolved = literal_program_for_spawn(exe.to_str().unwrap()).unwrap();
        assert_eq!(std::path::Path::new(&resolved), exe);
        let spec = CommandSpec::literal(&resolved, vec!["brief".into()]);
        // Exercise the same final-boundary method used by TerminalSession::spawn.
        // The ordinary resolver would prefer tool.exe.cmd on its second lookup.
        assert_eq!(spec.program_for_spawn().unwrap(), resolved);
        std::fs::remove_dir_all(dir).unwrap();
    }

    // Invoked as a child of the real ConPTY launch test below. The libtest `--`
    // separator accepts arbitrary filter tokens, letting this executable itself
    // capture the original argv without a shell or another tool dependency.
    #[test]
    fn literal_argv_fixture() {
        let Ok(path) = std::env::var("MUXEL_TEST_ARGV_CAPTURE") else {
            return;
        };
        let args: Vec<_> = std::env::args()
            .skip_while(|arg| arg != "--")
            .skip(1)
            .collect();
        std::fs::write(path, serde_json::to_vec(&args).unwrap()).unwrap();
    }

    #[test]
    fn native_pty_preserves_literal_argv() {
        let _guard = ENV_LOCK.lock().unwrap();
        let capture =
            std::env::temp_dir().join(format!("muxel-argv-{}.json", uuid::Uuid::new_v4()));
        let payload: Vec<String> = [
            "a brief with spaces",
            "say \"hello\"",
            "",
            "--",
            "--background",
            "ampersand & pipe | percent % caret ^",
            "unicode 🦀",
            "trailing\\",
            "white space\\\\",
            "backslashes\\\\\"quote",
            "line one\nline two",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        let mut args: Vec<String> = [
            "--exact",
            "session::windows_spawn_resolve::literal_argv_fixture",
            "--nocapture",
            "--",
        ]
        .into_iter()
        .map(str::to_string)
        .collect();
        args.extend(payload.clone());
        let program =
            literal_program_for_spawn(std::env::current_exe().unwrap().to_str().unwrap()).unwrap();
        let mut spec = CommandSpec::literal(program, args);
        spec.env.push((
            "MUXEL_TEST_ARGV_CAPTURE".into(),
            capture.display().to_string(),
        ));
        let (session, rx) =
            TerminalSession::spawn(spec, 80, 24).expect("spawn native argv fixture");
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut received: Option<Vec<String>> = None;
        while Instant::now() < deadline {
            // ConPTY can keep its output pipe open after the child writes its
            // result. The contract here is argv delivery, not pipe teardown.
            if let Ok(bytes) = std::fs::read(&capture)
                && let Ok(args) = serde_json::from_slice(&bytes)
            {
                received = Some(args);
                break;
            }
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    if bytes.windows(4).any(|window| window == b"\x1b[6n") {
                        session.write_input(b"\x1b[1;1R");
                    }
                }
                Ok(PtyChunk::Exit { .. }) | Err(async_channel::TryRecvError::Closed) => {
                    break;
                }
                Err(async_channel::TryRecvError::Empty) => {}
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        session.kill();
        drop(_guard);
        let received = received
            .or_else(|| {
                std::fs::read(&capture)
                    .ok()
                    .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            })
            .expect("native child captured argv");
        std::fs::remove_file(capture).unwrap();
        assert_eq!(received, payload);
    }

    #[test]
    fn prefers_cmd_over_shebang_shim() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("muxel-spawn-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("fakeagent");
        {
            let mut f = std::fs::File::create(&shim).unwrap();
            writeln!(f, "#!/bin/sh").unwrap();
            writeln!(f, "echo hi").unwrap();
        }
        let cmd = dir.join("fakeagent.cmd");
        {
            let mut f = std::fs::File::create(&cmd).unwrap();
            writeln!(f, "@echo off").unwrap();
        }

        let prev = std::env::var_os("PATH");
        let prev_pathext = std::env::var_os("PATHEXT");
        let mut path = dir.as_os_str().to_owned();
        path.push(";");
        if let Some(p) = &prev {
            path.push(p);
        }
        // SAFETY: single-threaded unit test; PATH restored before return.
        unsafe {
            std::env::set_var("PATH", &path);
            std::env::set_var("PATHEXT", ".COM;.EXE;.BAT;.CMD");
        }

        let resolved = resolve_program_for_spawn_windows("fakeagent");
        assert!(
            resolved.to_ascii_lowercase().ends_with("fakeagent.cmd"),
            "expected .cmd, got {resolved}"
        );

        unsafe {
            match prev {
                Some(p) => std::env::set_var("PATH", p),
                None => std::env::remove_var("PATH"),
            }
            match prev_pathext {
                Some(p) => std::env::set_var("PATHEXT", p),
                None => std::env::remove_var("PATHEXT"),
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn codex_cmd_prefers_its_packaged_native_executable() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = std::env::temp_dir().join(format!("muxel-codex-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let (package, target) = if cfg!(target_arch = "aarch64") {
            ("codex-win32-arm64", "aarch64-pc-windows-msvc")
        } else {
            ("codex-win32-x64", "x86_64-pc-windows-msvc")
        };
        let native = dir
            .join("node_modules")
            .join("@openai")
            .join("codex")
            .join("node_modules")
            .join("@openai")
            .join(package)
            .join("vendor")
            .join(target)
            .join("bin")
            .join("codex.exe");
        std::fs::create_dir_all(native.parent().unwrap()).unwrap();
        std::fs::write(dir.join("codex.cmd"), "@echo off\r\n").unwrap();
        std::fs::write(&native, b"fixture").unwrap();

        let prev = std::env::var_os("PATH");
        let prev_pathext = std::env::var_os("PATHEXT");
        unsafe {
            std::env::set_var("PATH", &dir);
            std::env::set_var("PATHEXT", ".COM;.EXE;.BAT;.CMD");
        }
        let resolved = resolve_program_for_spawn_windows("codex");
        let literal = literal_program_for_spawn("codex")
            .expect("packaged native Codex supports literal argv");
        unsafe {
            match prev {
                Some(value) => std::env::set_var("PATH", value),
                None => std::env::remove_var("PATH"),
            }
            match prev_pathext {
                Some(value) => std::env::set_var("PATHEXT", value),
                None => std::env::remove_var("PATHEXT"),
            }
        }

        assert_eq!(std::path::PathBuf::from(resolved), native);
        assert_eq!(std::path::PathBuf::from(literal), native);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn batch_runner_preserves_argument_boundaries_and_empty_positions() {
        let dir = std::env::temp_dir().join(format!("muxel-batch-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let command = dir.join("echo-one.cmd");
        std::fs::write(&command, "@echo off\r\necho ARGS=[%~1][%~2][%~3]\r\n").unwrap();
        let spec = CommandSpec::program(
            command.to_string_lossy(),
            vec![
                "Reply with exactly OK".to_string(),
                String::new(),
                "--version".to_string(),
            ],
        );
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn batch fixture");
        const EXPECTED: &[u8] = b"ARGS=[Reply with exactly OK][][--version]";
        let deadline = Instant::now() + Duration::from_secs(15);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    if bytes.windows(4).any(|window| window == b"\x1b[6n") {
                        session.write_input(b"\x1b[1;1R");
                    }
                    output.extend(bytes);
                    if output
                        .windows(EXPECTED.len())
                        .any(|window| window == EXPECTED)
                    {
                        break;
                    }
                }
                Ok(PtyChunk::Exit { .. }) | Err(async_channel::TryRecvError::Closed) => break,
                Err(async_channel::TryRecvError::Empty) => {}
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = String::from_utf8_lossy(&output);
        assert!(
            output
                .as_bytes()
                .windows(EXPECTED.len())
                .any(|window| window == EXPECTED),
            "batch runner changed argument boundaries: {output:?}"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Manual installed-tool smoke test for the full PTY launch path. Ignored in
    /// CI because Codex is not a repo dependency; run it on Windows when changing
    /// npm shim resolution or launch-time instructions.
    #[test]
    #[ignore = "requires an installed npm Codex CLI"]
    fn installed_codex_accepts_instruction_argv_through_pty() {
        let resolved = resolve_program_for_spawn_windows("codex");
        assert!(
            resolved.to_ascii_lowercase().ends_with("codex.exe"),
            "installed Codex did not resolve to its packaged native executable: {resolved}"
        );
        let instruction = muxel_core::codex_developer_instructions_override(
            "Use [name](file:///D:/dev/project/file.rs#L12C4); keep <targets> exact.",
        );
        let spec = CommandSpec::program(
            "codex",
            vec!["--config".to_string(), instruction, "--version".to_string()],
        );
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn Codex");
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut output = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    if bytes.windows(4).any(|window| window == b"\x1b[6n") {
                        session.write_input(b"\x1b[1;1R");
                    }
                    output.extend(bytes);
                }
                Ok(PtyChunk::Exit { .. }) | Err(async_channel::TryRecvError::Closed) => break,
                Err(async_channel::TryRecvError::Empty) => {}
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        let output = String::from_utf8_lossy(&output);
        assert!(
            output.contains("codex-cli"),
            "Codex did not reach its public CLI: {output:?}"
        );
    }
}

#[cfg(test)]
mod runtime_fact_tests {
    use super::{
        CommandSpec, PtyOutput, RESIZE_SETTLE, TerminalSession, draft_after_input, next_input_state,
    };
    use async_channel::TryRecvError;
    use parking_lot::Mutex;
    use std::sync::Arc;
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant, SystemTime};

    #[test]
    fn draft_state_changes_only_for_admitted_input_or_submission() {
        assert!(!draft_after_input(false, false, false));
        assert!(draft_after_input(false, false, true));
        assert!(!draft_after_input(true, true, true));
        assert!(draft_after_input(true, false, true));
    }

    #[test]
    fn every_input_write_advances_the_revision_and_only_text_moves_the_draft() {
        let state = |rev: u64, draft: bool| (rev << 1) | u64::from(draft);
        assert_eq!(
            next_input_state(state(4, false), Some(false)),
            state(5, true)
        );
        assert_eq!(
            next_input_state(state(4, true), Some(true)),
            state(5, false)
        );
        // A mouse or wheel report leaves the draft as it was.
        assert_eq!(next_input_state(state(4, true), None), state(5, true));
        assert_eq!(next_input_state(state(4, false), None), state(5, false));
    }

    #[test]
    fn runtime_invalidations_are_bounded_and_input_facts_are_atomic() {
        #[cfg(windows)]
        let spec = CommandSpec::program(
            "cmd.exe",
            vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
        );
        #[cfg(unix)]
        let spec = CommandSpec::program("/bin/cat", vec![]);

        let (session, _output) = TerminalSession::spawn(spec, 80, 24).expect("spawn fixture");
        let events = session.runtime_events();

        session.write_input(b"x");
        assert_eq!(session.input_snapshot(), (1, true));
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));

        // A soft newline is ordinary draft input; the view deliberately does
        // not mark it as a turn submission.
        session.write_input(b"\x1b\r");
        assert_eq!(session.input_snapshot(), (2, true));

        session.mark_turn_submitted();
        session.write_input(b"\r");
        assert_eq!(session.input_snapshot(), (3, false));
        assert_eq!(events.try_recv(), Ok(()));
        assert!(matches!(events.try_recv(), Err(TryRecvError::Empty)));

        // Read long before the provider switched; then the switch is reported.
        let now = SystemTime::now();
        let mut stale = PtyOutput::default();
        stale.push(b"\x1b]0;stale title\x07\x07", now - Duration::from_secs(10));
        session.process_drained(&stale);
        assert!(session.has_bell());
        session.provider_switched(now, now);
        // The retired conversation's bell goes with its title.
        assert!(!session.has_bell());
        assert_eq!(session.title(), None);
        let read = session.runtime_evidence(None);
        assert!(read.titles.is_empty());
        assert!(!read.overflowed);
        assert!(!read.submitted);
        assert_eq!(events.try_recv(), Ok(()));
        session.clear_bell();
        assert_eq!(events.try_recv(), Ok(()));

        assert!(session.resize(81, 24));
        std::thread::sleep(RESIZE_SETTLE + Duration::from_millis(10));
        assert!(!session.resize(81, 24));
        assert_eq!(events.try_recv(), Ok(()));
        session.kill();
    }

    #[test]
    fn failed_submission_admission_leaves_no_facts_or_pending_marker() {
        struct RefuseSubmission {
            on_flush: bool,
        }
        impl std::io::Write for RefuseSubmission {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                if self.on_flush {
                    Ok(bytes.len())
                } else {
                    Err(std::io::ErrorKind::BrokenPipe.into())
                }
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Err(std::io::ErrorKind::BrokenPipe.into())
            }
        }

        for on_flush in [false, true] {
            let session = fixture();
            session.paste("prompt");
            let pasted = session.input_snapshot();
            let events = session.runtime_events();
            *session.writer.lock() = Box::new(RefuseSubmission { on_flush });
            assert!(session.try_submit_input(b"\r").is_err());
            assert_eq!(session.input_snapshot(), pasted);
            assert!(!session.has_submitted_turn());
            assert!(!session.runtime_evidence(None).submitted);
            assert!(!session.submission_pending.load(Ordering::Acquire));
            assert!(events.is_empty());

            // A recovered writer must treat the next key as ordinary draft
            // input, and only the following successful Enter as a submission.
            *session.writer.lock() = Box::new(std::io::sink());
            session.try_write_input(b"x").unwrap();
            assert_eq!(session.input_snapshot(), (pasted.0 + 1, true));
            assert!(events.is_empty());
            session.try_submit_input(b"\r").unwrap();
            assert_eq!(session.input_snapshot(), (pasted.0 + 2, false));
            assert!(session.has_submitted_turn());
            assert!(session.runtime_evidence(None).submitted);
            assert_eq!(events.try_recv(), Ok(()));
            session.kill();
        }
    }

    #[test]
    fn resume_selector_submission_keeps_its_pre_admission_timestamp() {
        struct HookDuringAdmission {
            hook_at: Arc<Mutex<Option<SystemTime>>>,
        }
        impl std::io::Write for HookDuringAdmission {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                assert_eq!(bytes, b"\r");
                // The writer has admitted Enter; the provider can publish its
                // resume hook before write_all/flush return to the caller.
                *self.hook_at.lock() = Some(SystemTime::now());
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                let hook_at = self.hook_at.lock().expect("Enter admitted");
                // Establish a strict order even on a coarse wall clock. There
                // is no scheduling race or fixed sleep in the assertion.
                let deadline = Instant::now() + Duration::from_secs(1);
                while SystemTime::now() <= hook_at {
                    assert!(Instant::now() < deadline, "wall clock did not advance");
                    std::thread::yield_now();
                }
                Ok(())
            }
        }

        let session = fixture();
        session.paste("/resume replacement");
        let pasted = session.input_snapshot();
        let hook_at = Arc::new(Mutex::new(None));
        *session.writer.lock() = Box::new(HookDuringAdmission {
            hook_at: hook_at.clone(),
        });
        session.try_submit_input(b"\r").unwrap();
        let submitted = session.input_snapshot();
        let hook_at = hook_at.lock().expect("hook published during admission");
        // Muxel learns of the hook after the writer returns. Its timestamp,
        // not the time the report was read, decides who received the Enter.
        session.provider_switched_at(hook_at, hook_at, SystemTime::now());
        let replacement_submitted = session.runtime_evidence(None).submitted;
        let launch_submitted = session.has_submitted_turn();

        // A later submission really does belong to the replacement.
        *session.writer.lock() = Box::new(std::io::sink());
        session.try_submit_input(b"\r").unwrap();
        let later_submitted = session.runtime_evidence(None).submitted;
        session.kill();

        assert_eq!(submitted, (pasted.0 + 1, false));
        assert!(launch_submitted);
        assert!(
            !replacement_submitted,
            "the resume selector Enter must not arm the replacement conversation"
        );
        assert!(later_submitted, "a later Enter must still arm a turn");
    }

    fn fixture() -> std::sync::Arc<TerminalSession> {
        #[cfg(windows)]
        let spec = CommandSpec::program(
            "cmd.exe",
            vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
        );
        #[cfg(unix)]
        let spec = CommandSpec::program("/bin/cat", vec![]);
        TerminalSession::spawn(spec, 80, 24)
            .expect("spawn fixture")
            .0
    }

    #[test]
    fn pointer_reports_advance_the_input_revision_but_not_the_draft() {
        let session = fixture();
        session.write_input(b"x");
        let (typed, draft) = session.input_snapshot();
        assert!(draft);
        session.report_mouse_button(0, 0, 0, true, false, false, false);
        session.report_mouse_button(0, 0, 0, false, false, false, false);
        assert_eq!(session.input_snapshot(), (typed + 2, true));
        // A wheel turn that only scrolls muxel's own scrollback never reaches
        // the program.
        session.scroll_wheel(-40.0, 20.0, 0, 0);
        assert_eq!(session.input_snapshot(), (typed + 2, true));
        session.kill();
    }
}

// These tests spawn `/bin/sh` and `/bin/cat`, so they are Unix-only.
#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use alacritty_terminal::grid::Dimensions;
    use alacritty_terminal::index::{Column, Line, Point as GridPoint};
    use std::time::{Duration, Instant};

    /// A dropped/pasted filename must be shell-quoted so it lands as an inert
    /// argument and can't run when the user hits Enter. Regression for the
    /// `{:?}` (Debug, not shell) quoting that left `$(…)`/backticks live.
    #[test]
    fn paste_paths_quoting_blocks_injection() {
        let quote_path_for_shell =
            |path: &str| muxel_core::path_paste::quote_path(PasteShell::Posix, path).unwrap();
        // Command-substitution payload → wrapped whole, nothing executes.
        assert_eq!(
            quote_path_for_shell("/tmp/$(touch pwned).txt"),
            "'/tmp/$(touch pwned).txt'"
        );
        // Backtick payload → likewise inert.
        assert_eq!(quote_path_for_shell("`id`.txt"), "'`id`.txt'");
        // Embedded single quote is escaped, not left to close the quote early.
        assert_eq!(quote_path_for_shell("a'b"), "'a'\\''b'");
        // Ordinary paths pass through unwrapped (sh_quote fast path).
        assert_eq!(quote_path_for_shell("/home/u/file.txt"), "/home/u/file.txt");
    }

    /// A forwarded mouse press must be remembered so the mouse-up handler can
    /// always emit the matching release (even if the pointer left the pane),
    /// and the release must clear it — otherwise the child app is left with a
    /// phantom held button.
    #[test]
    fn mouse_press_release_pairing() {
        let (session, _rx) =
            TerminalSession::spawn(CommandSpec::program("/bin/cat", vec![]), 80, 24)
                .expect("spawn");
        assert_eq!(session.mouse_press_pending(), None);
        session.report_mouse_button(3, 4, 0, true, false, false, false);
        assert_eq!(session.mouse_press_pending(), Some(0));
        session.report_mouse_button(3, 4, 0, false, false, false, false);
        assert_eq!(session.mouse_press_pending(), None);
    }

    /// A device-attributes query is answered by the PTY reader thread, without
    /// waiting for the UI to drain the output. A reply that waited could miss the
    /// asker's window: tmux then forwards it to the pane as typed keys, and a
    /// remote agent's prompt fills with `0;2501;1c`. Nothing here ever calls
    /// `process_output`, yet the child must still get its answer.
    #[test]
    fn device_attributes_are_answered_without_draining_output() {
        let out = std::env::temp_dir().join(format!("muxel-da-{}", Uuid::new_v4()));
        let script = format!(
            "stty raw -echo; printf '\\033[>c'; head -c 12 > '{}'",
            out.display()
        );
        let (session, rx) = TerminalSession::spawn(
            CommandSpec::program("/bin/sh", vec!["-c".into(), script]),
            80,
            24,
        )
        .expect("spawn");

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.try_recv() {
                Ok(PtyChunk::Exit { .. }) | Err(async_channel::TryRecvError::Closed) => break,
                Ok(PtyChunk::Output(_)) | Err(async_channel::TryRecvError::Empty) => {}
            }
            if Instant::now() > deadline {
                session.kill();
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let reply = std::fs::read(&out).unwrap_or_default();
        let _ = std::fs::remove_file(&out);
        assert_eq!(String::from_utf8_lossy(&reply), "\x1b[>0;2501;1c");
    }

    /// End-to-end check of the backend: spawn a process, drain its PTY output
    /// through the VTE parser, and confirm the text lands in the emulator grid.
    #[test]
    fn output_lands_in_grid() {
        let spec = CommandSpec::program("/bin/sh", vec!["-c".into(), "printf 'MUXEL_OK'".into()]);
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.recv_blocking() {
                Ok(PtyChunk::Output(bytes)) => session.process_output(&bytes),
                Ok(PtyChunk::Exit { .. }) => break,
                Err(_) => break,
            }
            if Instant::now() > deadline {
                break;
            }
        }

        let text = session.with_term(|term| {
            let grid = term.grid();
            let mut s = String::new();
            for row in 0..grid.screen_lines() {
                for col in 0..grid.columns() {
                    s.push(
                        grid[GridPoint {
                            line: Line(row as i32),
                            column: Column(col),
                        }]
                        .c,
                    );
                }
            }
            s
        });

        assert!(
            text.contains("MUXEL_OK"),
            "grid did not contain expected output; got: {:?}",
            text.trim()
        );
    }

    /// `visible_text()` returns the screen so status detection can scan it for
    /// markers like the Claude "esc to interrupt" working footer.
    #[test]
    fn visible_text_scans_screen_for_markers() {
        let spec = CommandSpec::program(
            "/bin/sh",
            vec!["-c".into(), "printf 'foo esc to interrupt bar'".into()],
        );
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.recv_blocking() {
                Ok(PtyChunk::Output(bytes)) => session.process_output(&bytes),
                Ok(PtyChunk::Exit { .. }) => break,
                Err(_) => break,
            }
            if Instant::now() > deadline {
                break;
            }
        }
        let screen = session.visible_text();
        assert!(
            screen.contains("esc to interrupt"),
            "visible_text should expose the marker; got: {:?}",
            screen.trim()
        );
        assert!(
            !screen.contains("❯ 1."),
            "no permission prompt was rendered"
        );
    }

    #[test]
    fn write_input_reaches_child() {
        // `cat` echoes stdin back; feed it a line and confirm it shows up.
        let spec = CommandSpec::program("/bin/cat", vec![]);
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");
        session.write_input(b"hello-muxel\n");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = false;
        while Instant::now() < deadline && !seen {
            match rx.recv_blocking() {
                Ok(PtyChunk::Output(bytes)) => {
                    session.process_output(&bytes);
                    let text = session.with_term(|term| {
                        let grid = term.grid();
                        let mut s = String::new();
                        for row in 0..grid.screen_lines() {
                            for col in 0..grid.columns() {
                                s.push(
                                    grid[GridPoint {
                                        line: Line(row as i32),
                                        column: Column(col),
                                    }]
                                    .c,
                                );
                            }
                        }
                        s
                    });
                    if text.contains("hello-muxel") {
                        seen = true;
                    }
                }
                Ok(PtyChunk::Exit { .. }) | Err(_) => break,
            }
        }
        session.kill();
        assert!(seen, "child did not echo written input back into the grid");
    }

    /// OSC-52 copy: the base64 payload is decoded by alacritty and queued for
    /// the view to land on the system clipboard.
    #[test]
    fn osc52_store_lands_in_pending_queue() {
        use alacritty_terminal::term::ClipboardType;
        let (session, _rx) =
            TerminalSession::spawn(CommandSpec::program("/bin/cat", vec![]), 80, 24)
                .expect("spawn");
        session.process_output(b"\x1b]52;c;aGVsbG8=\x07"); // base64("hello")
        let stores = session.take_clipboard_stores();
        assert_eq!(stores.len(), 1);
        assert_eq!(stores[0].0, ClipboardType::Clipboard);
        assert_eq!(stores[0].1, "hello");
        assert!(
            session.take_clipboard_stores().is_empty(),
            "drained on take"
        );
        session.kill();
    }

    /// `recent_text()` reaches into scrollback, rejoins soft-wrapped rows and skips
    /// the placeholder cell behind each wide glyph — read-aloud needs a reply that
    /// scrolled off the screen, as the lines the agent actually wrote.
    #[test]
    fn recent_text_reads_scrollback_and_rejoins_wraps() {
        let (session, _rx) =
            TerminalSession::spawn(CommandSpec::program("/bin/cat", vec![]), 20, 5).expect("spawn");
        session.process_output("0123456789ABCDEFGHIJKLMNOP\r\n".as_bytes());
        for i in 1..=8 {
            session.process_output(format!("line {i}\r\n").as_bytes());
        }
        session.process_output("日本 done\r\n".as_bytes());

        let all = session.recent_text(1000);
        assert!(
            all.contains("0123456789ABCDEFGHIJKLMNOP\n"),
            "wrapped row not rejoined / history not read: {all:?}"
        );
        assert!(all.contains("line 1\nline 2\n"), "{all:?}");
        assert!(
            all.contains("日本 done\n"),
            "wide-glyph spacer leaked: {all:?}"
        );
        // A small window is the bottom of the buffer, not the top.
        let tail = session.recent_text(3);
        assert!(!tail.contains("line 1"), "{tail:?}");
        assert!(tail.contains("日本 done"), "{tail:?}");
        session.kill();
    }

    /// Collect raw PTY bytes until `needle` shows up (the reply written to the
    /// child's stdin is echoed back by the tty/cat). Non-blocking receive with a
    /// hard deadline — `cat` never exits, so a blocking recv would hang the test
    /// binary forever if the reply never arrives.
    fn wait_for_reply(rx: &PtyReceiver, needle: &[u8]) -> bool {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen: Vec<u8> = Vec::new();
        while Instant::now() < deadline {
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    seen.extend_from_slice(&bytes);
                    if seen.windows(needle.len()).any(|w| w == needle) {
                        return true;
                    }
                }
                Ok(PtyChunk::Exit { .. }) => return false,
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(async_channel::TryRecvError::Closed) => return false,
            }
        }
        false
    }

    /// OSC-52 read probe: answered with a well-formed EMPTY reply — support is
    /// detectable, but the clipboard never leaks to the child. The query comes
    /// from the child, since the reader thread is what answers it. The needle is
    /// the reply as the tty echoes it, control chars in caret notation (`ESC` →
    /// `^[`, `BEL` → `^G`): nothing between `c;` and the terminator, and unlike
    /// the raw query the child printed.
    #[test]
    fn osc52_load_answers_empty() {
        let (session, rx) = TerminalSession::spawn(
            CommandSpec::program(
                "/bin/sh",
                vec!["-c".into(), "printf '\\033]52;c;?\\007'; exec cat".into()],
            ),
            80,
            24,
        )
        .expect("spawn");
        assert!(
            wait_for_reply(&rx, b"^[]52;c;^G"),
            "empty OSC-52 reply should reach the PTY"
        );
        session.kill();
    }

    #[cfg(unix)]
    #[test]
    fn uuid_title_tracks_session_switch_and_ignores_display_title() {
        let (session, _rx) =
            TerminalSession::spawn(CommandSpec::program("/bin/cat", vec![]), 80, 24)
                .expect("spawn");
        let id = "019f95d7-db31-7db0-904d-9e08330e0000";
        let resumed = "019f95d7-db31-7db0-904d-9e08330e0001";
        session.process_output(format!("\x1b]0;{id}\x07").as_bytes());
        session.process_output(b"\x1b]0;Review changes\x07");
        assert_eq!(session.title().as_deref(), Some("Review changes"));
        assert_eq!(session.session_id_hint().as_deref(), Some(id));
        session.process_output(format!("\x1b]0;{resumed}\x07").as_bytes());
        assert_eq!(session.session_id_hint().as_deref(), Some(resumed));
        session.kill();
    }

    /// The reader thread harvests the child's exit code after EOF, so the app
    /// can tell a clean exit from a crash.
    #[test]
    fn exit_code_is_reported() {
        let (_session, rx) = TerminalSession::spawn(
            CommandSpec::program("/bin/sh", vec!["-c".into(), "exit 7".into()]),
            80,
            24,
        )
        .expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                Instant::now() < deadline,
                "no exit chunk within the deadline"
            );
            match rx.try_recv() {
                Ok(PtyChunk::Output(_)) => {}
                Ok(PtyChunk::Exit {
                    code,
                    signal,
                    read_error,
                }) => {
                    assert_eq!(code, Some(7));
                    assert_eq!(signal, None, "a normal exit carries no signal");
                    assert_eq!(read_error, None, "a normal exit is a clean EOF");
                    break;
                }
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("channel closed without an exit chunk")
                }
            }
        }
    }

    /// A signalled child reports `code = 1` (portable_pty substitutes 1 when the
    /// OS gives no exit code), which is indistinguishable from a real `exit(1)`
    /// unless the signal name comes with it. Dropping the signal is what left a
    /// pane full of dead agents looking exactly like four crashed ones.
    #[test]
    fn signal_is_reported_alongside_the_substituted_code() {
        let (_session, rx) = TerminalSession::spawn(
            // The shell SIGKILLs itself: no exit code, only a signal.
            CommandSpec::program("/bin/sh", vec!["-c".into(), "kill -9 $$".into()]),
            80,
            24,
        )
        .expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(
                Instant::now() < deadline,
                "no exit chunk within the deadline"
            );
            match rx.try_recv() {
                Ok(PtyChunk::Output(_)) => {}
                Ok(PtyChunk::Exit { code, signal, .. }) => {
                    assert_eq!(
                        code,
                        Some(1),
                        "portable_pty substitutes 1 for a signalled child"
                    );
                    assert!(
                        signal.is_some(),
                        "the signal name must survive, or `code=1` is ambiguous"
                    );
                    break;
                }
                Err(async_channel::TryRecvError::Empty) => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("channel closed without an exit chunk")
                }
            }
        }
    }

    /// Closing a pane drops the UI receiver, and the kill may land while output
    /// is still in flight. The reader must reap the child anyway — it used to
    /// bail out on the failed send without waiting, leaving a permanent zombie
    /// (and its exit code unharvested) for every close with pending output.
    #[cfg(target_os = "linux")]
    #[test]
    fn child_is_reaped_after_receiver_drop() {
        let (session, rx) =
            TerminalSession::spawn(CommandSpec::program("/bin/cat", vec![]), 80, 24)
                .expect("spawn");
        let pid = session.child_pid.expect("child pid");
        // UI gone first; then trigger output so the reader hits the dead channel.
        drop(rx);
        session.write_input(b"ping\n");
        std::thread::sleep(Duration::from_millis(100));
        session.kill();
        let proc_dir = format!("/proc/{pid}");
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            if !std::path::Path::new(&proc_dir).exists() {
                return; // reaped — pid fully gone
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        let stat = std::fs::read_to_string(format!("{proc_dir}/stat")).unwrap_or_default();
        panic!("child was never reaped; /proc stat: {stat:?}");
    }

    /// A shell whose background descendant ignores SIGHUP and keeps the PTY
    /// slave open without writing. Returns the shell's and descendant's pids.
    #[cfg(unix)]
    fn spawn_with_silent_descendant() -> (Arc<TerminalSession>, PtyReceiver, u32, libc::pid_t) {
        let script = "(trap '' HUP; exec sleep 30) & echo \"descendant:$!\"; wait";
        let (session, rx) = TerminalSession::spawn(
            CommandSpec::program("/bin/sh", vec!["-c".into(), script.into()]),
            80,
            24,
        )
        .expect("spawn");
        let shell = session.child_pid.expect("shell pid");
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        let descendant = loop {
            let text = String::from_utf8_lossy(&seen);
            if let Some(rest) = text.split("descendant:").nth(1)
                && let Some(end) = rest.find(['\r', '\n'])
            {
                break rest[..end].parse().expect("descendant pid");
            }
            assert!(Instant::now() < deadline, "no descendant pid in {text:?}");
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => seen.extend(bytes),
                Ok(PtyChunk::Exit { .. }) => panic!("shell exited before the pane closed"),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        (session, rx, shell, descendant)
    }

    #[cfg(unix)]
    fn kill_descendant(descendant: libc::pid_t) {
        // SAFETY: `kill` takes plain values; this is the exact pid the shell
        // reported for the descendant this test started.
        unsafe { libc::kill(descendant, libc::SIGKILL) };
    }

    /// Whether `pid` has been reaped: signal 0 only probes for existence,
    /// and a zombie still answers until its parent waits on it.
    #[cfg(unix)]
    fn process_reaped(pid: u32) -> bool {
        // SAFETY: signal 0 delivers nothing; the pid is a child this test
        // started and the only thread reaping it is the session's reader.
        let missing = unsafe { libc::kill(pid as libc::pid_t, 0) } != 0;
        missing && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH)
    }

    /// A descendant that ignores SIGHUP can keep the PTY slave open, and
    /// silent, after its pane closes. Closing the pane must still wake the
    /// reader with no output arriving, so it reaps the shell rather than stay
    /// parked on the PTY until the descendant writes or exits.
    #[cfg(unix)]
    #[test]
    fn closed_pane_releases_reader_while_a_silent_descendant_holds_the_pty() {
        let (session, rx, shell, descendant) = spawn_with_silent_descendant();

        // Close the pane: receiver first, then the session (SIGHUP to the
        // shell). Only the reader reaps the shell, so a vanished pid means the
        // reader left the PTY although nothing more was written to it.
        drop(rx);
        drop(session);
        let deadline = Instant::now() + Duration::from_secs(10);
        while !process_reaped(shell) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        let reaped = process_reaped(shell);
        kill_descendant(descendant);
        assert!(
            reaped,
            "the reader stayed parked on a PTY held open by a silent descendant"
        );
    }

    /// The same descendant with the session dropped first and the receiver
    /// kept. The reader must stop waiting for output no terminal will render,
    /// reap the shell, and end the stream with Exit and then `Closed`, so a
    /// receiver that keeps draining is not left waiting on it.
    #[cfg(unix)]
    #[test]
    fn dropped_session_ends_the_stream_while_a_silent_descendant_holds_the_pty() {
        let (session, rx, shell, descendant) = spawn_with_silent_descendant();

        drop(session);
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut saw_exit = false;
        let closed = loop {
            match rx.try_recv() {
                Ok(PtyChunk::Output(_)) => {}
                Ok(PtyChunk::Exit { .. }) => saw_exit = true,
                Err(async_channel::TryRecvError::Closed) => break true,
                Err(async_channel::TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(async_channel::TryRecvError::Empty) => break false,
            }
        };
        // The reader reaps before it sends Exit and closes the stream.
        let reaped = process_reaped(shell);
        kill_descendant(descendant);
        assert!(
            closed,
            "the stream stayed open after its session dropped (exit seen: {saw_exit})"
        );
        assert!(saw_exit, "the stream closed without Exit");
        assert!(reaped, "the shell was not reaped before the stream closed");
    }

    /// Spawn `sh -c script` with `$MUXEL_HUP_MARKER` naming a fresh marker
    /// file, and wait for the script to print `ready`.
    #[cfg(unix)]
    fn spawn_hangup_script(
        script: &str,
        label: &str,
    ) -> (Arc<TerminalSession>, PtyReceiver, std::path::PathBuf) {
        let marker =
            std::env::temp_dir().join(format!("muxel-hup-cleanup-{}-{label}", std::process::id()));
        let _ = std::fs::remove_file(&marker);
        let mut spec = CommandSpec::program("/bin/sh", vec!["-c".into(), script.into()]);
        spec.env.push((
            "MUXEL_HUP_MARKER".into(),
            marker.to_string_lossy().into_owned(),
        ));
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");
        read_until(&rx, "ready");
        (session, rx, marker)
    }

    /// Read output until `needle` appears.
    #[cfg(unix)]
    fn read_until(rx: &PtyReceiver, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = Vec::new();
        while !String::from_utf8_lossy(&seen).contains(needle) {
            assert!(Instant::now() < deadline, "no {needle:?} from the shell");
            match rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => seen.extend(bytes),
                Ok(PtyChunk::Exit { .. }) => panic!("shell exited before {needle:?}"),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
    }

    /// A shell whose SIGHUP handler takes two seconds to "save" and then
    /// writes `saved` to a marker file before exiting. It ignores later HUPs:
    /// dropping the session closes the PTY master, and the kernel's own
    /// hangup that follows is outside muxel's control.
    #[cfg(unix)]
    fn spawn_with_slow_hangup_cleanup(
        order: &str,
    ) -> (Arc<TerminalSession>, PtyReceiver, std::path::PathBuf) {
        let script = "trap 'trap \"\" HUP; sleep 2; echo saved > \"$MUXEL_HUP_MARKER\"; exit 0' HUP; \
                      echo ready; while :; do sleep 0.1; done";
        spawn_hangup_script(script, order)
    }

    #[cfg(unix)]
    fn marker_saved(marker: &std::path::Path, within: Duration) -> bool {
        let deadline = Instant::now() + within;
        loop {
            if std::fs::read_to_string(marker).is_ok_and(|text| text.trim() == "saved") {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Dropping the session ends the stream without waiting on the child,
    /// but the child keeps its SIGHUP cleanup: it is not SIGKILLed while its
    /// handler is still saving.
    #[cfg(unix)]
    #[test]
    fn dropped_session_leaves_the_child_its_hangup_cleanup() {
        // Own both paths before spawning, including a panic while waiting for
        // ready. The release file must outlive the child, even on failure.
        struct Fixture {
            marker: std::path::PathBuf,
            release: std::path::PathBuf,
            child_pid: Option<u32>,
            queue: Option<Arc<OutputQueueState>>,
            drop_signal: Option<std::sync::mpsc::Sender<()>>,
            drop_worker: Option<JoinHandle<()>>,
            reaped: bool,
        }

        impl Drop for Fixture {
            fn drop(&mut self) {
                if !self.reaped {
                    let _ = std::fs::write(&self.release, "");
                    // Keep the reader as the sole owner of signalling and
                    // reaping. Closing the queue wakes it if the PTY is silent.
                    if let Some(queue) = &self.queue {
                        queue.force_child_exit_for_test();
                    }
                }
                if let Some(signal) = self.drop_signal.take() {
                    let _ = signal.send(());
                }
                if let Some(worker) = self.drop_worker.take() {
                    let deadline = Instant::now() + Duration::from_secs(3);
                    while !worker.is_finished() && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    if worker.is_finished() {
                        let _ = worker.join();
                    }
                }
                if let Some(pid) = self.child_pid
                    && !self.reaped
                {
                    let deadline = Instant::now() + Duration::from_secs(4);
                    while !process_reaped(pid) && Instant::now() < deadline {
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    self.reaped = process_reaped(pid);
                }
                if self.reaped || self.child_pid.is_none() {
                    let _ = std::fs::remove_file(&self.marker);
                    let _ = std::fs::remove_file(&self.release);
                }
            }
        }

        let marker =
            std::env::temp_dir().join(format!("muxel-hup-cleanup-{}-session", std::process::id()));
        let mut release = marker.clone().into_os_string();
        release.push(".release");
        let release = std::path::PathBuf::from(release);
        for path in [&marker, &release] {
            let _ = std::fs::remove_file(path);
            assert!(!path.exists(), "stale cleanup fixture: {path:?}");
        }
        let mut fixture = Fixture {
            marker,
            release,
            child_pid: None,
            queue: None,
            drop_signal: None,
            drop_worker: None,
            reaped: false,
        };

        // Hold cleanup until this observer has checked stream closure. A timed
        // save can finish while synchronous PTY destruction is still returning.
        let script = "trap 'trap \"\" HUP; sleep 2; \
                      while [ ! -f \"${MUXEL_HUP_MARKER}.release\" ]; do sleep 0.02; done; \
                      echo saved > \"$MUXEL_HUP_MARKER\"; exit 0' HUP; \
                      echo ready; while :; do sleep 0.1; done";
        let mut spec = CommandSpec::program("/bin/sh", vec!["-c".into(), script.into()]);
        spec.env.push((
            "MUXEL_HUP_MARKER".into(),
            fixture.marker.to_string_lossy().into_owned(),
        ));
        let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");
        let pid = session.child_pid.expect("fixture child pid");
        fixture.child_pid = Some(pid);
        fixture.queue = Some(session.output_queue.clone());
        let (drop_signal, drop_request) = std::sync::mpsc::channel();
        fixture.drop_signal = Some(drop_signal);
        // The worker owns the session before readiness can fail. The observer
        // never runs its destructor during a panic or a timeout.
        fixture.drop_worker = Some(std::thread::spawn(move || {
            let _ = drop_request.recv();
            drop(session);
        }));
        read_until(&rx, "ready");

        fixture.drop_signal.take().unwrap().send(()).unwrap();
        let deadline = Instant::now() + Duration::from_secs(3);
        let closed = loop {
            match rx.try_recv() {
                Ok(_) => {}
                Err(async_channel::TryRecvError::Closed) => break true,
                Err(async_channel::TryRecvError::Empty) if Instant::now() < deadline => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(async_channel::TryRecvError::Empty) => break false,
            }
        };
        let saved_before_release = fixture.marker.exists();
        // Release even after a timeout. Keep the pre-release closure result:
        // cleanup completing afterward must not turn a failure into a pass.
        let released = std::fs::write(&fixture.release, "").is_ok();
        let saved = released && marker_saved(&fixture.marker, Duration::from_secs(4));
        let worker_deadline = Instant::now() + Duration::from_secs(3);
        while !fixture.drop_worker.as_ref().unwrap().is_finished()
            && Instant::now() < worker_deadline
        {
            std::thread::sleep(Duration::from_millis(20));
        }
        let drop_result = if fixture.drop_worker.as_ref().unwrap().is_finished() {
            Some(fixture.drop_worker.take().unwrap().join())
        } else {
            None
        };
        let reap_deadline = Instant::now() + Duration::from_secs(3);
        while !process_reaped(pid) && Instant::now() < reap_deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        fixture.reaped = process_reaped(pid);
        let reaped = fixture.reaped;
        drop(fixture);
        assert!(released, "could not release the child's cleanup");
        assert!(
            matches!(drop_result, Some(Ok(()))),
            "session destructor did not finish or panicked"
        );
        assert!(closed, "the stream stayed open after its session dropped");
        assert!(
            !saved_before_release,
            "the child's cleanup ran before the observer released it"
        );
        assert!(
            saved,
            "the child was killed before its SIGHUP handler finished"
        );
        assert!(reaped, "the child was not reaped after its cleanup");
    }

    /// Dropping the receiver first stops the reader and hangs up the child;
    /// the child still gets to finish its SIGHUP cleanup.
    #[cfg(unix)]
    #[test]
    fn dropped_receiver_leaves_the_child_its_hangup_cleanup() {
        let (session, rx, marker) = spawn_with_slow_hangup_cleanup("receiver");

        drop(rx);
        let saved = marker_saved(&marker, Duration::from_secs(4));
        drop(session);
        let _ = std::fs::remove_file(&marker);
        assert!(
            saved,
            "the child was killed before its SIGHUP handler finished"
        );
    }

    /// A pane close while another owner still holds the session (as the
    /// view's startup task can): `kill` sends SIGHUP, then the receiver drops
    /// and wakes the reader. The reader must not send a second SIGHUP. This
    /// child restores the default action while it saves, so a second HUP
    /// kills it before the marker is written.
    #[cfg(unix)]
    #[test]
    fn retained_session_close_sends_one_hangup() {
        let script = "trap 'trap - HUP; echo saving; sleep 1; echo saved > \"$MUXEL_HUP_MARKER\"; exit 0' HUP; \
                      echo ready; while :; do sleep 0.1; done";
        let (session, rx, marker) = spawn_hangup_script(script, "retained");

        session.kill();
        // Drop the receiver only once the handler is running, so a second
        // SIGHUP cannot merge with the first while it is still pending.
        read_until(&rx, "saving");
        drop(rx);
        let saved = marker_saved(&marker, Duration::from_secs(4));
        drop(session);
        let _ = std::fs::remove_file(&marker);
        assert!(saved, "a second SIGHUP cut the child's cleanup short");
    }

    /// Closing the stream wakes a reader parked in its wait through the poller
    /// itself, not by closing a descriptor. On macOS a wake pipe's write end
    /// could be inherited by a process another thread spawned while the pipe
    /// was being created (there is no `pipe2`), and the reader then slept
    /// through its pane's close until that unrelated process exited.
    #[cfg(unix)]
    #[test]
    fn closing_the_stream_wakes_a_waiting_reader() {
        for close in ["session", "receiver"] {
            let (tx, rx) = output_queue::<PtyChunk>(1024, 16);
            let state = tx.state.clone();
            let poller = Arc::new(polling::Poller::new().unwrap());
            state.install_reader_wake(poller.clone());
            let closer = {
                let state = state.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(100));
                    match close {
                        "session" => state.close_session(),
                        _ => drop(rx),
                    }
                })
            };
            let started = Instant::now();
            let mut events = polling::Events::new();
            poller
                .wait(&mut events, Some(Duration::from_secs(5)))
                .unwrap();
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "closing the {close} did not wake the reader"
            );
            closer.join().unwrap();
            assert!(state.is_closed(), "{close}");
            assert!(
                state.counters.lock().reader_wake.is_none(),
                "the queue kept the reader's poller after waking it ({close})"
            );
            drop(tx);
        }
    }

    /// A stream that closed before the reader's wake was installed still
    /// wakes the reader's first wait.
    #[cfg(unix)]
    #[test]
    fn a_stream_closed_before_the_reader_waits_still_wakes_it() {
        let (tx, _rx) = output_queue::<PtyChunk>(1024, 16);
        tx.state.close_session();
        let poller = Arc::new(polling::Poller::new().unwrap());
        tx.state.install_reader_wake(poller.clone());
        let started = Instant::now();
        let mut events = polling::Events::new();
        poller
            .wait(&mut events, Some(Duration::from_secs(5)))
            .unwrap();
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(tx.state.counters.lock().reader_wake.is_none());
    }

    /// A finished reader releases its wake, so a receiver kept after the
    /// stream ends does not keep the reader's poller alive.
    #[test]
    fn finished_reader_releases_its_wake() {
        let (_session, rx) = TerminalSession::spawn(
            CommandSpec::program("/bin/sh", vec!["-c".into(), "exit 0".into()]),
            80,
            24,
        )
        .expect("spawn");
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.try_recv() {
                Ok(PtyChunk::Exit { .. }) => break,
                Ok(PtyChunk::Output(_)) => {}
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(
                        Instant::now() < deadline,
                        "no exit chunk within the deadline"
                    );
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(async_channel::TryRecvError::Closed) => {
                    panic!("channel closed without an exit chunk")
                }
            }
        }
        // The reader releases the wake before it sends Exit.
        assert!(
            rx.inner.inner.state.counters.lock().reader_wake.is_none(),
            "a finished reader left its wake installed"
        );
    }

    #[test]
    fn direct_child_reads_as_idle_foreground() {
        // The direct child (`cat`) is the terminal's foreground process group with
        // nothing running under it, so it reads as idle-foreground. (A shell running
        // a sub-command would put that command in a different group → false.)
        let spec = CommandSpec::program("/bin/cat", vec![]);
        let (session, _rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");
        // The kernel sets the foreground group as the child takes the pty; poll.
        let deadline = Instant::now() + Duration::from_secs(2);
        while Instant::now() < deadline && !session.is_idle_foreground() {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(
            session.is_idle_foreground(),
            "the direct child should be the foreground process group"
        );
        session.kill();
    }
}

#[cfg(test)]
mod startup_tests {
    use super::{CommandSpec, TerminalSession};
    use crate::startup::Guard;
    use std::{sync::atomic::Ordering, time::Duration};

    fn spawn() -> std::sync::Arc<TerminalSession> {
        let spec = if cfg!(windows) {
            CommandSpec::program("cmd.exe", vec!["/Q".into(), "/K".into()])
        } else {
            CommandSpec::program("/bin/cat", vec![])
        };
        TerminalSession::spawn(spec, 40, 12).expect("spawn").0
    }

    #[test]
    fn quiet_control_output_waits_for_the_actual_claude_composer() {
        let session = spawn();
        session.process_probe_output(b"\x1b[2J\x1b[H");
        let mut guard = Guard::new(0, Some(Duration::from_secs(30)));
        assert!(session.has_output());
        assert!(
            !guard
                .ready(
                    Duration::from_millis(2500),
                    0,
                    session.startup_screen().claude_composer(true)
                )
                .unwrap()
        );
        let frame = format!(
            "\x1b[2J\x1b[HClaude Code\r\n{}\r\n❯ Try a task\r\n{}\r\nmode\x1b[3;3H\x1b[?25h\x1b[?2004h",
            "─".repeat(30),
            "─".repeat(30)
        );
        session.process_probe_output(frame.as_bytes());
        assert!(session.startup_screen().claude_composer(true));
        assert!(
            !guard
                .ready(
                    Duration::from_millis(7000),
                    0,
                    session.startup_screen().claude_composer(true)
                )
                .unwrap()
        );
        assert!(
            guard
                .ready(
                    Duration::from_millis(7100),
                    0,
                    session.startup_screen().claude_composer(true)
                )
                .unwrap()
        );
        session.process_probe_output(b"\x1b[3;3H/b12-ready\x1b[K");
        assert!(!session.startup_screen().claude_composer(true));
        assert!(session.startup_screen().claude_composer(false));
        session.kill();
    }

    #[test]
    fn failed_submit_admission_does_not_mark_a_turn_submitted() {
        let session = spawn();
        session.input_closed.store(true, Ordering::Release);
        assert!(session.try_submit_input(b"\r").is_err());
        assert!(!session.has_submitted_turn());
        session.kill();
    }
}

#[cfg(test)]
mod input_rev_tests {
    use super::{CommandSpec, TermMode, TerminalSession};

    #[test]
    fn input_rev_counts_what_is_typed_after_a_paste() {
        let spec = if cfg!(windows) {
            CommandSpec::program("cmd.exe", vec!["/Q".into(), "/K".into()])
        } else {
            CommandSpec::program("/bin/cat", vec![])
        };
        let (session, _rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn");
        let before = session.input_rev();
        session.paste("a prompt");
        let pasted = session.input_rev();
        assert_eq!(pasted, before + 1);
        // Nothing written, and a focus report (not input): still unchanged. The
        // program asks for focus reports first, so the report is really sent.
        session.write_input(b"");
        session.process_probe_output(b"\x1b[?1004h");
        assert!(session.term.lock().mode().contains(TermMode::FOCUS_IN_OUT));
        session.report_focus(false);
        session.report_focus(true);
        // Cursor-position replies go straight to the writer, not user input.
        session.process_probe_output(b"\x1b[6n");
        assert_eq!(session.input_rev(), pasted);
        // A key typed after the paste: the prompt's Enter must now be withheld.
        session.write_input(b"x");
        assert_ne!(session.input_rev(), pasted);
        // A click is input too; a wheel turn that only scrolls muxel's own
        // scrollback never reaches the program, so it isn't.
        let typed = session.input_rev();
        session.report_mouse_button(0, 0, 0, true, false, false, false);
        session.report_mouse_button(0, 0, 0, false, false, false, false);
        assert_eq!(session.input_rev(), typed + 2);
        let clicked = session.input_rev();
        session.scroll_wheel(-40.0, 20.0, 0, 0);
        assert_eq!(session.input_rev(), clicked);
        // Mouse-mode wheel reports reach the child, as do alternate-screen
        // wheel turns translated to arrows. Both move the input revision.
        session.process_probe_output(b"\x1b[?1000h\x1b[?1006h");
        session.scroll_wheel(-40.0, 20.0, 0, 0);
        assert_ne!(session.input_rev(), clicked);
        let wheeled = session.input_rev();
        session.process_probe_output(b"\x1b[?1000l\x1b[?1049h\x1b[?1007h");
        session.scroll_wheel(-40.0, 20.0, 0, 0);
        assert_ne!(session.input_rev(), wheeled);
        session.kill();
    }
}
