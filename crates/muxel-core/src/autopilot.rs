//! Auto-continue: nudge a stalled agent to keep going when its plan isn't done.
//!
//! Agents that lay out a multi-phase plan sometimes finish the first phase and
//! stop, waiting, even though the todo list still has unchecked items. This is the
//! pure, I/O-free brain behind the pane's **Auto** toggle: given what an agent
//! pane is doing and what's on its screen, it decides whether to type `continue`.
//!
//! Everything here is deterministic and unit-tested. The app (`muxel` crate)
//! samples each auto-enabled pane every tick, feeds its status and visible screen
//! to [`AutoContinue::step`], and acts on the returned [`AutoAction`]. No timers,
//! no I/O, no agent-specific coupling beyond the todo-list heuristics below.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

/// What gets typed (then Enter) when the agent is nudged.
pub const AUTO_CONTINUE_MESSAGE: &str = "continue";

/// How many times in a row `continue` may fire without the todo list changing
/// before auto-continue gives up and hands the pane back to the user. This is the
/// guard against a fast failure loop — e.g. an agent that errors out the instant
/// it resumes, pausing again with the same unfinished list, forever.
pub const MAX_NO_PROGRESS_CONTINUES: u32 = 3;

/// Ticks (the app samples ~once a second) to wait before nudging the *same*
/// unchanged screen again — grace for the agent to react to the last nudge before
/// we conclude it did nothing. A nudge is never delayed when the todo list has
/// actually moved; the cooldown only paces retries against a frozen screen.
pub const COOLDOWN_TICKS: u32 = 5;

/// How many consecutive ticks the *whole visible screen* must be unchanged before
/// the pane counts as genuinely idle rather than mid-work.
///
/// This is the real "is it working?" test, and it doesn't depend on any status
/// marker: a working agent repaints every tick — a rotating spinner glyph,
/// streaming output — so its screen is never still, while a paused one is frozen.
/// Waiting for stillness is what stops auto-continue from firing over an agent that
/// is plainly busy but whose "working" marker muxel happened not to recognize.
/// ("Still" ignores digits — see [`stability_digest`] — so a lone ticking counter
/// on an otherwise idle screen doesn't masquerade as work.)
pub const STABLE_TICKS_REQUIRED: u32 = 3;

/// What an agent pane is doing, coarsened from its lifecycle status: the two
/// paused states (idle / finished-a-turn) collapse to one, since auto-continue
/// treats them alike.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PaneActivity {
    /// Generating or running tools — nothing to do but wait.
    Working,
    /// Waiting on a permission/approval prompt. Never auto-continued: it needs a
    /// real yes/no, and `continue` isn't an answer to it.
    Blocked,
    /// Idle or finished a turn — the state a nudge acts on.
    Paused,
}

/// What the app should do with a pane this tick.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AutoAction {
    /// Leave it alone.
    None,
    /// Type [`AUTO_CONTINUE_MESSAGE`] and press Enter.
    Continue,
    /// Auto-continue has disarmed itself: `continue` fired repeatedly without the
    /// todo list moving. The app tells the user and clears the toggle.
    StopStalled,
}

/// Per-pane auto-continue state. Runtime-only (never persisted): arming an agent
/// to keep itself going is not something to silently resume on a fresh launch.
#[derive(Clone, Debug, Default)]
pub struct AutoContinue {
    /// Whether the pane's Auto toggle is on.
    pub enabled: bool,
    /// Hash of the whole screen last tick, to notice any repaint at all.
    last_screen: Option<u64>,
    /// Consecutive ticks the whole screen has been unchanged (see
    /// [`STABLE_TICKS_REQUIRED`]).
    stable_ticks: u32,
    /// Ticks left before the same unchanged screen may be nudged again.
    cooldown: u32,
    /// The todo-list fingerprint at the last nudge. A *change* from this is proof
    /// the agent made progress and earns an immediate re-nudge; an unchanged one
    /// only re-nudges after the cooldown, and counts toward the stall guard.
    last_fingerprint: Option<u64>,
    /// Consecutive nudges fired against an unchanged todo list.
    no_progress: u32,
}

impl AutoContinue {
    /// Turn auto-continue on, ready to fire on the next pause with work left.
    pub fn enable(&mut self) {
        *self = Self {
            enabled: true,
            ..Self::default()
        };
    }

    /// Turn it off and forget everything.
    pub fn disable(&mut self) {
        *self = Self::default();
    }

    /// Drop the per-screen tracking (stillness, cooldown, progress fingerprint,
    /// stall count) while leaving the on/off state untouched.
    ///
    /// Call this whenever the pane's terminal is replaced under it — a restart, or
    /// a remote reattach that replays the old scrollback. Without it, the fresh
    /// screen is compared against the dead terminal's fingerprint and cooldown, so
    /// `continue` can fire again for work the agent already did. After it, the new
    /// terminal is judged from scratch: it must settle, then it nudges at most once
    /// for the current state.
    pub fn rebaseline(&mut self) {
        if self.enabled {
            self.enable();
        }
    }

    /// Decide what to do with the pane this tick, given what it's doing and the
    /// text on its screen. Advances the internal state (stability, cooldown, stall).
    ///
    /// Two ideas do the work, neither leaning on a status marker being recognized:
    ///
    /// - **Is it idle?** — the whole screen must hold still for
    ///   [`STABLE_TICKS_REQUIRED`] ticks. A working agent repaints (spinner,
    ///   elapsed counter) every tick, so it never settles; this is what keeps a
    ///   nudge from landing on a busy agent even when muxel misreads its status.
    /// - **Should it nudge again?** — re-firing keys off the todo list *changing*,
    ///   durable evidence of progress that survives the fast bounces (an agent that
    ///   errors out and re-pauses in under a tick) which slip through the sampling.
    pub fn step(&mut self, activity: PaneActivity, screen: &str) -> AutoAction {
        if !self.enabled {
            return AutoAction::None;
        }
        self.cooldown = self.cooldown.saturating_sub(1);

        // Track screen stillness every tick, whatever the status.
        let screen_hash = stability_digest(screen);
        if self.last_screen == Some(screen_hash) {
            self.stable_ticks = self.stable_ticks.saturating_add(1);
        } else {
            self.stable_ticks = 0;
            self.last_screen = Some(screen_hash);
        }

        // Act only on a pane that is both reported paused (not Working, and never a
        // Blocked permission prompt) AND has stopped repainting — a working agent
        // fails the stillness test even if its status was misclassified.
        if activity != PaneActivity::Paused || self.stable_ticks < STABLE_TICKS_REQUIRED {
            return AutoAction::None;
        }
        if !should_continue(screen) {
            return AutoAction::None; // the plan looks done and it isn't asking
        }

        let fingerprint = progress_fingerprint(screen);
        let progressed = self.last_fingerprint != Some(fingerprint);
        // Same list as the last nudge, still cooling down → give the agent more
        // time to react before trying again.
        if !progressed && self.cooldown > 0 {
            return AutoAction::None;
        }
        if progressed {
            self.no_progress = 0;
        } else {
            self.no_progress += 1;
            // Nudged repeatedly with the list frozen — it's achieving nothing.
            // Stand down and let the user look.
            if self.no_progress > MAX_NO_PROGRESS_CONTINUES {
                self.disable();
                return AutoAction::StopStalled;
            }
        }
        self.last_fingerprint = Some(fingerprint);
        self.cooldown = COOLDOWN_TICKS;
        AutoAction::Continue
    }
}

/// Hash the screen for stillness detection, **ignoring ASCII digits**.
///
/// A paused agent's screen can still carry a live counter — an elapsed-time
/// readout, a "2 shells still running" timer — that ticks every second. Hashing
/// those digits would make the screen look like it's always changing, i.e. always
/// working, so an idle agent would never be nudged (exactly the "phase completed
/// but it didn't continue" case). Digits are the only thing dropped: letters,
/// punctuation, and crucially a rotating spinner glyph are all kept, so genuine
/// activity still reads as activity and a busy agent is still left alone.
fn stability_digest(screen: &str) -> u64 {
    let mut h = DefaultHasher::new();
    for c in screen.chars().filter(|c| !c.is_ascii_digit()) {
        c.hash(&mut h);
    }
    h.finish()
}

/// Whether the screen shows a todo list with unfinished work: an `N pending`
/// summary with N ≥ 1, or at least one empty checkbox. Deliberately narrow — a
/// bare word like "pending" in prose must not trip it, so only the numeric
/// summary and the checkbox glyphs count.
pub fn has_pending_tasks(screen: &str) -> bool {
    if count_before(screen, "pending").is_some_and(|n| n >= 1) {
        return true;
    }
    screen.chars().any(is_empty_checkbox)
}

/// Phrases an agent uses when it *voluntarily* stops mid-task to check in — a
/// "shall I keep going?" moment. Seeing one is reason to nudge even with no todo
/// list on screen, since the agent has plainly parked more work. Matched
/// case-insensitively; kept to strong mid-task signals so a completion sign-off
/// ("all done — let me know if you need anything else") doesn't trip it.
const CHECKPOINT_PHRASES: &[&str] = &[
    "pause here",
    "shall i continue",
    "should i continue",
    "shall i proceed",
    "should i proceed",
    "want me to continue",
    "want me to proceed",
];

/// Whether the agent has stopped to ask (or recommend) whether to keep going —
/// e.g. "My recommendation is to pause here." or "Shall I continue?".
pub fn is_checkpoint_pause(screen: &str) -> bool {
    let lower = screen.to_lowercase();
    CHECKPOINT_PHRASES.iter().any(|p| lower.contains(p))
}

/// Whether an idle pane's screen is one auto-continue should act on: either a
/// todo list with work left, or the agent explicitly checking in about continuing.
fn should_continue(screen: &str) -> bool {
    has_pending_tasks(screen) || is_checkpoint_pause(screen)
}

/// Words whose preceding count changes as a todo list advances, across the
/// summary shapes agents render (`+1 pending, 5 completed`; `5 done, 1 in
/// progress, 5 open`). Any of them moving is progress.
const TALLY_WORDS: &[&str] = &["pending", "completed", "done", "in progress", "open"];

/// A cheap hash of just the parts of the screen that change when the todo list
/// makes progress: the task-summary counts and the tally of empty vs done
/// checkboxes. Blind to spinners, elapsed-time counters and other churn, so an
/// unchanged fingerprint means the plan genuinely didn't move.
pub fn progress_fingerprint(screen: &str) -> u64 {
    let mut h = DefaultHasher::new();
    for word in TALLY_WORDS {
        count_before(screen, word).unwrap_or(0).hash(&mut h);
    }
    screen
        .chars()
        .filter(|c| is_empty_checkbox(*c))
        .count()
        .hash(&mut h);
    screen
        .chars()
        .filter(|c| is_done_checkbox(*c))
        .count()
        .hash(&mut h);
    h.finish()
}

/// Empty (unchecked) todo-box glyphs an agent might render.
fn is_empty_checkbox(c: char) -> bool {
    matches!(c, '☐' | '⬜' | '▢' | '◻' | '◽' | '□')
}

/// Filled/checked todo-box glyphs.
fn is_done_checkbox(c: char) -> bool {
    matches!(c, '☑' | '☒' | '⬛' | '■' | '▣' | '✓' | '✔')
}

/// Parse the number immediately before `word`, e.g. `count_before("+1 pending", "pending") == Some(1)`.
/// Skips whitespace between the number and the word; stops at the first non-digit.
fn count_before(hay: &str, word: &str) -> Option<u32> {
    let idx = hay.find(word)?;
    let digits: String = hay[..idx]
        .chars()
        .rev()
        .skip_while(|c| c.is_whitespace())
        .take_while(|c| c.is_ascii_digit())
        .collect();
    if digits.is_empty() {
        return None;
    }
    digits.chars().rev().collect::<String>().parse().ok()
}

#[cfg(test)]
mod tests {
    use super::{
        AutoAction, AutoContinue, PaneActivity, has_pending_tasks, is_checkpoint_pause,
        progress_fingerprint,
    };

    // A todo panel like the one Claude renders, mid-plan.
    const MID_PLAN: &str = "\
Boogieing…
  ☐ Phase 2: Items, NPC services, alchemy
  ☐ Phase 3: Player economy (trade/stalls)
  … +1 pending, 5 completed
";
    // The same plan, one phase further along.
    const PROGRESSED: &str = "\
Boogieing…
  ☐ Phase 3: Player economy (trade/stalls)
  … +0 pending, 6 completed
";
    const ALL_DONE: &str = "\
Wrapped up.
  ☑ Phase 6: Social extras
  0 pending, 6 completed
";

    #[test]
    fn pending_is_detected_from_count_or_checkbox() {
        assert!(has_pending_tasks(MID_PLAN));
        // Checkbox alone, no summary.
        assert!(has_pending_tasks("  ☐ do the thing"));
        // Numeric summary alone, no glyph.
        assert!(has_pending_tasks("3 pending, 1 completed"));
    }

    #[test]
    fn a_finished_plan_and_bare_prose_do_not_count() {
        assert!(!has_pending_tasks(ALL_DONE));
        assert!(!has_pending_tasks("0 pending, 6 completed"));
        // The word in prose, with no number and no checkbox, must not trip it.
        assert!(!has_pending_tasks("the payment is pending confirmation"));
        assert!(!has_pending_tasks(""));
    }

    #[test]
    fn fingerprint_changes_only_when_the_list_moves() {
        // Spinner text / elapsed timer churn doesn't count as progress.
        let noisy = format!("{MID_PLAN}\n✳ Churned for 9m 26s");
        assert_eq!(progress_fingerprint(MID_PLAN), progress_fingerprint(&noisy));
        // A phase completing does.
        assert_ne!(
            progress_fingerprint(MID_PLAN),
            progress_fingerprint(PROGRESSED)
        );
    }

    /// Hold the pane paused on one unchanging screen until it acts (clearing the
    /// stillness gate), or `None` if it never does within a generous window.
    fn nudge_after_settling(a: &mut AutoContinue, screen: &str) -> AutoAction {
        for _ in 0..super::STABLE_TICKS_REQUIRED + 1 {
            match a.step(PaneActivity::Paused, screen) {
                AutoAction::None => {}
                act => return act,
            }
        }
        AutoAction::None
    }

    #[test]
    fn disabled_does_nothing() {
        let mut a = AutoContinue::default();
        for _ in 0..10 {
            assert_eq!(a.step(PaneActivity::Paused, MID_PLAN), AutoAction::None);
        }
    }

    #[test]
    fn settles_before_the_first_nudge() {
        let mut a = AutoContinue::default();
        a.enable();
        // The screen must hold still first; only then does it fire.
        for _ in 0..super::STABLE_TICKS_REQUIRED {
            assert_eq!(a.step(PaneActivity::Paused, MID_PLAN), AutoAction::None);
        }
        assert_eq!(a.step(PaneActivity::Paused, MID_PLAN), AutoAction::Continue);
        // Same still screen, now cooling down → no repeat.
        assert_eq!(a.step(PaneActivity::Paused, MID_PLAN), AutoAction::None);
    }

    #[test]
    fn a_working_agent_is_never_nudged() {
        // The over-fire bug: the agent is plainly busy — its spinner glyph rotates
        // every tick — but muxel misread the status as Paused and the todo list has
        // pending work. It must NOT nudge, because the screen never holds still.
        // (The rotating glyph is a non-digit change, so it survives the digit-blind
        // stillness test that ignores the elapsed-seconds counter beside it.)
        let spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
        let mut a = AutoContinue::default();
        a.enable();
        for t in 0..30 {
            let g = spinner[t % spinner.len()];
            let screen = format!("{MID_PLAN}\n✳ Implementing… ({t}s) {g}");
            assert_eq!(a.step(PaneActivity::Paused, &screen), AutoAction::None);
        }
        assert!(a.enabled);
    }

    #[test]
    fn an_idle_agent_with_a_ticking_background_timer_is_still_nudged() {
        // The "phase completed but it didn't continue" case: the turn has ended
        // (status Done) with pending tasks, but a background "N shells still running"
        // elapsed counter ticks every second. Only digits change, so the pane counts
        // as still and gets nudged — a live timer must not read as the agent working.
        let mut a = AutoContinue::default();
        a.enable();
        let mut fired = false;
        for t in 10..40 {
            let screen = format!("{MID_PLAN}\nSautéed for 11m {t}s · 2 shells still running");
            if a.step(PaneActivity::Paused, &screen) == AutoAction::Continue {
                fired = true;
                break;
            }
        }
        assert!(
            fired,
            "an idle agent behind a ticking timer should still be nudged"
        );
    }

    #[test]
    fn a_permission_prompt_is_never_answered() {
        // A Blocked prompt sits perfectly still, so it clears the stillness gate —
        // but `continue` is not a yes/no, so it must never be typed at one.
        let mut a = AutoContinue::default();
        a.enable();
        for _ in 0..super::STABLE_TICKS_REQUIRED + 5 {
            assert_eq!(a.step(PaneActivity::Blocked, MID_PLAN), AutoAction::None);
        }
    }

    #[test]
    fn refires_after_progress_but_only_once_the_new_state_settles() {
        // Fixes "it only typed continue once": after a nudge the agent completes a
        // phase and re-pauses, without muxel ever catching a Working sample. The
        // changed todo list earns another nudge — once the new screen has settled.
        let mut a = AutoContinue::default();
        a.enable();
        assert_eq!(nudge_after_settling(&mut a, MID_PLAN), AutoAction::Continue);
        assert_eq!(
            nudge_after_settling(&mut a, PROGRESSED),
            AutoAction::Continue
        );
    }

    #[test]
    fn a_checkpoint_pause_is_continued_even_without_a_todo_list() {
        // The agent voluntarily stopped to check in.
        assert!(is_checkpoint_pause(
            "My recommendation is to pause here.\nfollow-up ideas to consider…"
        ));
        assert!(is_checkpoint_pause("Done with that. Shall I continue?"));
        assert!(is_checkpoint_pause("WANT ME TO PROCEED with the refactor?"));
        // Ordinary prose (and a completion sign-off) must not trip it.
        assert!(!is_checkpoint_pause(
            "This section is about something else."
        ));
        assert!(!is_checkpoint_pause(
            "All phases complete. Let me know if you need anything."
        ));

        let mut a = AutoContinue::default();
        a.enable();
        let screen = "My recommendation is to pause here.\nfollow-up ideas…";
        assert_eq!(nudge_after_settling(&mut a, screen), AutoAction::Continue);
    }

    #[test]
    fn rebaseline_re_arms_a_replaced_terminal_without_flipping_the_toggle() {
        let mut a = AutoContinue::default();
        a.enable();
        // Settle and fire once, leaving it mid-cooldown with a remembered fingerprint.
        assert_eq!(nudge_after_settling(&mut a, MID_PLAN), AutoAction::Continue);

        // The terminal is replaced (a remote reattach). Re-baseline: it must forget
        // the old screen and start settling the new one from scratch — so the first
        // few ticks on the replayed scrollback do NOT immediately re-fire.
        a.rebaseline();
        assert!(a.enabled, "the Auto toggle must stay on across a reattach");
        for _ in 0..super::STABLE_TICKS_REQUIRED {
            assert_eq!(a.step(PaneActivity::Paused, MID_PLAN), AutoAction::None);
        }
        // Then exactly one nudge for the reattached state.
        assert_eq!(a.step(PaneActivity::Paused, MID_PLAN), AutoAction::Continue);

        // On a pane where Auto is off, re-baselining leaves it off.
        let mut off = AutoContinue::default();
        off.rebaseline();
        assert!(!off.enabled);
    }

    #[test]
    fn a_finished_plan_is_left_alone() {
        let mut a = AutoContinue::default();
        a.enable();
        // Settles, but there's nothing pending → never nudged.
        for _ in 0..super::STABLE_TICKS_REQUIRED + 5 {
            assert_eq!(a.step(PaneActivity::Paused, ALL_DONE), AutoAction::None);
        }
    }

    #[test]
    fn a_frozen_list_disarms_after_the_stall_limit() {
        let mut a = AutoContinue::default();
        a.enable();
        // A hung agent: the same unfinished screen, unchanging, forever. It settles,
        // nudges a bounded number of times (paced by the cooldown), then gives up.
        let mut continues = 0;
        let mut stopped = false;
        for _ in 0..1000 {
            match a.step(PaneActivity::Paused, MID_PLAN) {
                AutoAction::Continue => continues += 1,
                AutoAction::StopStalled => {
                    stopped = true;
                    break;
                }
                AutoAction::None => {} // settling or cooling down
            }
        }
        assert!(stopped, "auto-continue never gave up on a frozen list");
        // One initial nudge (progress unknown) + MAX_NO_PROGRESS_CONTINUES that saw
        // no movement, then it stops.
        assert_eq!(continues, 1 + super::MAX_NO_PROGRESS_CONTINUES);
        assert!(!a.enabled, "it should turn itself off when it gives up");
    }

    #[test]
    fn steady_progress_never_stalls() {
        let mut a = AutoContinue::default();
        a.enable();
        // A different list each pause: keeps nudging, never trips the stall guard.
        for i in 0..12 {
            let screen = if i % 2 == 0 { MID_PLAN } else { PROGRESSED };
            assert_eq!(nudge_after_settling(&mut a, screen), AutoAction::Continue);
        }
        assert!(a.enabled);
    }
}
