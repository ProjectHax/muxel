use super::{
    AgentStatus, BACKGROUND_HOLD, BACKGROUND_WAKE_GRACE, BackgroundHold, RuntimeStatusState,
    TerminalRuntimeSnapshot, TerminalStatusCoverage, TitleProvider,
};
use crate::session::{CommandSpec, LaunchBinding, PtyReceiver, TerminalSession};
use muxel_core::control::BackgroundWork;
use std::sync::Arc;
use std::time::{Duration, Instant};

const SUBAGENT_CAP: Duration = Duration::from_secs(3 * 60 * 60);
const SHELL: BackgroundWork = BackgroundWork {
    shells: 1,
    monitors: 0,
    subagents: false,
};
const SUBAGENTS: BackgroundWork = BackgroundWork {
    shells: 0,
    monitors: 0,
    subagents: true,
};

#[test]
fn pr136_shared_expired_hold_cannot_restart_without_a_new_turn() {
    let now = Instant::now();
    let mut hold = BackgroundHold::default();
    hold.observe(Some(SHELL), true, now);
    assert!(hold.observe(Some(SHELL), false, now).0);
    let expired = now + BACKGROUND_HOLD;
    assert!(!hold.observe(Some(SHELL), false, expired).0);
    assert!(
        !hold
            .observe(None, false, expired + Duration::from_secs(1))
            .0
    );
    assert!(
        !hold
            .observe(Some(SHELL), false, expired + Duration::from_secs(2))
            .0,
        "a redraw cannot start another hold for an already released turn",
    );
    let next = expired + Duration::from_secs(3);
    hold.observe(Some(SHELL), true, next);
    assert!(hold.observe(Some(SHELL), false, next).0);
}

#[test]
fn pr136_shared_footer_return_cannot_rehold_a_released_turn() {
    let now = Instant::now();
    let mut hold = BackgroundHold::default();
    hold.observe(Some(SUBAGENTS), true, now);
    assert!(hold.observe(Some(SUBAGENTS), false, now).0);
    let hidden = now + Duration::from_secs(1);
    assert!(hold.observe(None, false, hidden).0);
    assert!(!hold.observe(None, false, hidden + BACKGROUND_WAKE_GRACE).0);
    assert!(
        !hold
            .observe(Some(SUBAGENTS), false, hidden + BACKGROUND_WAKE_GRACE)
            .0,
        "the same turn stays released when its footer returns",
    );
}

#[test]
fn pr136_shared_subagent_hints_have_a_three_hour_safety_cap() {
    let now = Instant::now();
    let mut hold = BackgroundHold::default();
    hold.observe(Some(SUBAGENTS), true, now);
    assert_eq!(
        hold.observe(Some(SUBAGENTS), false, now),
        (true, Some(now + SUBAGENT_CAP)),
    );
    assert!(
        hold.observe(
            Some(SUBAGENTS),
            false,
            now + SUBAGENT_CAP - Duration::from_secs(1)
        )
        .0
    );
    assert!(!hold.observe(Some(SUBAGENTS), false, now + SUBAGENT_CAP).0);
}

#[test]
fn pr136_shared_released_pane_cannot_hold_a_late_listing() {
    for work in [SHELL, SUBAGENTS] {
        let now = Instant::now();
        let mut hold = BackgroundHold::default();
        assert_eq!(hold.observe(None, false, now), (false, None));
        assert_eq!(
            hold.observe(Some(work), false, now + Duration::from_secs(1)),
            (false, None),
            "a late listing cannot undo an already published finish",
        );
    }
}

struct Probe {
    session: Arc<TerminalSession>,
    _receiver: PtyReceiver,
    runtime: RuntimeStatusState,
    start: Instant,
}

impl Probe {
    fn new() -> Self {
        #[cfg(windows)]
        let command = CommandSpec::program(
            "cmd.exe",
            vec!["/d".into(), "/q".into(), "/c".into(), "more".into()],
        );
        #[cfg(unix)]
        let command = CommandSpec::program("/bin/cat", vec![]);
        let (session, receiver) =
            TerminalSession::spawn(command.with_binding(LaunchBinding::Known(None)), 100, 24)
                .expect("spawn inert review fixture");
        Self {
            session,
            _receiver: receiver,
            runtime: RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible),
            start: Instant::now(),
        }
    }

    fn sample(&mut self, after: Duration, footer: &str, reports: &[u8]) -> TerminalRuntimeSnapshot {
        let rule = "─".repeat(100);
        let screen =
            format!("Claude Code\n● Finished the foreground task.\n{rule}\n❯ \n{rule}\n{footer}");
        self.sample_screen(after, &screen, reports)
    }

    fn sample_screen(
        &mut self,
        after: Duration,
        screen: &str,
        reports: &[u8],
    ) -> TerminalRuntimeSnapshot {
        let mut wire = "\x1b[2J\x1b[H\x1b]0;✳ Review task\x07\x1b[?2004h".to_string();
        for (index, row) in screen.lines().enumerate() {
            wire.push_str(&format!("\x1b[{};1H{row}", index + 1));
        }
        wire.push_str("\x1b[4;3H");
        self.session.test_program_status(wire.as_bytes());
        if !reports.is_empty() {
            self.session.test_program_status(reports);
        }
        self.runtime.reconcile(
            &self.session,
            TitleProvider::Claude,
            &[],
            &[],
            false,
            self.start + after,
        );
        self.runtime.snapshot(&self.session)
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.session.kill();
    }
}

const AGENT_FOOTER: &str = "⏵⏵ auto mode on · /tasks to see subagents · ← 4 agents";
const WORKING: &[u8] = b"\x1b]7501;state=working\x07";
const DONE: &[u8] = b"\x1b]7501;state=done\x07";

#[test]
fn pr136_shared_reconcile_coalesced_turn_restarts_its_hold_clock_once() {
    let mut probe = Probe::new();
    probe.sample(Duration::ZERO, AGENT_FOOTER, WORKING);
    assert_eq!(
        probe
            .sample(Duration::from_secs(1), AGENT_FOOTER, DONE)
            .status,
        AgentStatus::Working
    );
    let next_end = Duration::from_secs(1000);
    let batch = b"\x1b]7501;state=working\x07\x1b]7501;state=done\x07";
    let held = probe.sample(next_end, AGENT_FOOTER, batch);
    assert_eq!(held.status, AgentStatus::Working);
    assert_eq!(
        probe.runtime.launch_completion_revision, 2,
        "both program turns remain in launch history"
    );
    assert_eq!(
        held.completion_revision, 0,
        "a hold does not publish an early Done event"
    );
    let original_expiry = Duration::from_secs(1) + SUBAGENT_CAP;
    assert_eq!(
        probe.sample(original_expiry, AGENT_FOOTER, b"").status,
        AgentStatus::Working,
        "the newly completed turn owns a fresh clock despite sharing one PTY batch",
    );
    assert_eq!(
        probe
            .sample(next_end + SUBAGENT_CAP, AGENT_FOOTER, b"")
            .status,
        AgentStatus::Done
    );
    assert_eq!(probe.runtime.launch_completion_revision, 2);
}

#[test]
fn pr136_shared_reconcile_blocked_ui_cannot_rearm_a_released_turn() {
    let mut probe = Probe::new();
    // Ordered title frames can complete a turn between two UI reads.
    probe
        .session
        .test_program_status("\x1b]0;◑ Review task\x07".as_bytes());
    assert_eq!(
        probe.sample(Duration::ZERO, AGENT_FOOTER, b"").status,
        AgentStatus::Working
    );
    probe.sample(Duration::from_secs(1), "? for shortcuts", b"");
    assert_ne!(
        probe
            .sample(Duration::from_secs(7), "? for shortcuts", b"")
            .status,
        AgentStatus::Working
    );
    let dialog = "Claude Code\nDo you want to proceed?\n❯ 1. Yes\n  2. No\nEsc to cancel";
    assert_eq!(
        probe
            .sample_screen(Duration::from_secs(8), dialog, b"")
            .status,
        AgentStatus::Blocked
    );
    assert_ne!(
        probe
            .sample(Duration::from_secs(9), AGENT_FOOTER, b"")
            .status,
        AgentStatus::Working,
        "a Blocked-only UI frame is not a new Working turn",
    );
}

#[test]
fn pr136_shared_reconcile_error_then_idle_cannot_rearm_a_released_turn() {
    let mut probe = Probe::new();
    probe.sample(Duration::ZERO, AGENT_FOOTER, WORKING);
    probe.sample(Duration::from_secs(1), AGENT_FOOTER, DONE);
    let failed = probe.sample(
        Duration::from_secs(2),
        AGENT_FOOTER,
        b"\x1b]7501;state=error\x07",
    );
    assert_eq!(failed.status, AgentStatus::Done);
    assert_ne!(
        probe
            .sample(
                Duration::from_secs(3),
                AGENT_FOOTER,
                b"\x1b]7501;state=idle\x07"
            )
            .status,
        AgentStatus::Working,
        "an already reported failure stays released until actual new work",
    );
}

#[test]
fn pr136_shared_hidden_footer_releases_once_in_the_runtime() {
    let mut probe = Probe::new();
    probe.sample(Duration::ZERO, AGENT_FOOTER, WORKING);
    assert_eq!(
        probe
            .sample(Duration::from_secs(1), AGENT_FOOTER, DONE)
            .status,
        AgentStatus::Working
    );
    assert_eq!(
        probe
            .sample(Duration::from_secs(2), "? for shortcuts", b"")
            .status,
        AgentStatus::Working
    );
    let released = probe.sample(Duration::from_secs(8), "? for shortcuts", b"");
    assert_eq!(released.status, AgentStatus::Done);
    let returned = probe.sample(Duration::from_secs(9), AGENT_FOOTER, b"");
    assert_eq!(
        returned.status,
        AgentStatus::Done,
        "returning chrome cannot make another Working→Done notification cycle"
    );
    assert_eq!(returned.completion_revision, released.completion_revision);
    assert_eq!(probe.runtime.launch_completion_revision, 1);
}
