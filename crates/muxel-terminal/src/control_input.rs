//! Outside-control (`muxel ctl`) input, and its route through tmux for a
//! terminal whose program is a tmux client.
//!
//! Bytes written to a tmux client's terminal pass through that client's key
//! tables before any pane sees them: the prefix, `prefix2`, `bind -n` keys and
//! the extended-key decoder can turn them into tmux commands, such as opening a
//! shell window inside an agent's pane. So ctl input to such a terminal goes to
//! its tmux pane through tmux itself (`paste-buffer`, `send-keys`), on the
//! server the client attached to, and never through the client. The pane is the
//! agent's own, the one its session was created with, found by tmux pane id at
//! the first ctl write and kept for the terminal's life, whichever pane the user
//! has made active. If it can't be found, the write is refused; it never falls
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

/// After a failed lookup of the pane, how long until the next may run. Each
/// lookup is a tmux command, over SSH for a remote pane (a full login when the
/// host's connection can't be shared), so a caller retrying a pane that can't
/// be reached never runs one more often than this allows.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct PinBackoff {
    failures: u32,
    next: Option<Instant>,
}

impl PinBackoff {
    /// The first wait, doubled after each further failure up to [`Self::MAX`].
    const FIRST: Duration = Duration::from_secs(2);
    const MAX: Duration = Duration::from_secs(120);

    /// The wait after `failures` failed lookups in a row.
    pub(crate) fn delay(failures: u32) -> Duration {
        if failures == 0 {
            return Duration::ZERO;
        }
        Self::FIRST
            .saturating_mul(1 << (failures - 1).min(16))
            .min(Self::MAX)
    }

    /// How long until a lookup may run, or `None` when one may run at `now`.
    pub(crate) fn wait(&self, now: Instant) -> Option<Duration> {
        self.next.filter(|&next| next > now).map(|next| next - now)
    }

    pub(crate) fn failed(&mut self, now: Instant) {
        self.failures = self.failures.saturating_add(1);
        self.next = Some(now + Self::delay(self.failures));
    }
}

/// One write `muxel ctl` makes to a terminal.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ControlInput {
    /// Text pasted as one paste. A terminal muxel writes to gets what
    /// [`crate::TerminalSession::try_paste`] sends; a tmux pane gets
    /// [`tmux::control_paste_input`]'s bytes, bracketed when it asked.
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
    /// Whether muxel created the session with a program of its own (an agent),
    /// rather than the user's default shell. See [`tmux::parse_control_pane`].
    with_command: bool,
}

impl TmuxInputRoute {
    pub fn new(
        session: impl Into<String>,
        with_command: bool,
        run: impl Fn(&[String], Option<&[u8]>) -> io::Result<Output> + Send + Sync + 'static,
    ) -> Self {
        Self {
            session: session.into(),
            run: Arc::new(run),
            with_command,
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
    /// The agent's tmux pane couldn't be found, or has gone. Nothing was typed.
    Unresolved(String),
    /// tmux refused the command, or it couldn't be started. Nothing was typed.
    Refused(String),
    /// The command started, but no answer came back in time or its connection
    /// dropped: it may or may not have been delivered.
    Unknown(String),
    /// Part of the write was delivered (some of its keys), then the rest failed.
    Partial(String),
}

impl ControlWriteError {
    /// Whether some of the write may have reached the pane: a write that may
    /// have typed counts as typed.
    pub fn may_have_typed(&self) -> bool {
        matches!(self, Self::Closed(_) | Self::Unknown(_) | Self::Partial(_))
    }
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
            Self::Partial(why) => write!(f, "part of it was typed, then {why}"),
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

struct Shared {
    /// The agent's pane, once found. It never changes after that.
    pin: Mutex<Option<String>>,
    /// Failed lookups of the pane, so one that can't be found isn't looked up
    /// again too soon.
    backoff: Mutex<PinBackoff>,
    closed: AtomicBool,
}

struct Job(ControlInput, async_channel::Sender<Delivered>);

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
                pin: Mutex::new(None),
                backoff: Mutex::new(PinBackoff::default()),
                closed: AtomicBool::new(false),
            }),
            worker: Mutex::new(None),
        }
    }

    /// The tmux pane id ctl input goes to, once found.
    pub(crate) fn pinned_pane(&self) -> Option<String> {
        self.shared.pin.lock().clone()
    }

    /// Queue `input` for the agent's pane, which the first write looks up.
    pub(crate) fn deliver(&self, input: ControlInput) -> async_channel::Receiver<Delivered> {
        let (tx, rx) = async_channel::bounded(1);
        if let Err(Job(_, tx)) = self.send(Job(input, tx)) {
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
    while let Ok(Job(input, done)) = jobs.recv() {
        let result = if shared.closed.load(Ordering::Acquire) {
            Err(ControlWriteError::Refused("the terminal closed".into()))
        } else {
            pinned_pane(route, shared, Instant::now())
                .and_then(|pane| deliver(&route.run, &pane, &input))
        };
        let _ = done.try_send(result);
    }
}

/// The agent's pane, looking it up if this terminal has none yet. Once found
/// it never changes: a pane that later vanishes fails delivery rather than
/// redirecting it. After a failed lookup, writes fail without running tmux
/// until [`PinBackoff`] allows the next one.
fn pinned_pane(
    route: &TmuxInputRoute,
    shared: &Shared,
    now: Instant,
) -> Result<String, ControlWriteError> {
    if let Some(pane) = shared.pin.lock().clone() {
        return Ok(pane);
    }
    let unresolved = |why: &str| {
        ControlWriteError::Unresolved(format!(
            "couldn't find the agent's tmux pane in session {} ({why}); ctl input to a tmux pane never goes through its client",
            route.session
        ))
    };
    if let Some(wait) = shared.backoff.lock().wait(now) {
        return Err(unresolved(&format!(
            "the last lookup failed; the next can run in {} s",
            wait.as_secs_f64().ceil()
        )));
    }
    let args = tmux::control_pane_args(&route.session);
    let why = match (route.run)(&args, None) {
        Ok(out) if out.status.success() => {
            let panes = String::from_utf8_lossy(&out.stdout);
            match tmux::parse_control_pane(&route.session, &panes, route.with_command) {
                Some(pane) => {
                    *shared.pin.lock() = Some(pane.clone());
                    return Ok(pane);
                }
                None if panes.trim().is_empty() => "tmux listed no panes".to_string(),
                None => "the pane the session was created with has gone".to_string(),
            }
        }
        Ok(out) => stderr_line(&out),
        Err(error) => error.to_string(),
    };
    shared.backoff.lock().failed(now);
    Err(unresolved(&why))
}

/// Deliver `input` to `pane`. A refusal from a pane that has since gone is
/// reported as unresolved: the agent's pane is no more, and no other takes its
/// place.
fn deliver(run: &TmuxRunner, pane: &str, input: &ControlInput) -> Delivered {
    match deliver_once(run, pane, input) {
        Err(ControlWriteError::Refused(why)) if pane_gone(run, pane) => {
            Err(ControlWriteError::Unresolved(format!(
                "the agent's tmux pane {pane} is gone ({why}); ctl input to a tmux pane never goes through its client"
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
        ControlInput::Paste(text) => paste(run, pane, &tmux::control_paste_input(text), true),
        ControlInput::Keys { keys, .. } => {
            for (i, key_step) in tmux::control_key_steps(keys).into_iter().enumerate() {
                let pressed = match key_step {
                    tmux::KeyStep::Send(keys) => {
                        step(run, &tmux::control_keys_args(pane, &keys), None)
                    }
                    tmux::KeyStep::Paste(text) => paste(run, pane, text.as_bytes(), false),
                };
                match pressed {
                    Ok(()) => {}
                    // Earlier keys of this write went in.
                    Err(error) if i > 0 => return Err(partial(error)),
                    Err(error) => return Err(error),
                }
            }
            Ok(())
        }
        ControlInput::Submit => step(run, &tmux::control_keys_args(pane, &[Key::Enter]), None),
    }
}

/// `error` for the rest of a write whose first part was delivered.
fn partial(error: ControlWriteError) -> ControlWriteError {
    ControlWriteError::Partial(match error {
        ControlWriteError::Refused(why) | ControlWriteError::Unresolved(why) => {
            format!("tmux refused the rest ({why})")
        }
        ControlWriteError::Unknown(why) => format!("tmux didn't answer for the rest ({why})"),
        ControlWriteError::Closed(why) | ControlWriteError::Partial(why) => why,
    })
}

/// Paste `bytes` as they are from a buffer of its own, bracketed (`-p`) only
/// when `bracket` and the pane asked for it.
fn paste(run: &TmuxRunner, pane: &str, bytes: &[u8], bracket: bool) -> Delivered {
    let buffer = tmux::control_buffer_name(uuid::Uuid::new_v4());
    let pasted = step(
        run,
        &tmux::control_paste_args(&buffer, pane, bracket),
        Some(bytes),
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
    use super::{
        ControlInput, ControlWrite, ControlWriteError, PinBackoff, Shared, TmuxInput,
        TmuxInputRoute, pinned_pane,
    };
    use crate::session::{CommandSpec, TerminalSession};
    use muxel_core::control::Key;
    use parking_lot::Mutex;
    use std::{
        io,
        process::Output,
        sync::{
            Arc,
            atomic::{AtomicBool, AtomicUsize, Ordering},
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

    /// What a fake tmux lists for an agent session: the agent's pane `%7`,
    /// started with its program, and a user's split `%9` with none.
    const AGENT_PANES: &str = "muxel_p_1:%9:0\nmuxel_p_1:%7:1\n";

    /// A fake tmux for an agent session: records every command, lists
    /// [`AGENT_PANES`] and answers every other command with `answer`.
    fn route(answer: fn(&[String]) -> io::Result<Output>) -> (TmuxInputRoute, Calls) {
        let calls: Calls = Arc::default();
        let seen = calls.clone();
        let route = TmuxInputRoute::new(
            "muxel_p_1",
            true,
            move |args: &[String], stdin: Option<&[u8]>| {
                seen.lock().push((args.to_vec(), stdin.map(<[u8]>::to_vec)));
                if args[0] == "list-panes" {
                    return Ok(output(0, AGENT_PANES, ""));
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
    fn writes_go_in_order_to_the_agents_pane_found_once() {
        let (route, calls) = route(|_| Ok(output(0, "", "")));
        let input = TmuxInput::new(route);
        assert_eq!(
            input.pinned_pane(),
            None,
            "nothing is looked up before a write"
        );
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
        assert_eq!(
            input.pinned_pane().as_deref(),
            Some("%7"),
            "the agent's pane, not the user's split"
        );
        let calls = calls.lock();
        let commands: Vec<&str> = calls.iter().map(|(args, _)| args[0].as_str()).collect();
        assert_eq!(
            commands,
            ["list-panes", "load-buffer", "send-keys", "send-keys"],
            "the pane is looked up once, then every write targets it"
        );
        assert!(calls[1].0.ends_with(&["-t".to_string(), "%7".to_string()]));
        assert!(calls[1].0.contains(&"-p".to_string()), "{:?}", calls[1].0);
        assert!(calls[1].0.contains(&"-r".to_string()), "{:?}", calls[1].0);
        assert_eq!(calls[1].1.as_deref(), Some(&b"two\rlines"[..]));
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

    /// Keys refused after earlier keys of the same press went in are a partial
    /// write, which counts as typed; a refusal of the first is not.
    #[test]
    fn keys_refused_after_earlier_ones_went_in_are_partly_typed() {
        let (route, calls) = route(|args| {
            Ok(match args[0].as_str() {
                "send-keys" if args.contains(&"Enter".to_string()) => output(1, "", "not now\n"),
                "display-message" => output(0, "%7\n", ""),
                _ => output(0, "", ""),
            })
        });
        let input = TmuxInput::new(route);
        let press = |keys: Vec<Key>| ControlInput::Keys {
            keys,
            app_cursor: false,
        };
        let error = deliver(
            &input,
            press(vec![Key::Char('a'), Key::Char('é'), Key::Enter]),
        )
        .unwrap_err();
        assert_eq!(
            error,
            ControlWriteError::Partial("tmux refused the rest (not now)".into())
        );
        assert!(error.may_have_typed());
        assert_eq!(
            commands(&calls),
            ["list-panes", "send-keys", "load-buffer", "send-keys"],
            "a partial write isn't taken for a pane that has gone"
        );

        let error = deliver(&input, press(vec![Key::Enter])).unwrap_err();
        assert_eq!(error, ControlWriteError::Refused("not now".into()));
        assert!(!error.may_have_typed());
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
            true,
            move |args: &[String], stdin: Option<&[u8]>| {
                seen.lock().push((args.to_vec(), stdin.map(<[u8]>::to_vec)));
                Ok(match args[0].as_str() {
                    "list-panes" => output(0, AGENT_PANES, ""),
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

    /// muxel sees only the client terminal's bracketed-paste mode, which can
    /// differ from the agent pane's, so a paste through tmux is the same bytes
    /// either way: line breaks as CR and no ESC, bracketed by tmux only if the
    /// pane asked.
    #[test]
    fn a_paste_through_tmux_is_the_same_whatever_the_clients_mode() {
        let (session, _rx, calls) = tmux_terminal(Arc::default());
        let text = "one\r\ntwo\nthree\x1b[201~four\r";
        finish(session.control_write(ControlInput::Paste(text.into()))).unwrap();
        session.process_output(b"\x1b[?2004h");
        assert!(session.is_bracketed_paste());
        finish(session.control_write(ControlInput::Paste(text.into()))).unwrap();
        let calls = calls.lock();
        let pastes: Vec<_> = calls
            .iter()
            .filter(|(args, _)| args[0] == "load-buffer")
            .collect();
        assert_eq!(pastes.len(), 2);
        for (args, bytes) in pastes {
            assert_eq!(bytes.as_deref(), Some(&b"one\rtwo\rthree[201~four\r"[..]));
            assert!(args.contains(&"-p".to_string()) && args.contains(&"-r".to_string()));
        }
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
                "list-panes",
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
                matches!(&error, ControlWriteError::Unresolved(why) if why.contains("tmux pane %7 is gone")),
                "{error:?}"
            );
            assert!(!error.may_have_typed());
        }
        assert_eq!(input.pinned_pane().as_deref(), Some("%7"));
        let lookups = commands(&calls)
            .iter()
            .filter(|command| *command == "list-panes")
            .count();
        assert_eq!(lookups, 1, "a pane that has gone is never looked up again");
    }

    #[test]
    fn an_unanswered_command_is_reported_as_unknown_not_refused() {
        let (route, _) = route(|_| Err(io::Error::new(io::ErrorKind::TimedOut, "timed out")));
        let input = TmuxInput::new(route);
        let error = deliver(&input, ControlInput::Submit).unwrap_err();
        assert!(matches!(error, ControlWriteError::Unknown(_)), "{error:?}");
        assert!(error.may_have_typed());
    }

    #[test]
    fn a_pane_that_cant_be_found_is_refused_and_never_falls_back() {
        let calls: Calls = Arc::default();
        let seen = calls.clone();
        let route = TmuxInputRoute::new(
            "muxel_p_1",
            true,
            move |args: &[String], stdin: Option<&[u8]>| {
                seen.lock().push((args.to_vec(), stdin.map(<[u8]>::to_vec)));
                // The agent's pane has gone; the user's split is left.
                Ok(output(0, "muxel_p_1:%9:0\n", ""))
            },
        );
        let input = TmuxInput::new(route);
        let error = deliver(&input, ControlInput::Paste("x".into())).unwrap_err();
        assert!(
            matches!(&error, ControlWriteError::Unresolved(why) if why.contains("created with has gone")),
            "{error:?}"
        );
        // Inside the backoff window: refused without running tmux.
        let error = deliver(&input, ControlInput::Submit).unwrap_err();
        assert!(
            matches!(&error, ControlWriteError::Unresolved(why) if why.contains("the next can run in 2 s")),
            "{error:?}"
        );
        assert_eq!(
            commands(&calls),
            ["list-panes"],
            "nothing is typed anywhere without the agent's pane"
        );
        assert_eq!(input.pinned_pane(), None);
    }

    /// The wait after each failed lookup in a row: two seconds, doubling, at
    /// most two minutes.
    #[test]
    fn pane_lookups_back_off_from_two_seconds_to_two_minutes() {
        let secs = |failures| PinBackoff::delay(failures).as_secs();
        let schedule: Vec<u64> = (0..=9).map(secs).collect();
        assert_eq!(schedule, [0, 2, 4, 8, 16, 32, 64, 120, 120, 120]);
        assert_eq!(secs(u32::MAX), 120);

        let t0 = Instant::now();
        let mut backoff = PinBackoff::default();
        assert_eq!(backoff.wait(t0), None, "the first lookup runs at once");
        backoff.failed(t0);
        assert_eq!(backoff.wait(t0), Some(Duration::from_secs(2)));
        assert_eq!(
            backoff.wait(t0 + Duration::from_millis(500)),
            Some(Duration::from_millis(1500))
        );
        assert_eq!(backoff.wait(t0 + Duration::from_secs(2)), None);
        backoff.failed(t0 + Duration::from_secs(2));
        assert_eq!(
            backoff.wait(t0 + Duration::from_secs(2)),
            Some(Duration::from_secs(4))
        );
    }

    /// A pane that can't be found runs one lookup per backoff window however
    /// often writes come, and once found is never looked up again.
    #[test]
    fn writes_inside_the_backoff_window_run_no_tmux() {
        let lookups = Arc::new(AtomicUsize::new(0));
        let found = Arc::new(AtomicBool::new(false));
        let route = {
            let (lookups, found) = (lookups.clone(), found.clone());
            TmuxInputRoute::new("muxel_p_1", true, move |args: &[String], _| {
                assert_eq!(args[0], "list-panes");
                lookups.fetch_add(1, Ordering::SeqCst);
                Ok(if found.load(Ordering::SeqCst) {
                    output(0, AGENT_PANES, "")
                } else {
                    output(
                        1,
                        "",
                        "ssh: connect to host h port 22: Connection refused\n",
                    )
                })
            })
        };
        let shared = Shared {
            pin: Mutex::new(None),
            backoff: Mutex::new(PinBackoff::default()),
            closed: AtomicBool::new(false),
        };
        let t0 = Instant::now();
        let at = |secs: u64| t0 + Duration::from_secs(secs);
        // A write every second; lookups run only at 0, 2, 6, 14 and 30 s.
        let mut ran_at = Vec::new();
        for second in 0..=30 {
            let before = lookups.load(Ordering::SeqCst);
            let error = pinned_pane(&route, &shared, at(second)).unwrap_err();
            assert!(
                matches!(error, ControlWriteError::Unresolved(_)),
                "{error:?}"
            );
            if lookups.load(Ordering::SeqCst) > before {
                ran_at.push(second);
            }
        }
        assert_eq!(ran_at, [0, 2, 6, 14, 30]);

        found.store(true, Ordering::SeqCst);
        assert!(
            pinned_pane(&route, &shared, at(31)).is_err(),
            "still waiting"
        );
        assert_eq!(pinned_pane(&route, &shared, at(62)).unwrap(), "%7");
        assert_eq!(pinned_pane(&route, &shared, at(63)).unwrap(), "%7");
        assert_eq!(lookups.load(Ordering::SeqCst), 6);
    }
}

/// ctl input through a real tmux, to an agent pane behind an attached client,
/// on a throwaway socket no muxel profile uses. No shell grant exists at this
/// level: these writes are what an agent pane gets with the grant off. Skipped
/// where tmux isn't installed; CI installs it. Nothing here runs
/// `capture-pane`: what the client shows is read from its own terminal.
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
        sync::Arc,
        time::{Duration, Instant},
    };

    /// A recorder pane: `sh RECORDER <file> <label> <mode>`. With mode
    /// `bracketed` it asks for bracketed paste, as agents' editors do. It takes
    /// its terminal raw so every byte arrives as sent, says `<label> ready`
    /// (tmux reads a pane's output in order, so its paste mode is set once that
    /// shows in the client) and records every byte in `<file>`.
    const RECORDER: &str = "[ \"$3\" = bracketed ] && printf '\\033[?2004h'\nstty raw -echo\nprintf '%s ready\\r\\n' \"$2\"\nexec cat > \"$1\"\n";

    #[derive(Clone, Copy)]
    enum Mode {
        Bracketed,
        Plain,
    }

    impl Mode {
        fn arg(self) -> &'static str {
            match self {
                Self::Bracketed => "bracketed",
                Self::Plain => "plain",
            }
        }
    }

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

    /// A terminal running `tmux new-session -A` for `name`: it creates the
    /// session with the agent recorder as its program, or attaches to it.
    fn client(socket: &str, name: &str, launch: &[String]) -> (Arc<TerminalSession>, PtyReceiver) {
        let mut args: Vec<String> = ["-L", socket, "-f", "/dev/null"].map(String::from).into();
        args.extend(["new-session", "-A", "-s", name, "--"].map(String::from));
        args.extend(launch.iter().cloned());
        let route_socket = socket.to_string();
        let route =
            TmuxInputRoute::new(name, true, move |args: &[String], stdin: Option<&[u8]>| {
                run(&route_socket, args, stdin)
            });
        let mut spec = CommandSpec::program("tmux", args).with_tmux_input(route);
        spec.env_remove.push("TMUX".into());
        TerminalSession::spawn(spec, 80, 24).expect("spawn tmux client")
    }

    struct Agent {
        socket: String,
        name: String,
        dir: PathBuf,
        script: PathBuf,
        recv: PathBuf,
        launch: Vec<String>,
        session: Arc<TerminalSession>,
        rx: PtyReceiver,
    }

    /// Another client terminal on the agent's session, as after muxel
    /// restarts or reconnects.
    struct Client {
        session: Arc<TerminalSession>,
        _rx: PtyReceiver,
    }

    impl Drop for Client {
        fn drop(&mut self) {
            self.session.kill();
        }
    }

    impl Agent {
        /// A tmux client in a terminal, attached to a session whose one pane
        /// runs an agent that asks for bracketed paste. No ctl write has looked
        /// up its pane yet.
        fn start() -> Option<Self> {
            Self::start_as(Mode::Bracketed)
        }

        fn start_as(mode: Mode) -> Option<Self> {
            let Ok(version) = Command::new("tmux").arg("-V").output() else {
                eprintln!("tmux is not installed; skipping");
                return None;
            };
            let id = uuid::Uuid::new_v4().simple().to_string();
            let socket = format!("mxltest-{id}");
            let dir = std::env::temp_dir().join(format!("muxel-ctl-tmux-{id}"));
            std::fs::create_dir_all(&dir).unwrap();
            let script = dir.join("recorder.sh");
            std::fs::write(&script, RECORDER).unwrap();
            let recv = dir.join("agent.bin");
            let name = format!("muxel_ctl_{}", &id[..8]);
            let launch: Vec<String> = vec![
                "sh".into(),
                script.display().to_string(),
                recv.display().to_string(),
                "agent".into(),
                mode.arg().into(),
            ];
            let (session, rx) = client(&socket, &name, &launch);
            let agent = Self {
                socket,
                name,
                dir,
                script,
                recv,
                launch,
                session,
                rx,
            };
            agent.until("the client attached to the agent", |agent| {
                agent.recv.exists()
                    && agent.shows("agent ready")
                    && agent.format("#{session_attached}") == "1"
            });
            assert!(agent.session.delivers_through_tmux());
            assert_eq!(agent.session.tmux_input_pane(), None);
            eprintln!(
                "real {}: {} attached",
                String::from_utf8_lossy(&version.stdout).trim(),
                agent.name
            );
            Some(agent)
        }

        /// Whether the client's terminal shows `text`: what tmux drew there.
        fn shows(&self, text: &str) -> bool {
            self.session.recent_text(50).contains(text)
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

        /// `format` for the session's active pane.
        fn format(&self, format: &str) -> String {
            let out = tmux_command(&self.socket)
                .args(["display-message", "-p", "-t", &self.target(), format])
                .stdin(Stdio::null())
                .output()
                .unwrap();
            String::from_utf8_lossy(&out.stdout).trim_end().to_string()
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
        /// `new-window`) with it as the pane's program, as a user would, and
        /// wait until it is ready; returns the file it records into.
        fn open(&self, command: &str, label: &str, mode: Mode) -> PathBuf {
            let recv = self.dir.join(format!("{label}.bin"));
            let (target, script, file) = (
                self.target(),
                self.script.display().to_string(),
                recv.display().to_string(),
            );
            self.tmux(&[
                command,
                "-t",
                &target,
                "sh",
                &script,
                &file,
                label,
                mode.arg(),
            ]);
            self.until(&format!("the {label} was ready"), |agent| {
                recv.exists() && agent.shows(&format!("{label} ready"))
            });
            recv
        }

        /// Split the window with no command, as a user's split runs their
        /// shell (here a recorder set as the default shell): the new pane has
        /// no start command, and is active.
        fn split_shell(&self, label: &str) -> PathBuf {
            use std::os::unix::fs::PermissionsExt as _;
            let recv = self.dir.join(format!("{label}.bin"));
            let shell = self.dir.join(format!("{label}.sh"));
            std::fs::write(
                &shell,
                format!(
                    "#!/bin/sh\nexec sh '{}' '{}' '{label}' plain\n",
                    self.script.display(),
                    recv.display()
                ),
            )
            .unwrap();
            std::fs::set_permissions(&shell, std::fs::Permissions::from_mode(0o755)).unwrap();
            self.tmux(&[
                "set-option",
                "-g",
                "default-shell",
                &shell.display().to_string(),
            ]);
            self.tmux(&["split-window", "-t", &self.target()]);
            self.until(&format!("the {label} was ready"), |agent| {
                recv.exists() && agent.shows(&format!("{label} ready"))
            });
            assert_eq!(
                self.format("#{pane_start_command}"),
                "",
                "a split without a command has no start command"
            );
            recv
        }

        /// A second client terminal attached to the same session.
        fn attach_again(&self) -> Client {
            let (session, rx) = client(&self.socket, &self.name, &self.launch);
            self.until("the second client attached", |agent| {
                agent.format("#{session_attached}") == "2"
            });
            Client { session, _rx: rx }
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

        /// Pump the client's output into its terminal, as muxel's does, until
        /// `ready`, or fail the test.
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

        /// What the client terminal's own paste mode is now, for the log: muxel
        /// sees only this, never the agent pane's.
        fn log_client_mode(&self, when: &str) {
            eprintln!(
                "{when}: the client terminal's bracketed paste is {}",
                if self.session.is_bracketed_paste() {
                    "on"
                } else {
                    "off"
                }
            );
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
        let prompt = "first line \u{2713} \u{e9}\nsecond \u{2014} \u{65e5}\u{672c}; $HOME `id` 'q' \"d\" \\\r\n\tthird\x1b[31m end";
        agent.write(ControlInput::Paste(prompt.into())).unwrap();
        agent.write(ControlInput::Submit).unwrap();
        let mut expected = b"\x1b[200~".to_vec();
        expected.extend(
            prompt
                .replace('\x1b', "")
                .replace("\r\n", "\r")
                .replace('\n', "\r")
                .as_bytes(),
        );
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

    /// The agent's pane asked for bracketed paste and the user's active pane
    /// didn't. tmux brackets a `-p` paste only for a pane that asked, and muxel
    /// can't see which did, so a prompt holding the bracket's own end sequence
    /// must still arrive as one paste.
    #[test]
    fn a_bracketed_agent_gets_one_whole_paste_while_an_unbracketed_pane_is_active() {
        let Some(agent) = Agent::start_as(Mode::Bracketed) else {
            return;
        };
        let split = agent.open("split-window", "split", Mode::Plain);
        agent.log_client_mode("an unbracketed split active");
        agent
            .write(ControlInput::Paste("one\x1b[201~two\r\nthree\nfour".into()))
            .unwrap();
        agent.write(ControlInput::Submit).unwrap();
        let expected = b"\x1b[200~one[201~two\rthree\rfour\x1b[201~\r";
        agent.until_received(expected);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(agent.received(), expected);
        assert!(read(&split).is_empty(), "{:?}", read(&split));
    }

    /// The reverse: the agent's pane didn't ask for bracketed paste and the
    /// user's active pane did. The agent gets the prompt as typed lines, each
    /// break a CR, the key Enter sends, and no ESC to start a sequence.
    #[test]
    fn an_unbracketed_agent_gets_typed_lines_while_a_bracketed_pane_is_active() {
        let Some(agent) = Agent::start_as(Mode::Plain) else {
            return;
        };
        let split = agent.open("split-window", "split", Mode::Bracketed);
        agent.log_client_mode("a bracketed split active");
        agent
            .write(ControlInput::Paste("one\r\ntwo\nthree\x1b[1m end".into()))
            .unwrap();
        agent.write(ControlInput::Submit).unwrap();
        let expected = b"one\rtwo\rthree[1m end\r";
        agent.until_received(expected);
        std::thread::sleep(Duration::from_millis(300));
        assert_eq!(agent.received(), expected);
        assert!(read(&split).is_empty(), "{:?}", read(&split));
    }

    /// The user splits the agent's window and leaves their shell active; then
    /// the first ctl write, and the first from a client attached later (muxel
    /// restarted or reconnected), both find the agent's pane.
    #[test]
    fn the_agents_pane_is_found_whichever_pane_the_user_left_active() {
        let Some(agent) = Agent::start() else { return };
        let agent_pane = agent.format("#{pane_id}");
        let shell = agent.split_shell("shell");
        assert_ne!(
            agent.format("#{pane_id}"),
            agent_pane,
            "the shell is active"
        );

        agent.write(ControlInput::Paste("first".into())).unwrap();
        agent.until_received(b"\x1b[200~first\x1b[201~");
        assert_eq!(agent.session.tmux_input_pane(), Some(agent_pane.clone()));

        let again = agent.attach_again();
        assert_eq!(again.session.tmux_input_pane(), None);
        finish(
            again
                .session
                .control_write(ControlInput::Paste("again".into())),
        )
        .unwrap();
        finish(again.session.control_write(ControlInput::Submit)).unwrap();
        agent.until_received(b"\x1b[200~first\x1b[201~\x1b[200~again\x1b[201~\r");
        assert_eq!(again.session.tmux_input_pane(), Some(agent_pane));
        assert!(read(&shell).is_empty(), "{:?}", read(&shell));
    }

    #[test]
    fn ctl_input_stays_on_the_agents_pane_after_the_user_opens_a_window_and_a_split() {
        let Some(agent) = Agent::start() else { return };
        agent.write(ControlInput::Paste("first".into())).unwrap();
        agent.until_received(b"\x1b[200~first\x1b[201~");
        let pinned = agent.session.tmux_input_pane().unwrap();
        let window = agent.open("new-window", "window", Mode::Bracketed);
        let split = agent.open("split-window", "split", Mode::Bracketed);
        assert_ne!(
            agent.format("#{pane_id}"),
            pinned,
            "the split is active now"
        );
        agent
            .write(ControlInput::Paste("to the agent".into()))
            .unwrap();
        agent.write(ControlInput::Submit).unwrap();
        agent.until_received(b"\x1b[200~first\x1b[201~\x1b[200~to the agent\x1b[201~\r");
        assert_eq!(agent.session.tmux_input_pane(), Some(pinned));
        assert!(read(&window).is_empty() && read(&split).is_empty());
    }

    #[test]
    fn a_vanished_pane_fails_delivery_and_nothing_is_typed_anywhere() {
        let Some(agent) = Agent::start() else { return };
        agent.write(ControlInput::Paste("first".into())).unwrap();
        agent.until_received(b"\x1b[200~first\x1b[201~");
        let pinned = agent.session.tmux_input_pane().unwrap();
        // The session, and its client, live on in another window.
        let window = agent.open("new-window", "window", Mode::Bracketed);
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
        // The pane is never looked up again for whatever is active now.
        assert_eq!(agent.session.tmux_input_pane(), Some(pinned));
        std::thread::sleep(Duration::from_millis(300));
        assert!(read(&window).is_empty(), "{:?}", read(&window));
    }

    /// The agent's pane has gone before any ctl write and only the user's
    /// shell is left: no pane is found, nothing is typed, and the next lookup
    /// waits out its backoff.
    #[test]
    fn with_the_agents_pane_gone_the_users_shell_is_never_found() {
        let Some(agent) = Agent::start() else { return };
        let agent_pane = agent.format("#{pane_id}");
        let shell = agent.split_shell("shell");
        agent.tmux(&["kill-pane", "-t", &agent_pane]);

        let failed = agent.write(ControlInput::Paste("lost".into()));
        assert!(
            matches!(&failed, Err(ControlWriteError::Unresolved(why)) if why.contains("created with has gone")),
            "{failed:?}"
        );
        let failed = agent.write(ControlInput::Submit);
        assert!(
            matches!(&failed, Err(ControlWriteError::Unresolved(why)) if why.contains("the next can run in")),
            "{failed:?}"
        );
        assert_eq!(agent.session.tmux_input_pane(), None);
        std::thread::sleep(Duration::from_millis(300));
        assert!(read(&shell).is_empty(), "{:?}", read(&shell));
    }
}
