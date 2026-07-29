# Agent lifecycle, attention, and recency

## Reader

Muxel maintainers implementing or reviewing agent-status detection and its sidebar UI.

## Problem

Muxel hosts long-lived agent terminals. A pane may remain open for days, work for
minutes while unattended, then sit idle overnight. The sidebar must answer three
different questions without moving rows or filling every row with noise:

1. What is working or blocked now?
2. Which agent finished work that has not been attended?
3. Which idle panes are newly attended or old enough to clean up?

The existing detector combines terminal-screen markers, bell events, recent PTY
output, and process exit. It also latches a reliable Working-to-Idle transition as
Done until the pane is focused. This is a sound base, but its timestamps are not
persisted and marker-less agents can mistake echoed typing for work.

PTY means pseudoterminal: the OS interface through which Muxel runs an interactive
CLI and receives its screen-control byte stream. PTY output is useful evidence of
activity, but it does not by itself prove that an agent started or completed work.

## Product decisions

This document supersedes the earlier hook-first proposal in
`D:/dev/muxel-desk/plans/agent-lifecycle-status-bridge.md` for V1 detection.
Later research found provider terminal titles to be the only high-fidelity signal
that is cheap enough and available across ordinary interactive TUIs. Hooks remain
a future option for exact completion/failure events, but their helper packaging,
trust, configuration attachment, and lack of a scoped Grok seam are not justified
before the title experiment matrix is proven.

### Stable tree, informative labels

The project tree never reorders itself by activity. Stable placement is more
valuable than automatic recency sorting in a flight desk with ten or more panes.

V1 changes only the existing row:

- `working`: live work.
- `blocked · 2h`: waiting for user input; age starts when Blocked begins.
- `done · 8h`: completed work not yet attended; Done never expires.
- `idle · 12m`: recently attended; age starts when Done is acknowledged.
- `idle`: ordinary middle age; no extra text.
- `stale · 4d`: an old attended pane that may deserve cleanup.

Initial age buckets:

| State | Age | Label |
| --- | ---: | --- |
| Idle | under 1 minute | `idle` |
| Idle | 1 minute through 59 minutes | `idle · Nm` |
| Idle | 1 hour through 5 hours | `idle · Nh` |
| Idle | 6 hours through 2 days | `idle` |
| Idle | 3 days and older | `stale · Nd` |
| Blocked | any known age | `blocked · Nm/Nh/Nd` |
| Done | any known age | `done · Nm/Nh/Nd` |

The exact timestamp remains available to later tooltips/history. V1 stores it even
if the current Tag component cannot expose a useful tooltip without more UI work.

`ancient` is intentionally deferred. A playful word repeated across an old project
quickly becomes wallpaper. The stored timestamps let us test alternate copy later.

### Done is attention debt; Idle age is time since attention

Done and recency are different concepts.

- A semantic or reliable marker-based completion sets `completed_at` and latches
  Done until attendance. It survives restarts and has no timeout.
- Attending the pane records `last_attended_at`, clears the live terminal latch,
  and starts the visible Idle clock.
- Every deliberate terminal selection refreshes `last_attended_at`, even when no
  completion is pending. This lets ordinary long-lived panes become Stale; it also
  means Stale literally means “not visited,” not “process produced no bytes.”
- Merely having PTY output remains weak runtime evidence. It never creates a
  durable unseen-completion claim.
- Blocked remains Blocked until the detector reports another state. Its age is
  derived from the first transition into Blocked.

V1 preserves Muxel's existing attendance action: explicitly selecting/focusing the
pane. A dwell timer is deferred because it changes every keyboard and programmatic
focus path; durable persistence and correct status signals should land first.

### Evidence must remain honest

Strong signals:

- a provider's semantic OSC terminal title;
- a configured on-screen working or blocked marker;
- terminal bell;
- child-process exit.

Weak signals:

- recent PTY output for a marker-less agent;
- screen redraws, resize output, and echoed user input.

Weak output may support temporary Working display for compatibility. It must not
set `completed_at` or create a Done latch. The UI must not say `finished` when the
only observation was `last output`.

## Provider title contracts

Terminal programs set their title with OSC escape sequences. Muxel already parses
and retains the latest title for pane naming and Codex session identity. Lifecycle
detection adds a separate parser so provider state decorations never leak into the
persisted display name.

### Codex

V1 supports the current Codex semantic title contract (minimum supported version:
0.145.0). Muxel should request the activity/run-state title items at launch when a
stable invocation-local configuration is available. The parser must still detect
capability from title values actually observed; it must not infer correctness from
a version string alone. An unrecognized title falls back to existing markers/bell/
PTY behavior.

Muxel does not install or upgrade Codex. Diagnostics may recommend upgrading when
the expected semantic title contract is absent.

### Claude

Claude 2.1.220 publishes `✳ <session>` while idle, alternates `⠂` and `⠐`
roughly once a second for the full turn, then restores `✳` on completion. Treat
only those confirmed prefixes as semantic state, and expire a working frame after
three seconds in case the process or transport freezes. Strip the prefix before
accepting the remainder as an automatic pane name. Keep the on-screen
`esc to interrupt` marker as corroboration and backward-compatible fallback.

Claude also supports OSC 9;4 terminal progress, controlled by
`terminalProgressBarEnabled`, but emits it only for terminals it recognizes as
supporting the protocol. Muxel currently presents `TERM=xterm-256color`, and the
local trace contained no OSC 9;4 events. Do not lie about terminal identity merely
to enable a redundant signal.

### Grok

Grok 0.2.112 defaults its title items to action-required, spinner, activity,
session-name, and `grok`. Its source holds each of eight braille frames for about
264 ms. A valid trace observed:

```text
grok
⠦ - Waiting for response… - grok
⠹ - Responding - <generated session title> - grok
<generated session title> - grok
```

Treat an exact spinner segment plus freshness as Working, `⚠ Action Required` as
Blocked, and the stable title without a spinner as Idle. Strip the state/activity
segments and trailing `grok` before pane naming. The title config is user-editable,
so an unknown custom shape falls back rather than being guessed. Muxel must not
rewrite the global Grok config.

Grok intentionally blinks the action-required title item while unfocused. Muxel
holds the last positive Blocked edge for 750 ms through that off-frame. A stable
Idle title clears the hold immediately; resolving a prompt can therefore leave no
more than a short Blocked tail before Working resumes.

Grok supports OSC 9;4 too, but its own support matrix disables that channel for
Alacritty/xterm-style terminals. The local Muxel trace therefore correctly had no
progress events while its ordinary OSC title animated.

Provider parsing must be pure and table-tested. Provider-specific syntax belongs
in `muxel-terminal`; the app consumes a provider-neutral observation.

## Data model

Persist one backward-compatible struct on each terminal `Instance`:

```rust
pub struct AgentActivity {
    pub work_started_at: Option<i64>,
    pub completed_at: Option<i64>,
    pub blocked_at: Option<i64>,
    pub last_attended_at: Option<i64>,
}
```

Timestamps are Unix milliseconds in UTC. Millisecond ordering prevents a completion
in the same wall-clock second as attendance from disappearing. They are
timezone-independent and
compare directly. UI formatting uses local time only when an exact tooltip is
shown. Negative ages caused by clock adjustment clamp to zero.

The unseen Done state is derived:

```text
completed_at > last_attended_at
```

There is no persisted `unseen` boolean that can disagree with the timestamps.
Non-terminal panes leave the struct at its default.

Transition rules are pure and live in `muxel-core`:

- entering Working records `work_started_at` for the new run;
- entering Blocked records `blocked_at` once;
- entering Done records `completed_at` once for that completion;
- attending records `last_attended_at` and makes the persisted display Idle;
- returning to Working clears the active Blocked timestamp but retains history;
- PTY-only activity remains a runtime fallback and never records completion.

V1 does not store an event log or run ring buffer. The fields leave room for a
capped history later without forcing replay machinery into ordinary rendering.

## Runtime flow

1. `MuxelListener` records each OSC title change with a monotonically increasing
   generation, rather than exposing only the latest string.
2. `TerminalView` parses unseen title changes into a provider-neutral lifecycle
   observation and combines it with existing marker, bell, output, and exit signals.
3. `MuxelApp::tick` compares the displayed status with its previous status and
   applies one pure transition to the instance's persisted `AgentActivity`.
4. Changed activity data is persisted. Repeated identical one-second samples do
   not write the workspace file.
5. Focusing a Done pane records attendance before clearing the runtime Done latch.
6. Sidebar rendering derives a compact label from status, activity, and current
   time. It schedules no continuous animation; the existing tick is enough for
   minute/hour/day bucket changes.

## Sidebar layout

The status Tag owns its intrinsic width. The title owns only remaining width:

```text
[agent icon] [worktree dot] [title…                 ] [done · 8h]
```

The title container uses `flex_1`, `min_w_0`, hidden overflow, no wrapping, and an
ellipsis. The status Tag uses `flex_none`. A long title must never push the status
out of the sidebar.

Under narrow width the title truncates first. V1 keeps the full status label. If
future labels become materially longer, a later responsive rule may drop age text
while preserving the state word.

## Research probe

The probe is a developer binary in `muxel-terminal`, not product UI. It uses the
same `portable-pty` and Alacritty parser as Muxel so observations match production.

Goals:

- launch Claude, Codex, or Grok in an isolated PTY;
- record timestamped OSC title changes, OSC 9;4 progress state, bell events,
  output-idle boundaries, process exit, and scripted input boundaries;
- avoid recording conversation contents;
- write newline-delimited JSON suitable for replay and comparison;
- support an explicit command/argument list rather than shell interpolation;
- terminate only the child it launched;
- make no global CLI configuration changes.

The probe inherits its process cwd. Leaving `CommandSpec.cwd` unset starts ConPTY
in the user profile on Windows, which can open a provider project-picker instead
of exercising a turn. Initial-prompt scenarios should pass the prompt as one argv
token; scripted typing remains useful for the separate type-without-submit case.

### Evidence captured 2026-07-28

- Claude Code 2.1.220: idle star → alternating two-frame title spinner → idle star.
- Grok 0.2.112: eight-frame title spinner plus activity → stable generated title.
- Codex 0.145.0 with Muxel's invocation-local title config: Ready → Starting/
  Working spinner → Ready.
- None emitted OSC 9;4 under Muxel's current terminal identity. This is expected
  from the Claude and Grok support documentation, not evidence that the protocol
  does not exist.
- The first Claude/Grok traces were invalid: the probe omitted cwd, and the
  Windows batch runner collapsed a one-item argv collection. Both harness faults
  were fixed and regression-tested before the valid traces above were accepted.

Primary references:

- Claude terminal configuration and tmux passthrough:
  https://code.claude.com/docs/en/terminal-config
- Claude `terminalProgressBarEnabled` setting:
  https://code.claude.com/docs/en/settings
- Grok notification, title-item, and terminal support configuration:
  https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/docs/user-guide/05-configuration.md
- Grok title state machine:
  https://github.com/xai-org/grok-build/blob/main/crates/codegen/xai-grok-pager/src/notifications/title.rs

By default title records contain only their leading Unicode code point and character
count. Exact title text is emitted only with `--include-title-text`, for an isolated
harmless test session; provider titles can contain project or task names.

Build and run it as:

```powershell
cargo build -p muxel-terminal --bin agent-title-probe
target/debug/agent-title-probe.exe --log D:/tmp/codex-title.jsonl `
  --include-title-text -- codex --config "tui.terminal_title=['run-state','activity']"
```

Add `--script D:/tmp/probe-actions.json` before `--` for unattended runs. The JSON
is an array of relative-delay actions, for example
`[{"after_ms":5000,"input":"Reply OK.\r"},{"after_ms":15000,"kill":true}]`.
Input text is sent to the provider but only its byte count enters the trace. Keep
scripts under `D:/tmp`; never commit prompts or captured titles.

Probe scenarios per provider:

1. cold launch to ready;
2. type without submitting;
3. submit a short answer;
4. submit a tool-using task;
5. reach a permission/action-required prompt;
6. finish normally;
7. interrupt during work;
8. resume an existing session;
9. resize while idle and while working;
10. exit.
11. finish an agent turn while a spawned server or other background command keeps
    running and repainting the TUI.

For each scenario, compare title observations with visible provider state. Hooks or
structured provider output may be used as laboratory ground truth, but product
status must not depend on global hooks. Captured fixtures are scrubbed before they
enter the repository.

The probe is complete when it can produce a compact trace showing whether each
provider distinguishes Ready, Working, Blocked, Done, and Exit without inspecting
conversation text. In the background-command scenario, a semantic Ready/Idle title
must outrank continuing PTY output: status describes the agent's attention state,
not whether any descendant process is alive. A future `background · N` state needs
structured job-count evidence; provider prose such as `1 command still running`
is not a durable product contract.

## Automated test plan

### `muxel-core`

- old workspaces deserialize with default activity data;
- Working, Blocked, Done, and attendance transitions stamp only intended fields;
- repeated samples do not reset transition timestamps;
- Done survives serialization and derives unseen after reload;
- attendance clears unseen and starts Idle age;
- future timestamps clamp to a zero-age display;
- label boundaries cover 59m/60m, 5h/6h, and 2d/3d;
- Done and Blocked never lose their state word at old ages.

### `muxel-terminal`

- each confirmed provider title fixture maps to the expected neutral observation;
- lifecycle decorations strip cleanly from display titles;
- unrelated user/program titles remain untouched;
- title generation advances for changes and ResetTitle;
- weak PTY Working-to-Idle does not latch Done;
- marker/title semantic Working-to-Idle does latch Done.

### `muxel`

- a status transition updates and persists instance activity once;
- focusing a Done pane records attendance and clears the runtime latch;
- restored unseen completion renders Done before new PTY output;
- title and status remain within a narrow row layout by construction.

## Manual test plan

Use `target/debug/muxel.exe`, never `muxel-live.exe`.

1. Open a project containing Claude, Codex, Grok, a plain shell, and one pane with
   a deliberately long title.
2. Confirm the long title ellipsizes and every status remains right-aligned.
3. Type without submitting in each provider. Codex and Grok must not become Done;
   plain-shell PTY activity may briefly show Working but must return to Idle.
4. Run a short task. Confirm Working appears during the run and Done remains after
   completion while another pane is focused.
5. Restart Muxel without attending that pane. Confirm Done and its age survive.
6. Focus the Done pane. Confirm it becomes Idle and its recent age starts then.
7. Leave or fixture an attended pane beyond the recent window. Confirm ordinary
   medium-age Idle has no suffix and 3-day Idle reads `stale · 3d`.
8. Trigger a provider permission prompt. Confirm Blocked and its age remain until
   the prompt is resolved.
9. Resize the sidebar and terminal repeatedly. Confirm no false Done transition.
10. Verify browser/editor/diff panes are unchanged.

## Build gate

Run in the feature worktree:

```powershell
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo build -p muxel
```

If the hosted shell stalls in the native Whisper build, stop only that build tree
and have Chris run the remaining command in `D:/dev/muxel` after merging the feature
into `next`. A stalled build is not a pass.

## V2, deliberately deferred

- Next unattended agent command.
- Recent Agents palette sorted independently of the stable tree.
- Project-level blocked/unseen rollups.
- Capped run history and overnight digest.
- Per-provider capability diagnostics and update guidance.
- Configurable stale thresholds or playful labels such as `ancient`.
- Dwell-based attendance if accidental focus proves to clear Done too easily.

V1 succeeds if the fixed tree reliably shows current work, durable unattended
completion, recent attendance, and genuinely stale panes without false claims.
