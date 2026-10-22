//! Admission is short-lived; the human decision is polled over fresh requests.
//! This card has no focus handle or key handlers. Only mouse clicks decide it.

use super::*;
use gpui::prelude::FluentBuilder;

pub(crate) struct PreparedCreation {
    workspace: Option<Uuid>,
    pid: Uuid,
    options: ctl::NewOptions,
    preset: Option<AgentPreset>,
    preview: Instance,
}

/// What a preset pane will run, one argument per line like literal argv.
fn preset_argv(preview: &Instance) -> String {
    let argv: Vec<String> = std::iter::once(
        preview
            .program
            .clone()
            .unwrap_or_else(|| CommandSpec::shell().program),
    )
    .chain(preview.args.iter().cloned())
    .collect();
    serde_json::to_string_pretty(&argv).unwrap_or_default()
}

fn approval_now() -> u64 {
    static START: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();
    START.get_or_init(Instant::now).elapsed().as_millis() as u64
}

impl MuxelApp {
    pub(in crate::app) fn sync_control_approvals(&mut self, cx: &mut Context<Self>) {
        let before = self.control_approvals.len();
        let now = approval_now();
        if !self.settings.control_enabled {
            self.control_approvals
                .refuse_all(ErrorCode::ControlDisabled, now);
        } else if self.settings.control_allow_new == ctl::CreationPolicy::Off {
            self.control_approvals
                .refuse_all(ErrorCode::CreationDisabled, now);
        } else {
            self.control_approvals.expire(now);
        }
        let head = self.control_approvals.front().map(|entry| entry.id);
        if head != self.control_approval_notified {
            self.control_approval_notified = head;
            if let Some(entry) = self.control_approvals.front() {
                notify(
                    t("Allow outside terminal creation?").to_string(),
                    tf(
                        "{pane} in {project} is waiting for your click in muxel.",
                        &[
                            ("pane", entry.payload.preview.display_name()),
                            (
                                "project",
                                self.workspace
                                    .project(entry.payload.pid)
                                    .map(|p| p.name.as_str())
                                    .unwrap_or(""),
                            ),
                        ],
                    ),
                    None,
                );
            }
            cx.notify();
        } else if before != self.control_approvals.len() {
            cx.notify();
        }
    }

    pub(super) fn admit_control_new(
        &mut self,
        query: String,
        mut options: ctl::NewOptions,
        reply: std::sync::mpsc::Sender<ctl::Response>,
        cx: &mut Context<Self>,
    ) {
        self.sync_control_approvals(cx);
        let prepared = (|| -> CtlResult<_> {
            let pid = self.control_new_destination(&query)?;
            let preview = self.prepare_control_new(pid, &options)?;
            let preset = preview
                .preset_id
                .and_then(|id| self.presets.iter().find(|p| p.id == id))
                .cloned();
            if let Some(preset) = &preset {
                options.preset = Some(preset.id.to_string());
            }
            options.cwd = preview.launch_dir.clone();
            Ok(PreparedCreation {
                workspace: self.current_workspace,
                pid,
                options,
                preset,
                preview,
            })
        })();
        let prepared = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                let _ = reply.send(ctl::Response::err(error));
                return;
            }
        };
        match self.creation_decision(&prepared.preview) {
            ctl::CreationDecision::Refuse(code) => {
                let _ = reply.send(ctl::Response::err(ctl::creation_refusal(code)));
            }
            ctl::CreationDecision::Allow => {
                self.defer_control_new(prepared, false, None, Some(reply), cx)
            }
            ctl::CreationDecision::Ask => {
                let result = self
                    .control_approvals
                    .admit(prepared, approval_now())
                    .and_then(|id| self.control_approvals.status(id));
                let response = match result {
                    Ok(value) => ctl::Response::ok(value),
                    Err(error) => ctl::Response::err(error),
                };
                let _ = reply.send(response);
                self.sync_control_approvals(cx);
                cx.notify();
            }
        }
    }

    fn defer_control_new(
        &mut self,
        prepared: PreparedCreation,
        approved: bool,
        request_id: Option<Uuid>,
        reply: Option<std::sync::mpsc::Sender<ctl::Response>>,
        cx: &mut Context<Self>,
    ) {
        let handle = self
            .secondary_windows
            .iter()
            .find(|s| s.pid == prepared.pid)
            .map(|s| s.handle)
            .or(self.main_window);
        let app = cx.weak_entity();
        cx.defer(move |cx| {
            let result: CtlResult<Value> = match handle {
                Some(handle) => handle
                    .update(cx, |_, window, cx| {
                        app.update(cx, |app, cx| {
                            if app.current_workspace != prepared.workspace {
                                return Err("the workspace changed before the pane could open".into());
                            }
                            let unchanged = match &prepared.preset {
                                Some(expected) => app.presets.iter().any(|p| p == expected),
                                None => prepared.preview.literal_argv || app.presets.is_empty(),
                            };
                            if !unchanged {
                                return Err(
                                    "the preset changed before the pane could open; retry the command"
                                        .into(),
                                );
                            }
                            app.control_new(prepared.pid, prepared.options, approved, window, cx)
                        })
                        .unwrap_or_else(|_| Err("muxel is shutting down".into()))
                    })
                    .unwrap_or_else(|_| Err("the destination window has closed".into())),
                None => Err("no muxel window is available".into()),
            };
            let response = match result {
                Ok(value) => ctl::Response::ok(value),
                Err(error) => ctl::Response::err(error),
            };
            if let Some(id) = request_id {
                let _ = app.update(cx, |app, cx| {
                    app.control_approvals
                        .finish(id, response.clone(), approval_now());
                    app.sync_control_approvals(cx);
                    cx.notify();
                });
            }
            if let Some(reply) = reply {
                let _ = reply.send(response);
            }
        });
    }

    fn decide_control_approval(&mut self, id: Uuid, allow: bool, cx: &mut Context<Self>) {
        self.sync_control_approvals(cx);
        if allow {
            if let Some(prepared) = self.control_approvals.take(id, approval_now()) {
                self.defer_control_new(prepared, true, Some(id), None, cx);
            }
        } else {
            self.control_approvals.deny(id, approval_now());
        }
        self.sync_control_approvals(cx);
        cx.notify();
    }

    pub(in crate::app) fn render_control_approval(
        &mut self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let now = approval_now();
        if self.control_approvals.mark_visible(now) {
            let id = self.control_approvals.front()?.id;
            cx.spawn(async move |this, cx| {
                cx.background_executor()
                    .timer(Duration::from_millis(ctl::APPROVAL_ARM_DELAY_MS))
                    .await;
                let _ = this.update(cx, |this, cx| {
                    if this
                        .control_approvals
                        .front()
                        .is_some_and(|entry| entry.id == id)
                    {
                        cx.notify();
                    }
                });
            })
            .detach();
        }
        let entry = self.control_approvals.front()?;
        let id = entry.id;
        let armed = self.control_approvals.is_armed(id, now);
        let request = &entry.payload;
        let preview = &request.preview;
        // Every value is caller- or configuration-supplied text: escape hidden
        // characters so none can draw fake rows or reorder its neighbours.
        let mut details = vec![
            (
                t("Caller (declared)"),
                request
                    .options
                    .from
                    .as_deref()
                    .map(ctl::card_value)
                    .unwrap_or_else(|| t("Not supplied").to_string()),
            ),
            (
                t("Project"),
                ctl::card_value(
                    self.workspace
                        .project(request.pid)
                        .map(|p| p.name.as_str())
                        .unwrap_or(""),
                ),
            ),
            (
                t("Preset"),
                if preview.literal_argv {
                    t("Literal command").to_string()
                } else {
                    ctl::card_value(&preview.preset)
                },
            ),
            (
                t("Directory"),
                preview
                    .launch_dir
                    .as_ref()
                    .map(|p| ctl::card_value(&p.display().to_string()))
                    .unwrap_or_default(),
            ),
            (t("Pane name"), ctl::card_value(preview.display_name())),
        ];
        let parameters = [
            request
                .options
                .model
                .as_ref()
                .map(|model| tf("model {value}", &[("value", model)])),
            request
                .options
                .effort
                .as_ref()
                .map(|effort| tf("effort {value}", &[("value", effort)])),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" · ");
        if !parameters.is_empty() {
            details.insert(3, (t("Parameters"), ctl::card_value(&parameters)));
        }
        if let Some(pane) = request
            .options
            .requesting_pane
            .filter(|id| self.terminals.contains_key(id))
            .and_then(|id| self.workspace.instance(id))
        {
            details.push((
                t("Requesting pane (declared)"),
                ctl::card_value(&format!(
                    "{} ({})",
                    pane.display_name(),
                    ctl::short_id(pane.id)
                )),
            ));
        }
        if let Some(argv) = &request.options.argv {
            details.push((
                t("Full argv"),
                ctl::card_lines(&serde_json::to_string_pretty(argv).unwrap_or_default()),
            ));
        } else {
            // A preset's name says nothing about what it runs ("Codex" may run
            // `ssh me@devbox`): show the program and its arguments as well.
            details.push((t("Command"), ctl::card_lines(&preset_argv(preview))));
        }
        if let Some(prompt) = &request.options.prompt {
            // The task is what is being approved: all of it, in the scrolling
            // details. Validation refuses a task longer than the card shows.
            let shown = ctl::card_prompt(prompt, ctl::MAX_PROMPT_CHARS);
            debug_assert_eq!(shown.omitted_chars, 0);
            details.push((t("Prompt"), shown.text));
        }
        let waiting = self.control_approvals.len().saturating_sub(1);
        let card = div()
            .id("control-approval")
            .absolute()
            .occlude()
            .right_4()
            .bottom_4()
            .w(px(460.0))
            .max_h(px(480.0))
            .flex()
            .flex_col()
            .gap_3()
            .p_4()
            .bg(cx.theme().background)
            .border_1()
            .border_color(cx.theme().border)
            .rounded(cx.theme().radius_lg)
            .shadow_lg()
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().font_semibold().child(t("Allow outside terminal creation?")))
            .child(
                div().text_xs().text_color(cx.theme().muted_foreground).child(t(
                    "Caller labels are declared, not verified identities. Approval expires 120 seconds after arrival.",
                )),
            )
            .child(
                div()
                    .id("control-approval-details")
                    .overflow_y_scroll()
                    .max_h(px(280.0))
                    .min_h_0()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .children(details.into_iter().map(|(label, value)| {
                        div().flex().flex_col().flex_none().gap_1()
                            .child(div().text_xs().text_color(cx.theme().muted_foreground).child(label))
                            .child(div().text_sm().child(value))
                    })),
            )
            .child(div().text_xs().child(tf(
                "{count} more waiting", &[("count", &waiting.to_string())],
            )))
            .child(
                div().flex().justify_end().gap_2()
                    .child(
                        div().id("control-approval-deny").cursor_default()
                            .when(armed, |button| button.cursor_pointer())
                            .opacity(if armed { 1.0 } else { 0.45 }).px_3().py_2()
                            .rounded(cx.theme().radius).border_1().border_color(cx.theme().border)
                            .child(t("Deny"))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                this.sync_control_approvals(cx);
                                this.control_approvals.press(id, approval_now());
                            }))
                            .on_click(cx.listener(move |this, event, _, cx| {
                                if matches!(event, ClickEvent::Mouse(mouse) if mouse.up.click_count == 1) {
                                    this.decide_control_approval(id, false, cx)
                                }
                            })),
                    )
                    .child(
                        div().id("control-approval-allow").cursor_default()
                            .when(armed, |button| button.cursor_pointer())
                            .opacity(if armed { 1.0 } else { 0.45 }).px_3().py_2()
                            .rounded(cx.theme().radius).bg(cx.theme().primary)
                            .text_color(cx.theme().primary_foreground).child(t("Allow once"))
                            .on_mouse_down(MouseButton::Left, cx.listener(move |this, _, _, cx| {
                                this.sync_control_approvals(cx);
                                this.control_approvals.press(id, approval_now());
                            }))
                            .on_click(cx.listener(move |this, event, _, cx| {
                                if matches!(event, ClickEvent::Mouse(mouse) if mouse.up.click_count == 1) {
                                    this.decide_control_approval(id, true, cx)
                                }
                            })),
                    ),
            )
            .into_any_element();
        Some(card)
    }
}

#[cfg(test)]
mod tests {
    use super::preset_argv;
    use muxel_core::{AgentPreset, Instance};

    #[test]
    fn the_card_shows_what_a_preset_runs_not_just_its_name() {
        let mut preset = AgentPreset::codex();
        preset.program = Some("ssh".into());
        preset.args = vec!["me@devbox".into(), "-t".into(), "".into()];
        let preview = Instance::from_preset(uuid::Uuid::new_v4(), &preset);
        assert_eq!(preview.preset, "Codex");
        let shown: Vec<String> = serde_json::from_str(&preset_argv(&preview)).unwrap();
        assert_eq!(shown, ["ssh", "me@devbox", "-t", ""]);
        // The default shell names the shell it will start.
        let shell = Instance::shell(uuid::Uuid::new_v4());
        let shown: Vec<String> = serde_json::from_str(&preset_argv(&shell)).unwrap();
        assert_eq!(shown, [muxel_terminal::CommandSpec::shell().program]);
    }
}
