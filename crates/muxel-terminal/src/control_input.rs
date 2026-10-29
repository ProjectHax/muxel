//! Outside-control (`muxel ctl`) input, and its route through tmux for a
//! terminal whose program is a tmux client.
//!
//! Bytes written to a tmux client's terminal pass through that client's key
//! tables before any pane sees them: the prefix, `prefix2`, `bind -n` keys and
//! the extended-key decoder can turn them into tmux commands, such as opening a
//! shell window inside an agent's pane. So ctl input to such a terminal goes to
//! its tmux pane through tmux itself (`paste-buffer`, `send-keys`), on the
//! server the client attached to, and never through the client. Which pane is
//! fixed once per terminal, when its client attaches: the pane it showed then,
//! by tmux pane id. If that can't be found, the write is refused; it never falls
//! back to the client.
//!
//! Every other terminal takes the same input as PTY bytes, as it always has.

use muxel_core::control::Key;
use muxel_core::tmux;
use parking_lot::Mutex;
use std::{
    io,
    process::Output,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    time::{Duration, Instant, SystemTime},
};

/// How long output waits before it tries to fix the pane again after a try
/// failed, so a client streaming output doesn't run tmux for every chunk.
const PIN_RETRY: Duration = Duration::from_secs(1);

/// One write `muxel ctl` makes to a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlInput {
    /// Text pasted as one paste, as [`crate::TerminalSession::try_paste`]
    /// pastes it: while the terminal is in bracketed-paste mode, bracketed and
    /// without ESC; otherwise with each line break sent as CR.
    Paste(String),
    /// Keys pressed together. `app_cursor` is the cursor-key mode a directly
    /// written terminal's arrow bytes are encoded for; tmux encodes keys for its
    /// own pane's modes.
    Keys { keys: Vec<Key>, app_cursor: bool },
    /// The Enter that submits a turn. Its lifecycle facts are recorded only once
    /// the Enter is admitted.
    Submit,
}

/// Runs `tmux <args>` on the server holding a terminal's tmux session, on this
/// machine or over SSH, with `stdin` (when given) as its standard input.
/// Blocking, so it only runs on a terminal's input worker; it must bound its own
/// time. An `Err` of kind `TimedOut` or `ConnectionAborted` means the command
/// may have run; any other `Err` means it never started.
pub type TmuxRunner = Arc<dyn Fn(&[String], Option<&[u8]>) -> io::Result<Output> + Send + Sync>;

/// How a terminal whose program is a tmux client takes ctl input: through its
/// session's tmux server. Built with the launch, from the session and server the
/// client attaches to.
#[derive(Clone)]
pub struct TmuxInputRoute {
    session: String,
    run: TmuxRunner,
    /// While outside control is on, the pane is fixed as soon as the client
    /// draws, rather than at the first ctl write.
    pin_on_attach: Arc<AtomicBool>,
}

impl TmuxInputRoute {
    pub fn new(
        session: impl Into<String>,
        pin_on_attach: Arc<AtomicBool>,
        run: impl Fn(&[String], Option<&[u8]>) -> io::Result<Output> + Send + Sync + 'static,
    ) -> Self {
        Self {
            session: session.into(),
            run: Arc::new(run),
            pin_on_attach,
        }
    }
}

impl std::fmt::Debug for TmuxInputRoute {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TmuxInputRoute")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}

/// Why a ctl write wasn't delivered.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlWriteError {
    /// The terminal's PTY writer is gone.
    Closed(String),
    /// The tmux pane this terminal showed when it attached couldn't be found.
    /// Nothing was typed.
    Unresolved(String),
    /// tmux refused the command, or it couldn't be started. Nothing was typed.
    Refused(String),
    /// The command started, but no answer came back in time or its connection
    /// dropped: it may or may not have been delivered.
    Unknown(String),
}

impl std::fmt::Display for ControlWriteError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Closed(why) => write!(f, "the terminal's input is closed ({why})"),
            Self::Unresolved(why) => write!(f, "{why}"),
            Self::Refused(why) => write!(f, "tmux refused it ({why}), so nothing was typed"),
            Self::Unknown(why) => write!(
                f,
                "tmux didn't answer ({why}), so it may or may not have been typed"
            ),
        }
    }
}

type Delivered = Result<(), ControlWriteError>;

/// A ctl write, done at once for a terminal muxel writes to itself, or queued
/// on its tmux route. Await [`Self::finish`] before the next write that depends
/// on it: a queued submit records its facts there.
#[must_use = "a queued ctl write reports, and a submit records, only when finished"]
pub struct ControlWrite(WriteState);

impl std::fmt::Debug for ControlWrite {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &self.0 {
            WriteState::Done(result) => f.debug_tuple("Done").field(result).finish(),
            WriteState::Queued { submit, .. } => f
                .debug_struct("Queued")
                .field("submit", &submit.is_some())
                .finish(),
        }
    }
}

enum WriteState {
    Done(Delivered),
    Queued {
        delivered: async_channel::Receiver<Delivered>,
        submit: Option<(Arc<crate::TerminalSession>, SystemTime)>,
    },
}

impl ControlWrite {
    pub(crate) fn done(result: Delivered) -> Self {
        Self(WriteState::Done(result))
    }

    pub(crate) fn queued(
        delivered: async_channel::Receiver<Delivered>,
        submit: Option<(Arc<crate::TerminalSession>, SystemTime)>,
    ) -> Self {
        Self(WriteState::Queued { delivered, submit })
    }

    /// Whether this write went to tmux rather than to the PTY.
    pub fn is_queued(&self) -> bool {
        matches!(self.0, WriteState::Queued { .. })
    }

    /// A direct write's outcome, which is known at once; `None` while queued.
    pub fn ready(&self) -> Option<Delivered> {
        match &self.0 {
            WriteState::Done(result) => Some(result.clone()),
            WriteState::Queued { .. } => None,
        }
    }

    /// The write's outcome. A direct write is already done; a queued one waits
    /// for tmux to answer.
    pub async fn finish(self) -> Delivered {
        match self.0 {
            WriteState::Done(result) => result,
            WriteState::Queued { delivered, submit } => {
                let result = delivered.recv().await.unwrap_or_else(|_| {
                    Err(ControlWriteError::Unknown(
                        "the terminal closed before tmux answered".into(),
                    ))
                });
                if result.is_ok()
                    && let Some((session, at)) = submit
                {
                    session.commit_submission(at);
                }
                result
            }
        }
    }
}

enum Pin {
    Unresolved,
    Pinned(String),
}

struct Shared {
    pin: Mutex<Pin>,
    pin_queued: AtomicBool,
    /// When output may next try to fix the pane, after a failed try.
    retry_at: Mutex<Option<Instant>>,
    closed: AtomicBool,
}

enum Job {
    Pin,
    Deliver(ControlInput, async_channel::Sender<Delivered>),
}

/// A terminal's tmux route and its ordered input worker. ctl writes to one
/// terminal run one at a time, in the order they were made.
pub(crate) struct TmuxInput {
    route: TmuxInputRoute,
    shared: Arc<Shared>,
    worker: Mutex<Option<mpsc::Sender<Job>>>,
}

impl TmuxInput {
    pub(crate) fn new(route: TmuxInputRoute) -> Self {
        Self {
            route,
            shared: Arc::new(Shared {
                pin: Mutex::new(Pin::Unresolved),
                pin_queued: AtomicBool::new(false),
                retry_at: Mutex::new(None),
                closed: AtomicBool::new(false),
            }),
            worker: Mutex::new(None),
        }
    }

    /// The tmux pane id ctl input goes to, once fixed.
    pub(crate) fn pinned_pane(&self) -> Option<String> {
        match &*self.shared.pin.lock() {
            Pin::Pinned(pane) => Some(pane.clone()),
            Pin::Unresolved => None,
        }
    }

    /// Output arrived: while outside control is on and the pane isn't fixed
    /// yet, try now. Before the client has attached the session may not exist,
    /// so a failed try is repeated by later output, at most once a second.
    pub(crate) fn note_output(&self) {
        if self.route.pin_on_attach.load(Ordering::Acquire)
            && self
                .shared
                .retry_at
                .lock()
                .is_none_or(|at| Instant::now() >= at)
        {
            self.pin();
        }
    }

    /// Fix the pane now, unless it is fixed or a try is already queued.
    pub(crate) fn pin(&self) {
        if self.pinned_pane().is_some() || self.shared.pin_queued.swap(true, Ordering::AcqRel) {
            return;
        }
        if self.send(Job::Pin).is_err() {
            self.shared.pin_queued.store(false, Ordering::Release);
        }
    }

    /// Queue `input` for the pinned pane. A paste's text is already what the
    /// pane should get: [`crate::TerminalSession::control_write`] prepares it
    /// as a direct paste would.
    pub(crate) fn deliver(&self, input: ControlInput) -> async_channel::Receiver<Delivered> {
        let (tx, rx) = async_channel::bounded(1);
        if let Err(Job::Deliver(_, tx)) = self.send(Job::Deliver(input, tx)) {
            let _ = tx.try_send(Err(ControlWriteError::Refused(
                "muxel couldn't start its tmux input worker".into(),
            )));
        }
        rx
    }

    fn send(&self, job: Job) -> Result<(), Job> {
        let mut worker = self.worker.lock();
        if worker.is_none() {
            let (tx, rx) = mpsc::channel();
            let route = self.route.clone();
            let shared = self.shared.clone();
            let spawned = std::thread::Builder::new()
                .name("muxel-tmux-input".into())
                .spawn(move || work(&route, &shared, rx));
            if spawned.is_err() {
                return Err(job);
            }
            *worker = Some(tx);
        }
        worker
            .as_ref()
            .expect("worker started above")
            .send(job)
            .map_err(|error| error.0)
    }
}

impl Drop for TmuxInput {
    fn drop(&mut self) {
        self.shared.closed.store(true, Ordering::Release);
        // Dropping the sender ends the worker after its current job.
        self.worker.lock().take();
    }
}

fn work(route: &TmuxInputRoute, shared: &Shared, jobs: mpsc::Receiver<Job>) {
    while let Ok(job) = jobs.recv() {
        match job {
            Job::Pin => {
                shared.pin_queued.store(false, Ordering::Release);
                if !shared.closed.load(Ordering::Acquire) && pinned_pane(route, shared).is_err() {
                    *shared.retry_at.lock() = Some(Instant::now() + PIN_RETRY);
                }
            }
            Job::Deliver(input, done) => {
                let result = if shared.closed.load(Ordering::Acquire) {
                    Err(ControlWriteError::Refused("the terminal closed".into()))
                } else {
                    pinned_pane(route, shared).and_then(|pane| deliver(&route.run, &pane, &input))
                };
                let _ = done.try_send(result);
            }
        }
    }
}

/// The pinned pane, fixing it from the session's active pane if this terminal
/// has none yet. Once fixed it never changes: a pane that later vanishes fails
/// delivery rather than redirecting it to whatever is active now.
fn pinned_pane(route: &TmuxInputRoute, shared: &Shared) -> Result<String, ControlWriteError> {
    if let Pin::Pinned(pane) = &*shared.pin.lock() {
        return Ok(pane.clone());
    }
    let args = tmux::control_pane_args(&route.session);
    let why = match (route.run)(&args, None) {
        Ok(out) if out.status.success() => {
            match tmux::parse_control_pane(&route.session, &String::from_utf8_lossy(&out.stdout)) {
                Some(pane) => {
                    *shared.pin.lock() = Pin::Pinned(pane.clone());
                    return Ok(pane);
                }
                None => "tmux has no such session".to_string(),
            }
        }
        Ok(out) => stderr_line(&out),
        Err(error) => error.to_string(),
    };
    Err(ControlWriteError::Unresolved(format!(
        "couldn't find the tmux pane of session {} ({why}); ctl input to a tmux pane never goes through its client",
        route.session
    )))
}

/// Deliver `input` to `pane`. A refusal from a pane that has since gone is
/// reported as unresolved: the pane this terminal attached to is no more, and
/// no other takes its place.
fn deliver(run: &TmuxRunner, pane: &str, input: &ControlInput) -> Delivered {
    match deliver_once(run, pane, input) {
        Err(ControlWriteError::Refused(why)) if pane_gone(run, pane) => {
            Err(ControlWriteError::Unresolved(format!(
                "the tmux pane {pane} this terminal showed when it attached is gone ({why}); ctl input to a tmux pane never goes through its client"
            )))
        }
        delivered => delivered,
    }
}

/// Whether tmux answered that `pane` doesn't exist. An unanswered or unstarted
/// check says nothing either way.
fn pane_gone(run: &TmuxRunner, pane: &str) -> bool {
    run(&tmux::control_pane_exists_args(pane), None).is_ok_and(|out| {
        !out.status.success() || String::from_utf8_lossy(&out.stdout).trim_end() != pane
    })
}

fn deliver_once(run: &TmuxRunner, pane: &str, input: &ControlInput) -> Delivered {
    match input {
        ControlInput::Paste(text) => paste(run, pane, text, true),
        ControlInput::Keys { keys, .. } => {
            for key_step in tmux::control_key_steps(keys) {
                match key_step {
                    tmux::KeyStep::Send(keys) => {
                        step(run, &tmux::control_keys_args(pane, &keys), None)?
                    }
                    tmux::KeyStep::Paste(text) => paste(run, pane, &text, false)?,
                }
            }
            Ok(())
        }
        ControlInput::Submit => step(run, &tmux::control_keys_args(pane, &[Key::Enter]), None),
    }
}

/// Paste `text` byte for byte from a buffer of its own, bracketed (`-p`) only
/// when `bracket` and the pane asked for it.
fn paste(run: &TmuxRunner, pane: &str, text: &str, bracket: bool) -> Delivered {
    let buffer = tmux::control_buffer_name(uuid::Uuid::new_v4());
    let pasted = step(
        run,
        &tmux::control_paste_args(&buffer, pane, bracket),
        Some(text.as_bytes()),
    );
    if pasted.is_err() {
        // A failed paste leaves its buffer behind.
        let _ = run(&tmux::delete_buffer_args(&buffer), None);
    }
    pasted
}

fn step(run: &TmuxRunner, args: &[String], stdin: Option<&[u8]>) -> Delivered {
    match run(args, stdin) {
        Ok(out) if out.status.success() => Ok(()),
        Ok(out) => Err(ControlWriteError::Refused(stderr_line(&out))),
        Err(error)
            if matches!(
                error.kind(),
                io::ErrorKind::TimedOut | io::ErrorKind::ConnectionAborted
            ) =>
        {
            Err(ControlWriteError::Unknown(error.to_string()))
        }
        Err(error) => Err(ControlWriteError::Refused(error.to_string())),
    }
}

fn stderr_line(out: &Output) -> String {
    let text = String::from_utf8_lossy(&out.stderr);
    let text = text.trim();
    if text.is_empty() {
        format!("tmux exited with {}", out.status)
    } else {
        text.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{ControlInput, ControlWrite, ControlWriteError, TmuxInput, TmuxInputRoute};
    use crate::session::{CommandSpec, TerminalSession};
    use muxel_core::control::Key;
    use parking_lot::Mutex;
    use std::{
        io,
        process::Output,
        sync::{
            Arc,
            atomic::{AtomicBool, Ordering},
        },
        time::{Duration, Instant},
    };

    #[cfg(unix)]
    fn status(code: i32) -> std::process::ExitStatus {
        use std::os::unix::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code << 8)
    }

    #[cfg(windows)]
    fn status(code: i32) -> std::process::ExitStatus {
        use std::os::windows::process::ExitStatusExt;
        std::process::ExitStatus::from_raw(code as u32)
    }

    fn output(code: i32, stdout: &str, stderr: &str) -> Output {
        Output {
            status: status(code),
            stdout: stdout.as_bytes().to_vec(),
            stderr: stderr.as_bytes().to_vec(),
        }
    }

    type Calls = Arc<Mutex<Vec<(Vec<String>, Option<Vec<u8>>)>>>;

    /// A fake tmux: records every command, answers the pin query with `%7`
    /// and every other command with `answer`.
    fn route(answer: fn(&[String]) -> io::Result<Output>) -> (TmuxInputRoute, Calls) {
        let calls: Calls = Arc::default();
        let seen = calls.clone();
        let route = TmuxInputRoute::new(
            "muxel_p_1",
            Arc::new(AtomicBool::new(false)),
            move |args: &[String], stdin: Option<&[u8]>| {
                seen.lock().push((args.to_vec(), stdin.map(<[u8]>::to_vec)));
                if args
                    .last()
                    .is_some_and(|format| format == "#{session_name}:#{pane_id}")
                {
                    return Ok(output(0, "muxel_p_1:%7\n", ""));
                }
                answer(args)
            },
        );
        (route, calls)
    }

    fn commands(calls: &Calls) -> Vec<String> {
        calls
            .lock()
            .iter()
            .map(|(args, _)| args[0].clone())
            .collect()
    }

    fn deliver(input: &TmuxInput, write: ControlInput) -> Result<(), ControlWriteError> {
        input.deliver(write).recv_blocking().unwrap()
    }

    /// Drive a write to its end, as the app's foreground task awaits it.
    pub(super) fn finish(write: ControlWrite) -> Result<(), ControlWriteError> {
        let mut write = std::pin::pin!(write.finish());
        let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let std::task::Poll::Ready(done) = write.as_mut().poll(&mut cx) {
                return done;
            }
            assert!(Instant::now() < deadline, "tmux never answered");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn writes_go_in_order_to_the_pane_pinned_once() {
        let (route, calls) = route(|_| Ok(output(0, "", "")));
        let input = TmuxInput::new(route);
        deliver(&input, ControlInput::Paste("two\nlines".into())).unwrap();
        deliver(
            &input,
            ControlInput::Keys {
                keys: vec![Key::Ctrl('b'), Key::Char('c')],
                app_cursor: true,
            },
        )
        .unwrap();
        deliver(&input, ControlInput::Submit).unwrap();
        assert_eq!(input.pinned_pane().as_deref(), Some("%7"));
        let calls = calls.lock();
        let commands: Vec<&str> = calls.iter().map(|(args, _)| args[0].as_str()).collect();
        assert_eq!(
            commands,
            ["display-message", "load-buffer", "send-keys", "send-keys"],
            "the pane is resolved once, then every write targets it"
        );
        assert!(calls[1].0.ends_with(&["-t".to_string(), "%7".to_string()]));
        assert!(calls[1].0.contains(&"-p".to_string()), "{:?}", calls[1].0);
        assert_eq!(calls[1].1.as_deref(), Some(&b"two\nlines"[..]));
        assert_eq!(calls[2].0, ["send-keys", "-t", "%7", "C-b", "0x63"]);
        assert_eq!(calls[3].0, ["send-keys", "-t", "%7", "Enter"]);
        assert!(calls[2].1.is_none() && calls[3].1.is_none());
    }

    #[test]
    fn a_key_outside_ascii_is_pasted_raw_between_the_named_keys() {
        let (route, calls) = route(|_| Ok(output(0, "", "")));
        let input = TmuxInput::new(route);
        deliver(
            &input,
            ControlInput::Keys {
                keys: vec![Key::Char('a'), Key::Char('é'), Key::Enter],
                app_cursor: false,
            },
        )
        .unwrap();
        let calls = calls.lock();
        assert_eq!(calls[1].0, ["send-keys", "-t", "%7", "0x61"]);
        assert_eq!(calls[2].0[0], "load-buffer");
        assert!(
            !calls[2].0.contains(&"-p".to_string()),
            "a typed character is never bracketed: {:?}",
            calls[2].0
        );
        assert_eq!(calls[2].1.as_deref(), Some("é".as_bytes()));
        assert_eq!(calls[3].0, ["send-keys", "-t", "%7", "Enter"]);
        assert_eq!(calls.len(), 4);
    }

    /// A terminal whose program would be a tmux client, behind a fake tmux
    /// that records every command and refuses `send-keys` while `refuse` is set.
    fn tmux_terminal(
        refuse: Arc<AtomicBool>,
    ) -> (Arc<TerminalSession>, crate::session::PtyReceiver, Calls) {
        let calls: Calls = Arc::default();
        let seen = calls.clone();
        let route = TmuxInputRoute::new(
            "muxel_p_1",
            Arc::new(AtomicBool::new(false)),
            move |args: &[String], stdin: Option<&[u8]>| {
                seen.lock().push((args.to_vec(), stdin.map(<[u8]>::to_vec)));
                Ok(match args[0].as_str() {
                    "display-message" if args[4] == "#{session_name}:#{pane_id}" => {
                        output(0, "muxel_p_1:%7\n", "")
                    }
                    "display-message" => output(0, "%7\n", ""),
                    "send-keys" if refuse.load(Ordering::Acquire) => output(1, "", "not now\n"),
                    _ => output(0, "", ""),
                })
            },
        );
        let spec = if cfg!(windows) {
            CommandSpec::program("cmd.exe", vec!["/D".into(), "/Q".into(), "/K".into()])
        } else {
            CommandSpec::program("/bin/cat", vec![])
        };
        let (session, rx) =
            TerminalSession::spawn(spec.with_tmux_input(route), 80, 24).expect("spawn PTY");
        (session, rx, calls)
    }

    /// Master pasted into the tmux client what a direct paste sends, by the
    /// client terminal's own bracketed-paste mode; tmux carried that to the
    /// pane. Through tmux the pane gets the same bytes.
    #[test]
    fn a_paste_through_tmux_sends_what_a_direct_paste_would() {
        let (session, _rx, calls) = tmux_terminal(Arc::default());
        let text = "one\r\ntwo\nthree\x1b[1m";
        finish(session.control_write(ControlInput::Paste(text.into()))).unwrap();
        // tmux turns on its client's bracketed-paste mode for a pane that asks.
        session.process_output(b"\x1b[?2004h");
        assert!(session.is_bracketed_paste());
        finish(session.control_write(ControlInput::Paste(text.into()))).unwrap();
        let calls = calls.lock();
        let pastes: Vec<_> = calls
            .iter()
            .filter(|(args, _)| args[0] == "load-buffer")
            .collect();
        assert_eq!(pastes.len(), 2);
        assert_eq!(
            pastes[0].1.as_deref(),
            Some(&b"one\rtwo\rthree\x1b[1m"[..]),
            "outside bracketed paste: line breaks as CR, ESC kept"
        );
        assert_eq!(
            pastes[1].1.as_deref(),
            Some(&b"one\r\ntwo\nthree[1m"[..]),
            "inside bracketed paste: line breaks kept, ESC removed"
        );
        assert!(
            pastes
                .iter()
                .all(|(args, _)| args.contains(&"-p".to_string())),
            "tmux brackets either only if the pane asked"
        );
        drop(calls);
        session.kill();
    }

    /// The Enter of `keys enter` counts as a submitted turn only once tmux
    /// has taken it.
    #[test]
    fn a_submit_tmux_refuses_records_no_submitted_turn() {
        let refuse = Arc::new(AtomicBool::new(true));
        let (session, _rx, _calls) = tmux_terminal(refuse.clone());
        let refused = finish(session.control_write(ControlInput::Submit));
        assert!(
            matches!(refused, Err(ControlWriteError::Refused(_))),
            "{refused:?}"
        );
        assert!(!session.has_submitted_turn());

        refuse.store(false, Ordering::Release);
        finish(session.control_write(ControlInput::Submit)).unwrap();
        assert!(session.has_submitted_turn());
        session.kill();
    }

    #[test]
    fn a_refused_paste_reports_failure_and_deletes_its_buffer() {
        let (route, calls) = route(|args| {
            Ok(match args[0].as_str() {
                "load-buffer" => output(1, "", "target pane has exited\n"),
                // The pane is still there.
                "display-message" => output(0, "%7\n", ""),
                _ => output(0, "", ""),
            })
        });
        let input = TmuxInput::new(route);
        let error = deliver(&input, ControlInput::Paste("x".into())).unwrap_err();
        assert_eq!(
            error,
            ControlWriteError::Refused("target pane has exited".into())
        );
        assert_eq!(
            commands(&calls),
            [
                "display-message",
                "load-buffer",
                "delete-buffer",
                "display-message"
            ]
        );
        let calls = calls.lock();
        let buffer = &calls[1].0[2];
        assert_eq!(calls[2].0, ["delete-buffer", "-b", buffer.as_str()]);
        assert_eq!(
            calls[3].0,
            ["display-message", "-p", "-t", "%7", "#{pane_id}"]
        );
    }

    #[test]
    fn a_pinned_pane_that_has_gone_is_unresolved_and_never_replaced() {
        // Every command on the pane, its existence check included.
        let (route, calls) = route(|_| Ok(output(1, "", "can't find pane: %7\n")));
        let input = TmuxInput::new(route);
        for write in [
            ControlInput::Paste("x".into()),
            ControlInput::Submit,
            ControlInput::Keys {
                keys: vec![Key::Char('x')],
                app_cursor: false,
            },
        ] {
            let error = deliver(&input, write).unwrap_err();
            assert!(
                matches!(&error, ControlWriteError::Unresolved(why) if why.contains("%7 this terminal showed")),
                "{error:?}"
            );
        }
        assert_eq!(input.pinned_pane().as_deref(), Some("%7"));
        let pin_queries = calls
            .lock()
            .iter()
            .filter(|(args, _)| {
                args.last()
                    .is_some_and(|f| f == "#{session_name}:#{pane_id}")
            })
            .count();
        assert_eq!(
            pin_queries, 1,
            "a pane that has gone is never looked up again"
        );
    }

    #[test]
    fn an_unanswered_command_is_reported_as_unknown_not_refused() {
        let (route, _) = route(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")));
        let input = TmuxInput::new(route);
        assert!(matches!(
            deliver(&input, ControlInput::Submit),
            Err(ControlWriteError::Unknown(_))
        ));
    }

    #[test]
    fn a_pane_that_cant_be_found_is_refused_and_never_falls_back() {
        let calls: Calls = Arc::default();
        let seen = calls.clone();
        let route = TmuxInputRoute::new(
            "muxel_p_1",
            Arc::new(AtomicBool::new(false)),
            move |args: &[String], stdin: Option<&[u8]>| {
                seen.lock().push((args.to_vec(), stdin.map(<[u8]>::to_vec)));
                // No such session: display-message formats without a target.
                Ok(output(0, ":\n", ""))
            },
        );
        let input = TmuxInput::new(route);
        let error = deliver(&input, ControlInput::Paste("x".into())).unwrap_err();
        assert!(
            matches!(error, ControlWriteError::Unresolved(_)),
            "{error:?}"
        );
        assert!(
            calls
                .lock()
                .iter()
                .all(|(args, _)| args[0] == "display-message"),
            "nothing is typed anywhere without a pinned pane"
        );
        assert_eq!(input.pinned_pane(), None);
    }
}

/// ctl input through a real tmux, to an agent pane behind an attached client,
/// on a throwaway socket no muxel profile uses. No shell grant exists at this
/// level: these writes are what an agent pane gets with the grant off. Skipped
/// where tmux isn't installed; CI installs it.
#[cfg(all(test, unix))]
mod real_tmux_tests {
    use super::tests::finish;
    use super::{ControlInput, ControlWriteError, TmuxInputRoute};
    use crate::session::{CommandSpec, PtyChunk, PtyReceiver, TerminalSession};
    use muxel_core::control::Key;
    use std::{
        io::{self, Write as _},
        path::{Path, PathBuf},
        process::{Command, Output, Stdio},
        sync::{Arc, atomic::AtomicBool},
        time::{Duration, Instant},
    };

    /// The agent: it asks for bracketed paste, as agents' editors do, takes its
    /// terminal raw so every byte arrives as sent, says it is ready (tmux reads
    /// a pane's output in order, so its mode is set once that shows) and
    /// records every byte in `$1`.
    const AGENT: &str =
        "printf '\\033[?2004h'\nstty raw -echo\nprintf 'agent ready\\r\\n'\nexec cat > \"$1\"\n";

    fn tmux_command(socket: &str) -> Command {
        let mut tmux = Command::new("tmux");
        tmux.args(["-L", socket, "-f", "/dev/null"])
            .env_remove("TMUX");
        tmux
    }

    /// What muxel's runner does for a local pane, without its deadline.
    fn run(socket: &str, args: &[String], stdin: Option<&[u8]>) -> io::Result<Output> {
        let mut child = tmux_command(socket)
            .args(args)
            .stdin(if stdin.is_some() {
                Stdio::piped()
            } else {
                Stdio::null()
            })
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        if let Some(data) = stdin {
            child.stdin.take().unwrap().write_all(data)?;
        }
        child.wait_with_output()
    }

    struct Agent {
        socket: String,
        name: String,
        dir: PathBuf,
        script: PathBuf,
        recv: PathBuf,
        session: Arc<TerminalSession>,
        rx: PtyReceiver,
    }

    impl Agent {
        /// A tmux client in a terminal, attached to a session whose one pane
        /// runs the agent, with the client's tmux pane already fixed for ctl
        /// input (as it is at attach while outside control is on).
        fn start() -> Option<Self> {
            if Command::new("tmux").arg("-V").output().is_err() {
                eprintln!("tmux is not installed; skipping");
                return None;
            }
            let id = uuid::Uuid::new_v4().simple().to_string();
            let socket = format!("mxltest-{id}");
            let dir = std::env::temp_dir().join(format!("muxel-ctl-tmux-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("agent.sh");
            std::fs::write(&script, AGENT).unwrap();
            let recv = dir.join("agent.bin");
            let name = format!("muxel_ctl_{}", &id[..8]);
            let mut args: Vec<String> = ["-L", &socket, "-f", "/dev/null"].map(String::from).into();
            args.extend(["new-session", "-A", "-s", &name, "--", "sh"].map(String::from));
            args.push(script.display().to_string());
            args.push(recv.display().to_string());
            let route_socket = socket.clone();
            let route = TmuxInputRoute::new(
                name.clone(),
                Arc::new(AtomicBool::new(true)),
                move |args: &[String], stdin: Option<&[u8]>| run(&route_socket, args, stdin),
            );
            let mut spec = CommandSpec::program("tmux", args).with_tmux_input(route);
            spec.env_remove.push("TMUX".into());
            let (session, rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn tmux client");
            let agent = Self {
                socket,
                name,
                dir,
                script,
                recv,
                session,
                rx,
            };
            // A ctl paste is prepared by the client terminal's paste mode, as
            // a direct paste is, so wait until tmux has set it for the agent.
            agent.until("the client attached and its pane was fixed", |agent| {
                agent.recv.exists()
                    && agent.screen().contains("agent ready")
                    && agent.format("#{session_attached}") == "1"
                    && agent.session.tmux_input_pane().is_some()
                    && agent.session.is_bracketed_paste()
            });
            assert!(agent.session.delivers_through_tmux());
            eprintln!(
                "real tmux: {} attached, ctl input pinned to {}",
                agent.name,
                agent.session.tmux_input_pane().unwrap()
            );
            Some(agent)
        }

        fn tmux(&self, args: &[&str]) -> Output {
            let out = tmux_command(&self.socket)
                .args(args)
                .stdin(Stdio::null())
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "tmux {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
            out
        }

        fn target(&self) -> String {
            format!("={}:", self.name)
        }

        fn format(&self, format: &str) -> String {
            let out = tmux_command(&self.socket)
                .args(["display-message", "-p", "-t", &self.target(), format])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim_end().to_string()
        }

        fn screen(&self) -> String {
            let out = tmux_command(&self.socket)
                .args(["capture-pane", "-p", "-t", &self.target()])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        }

        fn windows(&self) -> usize {
            let out = self.tmux(&["list-windows", "-t", &self.target(), "-F", "#{window_id}"]);
            String::from_utf8_lossy(&out.stdout).lines().count()
        }

        fn buffers(&self) -> String {
            let out = tmux_command(&self.socket)
                .args(["list-buffers", "-F", "#{buffer_name}"])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).into_owned()
        }

        /// Run another recorder in a new pane made by `command` (such as
        /// `new-window`), as a user would; returns the file it records into.
        fn open(&self, command: &str, label: &str) -> PathBuf {
            let recv = self.dir.join(format!("{label}.bin"));
            let (target, script, file) = (
                self.target(),
                self.script.display().to_string(),
                recv.display().to_string(),
            );
            self.tmux(&[command, "-t", &target, "sh", &script, &file]);
            self.until(&format!("the {label} started"), |_| recv.exists());
            recv
        }

        fn write(&self, input: ControlInput) -> Result<(), ControlWriteError> {
            finish(self.session.control_write(input))
        }

        fn keys(&self, keys: &[Key]) -> Result<(), ControlWriteError> {
            self.write(ControlInput::Keys {
                keys: keys.to_vec(),
                app_cursor: false,
            })
        }

        fn received(&self) -> Vec<u8> {
            read(&self.recv)
        }

        /// Pump the client's output (which fixes the pane, as muxel's terminal
        /// does) until `ready`, or fail the test.
        fn until(&self, what: &str, ready: impl Fn(&Self) -> bool) {
            let deadline = Instant::now() + Duration::from_secs(15);
            loop {
                while let Ok(chunk) = self.rx.try_recv() {
                    if let PtyChunk::Output(bytes) = chunk {
                        self.session.process_output(&bytes);
                    }
                }
                if ready(self) {
                    return;
                }
                assert!(
                    Instant::now() < deadline,
                    "timed out waiting until {what}; the agent got {:?}",
                    String::from_utf8_lossy(&self.received())
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        fn until_received(&self, expected: &[u8]) {
            let what = format!("the agent got {:?}", String::from_utf8_lossy(expected));
            self.until(&what, |agent| agent.received() == expected);
        }
    }

    impl Drop for Agent {
        fn drop(&mut self) {
            let path =
                Some(self.format("#{socket_path}")).filter(|path| path.ends_with(&self.socket));
            let _ = tmux_command(&self.socket).arg("kill-server").output();
            self.session.kill();
            if let Some(path) = path {
                let _ = std::fs::remove_file(path);
            }
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    fn read(path: &Path) -> Vec<u8> {
        std::fs::read(path).unwrap_or_default()
    }

    /// ctl `keys ctrl-b c` is the prefix and "new window" to the client, but
    /// to the pane it is two bytes; and an extended-key encoding of ctrl-b
    /// (`esc [ 9 8 ; 5 u`), which a tmux client decodes back into the prefix,
    /// is bytes too.
    #[test]
    fn ctl_keys_reach_the_agent_as_bytes_and_open_no_window() {
        let Some(agent) = Agent::start() else { return };
        assert_eq!(agent.windows(), 1);
        agent.keys(&[Key::Ctrl('b'), Key::Char('c')]).unwrap();
        agent.until_received(b"\x02c");
        assert_eq!(agent.windows(), 1, "ctrl-b c opened a tmux window");

        let extended = [
            Key::Escape,
            Key::Char('['),
            Key::Char('9'),
            Key::Char('8'),
            Key::Char(';'),
            Key::Char('5'),
            Key::Char('u'),
            Key::Char('c'),
        ];
        agent.keys(&extended).unwrap();
        agent.until_received(b"\x02c\x1b[98;5uc");
        assert_eq!(agent.windows(), 1, "esc [ 9 8 ; 5 u c opened a tmux window");

        // Characters outside ASCII arrive as their UTF-8 bytes, unbracketed.
        agent
            .keys(&[Key::Char('é'), Key::Char('✓'), Key::Char('x')])
            .unwrap();
        agent.until_received("\x02c\x1b[98;5ucé✓x".as_bytes());

        // The same two bytes typed into the client do open one: the old path.
        agent.session.write_input(b"\x02c");
        agent.until("the client opened a window", |agent| agent.windows() == 2);
    }

    #[test]
    fn a_user_root_binding_in_ctl_text_does_not_fire() {
        let Some(agent) = Agent::start() else { return };
        agent.tmux(&["bind-key", "-n", "@", "new-window"]);
        agent.write(ControlInput::Paste("a@b".into())).unwrap();
        agent.keys(&[Key::Char('@')]).unwrap();
        agent.until_received(b"\x1b[200~a@b\x1b[201~@");
        assert_eq!(agent.windows(), 1, "the user's `bind -n @` fired");

        agent.session.write_input(b"@");
        agent.until("the client ran the binding", |agent| agent.windows() == 2);
    }

    #[test]
    fn a_multi_line_unicode_prompt_arrives_as_one_paste_and_enter_submits_once() {
        let Some(agent) = Agent::start() else { return };
        let prompt = "first line \u{2713} \u{e9}\nsecond \u{2014} \u{65e5}\u{672c}; $HOME `id` 'q' \"d\" \\\n\tthird\x1b[31m end";
        agent.write(ControlInput::Paste(prompt.into())).unwrap();
        agent.write(ControlInput::Submit).unwrap();
        let mut expected = b"\x1b[200~".to_vec();
        expected.extend(prompt.replace('\x1b', "").as_bytes());
        expected.extend(b"\x1b[201~\r");
        agent.until_received(&expected);
        let buffers = agent.buffers();
        assert!(
            !buffers.contains("muxel-ctl-"),
            "the paste left its buffer: {buffers}"
        );
        // Nothing more arrives: one paste, one Enter.
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(agent.received(), expected);
    }

    #[test]
    fn ctl_input_stays_on_the_pinned_pane_after_the_user_opens_a_window_and_a_split() {
        let Some(agent) = Agent::start() else { return };
        let pinned = agent.session.tmux_input_pane().unwrap();
        let window = agent.open("new-window", "window");
        let split = agent.open("split-window", "split");
        assert_ne!(
            agent.format("#{pane_id}"),
            pinned,
            "the split is active now"
        );
        agent
            .write(ControlInput::Paste("to the agent".into()))
            .unwrap();
        agent.write(ControlInput::Submit).unwrap();
        agent.until_received(b"\x1b[200~to the agent\x1b[201~\r");
        assert_eq!(agent.session.tmux_input_pane(), Some(pinned));
        assert!(read(&window).is_empty() && read(&split).is_empty());
    }

    #[test]
    fn a_vanished_pane_fails_delivery_and_nothing_is_typed_anywhere() {
        let Some(agent) = Agent::start() else { return };
        let pinned = agent.session.tmux_input_pane().unwrap();
        // The session, and its client, live on in another window.
        let window = agent.open("new-window", "window");
        agent.tmux(&["kill-pane", "-t", &pinned]);

        for write in [
            ControlInput::Paste("lost".into()),
            ControlInput::Submit,
            ControlInput::Keys {
                keys: vec![Key::Char('x')],
                app_cursor: false,
            },
        ] {
            let failed = agent.write(write);
            assert!(
                matches!(&failed, Err(ControlWriteError::Unresolved(why)) if why.contains(&pinned)),
                "{failed:?}"
            );
        }
        let buffers = agent.buffers();
        assert!(
            !buffers.contains("muxel-ctl-"),
            "the failed paste left its buffer: {buffers}"
        );
        // The pane is never re-resolved to whatever is active now.
        assert_eq!(agent.session.tmux_input_pane(), Some(pinned));
        std::thread::sleep(Duration::from_millis(300));
        assert!(read(&window).is_empty(), "{:?}", read(&window));
    }
}
