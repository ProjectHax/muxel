//! The app's side of `muxel ctl`: answering each outside-control request from the
//! live workspace. The transport is `crate::control`; the protocol, and every
//! decision that doesn't need the app (resolving names, reading a question off a
//! screen, telling when a turn is over), is `muxel_core::control`.
//!
//! Requests are answered on the UI thread from what is already in memory. The
//! slow part of `show` and `screen` — reading a transcript, asking tmux for its
//! scrollback — is gathered here and run on the background executor, as is the
//! check, before typing into an agent, that muxel on another computer sharing its
//! tmux session isn't mid-turn with it (see [`claim_agent`]).

use super::*;
use muxel_core::ReadAloudScope;
use muxel_core::control::{self as ctl, AgentEntry, AgentInfo, AgentState, Command, ErrorCode};
use muxel_core::readaloud;
use serde_json::{Value, json};

/// What a request comes to: its reply, or why there is none.
type CtlResult<T> = Result<T, ctl::Error>;

mod approval;
mod new;
pub(super) use approval::PreparedCreation;
#[cfg(test)]
#[path = "control_api/new_tests.rs"]
mod new_tests;

/// Shared command permission for typing into an existing terminal. This is
/// rechecked on every write, including panes created before the opt-in was revoked.
fn control_program_writable(inst: &Instance, allow_shells: bool) -> Result<(), String> {
    // Existing panes may run remotely. Match the provider name without
    // resolving local executables on the UI thread.
    if !allow_shells
        && (inst.literal_argv || !muxel_core::is_known_agent_program(inst.program.as_deref()))
    {
        let name = inst.display_name();
        return Err(format!(
            "typing into '{name}' requires command execution permission in Settings > \
             Grok Bot > \"Also allow typing into shell panes\""
        ));
    }
    Ok(())
}

/// A task queued by outside creation, or a prompt/answer typed through `ctl`.
/// Runtime-only ownership tells `wait` when submission is still pending and
/// which submitted turn it is waiting for.
pub(super) struct ControlTurn {
    prompt: Option<String>,
    sent_at: i64,
    /// How long `wait` expects the agent to take to show it started
    /// (`REPLY_GRACE_MS` for a prompt, `ANSWER_GRACE_MS` for an answer).
    grace_ms: i64,
    startup: Option<ctl::StartupTurn>,
}

impl ControlTurn {
    fn startup_error(&self) -> Option<String> {
        self.startup
            .as_ref()
            .and_then(ctl::StartupTurn::failure)
            .map(|failure| failure.message().to_owned())
    }

    fn startup(prompt: String) -> Self {
        Self {
            prompt: Some(prompt),
            sent_at: 0,
            grace_ms: ctl::REPLY_GRACE_MS,
            startup: Some(ctl::StartupTurn::default()),
        }
    }

    fn awaiting_reply(
        &self,
        status: Option<AgentState>,
        activity: &muxel_core::AgentActivity,
        now: i64,
    ) -> bool {
        match &self.startup {
            Some(startup) => startup.awaiting_reply(status, activity, now),
            None => ctl::awaiting_reply(Some(self.sent_at), self.grace_ms, status, activity, now),
        }
    }
}

impl Drop for ControlTurn {
    fn drop(&mut self) {
        if let Some(startup) = &self.startup {
            startup.cancel();
        }
    }
}

/// [`muxel_core::launches_known_agent`] for `instance`'s program, through the
/// `PATH` it would be found by: its preset's own, else muxel's.
fn launches_known_agent(instance: &Instance) -> bool {
    let path = instance
        .env
        .iter()
        .rev()
        .find(|var| var.key.eq_ignore_ascii_case("PATH"))
        .map(|var| std::ffi::OsString::from(&var.value))
        .or_else(|| std::env::var_os("PATH"));
    muxel_core::launches_known_agent(instance.program.as_deref(), path.as_deref(), launchable)
}

/// Whether a PATH search would launch `candidate`: an executable file, or on
/// Windows one with an extension it tries.
fn launchable(candidate: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(candidate)
            .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        ["", ".exe", ".cmd", ".bat", ".com"].iter().any(|ext| {
            let mut path = candidate.as_os_str().to_owned();
            path.push(ext);
            Path::new(&path).is_file()
        })
    }
}

/// Apply outside creation's launch policy after all program/transport wrappers.
/// A missing startup owner must not silently become ordinary runner automation.
pub(super) fn control_launch_spec(
    instance: Option<&Instance>,
    turn: Option<&ControlTurn>,
    owner: Option<ctl::StartupTurnOwner>,
    mut spec: CommandSpec,
) -> CommandSpec {
    spec.allow_shell_fallback &= !instance.is_some_and(|instance| instance.control_created);
    let owner = owner.filter(ctl::StartupTurnOwner::is_pending);
    // Keyed on the task, not on `control_created`: a duplicate drops that
    // approval marker, and must still never type a task nobody owns.
    if owner.is_none()
        && instance
            .is_some_and(|instance| instance.control_prompt && instance.system_prompt.is_some())
    {
        if let Some(startup) = turn.and_then(|turn| turn.startup.as_ref()) {
            startup.cancel();
        }
        // Runtime ownership cannot survive a workspace/app restart. A task
        // cancelled before installation must not replay from saved metadata.
        spec.startup_input = None;
        spec.auto_mode_presses = 0;
        spec.submit = false;
    }
    spec.startup_turn = owner;
    spec
}

/// A reply ready now, work to finish off the UI thread first, or typing to do
/// once any other muxel's claim on the agent has been checked.
enum Answer {
    Now(CtlResult<Value>),
    Later(Box<dyn FnOnce() -> CtlResult<Value> + Send>),
    Type(Box<Typing>),
    Open {
        project: String,
        options: ctl::NewOptions,
    },
}

/// What `send`, `answer` and `keys` type.
enum Input {
    /// Pasted, then submitted with Enter `SUBMIT_DELAY_MS` later.
    Prompt(String),
    /// Raw keys, `KEY_GAP_MS` apart; `true` marks one that submits.
    Keys(Vec<(Vec<u8>, bool)>),
}

/// The turn a piece of typing starts, for `wait` here and for other muxels.
struct Turn {
    /// The prompt, when it is one (an answer keeps the prompt it follows).
    prompt: Option<String>,
    grace_ms: i64,
    /// How long after typing starts the turn does: the prompt's Enter, the last key.
    lead_ms: i64,
}

/// Where an agent's tmux session lives — this machine or its project's SSH host —
/// and so where its claim is kept.
struct ClaimSite {
    loc: integrations::RepoLoc,
    session: muxel_core::tmux::SessionTarget,
}

/// Typing into an agent, already checked against the agent's own state.
struct Typing {
    iid: Uuid,
    input: Input,
    /// `None` for keys that start no turn (`esc`, arrows): no claim either.
    turn: Option<Turn>,
    site: Option<ClaimSite>,
    state: Option<AgentState>,
    activity: AgentActivity,
    /// `send --force`: type even over another muxel's claim.
    force: bool,
    /// `send --launch-id`: checked again immediately before the paste.
    launch_id: Option<Uuid>,
    reply: Value,
}

/// This muxel process, as its claims name it. Random, not secret.
fn owner_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| Uuid::new_v4().simple().to_string()[..12].to_string())
}

/// Shortest wait between writing a claim and reading it back.
const CLAIM_SETTLE_MIN: Duration = Duration::from_millis(200);

/// Matches the startup prompt's pause before its Enter: the agent has taken in
/// the pasted text, so the Enter submits it instead of adding a line to it.
const SUBMIT_DELAY_MS: u64 = 400;
/// Pause between the keys of one `keys` request.
const KEY_GAP_MS: u64 = 80;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

fn kind_name(kind: InstanceKind) -> &'static str {
    match kind {
        InstanceKind::Terminal => "terminal",
        InstanceKind::Editor => "editor",
        InstanceKind::Diff => "diff",
        InstanceKind::Browser => "browser",
    }
}

fn state_name(state: AgentState) -> String {
    serde_json::to_value(state)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

impl MuxelApp {
    /// Start or stop the control server to match the setting. Idempotent; called
    /// each tick (so a settings Cancel is honored too) and by the checkbox.
    pub(super) fn sync_control(&mut self, cx: &mut Context<Self>) {
        self.sync_control_approvals(cx);
        let want = self.settings.control_enabled;
        if want && self.control.is_none() && !self.control_failed {
            let (tx, rx) = async_channel::unbounded();
            match crate::control::Server::start(tx) {
                Ok(server) => {
                    self.control = Some(server);
                    self.control_task = Some(cx.spawn(async move |this: WeakEntity<Self>, cx| {
                        while let Ok(incoming) = rx.recv().await {
                            if this
                                .update(cx, |app, cx| app.answer_control(incoming, cx))
                                .is_err()
                            {
                                break;
                            }
                        }
                    }));
                }
                Err(error) => {
                    // Reported once; unticking and re-ticking the box retries.
                    self.control_failed = true;
                    log::warn!("outside control could not start: {error:#}");
                    self.add_event(
                        NotifKind::Error,
                        t("Outside control"),
                        format!("{}: {error:#}", t("Couldn't start")),
                    );
                }
            }
        } else if !want {
            self.control_failed = false;
            if self.control.is_some() {
                self.control = None;
                self.control_task = None;
            }
        }
    }

    // --- Settings → Grok Bot --------------------------------------------------------

    /// Settings → Grok Bot: setting Grok Bot (or any agent that can run commands
    /// on this computer) up to drive muxel, step by step — turn outside control on,
    /// let Grok Bot run commands here, give it the skill, check it works.
    pub(super) fn render_settings_grok_bot(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let (muted, border, radius) = (theme.muted_foreground, theme.border, theme.radius);
        let (accent, accent_fg) = (theme.primary, theme.primary_foreground);
        let (success, danger, well) = (theme.success, theme.danger, theme.muted);
        let mono = theme.mono_font_family.clone();
        let enabled = self.settings.control_enabled;
        let listening = self.control.is_some();
        let exe = crate::control::exe_path();

        let text = |s: SharedString| div().w_full().text_sm().child(s);
        let hint = |s: SharedString| div().w_full().text_xs().text_color(muted).child(s);
        let step = |n: u8, title: SharedString| {
            v_flex()
                .gap_2()
                .p_3()
                .rounded(radius)
                .border_1()
                .border_color(border)
                .child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .child(
                            div()
                                .size(px(20.0))
                                .flex_none()
                                .flex()
                                .items_center()
                                .justify_center()
                                .rounded_full()
                                .bg(accent)
                                .text_color(accent_fg)
                                .text_xs()
                                .child(n.to_string()),
                        )
                        .child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .child(title),
                        ),
                )
        };
        // Checkbox labels need a definite width to wrap (see `check_row`): the
        // column, less the step's padding and the checkbox.
        let label_w = {
            let column = if self.settings_pane_w < px(560.0) {
                self.settings_pane_w
            } else {
                px(560.0)
            };
            let w = column - px(24.0 + 28.0);
            if w < px(180.0) { px(180.0) } else { w }
        };
        let check = |checkbox: Checkbox, label: SharedString| {
            div()
                .flex()
                .items_start()
                .gap_2()
                .child(checkbox)
                .child(div().w(label_w).text_sm().child(label))
        };

        let (status_color, status) = if listening {
            (
                success,
                t(
                    "On. Only programs running as you on this computer can connect; nothing is reachable from the network.",
                ),
            )
        } else if enabled && self.control_failed {
            (
                danger,
                t("On, but muxel couldn't start listening. Untick and tick the box to retry."),
            )
        } else if enabled {
            (muted, t("Starting…"))
        } else {
            (
                muted,
                t("Off. Grok Bot can't reach muxel until this is on."),
            )
        };

        let test_result = match &self.settings_ui.grok_test {
            RemoteTestState::Idle => None,
            RemoteTestState::Testing => Some(hint(t("Testing…"))),
            RemoteTestState::Ok(msg) => Some(
                div()
                    .w_full()
                    .text_xs()
                    .text_color(success)
                    .child(format!("✓ {msg}")),
            ),
            RemoteTestState::Failed(msg) => Some(
                div()
                    .w_full()
                    .text_xs()
                    .text_color(danger)
                    .child(format!("✗ {msg}")),
            ),
        };

        let preview = self.settings_ui.grok_skill_preview.then(|| {
            div()
                .id("grok-skill-preview")
                .max_h(px(300.0))
                .overflow_y_scroll()
                .p_2()
                .rounded(radius)
                .border_1()
                .border_color(border)
                .bg(well)
                .font_family(mono.clone())
                .text_xs()
                .child(ctl::reflow_for_display(&ctl::skill(&exe)))
        });

        v_flex()
            .gap_3()
            .max_w(px(560.0))
            .child(text(t(
                "Grok Bot, xAI's agent app, can work with the agents you run in muxel: list your projects and agents, see which are working or waiting on you, read their replies, send them prompts and answer their questions. It does this by running the muxel ctl command on this computer. Set it up in four steps.",
            )))
            .child(
                step(1, t("Turn on outside control"))
                    .child(check(
                        Checkbox::new("grok-control")
                            .checked(enabled)
                            .on_click(cx.listener(|this, c: &bool, _w, cx| {
                                this.set_control_enabled(*c, cx)
                            })),
                        t("Allow outside tools to control muxel"),
                    ))
                    .child(self.settings_label(&t("Outside terminal creation"), cx))
                    .child(div().flex().gap_2().children([
                        (ctl::CreationPolicy::Off, t("Off")),
                        (ctl::CreationPolicy::Ask, t("Ask")),
                        (ctl::CreationPolicy::Allow, t("Allow")),
                    ].into_iter().map(|(policy, label)| {
                        Button::new(SharedString::from(format!("grok-control-new-{policy:?}")))
                            .ghost().label(label).selected(self.settings.control_allow_new == policy)
                            .disabled(!enabled)
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.settings.control_allow_new = policy;
                                this.persist_settings();
                                this.sync_control_approvals(cx);
                                cx.notify();
                            }))
                    })))
                    .child(hint(t(
                        "Off by default, including upgrades. Ask requires a click for each request. Allow opens presets that run a known agent (Claude, Codex, Grok, opencode, Amp, …) from an absolute path or PATH entry without a click; any other program, such as ssh, wsl or python, and any relative path still Asks. Literal commands and shell presets always Ask and also require the shell/command permission below. Approvals expire after 120 seconds. Turning creation or outside control off refuses all pending requests. Outside control off also stops the control server. Existing panes, admitted launches and typing permissions are unchanged. Toolbar creation is unaffected.",
                    )))
                    .child(check(
                        Checkbox::new("grok-control-shells")
                            .checked(self.settings.control_allow_shells)
                            .disabled(!enabled)
                            .on_click(cx.listener(|this, c: &bool, _w, cx| {
                                this.settings.control_allow_shells = *c;
                                this.persist_settings();
                                cx.notify();
                            })),
                        t("Also allow typing into shell panes. This permits writes to shell and raw-command panes. With terminal creation enabled, it also permits shell presets and all literal argv, including agent argv. Commands run as the host muxel process, outside the caller's sandbox. Configured non-literal agent presets do not need this command grant; presets are trusted configuration, not a sandbox."),
                    ))
                    .child(
                        div()
                            .flex()
                            .items_start()
                            .gap_2()
                            // Level with the first line of the (possibly wrapped) text.
                            .child(
                                div()
                                    .mt(px(4.0))
                                    .size(px(8.0))
                                    .flex_none()
                                    .rounded_full()
                                    .bg(status_color),
                            )
                            .child(div().flex_1().min_w_0().text_xs().text_color(muted).child(status)),
                    ),
            )
            .child(
                step(2, t("Let Grok Bot run commands on this computer"))
                    .child(text(t(
                        "Grok Bot works on a cloud computer unless you allow it to run commands on yours. In Grok Bot, open Settings → General → Bot → Execution on Local Computer and choose \"Ask every time\" (you approve each command, which shows exactly what it will run) or \"Always allow\".",
                    ))),
            )
            .child(
                step(3, t("Give Grok Bot the muxel skill"))
                    .child(text(t(
                        "The skill teaches Grok Bot the muxel commands and the rules for using them: one prompt at a time, wait for the reply, and leave your agents' permission prompts to you unless you've told it otherwise.",
                    )))
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .items_center()
                            .gap_2()
                            .child(
                                Button::new("grok-copy-skill")
                                    .primary()
                                    .icon(IconName::Copy)
                                    .label(t("Copy skill"))
                                    .on_click(cx.listener(|this, _e, _w, cx| this.copy_grok_skill(cx))),
                            )
                            .child(
                                Button::new("grok-preview-skill")
                                    .ghost()
                                    .label(if self.settings_ui.grok_skill_preview {
                                        t("Hide skill")
                                    } else {
                                        t("Show skill")
                                    })
                                    .on_click(cx.listener(|this, _e, _w, cx| {
                                        this.settings_ui.grok_skill_preview =
                                            !this.settings_ui.grok_skill_preview;
                                        cx.notify();
                                    })),
                            )
                            .children(self.settings_ui.grok_skill_copied.then(|| {
                                div()
                                    .text_xs()
                                    .text_color(success)
                                    .child(t("✓ Copied. Paste it into Grok Bot."))
                            })),
                    )
                    .child(hint(t(
                        "In Grok Bot, add a new skill and paste this in. Grok Bot's skills are shared by all your Bots, and one copy works on every computer you run muxel on: it tells Grok Bot how to find muxel on each one.",
                    )))
                    .children(preview),
            )
            .child(
                step(4, t("Check it works"))
                    .child(text(t(
                        "Test runs the same command Grok Bot will, and reports what muxel answered.",
                    )))
                    .child(
                        div().flex().items_center().gap_2().child(
                            Button::new("grok-test")
                                .ghost()
                                .icon(IconName::Play)
                                .label(t("Test"))
                                .disabled(!listening)
                                .on_click(cx.listener(|this, _e, _w, cx| this.run_grok_test(cx))),
                        ),
                    )
                    .children(test_result)
                    .child(hint(t(
                        "Then ask Grok Bot something like \"Which of my muxel agents are waiting on me?\"",
                    ))),
            )
            .child(self.settings_label(&t("The muxel command on this computer"), cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .p_2()
                            .rounded(radius)
                            .bg(well)
                            .font_family(mono)
                            .text_xs()
                            .child(format!("{} ctl", muxel_core::ssh::sh_quote(&exe))),
                    )
                    .child(
                        Button::new("grok-copy-exe")
                            .ghost()
                            .small()
                            .icon(IconName::Copy)
                            .tooltip(t("Copy"))
                            .on_click(cx.listener(|_this, _e, _w, cx| {
                                let exe = crate::control::exe_path();
                                cx.write_to_clipboard(ClipboardItem::new_string(exe));
                            })),
                    ),
            )
            .child(
                div().flex().child(
                    Button::new("grok-docs")
                        .ghost()
                        .small()
                        .icon(IconName::ExternalLink)
                        .label(t("Grok Bot help"))
                        .on_click(cx.listener(|_this, _e, _w, cx| {
                            cx.open_url("https://docs.x.ai/grok-bot");
                        })),
                ),
            )
            .into_any_element()
    }

    fn copy_grok_skill(&mut self, cx: &mut Context<Self>) {
        let skill = ctl::skill(&crate::control::exe_path());
        cx.write_to_clipboard(ClipboardItem::new_string(skill));
        self.settings_ui.grok_skill_copied = true;
        cx.notify();
    }

    /// Settings → Grok Bot → Test: run this muxel's own `muxel ctl projects`, as
    /// Grok Bot will, and show what came back.
    fn run_grok_test(&mut self, cx: &mut Context<Self>) {
        self.settings_ui.grok_test = RemoteTestState::Testing;
        cx.notify();
        let exe = crate::control::exe_path();
        cx.spawn(async move |this: WeakEntity<Self>, cx| {
            let shown = exe.clone();
            let result = cx
                .background_executor()
                .spawn(async move { integrations::run_muxel_ctl(&exe, &["projects"]) })
                .await;
            let state = match result {
                Err(error) => RemoteTestState::Failed(format!("couldn't run {shown}: {error}")),
                Ok(out) => {
                    let reply: Value = serde_json::from_slice(&out.stdout).unwrap_or(Value::Null);
                    match reply["projects"].as_array() {
                        Some(projects) => RemoteTestState::Ok(tn(
                            "Grok Bot can reach muxel on {host}: it sees {count} project.",
                            "Grok Bot can reach muxel on {host}: it sees {count} projects.",
                            projects.len(),
                            &[
                                ("host", reply["host"].as_str().unwrap_or("this computer")),
                                ("count", &projects.len().to_string()),
                            ],
                        )),
                        None => RemoteTestState::Failed(
                            reply["error"]
                                .as_str()
                                .map(str::to_string)
                                .unwrap_or_else(|| {
                                    String::from_utf8_lossy(&out.stderr).trim().to_string()
                                }),
                        ),
                    }
                }
            };
            let _ = this.update(cx, |app, cx| {
                app.settings_ui.grok_test = state;
                cx.notify();
            });
        })
        .detach();
    }

    pub(super) fn set_control_enabled(&mut self, on: bool, cx: &mut Context<Self>) {
        self.settings.control_enabled = on;
        self.control_failed = false;
        self.persist_settings();
        self.sync_control(cx);
        cx.notify();
    }

    fn answer_control(&mut self, incoming: crate::control::Incoming, cx: &mut Context<Self>) {
        let crate::control::Incoming { command, reply } = incoming;
        let respond = |result: CtlResult<Value>| match result {
            Ok(value) => ctl::Response::ok(value),
            Err(error) => ctl::Response::err(error),
        };
        match self.control_command(command, cx) {
            Answer::Open { project, options } => {
                self.admit_control_new(project, options, reply, cx);
            }
            Answer::Now(result) => {
                let _ = reply.send(respond(result));
            }
            Answer::Later(job) => {
                cx.background_executor()
                    .spawn(async move {
                        let _ = reply.send(respond(job()));
                    })
                    .detach();
            }
            Answer::Type(mut typing) => {
                let Some((site, turn)) = typing.site.take().zip(typing.turn.as_ref()) else {
                    // No tmux session (so nothing to share), or keys that start no turn.
                    let pressed = self.control_press(*typing, None, cx);
                    cx.spawn(async move |_this: WeakEntity<Self>, _cx| {
                        let _ = reply.send(respond(pressed.await));
                    })
                    .detach();
                    return;
                };
                let (grace_ms, lead_ms) = (turn.grace_ms, turn.lead_ms);
                let site = Arc::new(site);
                let (state, activity, force) =
                    (typing.state, typing.activity.clone(), typing.force);
                let can_force = matches!(typing.input, Input::Prompt(_));
                cx.spawn(async move |this: WeakEntity<Self>, cx| {
                    let claim_site = site.clone();
                    let claimed = cx
                        .background_executor()
                        .spawn(async move {
                            claim_agent(
                                &claim_site,
                                grace_ms,
                                lead_ms,
                                force,
                                can_force,
                                state,
                                &activity,
                            )
                        })
                        .await;
                    let result = match claimed {
                        Ok(claim) => {
                            let pressed = this.update(cx, |app, cx| {
                                app.control_press(*typing, Some(claim.at), cx)
                            });
                            let sent = match pressed {
                                Ok(pressed) => pressed.await,
                                Err(_) => Err("muxel is shutting down".into()),
                            };
                            if sent.is_err() {
                                cx.background_executor()
                                    .spawn(async move { release_claim(&site, &claim) })
                                    .await;
                            }
                            sent
                        }
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(respond(result));
                })
                .detach();
            }
        }
    }

    fn control_command(&mut self, command: Command, cx: &mut Context<Self>) -> Answer {
        self.sync_control_approvals(cx);
        if let Command::NewStatus { request_id } = &command {
            return Answer::Now(self.control_approvals.status(*request_id));
        }
        if matches!(
            &command,
            Command::New { .. }
                | Command::Send { .. }
                | Command::Answer { .. }
                | Command::Keys { .. }
        ) && !self.settings.control_enabled
        {
            return Answer::Now(Err(ctl::creation_refusal(ErrorCode::ControlDisabled)));
        }
        if matches!(&command, Command::New { .. })
            && self.settings.control_allow_new == ctl::CreationPolicy::Off
        {
            return Answer::Now(Err(ctl::creation_refusal(ErrorCode::CreationDisabled)));
        }
        if self.current_workspace.is_none() {
            return Answer::Now(Err(
                "no workspace is open in muxel yet: pick one in its workspace selector".into(),
            ));
        }
        let live: HashSet<Uuid> = self.workspace.instances.iter().map(|i| i.id).collect();
        self.control_turns.retain(|id, _| live.contains(id));
        let now = now_ms();
        match command {
            Command::New { project, options } => Answer::Open { project, options },
            Command::NewStatus { .. } => unreachable!("handled before workspace lookup"),
            Command::Projects => Answer::Now(Ok(self.control_projects(cx))),
            Command::Panes { project } => {
                Answer::Now(self.control_panes(project.as_deref(), now, cx))
            }
            Command::Status { agent: None } => {
                let agents: Vec<AgentInfo> = self
                    .control_instances()
                    .into_iter()
                    .map(|inst| self.control_info(inst, now, cx))
                    .collect();
                Answer::Now(Ok(json!({ "agents": agents })))
            }
            Command::Status { agent: Some(agent) } => Answer::Now(
                self.control_resolve(&agent)
                    .and_then(|iid| self.control_info_of(iid, now, cx))
                    .map(|info| json!(info)),
            ),
            Command::Show { agent, full } => match self.control_resolve(&agent) {
                Ok(iid) => self.control_show(iid, full, now, cx),
                Err(error) => Answer::Now(Err(error)),
            },
            Command::Screen { agent, lines } => match self.control_resolve(&agent) {
                Ok(iid) => self.control_screen(iid, lines, cx),
                Err(error) => Answer::Now(Err(error)),
            },
            Command::Send {
                agent,
                text,
                force,
                launch_id,
            } => match self.control_resolve(&agent) {
                Ok(iid) => typed(self.control_send(iid, text, force, launch_id, cx)),
                Err(error) => Answer::Now(Err(error)),
            },
            Command::Answer { agent, option } => typed(
                self.control_resolve(&agent)
                    .and_then(|iid| self.control_answer(iid, &option, cx)),
            ),
            Command::Keys { agent, keys } => typed(
                self.control_resolve(&agent)
                    .and_then(|iid| self.control_keys(iid, &keys, cx)),
            ),
        }
    }

    fn control_new_destination(&self, query: &str) -> Result<Uuid, String> {
        let named: Vec<_> = self
            .workspace
            .projects
            .iter()
            .map(|p| (p.id, p.name.as_str()))
            .collect();
        ctl::resolve_project(query, &named, self.workspace.active_project)
    }

    // --- What is there ---------------------------------------------------------

    /// Every pane of every project: each project's layout in order, then any of
    /// its panes popped out into their own windows.
    fn control_instances(&self) -> Vec<&Instance> {
        let mut out: Vec<&Instance> = Vec::new();
        for project in &self.workspace.projects {
            for iid in project.instances() {
                if let Some(inst) = self.workspace.instance(iid) {
                    out.push(inst);
                }
            }
            for inst in &self.workspace.instances {
                if inst.project_id == project.id
                    && self.popouts.contains_key(&inst.id)
                    && !out.iter().any(|o| o.id == inst.id)
                {
                    out.push(inst);
                }
            }
        }
        out
    }

    fn control_resolve(&self, query: &str) -> CtlResult<Uuid> {
        let instances = self.control_instances();
        let entries: Vec<AgentEntry> = instances
            .iter()
            .map(|inst| AgentEntry {
                id: inst.id,
                name: inst.display_name(),
                project: self
                    .workspace
                    .project(inst.project_id)
                    .map_or("", |p| p.name.as_str()),
            })
            .collect();
        ctl::resolve_agent(query, &entries, self.active_instance).map_err(Into::into)
    }

    /// What a terminal pane is doing. `None` for editors, diffs and browsers.
    fn control_state(&self, inst: &Instance, cx: &App) -> Option<AgentState> {
        if inst.kind != InstanceKind::Terminal {
            return None;
        }
        let id = inst.id;
        Some(if self.failed_launches.contains_key(&id) {
            AgentState::Failed
        } else if let Some(view) = self.terminal_view(id) {
            let view = view.read(cx);
            if self.reconnecting.contains_key(&id) {
                AgentState::Starting
            } else if view.exited() {
                AgentState::Exited
            } else {
                // The status the sidebar shows (refreshed each tick).
                match self
                    .last_status
                    .get(&id)
                    .copied()
                    .unwrap_or_else(|| view.status())
                {
                    AgentStatus::Working => AgentState::Working,
                    AgentStatus::Idle => AgentState::Idle,
                    AgentStatus::Blocked => AgentState::Blocked,
                    AgentStatus::Done => AgentState::Done,
                }
            }
        } else if self.terminal_launching.contains_key(&id) {
            AgentState::Starting
        } else {
            AgentState::NotRunning
        })
    }

    fn control_pane_numbers(&self, pid: Uuid) -> HashMap<Uuid, usize> {
        self.workspace
            .project(pid)
            .and_then(|p| p.layout.as_ref())
            .map(|layout| ctl::describe_layout(layout).1)
            .unwrap_or_default()
    }

    fn control_info_of(&self, iid: Uuid, now: i64, cx: &App) -> CtlResult<AgentInfo> {
        let inst = self
            .workspace
            .instance(iid)
            .ok_or("that agent has just closed")?;
        Ok(self.control_info(inst, now, cx))
    }

    fn control_info(&self, inst: &Instance, now: i64, cx: &App) -> AgentInfo {
        let pane = self
            .control_pane_numbers(inst.project_id)
            .get(&inst.id)
            .copied();
        self.control_info_in(inst, pane, now, cx)
    }

    fn control_info_in(
        &self,
        inst: &Instance,
        pane: Option<usize>,
        now: i64,
        cx: &App,
    ) -> AgentInfo {
        let project = self.workspace.project(inst.project_id);
        let status = self.control_state(inst, cx);
        let preset = inst
            .preset_id
            .and_then(|id| self.presets.iter().find(|p| p.id == id))
            .or_else(|| self.presets.iter().find(|p| p.name == inst.preset));
        let model_flag = preset
            .and_then(|p| p.model_flag.clone())
            .unwrap_or_else(|| "--model".to_string());
        let root = project.map(|p| match &p.remote {
            Some(remote) => remote.remote_root.clone(),
            None => p.root_path.to_string_lossy().into_owned(),
        });
        let cwd = inst
            .working_directory()
            .map(|p| p.to_string_lossy().into_owned())
            .or(root);
        AgentInfo {
            id: ctl::short_id(inst.id),
            uuid: inst.id,
            name: inst.display_name().to_string(),
            project: project.map(|p| p.name.clone()).unwrap_or_default(),
            project_id: ctl::short_id(inst.project_id),
            kind: kind_name(inst.kind).to_string(),
            program: inst.program.clone(),
            preset: inst.preset.clone(),
            model: ctl::flag_value(&inst.args, &model_flag),
            is_agent: inst.kind == InstanceKind::Terminal
                && readaloud::is_agent_program(inst.program.as_deref()),
            status,
            status_since: ctl::status_since(&inst.activity),
            launch_id: ctl::launch_id(
                status,
                self.terminals
                    .get(&inst.id)
                    .map(|view| view.read(cx).session().id),
            ),
            awaiting_reply: self
                .control_turns
                .get(&inst.id)
                .is_some_and(|turn| turn.awaiting_reply(status, &inst.activity, now)),
            startup_error: self
                .control_turns
                .get(&inst.id)
                .and_then(ControlTurn::startup_error),
            focused: self.active_instance == Some(inst.id),
            pane,
            worktree_branch: inst.worktree_branch.clone(),
            cwd,
            remote: project.is_some_and(|p| p.remote.is_some()),
        }
    }

    /// A project's own fields, shared by `projects` and `panes`.
    fn control_project_fields(&self, project: &Project) -> Value {
        let (path, remote) = match &project.remote {
            Some(remote) => (
                remote.remote_root.clone(),
                self.remotes
                    .iter()
                    .find(|h| h.id == remote.host_id)
                    .map(|h| h.name.clone()),
            ),
            None => (project.root_path.to_string_lossy().into_owned(), None),
        };
        json!({
            "id": ctl::short_id(project.id),
            "uuid": project.id,
            "name": project.name,
            "path": path,
            "remote": remote,
            "branch": self.project_branches.get(&project.id).cloned().flatten(),
            "active": self.workspace.active_project == Some(project.id),
        })
    }

    fn control_projects(&self, cx: &App) -> Value {
        let instances = self.control_instances();
        let projects: Vec<Value> = self
            .workspace
            .projects
            .iter()
            .map(|project| {
                let mut counts: std::collections::BTreeMap<String, usize> = Default::default();
                let mut panes = 0;
                for inst in instances.iter().filter(|i| i.project_id == project.id) {
                    panes += 1;
                    if let Some(state) = self.control_state(inst, cx) {
                        *counts.entry(state_name(state)).or_default() += 1;
                    }
                }
                let mut value = self.control_project_fields(project);
                value["panes"] = json!(panes);
                value["status"] = json!(counts);
                value
            })
            .collect();
        json!({ "projects": projects })
    }

    fn control_panes(&self, project: Option<&str>, now: i64, cx: &App) -> CtlResult<Value> {
        let pids: Vec<Uuid> = match project {
            Some(query) => {
                let named: Vec<(Uuid, &str)> = self
                    .workspace
                    .projects
                    .iter()
                    .map(|p| (p.id, p.name.as_str()))
                    .collect();
                vec![ctl::resolve_project(
                    query,
                    &named,
                    self.workspace.active_project,
                )?]
            }
            None => self.workspace.projects.iter().map(|p| p.id).collect(),
        };
        let instances = self.control_instances();
        let mut projects = Vec::new();
        for pid in pids {
            let Some(project) = self.workspace.project(pid) else {
                continue;
            };
            let (layout, panes) = project
                .layout
                .as_ref()
                .map(ctl::describe_layout)
                .unwrap_or_default();
            let agents: Vec<AgentInfo> = instances
                .iter()
                .filter(|i| i.project_id == pid)
                .map(|inst| self.control_info_in(inst, panes.get(&inst.id).copied(), now, cx))
                .collect();
            let mut value = self.control_project_fields(project);
            value["layout"] = layout;
            value["agents"] = json!(agents);
            projects.push(value);
        }
        Ok(json!({
            "focused": self.active_instance.map(ctl::short_id),
            "projects": projects,
        }))
    }

    // --- What an agent said ------------------------------------------------------

    fn control_show(&self, iid: Uuid, full: bool, now: i64, cx: &App) -> Answer {
        let info = match self.control_info_of(iid, now, cx) {
            Ok(info) => info,
            Err(error) => return Answer::Now(Err(error)),
        };
        let sent = self
            .control_turns
            .get(&iid)
            .and_then(|turn| turn.prompt.clone());
        // Not a live terminal: nothing on screen to read.
        let Some(source) = self.reply_source(iid, cx) else {
            return Answer::Now(Ok(json!({
                "agent": info,
                "last_prompt": sent.map(|text| json!({ "text": text, "source": "muxel" })),
                "last_reply": null,
                "question": null,
            })));
        };
        let visible = self
            .terminal_view(iid)
            .map(|view| view.read(cx).visible_text())
            .unwrap_or_default();
        let scope = if full {
            ReadAloudScope::WholeTurn
        } else {
            ReadAloudScope::FinalMessage
        };
        let inst = self.workspace.instance(iid);
        let site = inst.and_then(|inst| self.control_claim_site(inst));
        let activity = inst.map(|inst| inst.activity.clone()).unwrap_or_default();
        Answer::Later(Box::new(move || {
            let mut value = show_value(info, source, &visible, sent, scope);
            value["controller"] = site
                .as_ref()
                .and_then(read_claim)
                .map(|claim| controller_value(&claim, &value, &activity))
                .unwrap_or(Value::Null);
            Ok(value)
        }))
    }

    fn control_screen(&self, iid: Uuid, lines: Option<usize>, cx: &App) -> Answer {
        let lines = lines
            .unwrap_or(ctl::SCREEN_LINES)
            .clamp(1, ctl::SCREEN_LINES_MAX);
        let Some(source) = self.reply_source(iid, cx) else {
            return Answer::Now(Err(
                "that pane isn't a running terminal, so it has no screen".into(),
            ));
        };
        let id = ctl::short_id(iid);
        Answer::Later(Box::new(move || {
            let text = source.screen;
            let kept: Vec<&str> = text.trim_end().lines().collect();
            let tail = kept[kept.len().saturating_sub(lines)..].join("\n");
            Ok(json!({ "agent": id, "text": tail }))
        }))
    }

    // --- Typing into an agent ----------------------------------------------------

    /// The session outside control may type into, or why it may not.
    fn control_writable(&self, iid: Uuid, cx: &App) -> CtlResult<Arc<TerminalSession>> {
        let inst = self
            .workspace
            .instance(iid)
            .ok_or("that agent has just closed")?;
        let name = inst.display_name();
        if inst.kind != InstanceKind::Terminal {
            return Err(format!(
                "'{name}' is {} pane, not a terminal",
                match inst.kind {
                    InstanceKind::Editor => "an editor",
                    InstanceKind::Diff => "a diff",
                    _ => "a browser",
                }
            )
            .into());
        }
        control_program_writable(inst, self.settings.control_allow_shells)?;
        let refuse = |code, message: String| Err(ctl::Error::new(code, message));
        match self.control_state(inst, cx) {
            Some(AgentState::Exited) => {
                return refuse(ErrorCode::Exited, format!("'{name}' has exited"));
            }
            Some(AgentState::Failed) => {
                return refuse(ErrorCode::NotRunning, format!("'{name}' failed to start"));
            }
            Some(AgentState::NotRunning) => {
                return refuse(
                    ErrorCode::NotRunning,
                    format!(
                        "'{name}' isn't running this session: the user has to open it in muxel"
                    ),
                );
            }
            Some(AgentState::Starting) => {
                return refuse(
                    ErrorCode::Starting,
                    format!("'{name}' is still starting: try again in a moment"),
                );
            }
            _ => {}
        }
        self.terminal_view(iid)
            .map(|view| view.read(cx).session().clone())
            .ok_or_else(|| {
                ctl::Error::new(ErrorCode::NotRunning, format!("'{name}' isn't running"))
            })
    }

    /// Where `inst`'s claim lives: its tmux session, on this machine or its host.
    /// `None` without tmux — a plain PTY is this muxel's alone.
    fn control_claim_site(&self, inst: &Instance) -> Option<ClaimSite> {
        if self
            .remote_host_for_instance(inst.id)
            .is_some_and(|host| !host.use_tmux(inst.use_tmux))
        {
            return None;
        }
        Some(ClaimSite {
            session: {
                let tmux = InstanceTmux::capture(inst)?;
                muxel_core::tmux::SessionTarget::new(tmux.recorded_session?, tmux.socket)
            },
            loc: self.repo_loc(inst.project_id)?,
        })
    }

    /// Typing into `iid`, checked against its state, with its claim site.
    fn control_typing(
        &self,
        iid: Uuid,
        input: Input,
        turn: Option<Turn>,
        force: bool,
        reply: Value,
        cx: &App,
    ) -> CtlResult<Typing> {
        let inst = self
            .workspace
            .instance(iid)
            .ok_or("that agent has just closed")?;
        Ok(Typing {
            iid,
            input,
            turn,
            site: self.control_claim_site(inst),
            state: self.control_state(inst, cx),
            activity: inst.activity.clone(),
            force,
            launch_id: None,
            reply,
        })
    }

    fn control_send(
        &mut self,
        iid: Uuid,
        text: String,
        force: bool,
        launch_id: Option<Uuid>,
        cx: &mut Context<Self>,
    ) -> CtlResult<Typing> {
        let session = self.control_writable(iid, cx)?;
        ctl::check_launch_id(launch_id, Some(session.id))?;
        if text.trim().is_empty() {
            return Err("nothing to send".into());
        }
        let state = self
            .workspace
            .instance(iid)
            .and_then(|inst| self.control_state(inst, cx));
        if !force {
            match state {
                Some(AgentState::Working) => {
                    return Err(ctl::Error::new(
                        ErrorCode::Working,
                        "the agent is working: `wait` for it to finish (or pass --force to \
                         type the prompt anyway)",
                    ));
                }
                Some(AgentState::Blocked) => {
                    return Err(ctl::Error::new(
                        ErrorCode::Blocked,
                        "the agent is waiting on a question: see `show`, then `answer` it or \
                         press `keys`",
                    ));
                }
                _ => {}
            }
        }
        let turn = Turn {
            prompt: Some(text.clone()),
            grace_ms: ctl::REPLY_GRACE_MS,
            // The turn starts at the Enter, not the paste: an agent that counts
            // typing echo as work must not look finished before it was sent.
            lead_ms: SUBMIT_DELAY_MS as i64,
        };
        // The paste boundary builds the receipt from the terminal it admits to.
        let typing =
            self.control_typing(iid, Input::Prompt(text), Some(turn), force, Value::Null, cx)?;
        Ok(Typing {
            launch_id,
            ..typing
        })
    }

    fn control_answer(
        &mut self,
        iid: Uuid,
        option: &str,
        cx: &mut Context<Self>,
    ) -> CtlResult<Typing> {
        self.control_writable(iid, cx)?;
        let state = self
            .workspace
            .instance(iid)
            .and_then(|inst| self.control_state(inst, cx));
        if state != Some(AgentState::Blocked) {
            return Err(format!(
                "the agent isn't waiting on a question (it is {}): use `send` for a new prompt",
                state.map(state_name).unwrap_or_default()
            )
            .into());
        }
        let screen = self
            .terminal_view(iid)
            .map(|view| view.read(cx).visible_text())
            .unwrap_or_default();
        let question = ctl::pending_question(&screen).ok_or("couldn't read the question")?;
        let key = option.trim().trim_end_matches(['.', ')']);
        let Some(picked) = question.options.iter().find(|o| o.key == key) else {
            return Err(if question.options.is_empty() {
                "the question has no numbered options: see `show` or `screen`, and use \
                 `keys` (for example `keys AGENT y`)"
                    .into()
            } else {
                let keys: Vec<&str> = question.options.iter().map(|o| o.key.as_str()).collect();
                format!(
                    "there is no option '{key}': pick one of {}",
                    keys.join(", ")
                )
                .into()
            });
        };
        let reply = json!({
            "answered": picked.key,
            "label": picked.label,
            "agent": ctl::short_id(iid),
        });
        let turn = Turn {
            prompt: None,
            grace_ms: ctl::ANSWER_GRACE_MS,
            lead_ms: 0,
        };
        let input = Input::Keys(vec![(key.as_bytes().to_vec(), false)]);
        self.control_typing(iid, input, Some(turn), false, reply, cx)
    }

    fn control_keys(
        &mut self,
        iid: Uuid,
        keys: &[String],
        cx: &mut Context<Self>,
    ) -> CtlResult<Typing> {
        let session = self.control_writable(iid, cx)?;
        let app_cursor = session.is_app_cursor_mode();
        let presses: Vec<(Vec<u8>, bool)> = keys
            .iter()
            .map(|key| {
                ctl::key_bytes(key, app_cursor)
                    .map(|bytes| (bytes, ctl::key_submits(key)))
                    .ok_or_else(|| format!("unknown key '{key}'"))
            })
            .collect::<Result<_, _>>()?;
        let was_blocked = self
            .workspace
            .instance(iid)
            .and_then(|inst| self.control_state(inst, cx))
            == Some(AgentState::Blocked);
        let submits = presses.iter().any(|(_, submit)| *submit);
        let lead_ms = KEY_GAP_MS as i64 * presses.len().saturating_sub(1) as i64;
        // Answering a prompt, or submitting one, starts a turn `wait` should wait
        // for; other keys (esc, arrows) start none.
        let turn = if was_blocked {
            Some(ctl::ANSWER_GRACE_MS)
        } else if submits {
            Some(ctl::REPLY_GRACE_MS)
        } else {
            None
        }
        .map(|grace_ms| Turn {
            prompt: None,
            grace_ms,
            lead_ms,
        });
        let reply = json!({ "pressed": keys, "agent": ctl::short_id(iid) });
        self.control_typing(iid, Input::Keys(presses), turn, false, reply, cx)
    }

    /// Type `typing` into its agent, and note the turn it starts at `at` (when a
    /// claim fixed it) or when its Enter lands. The reply to a prompt comes once
    /// its Enter is written, or withheld.
    fn control_press(
        &mut self,
        typing: Typing,
        at: Option<i64>,
        cx: &mut Context<Self>,
    ) -> Task<CtlResult<Value>> {
        // The pane may have closed while another muxel's claim was checked.
        let session = match self.control_writable(typing.iid, cx) {
            Ok(session) => session,
            Err(error) => return Task::ready(Err(error)),
        };
        let Typing {
            iid,
            turn,
            reply,
            launch_id,
            ..
        } = typing;
        match typing.input {
            Input::Prompt(text) => {
                let reply = match paste_prompt(&session, iid, &text, launch_id) {
                    Ok(reply) => reply,
                    Err(error) => return Task::ready(Err(error)),
                };
                let pasted = session.input_rev();
                cx.spawn(async move |this: WeakEntity<Self>, cx| {
                    cx.background_executor()
                        .timer(Duration::from_millis(SUBMIT_DELAY_MS))
                        .await;
                    this.update(cx, |app, cx| {
                        let live = app.control_writable(iid, cx);
                        enter_verdict(live, &session, pasted, session.input_rev())?;
                        press_prompt_enter(&session, pasted)?;
                        // Claim timestamps predict settlement. Local wait starts
                        // at the actual Enter admission, never at the prediction.
                        app.note_control_turn(iid, turn, now_ms());
                        Ok(reply)
                    })
                    .unwrap_or_else(|_| Err("muxel is shutting down".into()))
                })
            }
            Input::Keys(presses) => {
                cx.spawn(async move |this: WeakEntity<Self>, cx| {
                    for (i, (bytes, submit)) in presses.into_iter().enumerate() {
                        if i > 0 {
                            cx.background_executor()
                                .timer(Duration::from_millis(KEY_GAP_MS))
                                .await;
                        }
                        // Recheck each key after its delay. A restarted pane
                        // must not receive input meant for its previous session.
                        let wrote = this
                            .update(cx, |app, cx| {
                                let Ok(current_session) = app.control_writable(iid, cx) else {
                                    return false;
                                };
                                if !Arc::ptr_eq(&session, &current_session) {
                                    return false;
                                }
                                if submit {
                                    session.mark_turn_submitted();
                                }
                                session.write_input(&bytes);
                                true
                            })
                            .unwrap_or(false);
                        if !wrote {
                            break;
                        }
                    }
                })
                .detach();
                let lead_ms = turn.as_ref().map_or(0, |turn| turn.lead_ms);
                self.note_control_turn(iid, turn, at.unwrap_or_else(|| now_ms() + lead_ms));
                Task::ready(Ok(reply))
            }
        }
    }

    /// Remember the turn a prompt or answer typed through `muxel ctl` started, so
    /// `wait` knows which one it is waiting for.
    fn note_control_turn(&mut self, iid: Uuid, turn: Option<Turn>, sent_at: i64) {
        let Some(turn) = turn else { return };
        let entry = self.control_turns.entry(iid).or_insert(ControlTurn {
            prompt: None,
            sent_at,
            grace_ms: turn.grace_ms,
            startup: None,
        });
        if let Some(startup) = entry.startup.take() {
            startup.cancel();
        }
        entry.sent_at = sent_at;
        entry.grace_ms = turn.grace_ms;
        if turn.prompt.is_some() {
            entry.prompt = turn.prompt;
        }
    }

    pub(super) fn cancel_control_startup(&self, iid: Uuid) {
        if let Some(startup) = self
            .control_turns
            .get(&iid)
            .and_then(|turn| turn.startup.as_ref())
        {
            startup.cancel();
        }
    }

    pub(super) fn claim_control_startup(&self, iid: Uuid) -> Option<ctl::StartupTurnOwner> {
        self.control_turns
            .get(&iid)
            .and_then(|turn| turn.startup.as_ref())?
            .claim()
    }
}

fn typed(typing: CtlResult<Typing>) -> Answer {
    match typing {
        Ok(typing) => Answer::Type(Box::new(typing)),
        Err(error) => Answer::Now(Err(error)),
    }
}

fn enter_verdict<T>(
    live: CtlResult<Arc<T>>,
    pasted_into: &Arc<T>,
    pasted: u64,
    now: u64,
) -> CtlResult<()> {
    match live {
        Ok(live) if Arc::ptr_eq(&live, pasted_into) => {}
        Ok(_) => return Err("the pane restarted after the prompt was pasted, so Enter wasn't pressed: check its `screen`".into()),
        Err(why) => return Err(format!("{why}; the prompt was pasted but Enter wasn't pressed: check its `screen`").into()),
    }
    if now != pasted {
        return Err(ctl::Error::new(
            ErrorCode::InputChanged,
            "local input changed after the prompt was pasted, so Enter wasn't pressed: check its `screen` before sending again",
        ));
    }
    Ok(())
}

fn paste_prompt(
    session: &TerminalSession,
    pane: Uuid,
    text: &str,
    expected_launch: Option<Uuid>,
) -> CtlResult<Value> {
    ctl::check_launch_id(expected_launch, Some(session.id))?;
    session
        .try_paste(text)
        .map_err(|error| format!("couldn't admit the prompt to the PTY writer: {error}"))?;
    Ok(ctl::pasted_prompt_receipt(pane, session.id))
}

fn press_prompt_enter(session: &Arc<TerminalSession>, pasted: u64) -> CtlResult<()> {
    enter_verdict(Ok(session.clone()), session, pasted, session.input_rev())?;
    session.try_write_input(b"\r").map_err(|error| {
        format!(
            "the prompt was pasted but Enter couldn't be admitted ({error}): check its `screen`"
        )
    })?;
    session.mark_turn_submitted();
    Ok(())
}

fn release_claim(site: &ClaimSite, claim: &ctl::Claim) {
    let Some(nonce) = claim.nonce else { return };
    let pattern = ctl::Claim::pattern(&claim.owner, nonce);
    if let Err(error) =
        integrations::unset_tmux_option_if(&site.loc, &site.session, ctl::CLAIM_OPTION, &pattern)
    {
        log::warn!("muxel ctl: couldn't release {}: {error:#}", site.session);
    }
}

fn read_claim(site: &ClaimSite) -> Option<ctl::Claim> {
    integrations::tmux_option(&site.loc, &site.session, ctl::CLAIM_OPTION)
        .and_then(|value| ctl::Claim::decode(&value))
}

/// Before typing into an agent whose tmux session other muxels may share: refuse
/// if another muxel's turn with it is still open, then claim it for this one and
/// read the claim back after a pause, so that when two muxels reach an idle agent
/// at the same moment only the later write types. The pause is twice the read's
/// round trip (so a slower host gets a longer one), but at least
/// [`CLAIM_SETTLE_MIN`]. Returns the claim's unique identity and shared timing
/// estimate. A prompt's local turn uses its actual Enter admission time.
///
/// Best effort: a host whose tmux can't be reached leaves the agent unclaimed
/// rather than untypeable — the pane itself works, or `control_writable` would
/// have refused it.
fn claim_agent(
    site: &ClaimSite,
    grace_ms: i64,
    lead_ms: i64,
    force: bool,
    can_force: bool,
    state: Option<AgentState>,
    activity: &AgentActivity,
) -> CtlResult<ctl::Claim> {
    let busy = |claim: &ctl::Claim, how: &str| {
        let hint = if can_force {
            "`wait` on the agent and try again (or pass --force to send anyway)"
        } else {
            "`wait` on the agent and try again"
        };
        ctl::Error::new(
            ErrorCode::ClaimedElsewhere,
            format!("muxel on {} {how}: {hint}", claim.host),
        )
    };
    let asked = Instant::now();
    let theirs = read_claim(site);
    let settle = (asked.elapsed() * 2).max(CLAIM_SETTLE_MIN);
    if !force
        && let Some(claim) = &theirs
        && ctl::claim_holds(claim, owner_id(), state, activity, now_ms())
    {
        let ago = (now_ms() - claim.at).max(0) / 1000;
        return Err(busy(
            claim,
            &format!("is using this agent (it typed into it {ago}s ago and that turn isn't over)"),
        ));
    }
    let at = now_ms() + settle.as_millis() as i64 + lead_ms;
    let mine = ctl::Claim {
        owner: owner_id().to_string(),
        host: crate::control::local_hostname().to_string(),
        at,
        grace_ms,
        nonce: Some(Uuid::new_v4()),
    };
    if let Err(error) =
        integrations::set_tmux_option(&site.loc, &site.session, ctl::CLAIM_OPTION, &mine.encode())
    {
        log::warn!("muxel ctl: couldn't claim {}: {error:#}", site.session);
        return Ok(ctl::Claim {
            at: now_ms() + lead_ms,
            ..mine
        });
    }
    std::thread::sleep(settle);
    match read_claim(site) {
        Some(claim) if !claim.same_turn(&mine) && !force => Err(busy(
            &claim,
            "started typing into this agent at the same moment",
        )),
        _ => Ok(mine),
    }
}

/// `show`'s `controller`: which muxel last typed into the agent through `muxel
/// ctl`, and whether that turn is still open as far as this muxel can see.
fn controller_value(claim: &ctl::Claim, shown: &Value, activity: &AgentActivity) -> Value {
    let state: Option<AgentState> = serde_json::from_value(shown["agent"]["status"].clone()).ok();
    json!({
        "host": claim.host,
        "at": claim.at,
        "this_muxel": claim.owner == owner_id(),
        "turn_open": ctl::awaiting_reply(Some(claim.at), claim.grace_ms, state, activity, now_ms()),
    })
}

/// `show`'s reply, built off the UI thread: the last prompt and reply — from
/// Claude's transcript while it is the conversation on screen, otherwise from the
/// pane's text — and, for a blocked agent, the question on its screen.
fn show_value(
    info: AgentInfo,
    source: ReplySource,
    visible: &str,
    sent: Option<String>,
    scope: ReadAloudScope,
) -> Value {
    let screen = source.screen;
    let transcript = source
        .transcript
        .as_deref()
        .and_then(|path| read_file_tail(path, READ_ALOUD_TRANSCRIPT_BYTES))
        .filter(|jsonl| {
            // A stale session binding must never answer for another conversation.
            readaloud::reply_from_claude_transcript(jsonl, scope)
                .is_some_and(|reply| readaloud::reply_on_screen(&reply, &screen))
        });
    let (reply, reply_from) = match transcript
        .as_deref()
        .and_then(|jsonl| readaloud::reply_from_claude_transcript(jsonl, scope))
    {
        Some(reply) => (Some(reply), "transcript"),
        None => (readaloud::reply_from_screen(&screen, scope), "screen"),
    };
    let prompt = transcript
        .as_deref()
        .and_then(ctl::prompt_from_claude_transcript)
        .map(|text| (text, "transcript"))
        .or_else(|| readaloud::prompt_from_screen(&screen).map(|text| (text, "screen")))
        .or_else(|| sent.map(|text| (text, "muxel")));
    let question = (info.status == Some(AgentState::Blocked))
        .then(|| ctl::pending_question(visible))
        .flatten();
    json!({
        "agent": info,
        "last_prompt": prompt.map(|(text, from)| json!({ "text": text, "source": from })),
        "last_reply": reply.map(|text| json!({ "text": text, "source": reply_from })),
        "question": question,
    })
}

#[cfg(test)]
mod prompt_submission_tests {
    use super::{enter_verdict, paste_prompt, press_prompt_enter};
    use muxel_core::control::ErrorCode;
    use muxel_terminal::{CommandSpec, TerminalSession};
    use std::sync::Arc;
    use uuid::Uuid;

    #[test]
    fn a_launch_guard_refuses_before_pasting_and_receipts_use_the_admitted_launch() {
        let spec = if cfg!(windows) {
            CommandSpec::program("cmd.exe", vec!["/Q".into(), "/K".into()])
        } else {
            CommandSpec::program("/bin/cat", vec![])
        };
        let (session, _rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn PTY");
        let pane = Uuid::new_v4();
        let before = session.input_rev();
        let refusal = paste_prompt(&session, pane, "prompt", Some(Uuid::new_v4())).unwrap_err();
        assert_eq!(refusal.code, Some(ErrorCode::LaunchMismatch));
        assert_eq!(
            session.input_rev(),
            before,
            "a refused launch must type nothing"
        );
        let receipt = paste_prompt(&session, pane, "prompt", Some(session.id)).unwrap();
        let revision = session.input_rev();
        session.kill();
        assert_ne!(revision, before);
        assert_eq!(receipt["pasted"], true);
        assert_eq!(receipt["launch_id"], session.id.to_string());
    }

    #[test]
    fn a_prompt_enter_is_withheld_after_intervening_input() {
        let spec = if cfg!(windows) {
            CommandSpec::program("cmd.exe", vec!["/Q".into(), "/K".into()])
        } else {
            CommandSpec::program("/bin/cat", vec![])
        };
        let (session, _rx) = TerminalSession::spawn(spec, 80, 24).expect("spawn PTY");
        session.paste("a prompt");
        let pasted = session.input_rev();
        session.write_input(b"x");
        let entered = press_prompt_enter(&session, pasted);
        let submitted = session.has_submitted_turn();
        let revision = session.input_rev();
        session.kill();
        assert!(
            entered.is_err(),
            "intervening input was submitted: {entered:?}"
        );
        assert!(!submitted);
        assert_eq!(revision, pasted + 1, "withholding must not queue Enter");
    }

    #[test]
    fn enter_rejects_closed_or_replaced_panes_and_accepts_unchanged_input() {
        let original = Arc::new(1_u8);
        assert_eq!(enter_verdict(Ok(original.clone()), &original, 7, 7), Ok(()));
        let closed = enter_verdict(Err("closed".into()), &original, 7, 7).unwrap_err();
        assert_eq!(
            closed.code, None,
            "a post-paste failure must not imply nothing was typed"
        );
        let replaced = enter_verdict(Ok(Arc::new(1)), &original, 7, 7).unwrap_err();
        assert_eq!(replaced.code, None);
        let changed = enter_verdict(Ok(original.clone()), &original, 7, 8).unwrap_err();
        assert_eq!(changed.code, Some(ErrorCode::InputChanged));
    }
}
