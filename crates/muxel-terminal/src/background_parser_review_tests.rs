use super::{AgentStatus, TitleProvider, provider_screen_read};
use muxel_core::control::BackgroundWork;

#[test]
fn pr136_shared_interruptible_claude_command_is_foreground() {
    let (status, _) = provider_screen_read(
        TitleProvider::Claude,
        "· 1 command still running · send a message to interrupt",
    );
    assert_eq!(status, Some(AgentStatus::Working));
}

#[test]
fn pr136_shared_interruptible_grok_commands_are_foreground() {
    for row in [
        "◎ 1 command still running",
        "◎ 2 commands still running · send a message to interrupt",
    ] {
        let (status, _) = provider_screen_read(TitleProvider::Grok, row);
        assert_eq!(status, Some(AgentStatus::Working), "{row}");
    }
}

#[test]
fn pr136_shared_unproven_legacy_task_kinds_are_not_subagents() {
    for kind in [
        "agent",
        "MCP task",
        "cloud session",
        "background task",
        "local agent",
        "team",
        "background dynamic workflow",
        "remote dynamic workflow",
    ] {
        let footer = format!("Auto mode on · 1 {kind}");
        assert_eq!(
            provider_screen_read(TitleProvider::Claude, &footer),
            (None, None),
            "{footer}",
        );
    }
}

#[test]
fn pr136_shared_supported_legacy_counts_survive_unproven_mixed_kinds() {
    for footer in [
        "Auto mode on · 1 shell · 2 agents",
        "Auto mode on · 1 shell, 2 monitors · 1 local agent",
        "Auto mode on · 1 shell · 2 ag\nents",
        "Auto mode on · 1 shell ·   \n2 agents",
        "Auto mode on · 1 shell, 2 monitors · 1 MCP task",
    ] {
        let monitors = u32::from(footer.contains("2 monitors")) * 2;
        assert_eq!(
            provider_screen_read(TitleProvider::Claude, footer),
            (
                None,
                Some(BackgroundWork {
                    shells: 1,
                    monitors,
                    subagents: false,
                }),
            ),
            "{footer}",
        );
    }
}

#[test]
fn pr136_shared_legacy_supported_counts_still_work() {
    assert_eq!(
        provider_screen_read(TitleProvider::Claude, "Auto mode on · 1 shell, 2 monitors",),
        (
            None,
            Some(BackgroundWork {
                shells: 1,
                monitors: 2,
                subagents: false,
            }),
        ),
    );
}

#[test]
fn pr136_shared_zero_grok_subagents_are_not_running() {
    let screen = "I finished.\n\nv Subagents 0\n╭─────────────╮\n│ >           │\n╰─────────────╯\nShift+Tab:mode  │  Ctrl+x:shortcuts";
    assert_eq!(
        provider_screen_read(TitleProvider::Grok, screen),
        (None, None),
    );
}

#[test]
fn pr136_shared_legacy_footer_cannot_mask_current_foreground_spinner() {
    for footer in [
        "Auto mode on · 1 shell",
        "Auto mode on · 1 agent",
        "Auto mode on · 1 shell · 2 agents",
    ] {
        let screen = format!("✻ Chewing on it… (20s · ↓ 171 tokens)\n{footer}");
        assert_eq!(
            provider_screen_read(TitleProvider::Claude, &screen).0,
            Some(AgentStatus::Working),
            "{screen}",
        );
    }
}

#[test]
fn pr136_shared_positive_or_unknown_footer_cannot_mask_interruptible_command() {
    let command = "· 1 command still running · send a message to interrupt";
    for footer in [
        "Auto mode on · 1 shell",
        "Auto mode on · 1 agent",
        "⏵⏵ auto mode on · 1 shell · ← 4 agents",
    ] {
        let screen = format!("{command}\n{footer}");
        assert_eq!(
            provider_screen_read(TitleProvider::Claude, &screen).0,
            Some(AgentStatus::Working),
            "{screen}",
        );
    }
    // A real current empty footer supersedes an old row quoted in a reply.
    let quoted = format!(
        "● The row read:\n{command}\n\n  [>_>] pane\n  ⏵⏵ auto mode on (shift+tab to cycle) · ← 4 agents"
    );
    assert_eq!(
        provider_screen_read(TitleProvider::Claude, &quoted),
        (None, None),
    );
}
