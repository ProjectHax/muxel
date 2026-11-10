use super::{
    AgentStatus, BACKGROUND_WAKE_GRACE, BackgroundHold, RuntimeStatusState,
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
    hold.observe(Some(SUBAGENTS), true, now);
    assert!(hold.observe(Some(SUBAGENTS), false, now).0);
    let expired = now + SUBAGENT_CAP;
    assert!(!hold.observe(Some(SUBAGENTS), false, expired).0);
    assert!(
        !hold
            .observe(None, false, expired + Duration::from_secs(1))
            .0
    );
    assert!(
        !hold
            .observe(Some(SUBAGENTS), false, expired + Duration::from_secs(2))
            .0,
        "a redraw cannot start another hold for an already released turn",
    );
    let next = expired + Duration::from_secs(3);
    hold.observe(Some(SUBAGENTS), true, next);
    assert!(hold.observe(Some(SUBAGENTS), false, next).0);
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
        self.sample_provider_screen(
            after,
            screen,
            reports,
            TitleProvider::Claude,
            "✳ Review task",
        )
    }

    fn sample_provider_screen(
        &mut self,
        after: Duration,
        screen: &str,
        reports: &[u8],
        provider: TitleProvider,
        title: &str,
    ) -> TerminalRuntimeSnapshot {
        let mut wire = format!("\x1b[2J\x1b[H\x1b]0;{title}\x07\x1b[?2004h");
        for (index, row) in screen.lines().enumerate() {
            wire.push_str(&format!("\x1b[{};1H{row}", index + 1));
        }
        wire.push_str("\x1b[4;3H");
        self.session.test_program_status(wire.as_bytes());
        if !reports.is_empty() {
            self.session.test_program_status(reports);
        }
        self.runtime
            .reconcile(&self.session, provider, &[], &[], false, self.start + after);
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
fn pr136_shared_policy_shell_monitor_turns_finish_with_unchanged_listing() {
    for (footer, background) in [
        ("⏵⏵ auto mode on · 1 shell", SHELL),
        (
            "⏵⏵ auto mode on · 2 monitors",
            BackgroundWork {
                shells: 0,
                monitors: 2,
                subagents: false,
            },
        ),
        (
            "⏵⏵ auto mode on · 1 shell · 2 monitors",
            BackgroundWork {
                shells: 1,
                monitors: 2,
                subagents: false,
            },
        ),
    ] {
        let mut probe = Probe::new();
        for turn in 0..2 {
            let started = Duration::from_secs(turn * 10);
            let working = probe.sample(started, footer, WORKING);
            assert_eq!(
                working.status,
                AgentStatus::Working,
                "{footer}, turn {turn}"
            );
            let done = probe.sample(started + Duration::from_secs(1), footer, DONE);
            assert_eq!(
                done.status,
                AgentStatus::Done,
                "non-agent listings do not delay completion: {footer}, turn {turn}"
            );
            assert_eq!(done.background, Some(background));
            assert_eq!(done.completion_revision, turn + 1);
            let stable = probe.sample(started + Duration::from_secs(2), footer, b"");
            assert_eq!(stable.status, AgentStatus::Done);
            assert_eq!(stable.background, Some(background));
            assert_eq!(
                stable.completion_revision, done.completion_revision,
                "an unchanged server listing does not create another finish"
            );
        }
    }
}

#[test]
fn pr136_shared_policy_plain_wait_finishes_shells_monitors_but_waits_for_subagents() {
    use muxel_core::control::{AgentState, WaitOutcome, awaiting_reply, wait_outcome};
    use muxel_core::{AgentActivity, AgentActivityState};
    for (footer, background, expected) in [
        (
            "⏵⏵ auto mode on · 1 shell",
            SHELL,
            Some(WaitOutcome::Finished),
        ),
        (
            "⏵⏵ auto mode on · 1 monitor",
            BackgroundWork {
                shells: 0,
                monitors: 1,
                subagents: false,
            },
            Some(WaitOutcome::Finished),
        ),
        (AGENT_FOOTER, SUBAGENTS, None),
        (
            "⏵⏵ auto mode on · 1 shell · 1 monitor · /tasks to see subagents · ← 4 agents",
            BackgroundWork {
                shells: 1,
                monitors: 1,
                subagents: true,
            },
            None,
        ),
    ] {
        let mut probe = Probe::new();
        let working = probe.sample(Duration::ZERO, footer, WORKING);
        assert_eq!(working.status, AgentStatus::Working);
        let finished = probe.sample(Duration::from_secs(1), footer, DONE);
        assert_eq!(finished.background, Some(background));
        let (state, activity_state) = match finished.status {
            AgentStatus::Working => (AgentState::Working, AgentActivityState::Working),
            AgentStatus::Idle => (AgentState::Idle, AgentActivityState::Idle),
            AgentStatus::Blocked => (AgentState::Blocked, AgentActivityState::Blocked),
            AgentStatus::Done => (AgentState::Done, AgentActivityState::Done),
        };
        let mut activity = AgentActivity::default();
        activity.observe(None, AgentActivityState::Working, 100);
        activity.observe(Some(AgentActivityState::Working), activity_state, 1_100);
        let awaiting = awaiting_reply(
            Some(0),
            muxel_core::control::REPLY_GRACE_MS,
            Some(state),
            &activity,
            1_100,
        );
        assert_eq!(
            wait_outcome(Some(state), awaiting),
            expected,
            "plain wait follows actual observed lifecycle: {footer}"
        );
        assert_eq!(
            finished.status,
            if background.subagents {
                AgentStatus::Working
            } else {
                AgentStatus::Done
            }
        );
    }
}

#[test]
fn pr136_shared_policy_initial_and_reattached_shell_metadata_forges_no_turn() {
    for footer in ["⏵⏵ auto mode on · 1 shell", "⏵⏵ auto mode on · 1 monitor"] {
        let mut probe = Probe::new();
        let initial = probe.sample(Duration::ZERO, footer, b"");
        assert_eq!(
            initial.status,
            AgentStatus::Idle,
            "listed metadata cannot create foreground work: {footer}"
        );
        assert!(initial.background.is_some());
        assert_eq!(initial.completion_revision, 0);
        // A fresh reducer reattaches to an idle reporting provider whose server
        // listing remains visible. No foreground Working or Done bytes occurred.
        probe.sample(Duration::from_secs(1), footer, b"\x1b]7501;state=idle\x07");
        probe.runtime = RuntimeStatusState::new(TerminalStatusCoverage::ProviderVisible);
        let restored = probe.sample(Duration::from_secs(2), footer, b"");
        assert_eq!(restored.status, AgentStatus::Idle);
        assert_eq!(restored.background, initial.background);
        assert_eq!(restored.completion_revision, 0);
        let stable = probe.sample(Duration::from_secs(3), footer, b"");
        assert_eq!(stable.status, AgentStatus::Idle);
        assert_eq!(stable.completion_revision, 0);
    }
}

#[test]
fn pr136_shared_policy_mixed_subagent_listing_holds_only_to_three_hour_cap() {
    let footer = "⏵⏵ auto mode on · 1 shell · 2 monitors · /tasks to see subagents · ← 4 agents";
    let background = Some(BackgroundWork {
        shells: 1,
        monitors: 2,
        subagents: true,
    });
    let mut probe = Probe::new();
    probe.sample(Duration::ZERO, footer, WORKING);
    let ended = Duration::from_secs(1);
    let held = probe.sample(ended, footer, DONE);
    assert_eq!(held.status, AgentStatus::Working);
    assert_eq!(held.background, background);
    assert_eq!(held.completion_revision, 0);
    let near = probe.sample(ended + SUBAGENT_CAP - Duration::from_secs(1), footer, b"");
    assert_eq!(near.status, AgentStatus::Working);
    assert_eq!(near.background, background);
    let expired = probe.sample(ended + SUBAGENT_CAP, footer, b"");
    assert_eq!(expired.status, AgentStatus::Done);
    assert_eq!(expired.background, background);
    let stable = probe.sample(ended + SUBAGENT_CAP + Duration::from_secs(1), footer, b"");
    assert_eq!(stable.status, AgentStatus::Done);
    assert_eq!(stable.completion_revision, expired.completion_revision);
}

#[test]
fn pr136_shared_policy_grok_title_turn_finishes_beside_shell_monitor_panel() {
    let mut probe = Probe::new();
    probe.session.mark_turn_submitted();
    let screen = "Finished.\nv Tasks 1\n· Run Server\nv Watchers 1\n· Monitor Watch server\n╭─────────────╮\n│ >           │\n╰─────────────╯\nShift+Tab:mode  │  Ctrl+x:shortcuts";
    let expected = Some(BackgroundWork {
        shells: 1,
        monitors: 1,
        subagents: false,
    });
    let working = probe.sample_provider_screen(
        Duration::ZERO,
        screen,
        b"",
        TitleProvider::Grok,
        "⠦ - Responding - Review task - grok",
    );
    assert_eq!(working.status, AgentStatus::Working);
    assert_eq!(working.background, expected);
    let ended = probe.sample_provider_screen(
        Duration::from_secs(1),
        screen,
        b"",
        TitleProvider::Grok,
        "Review task - grok",
    );
    assert_eq!(
        ended.status,
        AgentStatus::Done,
        "Grok Tasks and Watchers do not delay completion"
    );
    assert_eq!(ended.background, expected);
    let stable = probe.sample_provider_screen(
        Duration::from_secs(2),
        screen,
        b"",
        TitleProvider::Grok,
        "Review task - grok",
    );
    assert_eq!(stable.status, AgentStatus::Done);
    assert_eq!(stable.background, expected);
    assert_eq!(stable.completion_revision, ended.completion_revision);
}

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
