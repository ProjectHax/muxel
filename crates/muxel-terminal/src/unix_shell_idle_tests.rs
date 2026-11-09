//! Real shell close decisions, with the test owning every command's lifetime.

use super::{CommandSpec, PtyChunk, PtyReceiver, TerminalSession};
use muxel_core::control::AgentState;
use muxel_core::recoverable_close::{CloseDecision, CloseFacts, decide, shell_close_evidence};
use std::cell::RefCell;
use std::sync::Arc;
use std::time::{Duration, Instant};

const PROMPT: &str = "MUXEL_READY> ";

struct Shell {
    session: Arc<TerminalSession>,
    rx: PtyReceiver,
    job: Option<libc::pid_t>,
    marked: bool,
    pending: RefCell<String>,
}

impl Shell {
    fn start(marked: bool) -> Self {
        let mut spec = CommandSpec::program(
            "/bin/bash",
            vec![
                "--noprofile".into(),
                "--norc".into(),
                "-i".into(),
                "+H".into(),
            ],
        );
        spec.env_remove = vec!["PROMPT_COMMAND".into(), "BASH_ENV".into(), "ENV".into()];
        let prompt = if marked {
            format!("\\[\x1b]133;A\x07\\]{PROMPT}\\[\x1b]133;B\x07\\]")
        } else {
            PROMPT.into()
        };
        spec.env = vec![
            ("PS1".into(), prompt),
            ("HISTFILE".into(), "/dev/null".into()),
        ];
        let shell = Self::spawn(spec, marked);
        shell.read_prompt();
        assert_eq!(shell.session.shell_at_prompt(), marked);
        shell
    }

    fn spawn(spec: CommandSpec, marked: bool) -> Self {
        let (session, rx) = TerminalSession::spawn(spec, 120, 24).expect("spawn bash");
        Self {
            session,
            rx,
            job: None,
            marked,
            pending: RefCell::new(String::new()),
        }
    }

    fn read_until(&self, marker: &str) -> String {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let mut pending = self.pending.borrow_mut();
            if let Some(end) = pending.find(marker) {
                let rest = pending.split_off(end + marker.len());
                return std::mem::replace(&mut *pending, rest);
            }
            match self.rx.try_recv() {
                Ok(PtyChunk::Output(bytes)) => {
                    self.session.process_output(&bytes);
                    pending.push_str(&String::from_utf8_lossy(&bytes));
                }
                Ok(PtyChunk::Exit { .. }) => panic!("shell exited before {marker}"),
                Err(async_channel::TryRecvError::Closed) => panic!("shell stream closed"),
                Err(async_channel::TryRecvError::Empty) => {
                    assert!(Instant::now() < deadline, "no {marker} in {:?}", *pending);
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
        }
    }

    fn read_prompt(&self) -> String {
        self.read_until(if self.marked {
            "\x1b]133;B\x07"
        } else {
            PROMPT
        })
    }

    fn close_decision(&self) -> CloseDecision {
        decide(
            CloseFacts {
                status: AgentState::Idle,
                evidence: shell_close_evidence(
                    None,
                    false,
                    self.session.foreground_idle(),
                    self.session.shell_at_prompt(),
                ),
                attributable: true,
                awaiting_reply: false,
                another_tmux_client: false,
                session_may_outlive: false,
                launch_id: None,
                binding_rev: 0,
                input_rev: 0,
                completion_rev: 0,
            },
            true,
        )
    }

    fn start_job(&mut self, foreground: bool) {
        // $! names exactly the process this shell started. No wait for sleep
        // to finish: the test kills it and the shell explicitly reaps it.
        self.session
            .write_input(b"sleep 30 & muxel_job=$!; printf '\\nCHILD:%s\\n' \"$muxel_job\"\n");
        self.read_job_pid();
        if foreground {
            self.session.write_input(b"fg\n");
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                let group = self
                    .session
                    .master
                    .inner
                    .lock()
                    .as_ref()
                    .and_then(|master| master.process_group_leader());
                if group == self.job {
                    break;
                }
                assert!(Instant::now() < deadline, "job never took the foreground");
                std::thread::sleep(Duration::from_millis(10));
            }
        }
    }

    /// Start a child of the shell that leads a session of its own, as zsh's
    /// zpty worker does. With job control off, the job shares the shell's
    /// group, so it isn't a group leader and `setsid` succeeds.
    fn start_own_session_child(&mut self) {
        self.session.write_input(
            b"set +m; perl -MPOSIX -e 'POSIX::setsid() or exit 1; sleep 30' & muxel_job=$!; set -m; printf '\\nCHILD:%s\\n' \"$muxel_job\"\n",
        );
        self.read_job_pid();
        let pid = self.job.expect("test job");
        let deadline = Instant::now() + Duration::from_secs(10);
        // SAFETY: getsid only reads the session id of this test's child.
        while unsafe { libc::getsid(pid) } != pid {
            assert!(
                Instant::now() < deadline,
                "the child never started its own session"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Read the pid the shell printed for `$muxel_job`, then its next prompt.
    fn read_job_pid(&mut self) {
        // Wait for the command's PID output before accepting its next prompt.
        self.read_until("\nCHILD:");
        let pid_line = self.read_until("\n");
        self.job = Some(
            pid_line
                .trim()
                .parse()
                .expect("the shell reported its child's pid"),
        );
        self.read_prompt();
    }

    fn end_job(&mut self) {
        let pid = self.job.expect("test job");
        // SAFETY: this is the exact live child reported by this shell. The
        // shell waits below before another command or assertion can run.
        assert_eq!(unsafe { libc::kill(pid, libc::SIGKILL) }, 0);
        self.job = None;
        self.session
            .write_input(b"wait \"$muxel_job\"; printf '\\nJOB_%s\\n' REAPED\n");
        self.read_until("JOB_REAPED");
        self.read_prompt();
    }

    fn wait_for_exec(&self) {
        let pid = self.session.child_pid.expect("shell pid");
        let deadline = Instant::now() + Duration::from_secs(10);
        while executable(pid).is_none_or(|path| path.file_name() != Some("cat".as_ref())) {
            assert!(Instant::now() < deadline, "the shell never exec'd cat");
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            self.session
                .master
                .inner
                .lock()
                .as_ref()
                .and_then(|master| master.process_group_leader()),
            Some(pid as libc::pid_t),
            "exec keeps the shell's foreground group"
        );
        assert!(crate::process_tree_unix::shell_has_no_session_children(pid));
    }
}

/// Read the OS image independently of the idle predicate under test. `cat`
/// waits for input until this test closes its session, without a timed exit.
fn executable(pid: u32) -> Option<std::path::PathBuf> {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_link(format!("/proc/{pid}/exe")).ok()
    }
    #[cfg(target_os = "macos")]
    {
        use std::os::unix::ffi::OsStrExt;
        let mut path = [0u8; libc::PROC_PIDPATHINFO_MAXSIZE as usize];
        // SAFETY: path is a writable buffer whose exact size is passed.
        if unsafe {
            libc::proc_pidpath(
                pid as libc::pid_t,
                path.as_mut_ptr().cast(),
                path.len() as u32,
            )
        } <= 0
        {
            return None;
        }
        let path = std::ffi::CStr::from_bytes_until_nul(&path).ok()?;
        Some(std::ffi::OsStr::from_bytes(path.to_bytes()).into())
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = pid;
        None
    }
}

impl Drop for Shell {
    fn drop(&mut self) {
        if let Some(pid) = self.job.take() {
            // SAFETY: cleanup of this test's child if an assertion failed.
            unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        self.session.kill();
    }
}

#[test]
fn an_idle_prompt_is_idle() {
    for marked in [false, true] {
        let shell = Shell::start(marked);
        assert!(
            shell.session.is_idle_foreground(),
            "idle shell pid={:?}, launch={:?}, current={:?}",
            shell.session.child_pid,
            shell.session.child_identity,
            shell.session.child_pid.and_then(executable)
        );
        assert_eq!(
            shell.close_decision(),
            CloseDecision::Hold { forced: false }
        );
    }
}

#[test]
fn a_background_job_asks_before_close_until_that_pid_is_killed() {
    for marked in [false, true] {
        let mut shell = Shell::start(marked);
        shell.start_job(false);
        assert_eq!(shell.session.shell_at_prompt(), marked);
        assert_eq!(
            shell.close_decision(),
            CloseDecision::Ask,
            "a background job behind the prompt must ask (OSC 133: {marked})"
        );
        assert!(!shell.session.is_idle_foreground());
        shell.end_job();
        assert!(shell.session.is_idle_foreground());
        assert_eq!(
            shell.close_decision(),
            CloseDecision::Hold { forced: false }
        );
    }
}

#[test]
fn a_child_in_a_session_of_its_own_does_not_ask() {
    for marked in [false, true] {
        let mut shell = Shell::start(marked);
        shell.start_own_session_child();
        assert!(
            shell.session.is_idle_foreground(),
            "a child on a terminal of its own made the shell busy (OSC 133: {marked})"
        );
        assert_eq!(
            shell.close_decision(),
            CloseDecision::Hold { forced: false }
        );
        shell.end_job();
    }
}

#[test]
fn a_foreground_command_is_not_idle() {
    let mut shell = Shell::start(false);
    shell.start_job(true);
    assert!(!shell.session.is_idle_foreground());
    assert_eq!(shell.close_decision(), CloseDecision::Ask);
    shell.end_job();
    assert!(shell.session.is_idle_foreground());
}

#[test]
fn an_exec_replacement_asks_before_close() {
    for marked in [false, true] {
        let shell = Shell::start(marked);
        shell.session.write_input(b"exec /bin/cat\n");
        shell.wait_for_exec();
        assert_eq!(
            shell.close_decision(),
            CloseDecision::Ask,
            "a replacement with the shell's PID and no children must ask"
        );
    }
}

#[test]
fn an_immediate_exec_does_not_establish_an_idle_shell() {
    let shell = Shell::spawn(
        CommandSpec::program(
            "/bin/bash",
            vec![
                "--noprofile".into(),
                "--norc".into(),
                "-c".into(),
                "exec /bin/cat".into(),
            ],
        ),
        false,
    );
    shell.wait_for_exec();
    assert_eq!(shell.close_decision(), CloseDecision::Ask);
}

#[test]
fn an_exec_into_another_shell_stays_idle() {
    // `exec zsh` from a bashrc replaces the launched image with another shell
    // that waits at its own prompt; closing that pane needn't ask.
    let Some(zsh) = ["/bin/zsh", "/usr/bin/zsh"]
        .into_iter()
        .find(|path| std::path::Path::new(path).exists())
    else {
        eprintln!("zsh not installed; skipping");
        return;
    };
    let shell = Shell::start(false);
    // `%%` echoes as typed but prompts as a single `%`, so only the prompt matches.
    shell
        .session
        .write_input(format!("PS1='ZSH%%READY ' exec {zsh} -f -i\n").as_bytes());
    shell.read_until("ZSH%READY ");
    let deadline = Instant::now() + Duration::from_secs(10);
    while !shell.session.is_idle_foreground() {
        assert!(
            Instant::now() < deadline,
            "zsh at its prompt never read idle"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}
