//! The Settings sections that edit a list: Agents, Snippets, Runners, Loops,
//! Hosts and Identities. Each is a list on the left and the selected item's
//! editor on the right; the window's footer saves the item being edited.

use super::settings_kit::{
    Click, Form, Group, ListItem, Row, Seg, add_button, delete_button, title_button,
};
use super::settings_shell::unnamed_or;
use super::*;

impl MuxelApp {
    /// A list entry that opens its editor when clicked.
    fn list_item(
        &self,
        id: String,
        icon: AnyElement,
        label: &str,
        selected: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> ListItem {
        ListItem {
            id: id.into(),
            icon: Some(icon),
            label: unnamed_or(label).into(),
            sub: None,
            trailing: None,
            selected,
            dim: false,
            on_click: Box::new(on_click),
        }
    }

    /// The Delete button of an editor: asks first.
    fn delete_action(
        &self,
        id: &'static str,
        title: SharedString,
        body: String,
        action: ConfirmAction,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        delete_button(id, t("Delete…"))
            .on_click(cx.listener(move |this, _e, _w, cx| {
                this.request_confirm(title.clone(), body.clone(), t("Delete"), action.clone(), cx)
            }))
            .into_any_element()
    }

    /// One agent preset as a choice of a picker.
    fn preset_seg(
        &self,
        preset: &AgentPreset,
        selected: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Seg {
        Seg::new(unnamed_or(&preset.name), selected, on_click)
            .icon(agent_icon_obj(preset.program.as_deref()))
    }

    // ===== Agents =====

    pub(super) fn render_settings_agents(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let pal = form.pal;
        let items = self
            .presets
            .iter()
            .enumerate()
            .map(|(idx, preset)| {
                let selected = self.settings_ui.selected_preset == Some(idx);
                // Agents whose binary isn't installed are hidden from the
                // new-agent menus; say why here.
                let missing = !self.agent_runnable(preset);
                let fg = form.item_fg(selected, missing);
                let mut item = self.list_item(
                    format!("preset-item-{idx}"),
                    preset_icon(preset, form.rem * 0.9375, fg).into_any_element(),
                    &preset.name,
                    selected,
                    cx.listener(move |this, _e, window, cx| {
                        this.open_preset_editor(idx, window, cx)
                    }),
                );
                item.dim = missing;
                item.sub = missing.then(|| t("not installed"));
                // The first nine launch from the keyboard, by their place here.
                item.trailing = (idx < 9).then(|| {
                    div()
                        .flex_none()
                        .min_w(form.rem * 1.125)
                        .px_1()
                        .rounded(form.radius)
                        .border_1()
                        .border_color(if selected { fg } else { pal.card_border })
                        .text_xs()
                        .text_center()
                        .text_color(if selected { fg } else { pal.dim })
                        .child((idx + 1).to_string())
                        .into_any_element()
                });
                item
            })
            .collect();
        let detail = form.detail();
        let editor = match self.settings_ui.selected_preset {
            Some(idx) if idx < self.presets.len() => self.render_preset_editor(idx, &detail, cx),
            _ => detail.empty(t("Select a preset to edit, or add one.")),
        };
        form.master_detail(
            items,
            Some(
                add_button("add-preset", t("Add preset"))
                    .on_click(cx.listener(|this, _e, window, cx| this.add_preset(window, cx))),
            ),
            self.new_agent_shortcut_hint(),
            editor,
        )
        .into_any_element()
    }

    fn render_preset_editor(&self, idx: usize, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        use muxel_core::PresetKind;
        let ui = &self.settings_ui;
        let pal = form.pal;
        let Some(preset) = self.presets.get(idx) else {
            return div().into_any_element();
        };
        let inj = ui.p_injection.clone();
        let is_flag = matches!(inj, InjectionMode::CliFlag { .. });
        let is_muse = muxel_core::muse::is_program(Some(ui.p_program.read(cx).value().as_ref()));
        let name = unnamed_or(&preset.name);
        let actions = vec![
            title_button("dup-preset", t("Duplicate"))
                .on_click(
                    cx.listener(move |this, _e, window, cx| this.duplicate_preset(idx, window, cx)),
                )
                .into_any_element(),
            self.delete_action(
                "del-preset",
                t("Delete agent?"),
                tf(
                    "The “{name}” agent preset will be deleted.",
                    &[("name", &name)],
                ),
                ConfirmAction::DeletePreset(idx),
                cx,
            ),
        ];
        let kind_seg = |kind: PresetKind, label: SharedString| {
            Seg::new(
                label,
                ui.p_kind == kind,
                cx.listener(move |this, _e, _w, cx| {
                    this.settings_ui.p_kind = kind;
                    cx.notify();
                }),
            )
        };
        // A value and the flag that passes it, side by side.
        let pair = |value: &Entity<InputState>, flag: &Entity<InputState>| {
            let w = form.control_w();
            let flag_w = w * 0.42;
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(form.input_w(value, w - flag_w - px(4.0)))
                .child(form.input_w(flag, flag_w))
        };

        let mut col = v_flex().w(form.w).gap(form.rem).child(form.editor_title(
            Some(preset_icon(preset, form.rem * 1.25, pal.text).into_any_element()),
            name,
            actions,
        ));

        let mut launch = vec![
            form.choice_row(
                t("Type"),
                "preset-kind",
                vec![
                    kind_seg(PresetKind::Terminal, t("Agent")),
                    kind_seg(PresetKind::Browser, t("Browser")),
                ],
            ),
            Row::new(t("Name"), form.input(&ui.p_name)),
        ];
        if ui.p_kind == PresetKind::Browser {
            launch.push(Row::new(t("Homepage URL"), form.input(&ui.p_url)));
            return col
                .child(form.group(Group::new(t("Launch"), launch), &[]))
                .into_any_element();
        }
        launch.extend([
            Row::new(t("Program"), form.input(&ui.p_program))
                .desc(t("Blank runs your default shell.")),
            Row::new(t("Model"), pair(&ui.p_model, &ui.p_model_flag))
                .desc(t("A value, and the flag that passes it.")),
            Row::new(t("Effort"), pair(&ui.p_effort, &ui.p_effort_flag)),
            Row::new(t("Extra arguments"), form.input(&ui.p_args)).desc(t("Quotes group words.")),
        ]);
        col = col.child(form.group(Group::new(t("Launch"), launch), &[]));

        let inj_seg = |label: SharedString, selected: bool, mode: InjectionMode| {
            Seg::new(
                label,
                selected,
                cx.listener(move |this, _e, _w, cx| this.set_editor_injection(mode.clone(), cx)),
            )
        };
        let mut modes = vec![
            inj_seg(
                t("None"),
                matches!(inj, InjectionMode::None),
                InjectionMode::None,
            ),
            inj_seg(
                t("CLI flag"),
                is_flag,
                InjectionMode::CliFlag {
                    flag: String::new(),
                },
            ),
            inj_seg(
                t("Type-in"),
                matches!(inj, InjectionMode::TypeIn),
                InjectionMode::TypeIn,
            ),
        ];
        if is_muse || matches!(inj, InjectionMode::SessionHook) {
            modes.push(inj_seg(
                t("At session start"),
                matches!(inj, InjectionMode::SessionHook),
                InjectionMode::SessionHook,
            ));
        }
        let delivery = match inj {
            InjectionMode::None => t("The prompt is kept here but not sent."),
            InjectionMode::CliFlag { .. } => t("Passed on the command line with the flag below."),
            InjectionMode::TypeIn => t("Typed into the agent once it has started."),
            InjectionMode::SessionHook => t("Handed to the agent when its session starts."),
        };
        let mut prompt = vec![
            Row::stacked(t("Prompt"), form.text_area(&ui.p_prompt, 3.0))
                .desc(t("Instructions every session of this agent starts with.")),
            form.choice_row(t("Delivered by"), "preset-injection", modes)
                .desc(delivery),
        ];
        if is_flag {
            prompt.push(Row::new(t("Flag"), form.input(&ui.p_inj_flag)));
        }
        col = col.child(form.group(Group::new(t("System prompt"), prompt), &[]));

        // The fields most presets never touch stay folded away.
        let open = ui.preset_advanced;
        let chevron = if open {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        };
        let hover = pal.track;
        let mut advanced = v_flex().w(form.w).gap_2().child(
            div()
                .id("preset-advanced")
                .w(form.w)
                .px_2()
                .py_1()
                .flex()
                .items_center()
                .gap_2()
                .rounded(form.radius)
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(cx.listener(|this, _e, _w, cx| {
                    this.settings_ui.preset_advanced = !this.settings_ui.preset_advanced;
                    cx.notify();
                }))
                .child(Icon::new(chevron).small().text_color(pal.dim))
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::MEDIUM)
                        .child(t("Advanced")),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .text_color(pal.dim)
                        .child(t("Environment, status markers, runner startup delay")),
                ),
        );
        if open {
            advanced = advanced.child(form.group(
                Group::new(
                    "",
                    vec![
                        Row::stacked(t("Environment"), form.text_area(&ui.p_env, 2.0))
                            .desc(t("One KEY=VALUE per line.")),
                        Row::new(t("Working markers"), form.input(&ui.p_working_markers)).desc(t(
                            "Text that means the agent is busy. Comma-separated; blank uses the built-in detection.",
                        )),
                        Row::new(t("Blocked markers"), form.input(&ui.p_blocked_markers)).desc(t(
                            "Text that means it is waiting on you. Comma-separated; blank uses the built-in detection.",
                        )),
                        Row::new(
                            t("Runner startup delay"),
                            form.input(&ui.p_startup_delay),
                        )
                        .desc(t(
                            "Milliseconds to wait after the first output before a runner types. 0 waits for the output to go quiet.",
                        )),
                    ],
                ),
                &[],
            ));
        }
        col.child(advanced).into_any_element()
    }

    // ===== Snippets =====

    pub(super) fn render_settings_snippets(
        &self,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let items = self
            .snippets
            .iter()
            .enumerate()
            .map(|(idx, snippet)| {
                let selected = self.settings_ui.selected_snippet == Some(idx);
                let fg = form.item_fg(selected, false);
                let mut item = self.list_item(
                    format!("snippet-row-{idx}"),
                    Icon::new(IconName::SquareTerminal)
                        .small()
                        .text_color(fg)
                        .into_any_element(),
                    &snippet.name,
                    selected,
                    cx.listener(move |this, _e, window, cx| {
                        this.open_snippet_editor(idx, window, cx)
                    }),
                );
                item.trailing = snippet.submit.then(|| {
                    div()
                        .flex_none()
                        .text_xs()
                        .text_color(if selected { fg } else { form.pal.dim })
                        .child("↵")
                        .into_any_element()
                });
                item
            })
            .collect();
        let detail = form.detail();
        let editor = match self.settings_ui.selected_snippet {
            Some(idx) if idx < self.snippets.len() => self.render_snippet_editor(idx, &detail, cx),
            _ => detail.empty(t("Select a snippet to edit, or add one.")),
        };
        form.master_detail(
            items,
            Some(
                add_button("add-snippet", t("Add snippet"))
                    .on_click(cx.listener(|this, _e, window, cx| this.add_snippet(window, cx))),
            ),
            None,
            editor,
        )
        .into_any_element()
    }

    fn render_snippet_editor(&self, idx: usize, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.settings_ui;
        let name = self
            .snippets
            .get(idx)
            .map(|s| unnamed_or(&s.name))
            .unwrap_or_default();
        let delete = self.delete_action(
            "del-snippet",
            t("Delete snippet?"),
            tf("The “{name}” snippet will be deleted.", &[("name", &name)]),
            ConfirmAction::DeleteSnippet(idx),
            cx,
        );
        v_flex()
            .w(form.w)
            .gap(form.rem)
            .child(form.editor_title(None, name, vec![delete]))
            .child(form.group(
                Group::new(
                    "",
                    vec![
                        Row::new(t("Name"), form.input(&ui.sn_name)),
                        Row::stacked(t("Text"), form.text_area(&ui.sn_text, 6.0))
                            .desc(t("Typed into the focused pane.")),
                        Row::narrow(
                            t("Press Enter after typing"),
                            form.switch(
                                "sn-submit",
                                ui.sn_submit,
                                true,
                                cx.listener(|this, _on: &bool, _w, cx| {
                                    this.toggle_snippet_submit(cx)
                                }),
                            ),
                        )
                        .desc(t("Runs it, rather than leaving it at the prompt.")),
                    ],
                ),
                &[],
            ))
            .into_any_element()
    }

    // ===== Runners =====

    pub(super) fn render_settings_runners(
        &self,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let items = self
            .runners
            .iter()
            .enumerate()
            .map(|(idx, runner)| {
                let selected = self.settings_ui.selected_runner == Some(idx);
                let program = runner
                    .preset_id
                    .and_then(|id| self.presets.iter().find(|p| p.id == id))
                    .and_then(|p| p.program.clone());
                self.list_item(
                    format!("runner-row-{idx}"),
                    agent_icon(
                        program.as_deref(),
                        form.rem * 0.9375,
                        form.item_fg(selected, false),
                    )
                    .into_any_element(),
                    &runner.name,
                    selected,
                    cx.listener(move |this, _e, window, cx| {
                        this.open_runner_editor(idx, window, cx)
                    }),
                )
            })
            .collect();
        let detail = form.detail();
        let editor = match self.settings_ui.selected_runner {
            Some(idx) if idx < self.runners.len() => self.render_runner_editor(idx, &detail, cx),
            _ => detail.empty(t("Select a runner to edit, or add one.")),
        };
        form.master_detail(
            items,
            Some(
                add_button("add-runner", t("Add runner"))
                    .on_click(cx.listener(|this, _e, window, cx| this.add_runner(window, cx))),
            ),
            None,
            editor,
        )
        .into_any_element()
    }

    /// The "Shift+Tab presses at startup" stepper runners and loops share.
    fn presses_row(
        &self,
        form: &Form,
        id: &'static str,
        presses: u8,
        dec: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
        inc: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Row {
        Row::new(
            t("Auto mode"),
            form.stepper(
                id,
                presses.to_string(),
                (presses > 0).then(|| Box::new(dec) as Click),
                Some(Box::new(inc)),
            ),
        )
        .desc(t("How many times Shift+Tab is pressed at startup."))
    }

    fn render_runner_editor(&self, idx: usize, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.settings_ui;
        let picked = ui.r_preset_id;
        // "Current/default", then one choice per preset.
        let mut agents = vec![Seg::new(
            t("Current/default"),
            picked.is_none(),
            cx.listener(|this, _e, _w, cx| this.set_runner_preset(None, cx)),
        )];
        for preset in &self.presets {
            let id = preset.id;
            agents.push(self.preset_seg(
                preset,
                picked == Some(id),
                cx.listener(move |this, _e, _w, cx| this.set_runner_preset(Some(id), cx)),
            ));
        }
        let name = self
            .runners
            .get(idx)
            .map(|r| unnamed_or(&r.name))
            .unwrap_or_default();
        let delete = self.delete_action(
            "del-runner",
            t("Delete runner?"),
            tf("The “{name}” runner will be deleted.", &[("name", &name)]),
            ConfirmAction::DeleteRunner(idx),
            cx,
        );
        v_flex()
            .w(form.w)
            .gap(form.rem)
            .child(form.editor_title(None, name, vec![delete]))
            .child(form.group(
                Group::new(
                    "",
                    vec![
                        Row::new(t("Name"), form.input(&ui.r_name)),
                        form.choice_row(t("Agent"), "runner-agent", agents),
                        self.presses_row(
                            form,
                            "runner-presses",
                            ui.r_presses,
                            cx.listener(|this, _e, _w, cx| this.adjust_runner_presses(-1, cx)),
                            cx.listener(|this, _e, _w, cx| this.adjust_runner_presses(1, cx)),
                        ),
                        Row::stacked(t("Prompt"), form.text_area(&ui.r_prompt, 6.0)).desc(t(
                            "{{input}} is replaced with the details you add at run time.",
                        )),
                    ],
                ),
                &[],
            ))
            .into_any_element()
    }

    // ===== Loops =====

    pub(super) fn render_settings_loops(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let items = self
            .loops
            .iter()
            .enumerate()
            .map(|(idx, lp)| {
                let selected = self.settings_ui.selected_loop == Some(idx);
                let program = lp
                    .preset_id
                    .and_then(|id| self.presets.iter().find(|p| p.id == id))
                    .and_then(|p| p.program.clone());
                let mut item = self.list_item(
                    format!("loop-row-{idx}"),
                    agent_icon(
                        program.as_deref(),
                        form.rem * 0.9375,
                        form.item_fg(selected, !lp.enabled),
                    )
                    .into_any_element(),
                    &lp.name,
                    selected,
                    cx.listener(move |this, _e, window, cx| this.open_loop_editor(idx, window, cx)),
                );
                item.dim = !lp.enabled;
                item.sub = Some(loop_schedule_summary(&lp.schedule).into());
                item
            })
            .collect();
        let detail = form.detail();
        let editor = match self.settings_ui.selected_loop {
            Some(idx) if idx < self.loops.len() => self.render_loop_editor(idx, &detail, cx),
            _ => detail.empty(t("Select a loop to edit, or add one.")),
        };
        form.master_detail(
            items,
            Some(
                add_button("add-loop", t("Add loop"))
                    .on_click(cx.listener(|this, _e, window, cx| this.add_loop(window, cx))),
            ),
            None,
            editor,
        )
        .into_any_element()
    }

    fn render_loop_editor(&self, idx: usize, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.settings_ui;
        let pal = form.pal;
        let kind = ui.l_sched_kind;

        // No "Current" here, unlike runners: a loop fires unattended, so it runs
        // the agent it names, not whatever the toolbar has selected then.
        let agents = self
            .presets
            .iter()
            .map(|preset| {
                let id = preset.id;
                self.preset_seg(
                    preset,
                    ui.l_preset_id == Some(id),
                    cx.listener(move |this, _e, _w, cx| this.set_loop_preset(id, cx)),
                )
            })
            .collect();
        let projects = self
            .workspace
            .projects
            .iter()
            .map(|project| {
                let pid = project.id;
                Seg::new(
                    unnamed_or(&project.name),
                    ui.l_project_id == Some(pid),
                    cx.listener(move |this, _e, _w, cx| this.set_loop_project(pid, cx)),
                )
            })
            .collect();
        let schedule = [(0u8, t("Minutes")), (1, t("Hours")), (2, t("Daily"))]
            .into_iter()
            .map(|(k, label)| {
                Seg::new(
                    label,
                    kind == k,
                    cx.listener(move |this, _e, _w, cx| this.set_loop_sched_kind(k, cx)),
                )
            })
            .collect();
        let dim = |text: SharedString| div().text_sm().text_color(pal.dim).child(text);
        let when = if kind == 2 {
            Row::new(
                t("At"),
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(form.input_w(&ui.l_hour, form.rem * 3.5))
                    .child(":")
                    .child(form.input_w(&ui.l_minute, form.rem * 3.5)),
            )
            .desc(t("Local time, 24-hour."))
        } else {
            Row::new(
                t("Every"),
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(form.input_w(&ui.l_interval, form.rem * 5.0))
                    .child(dim(if kind == 0 { t("minutes") } else { t("hours") })),
            )
        };

        let name = self
            .loops
            .get(idx)
            .map(|l| unnamed_or(&l.name))
            .unwrap_or_default();
        let actions = vec![
            title_button("loop-run-now", t("Run now"))
                .icon(IconName::Play)
                .on_click(
                    cx.listener(move |this, _e, window, cx| this.run_loop_now(idx, window, cx)),
                )
                .into_any_element(),
            self.delete_action(
                "del-loop",
                t("Delete loop?"),
                tf("The “{name}” loop will be deleted.", &[("name", &name)]),
                ConfirmAction::DeleteLoop(idx),
                cx,
            ),
        ];
        v_flex()
            .w(form.w)
            .gap(form.rem)
            .child(form.editor_title(None, name, actions))
            .child(form.group(
                Group::new(
                    "",
                    vec![
                        Row::new(t("Name"), form.input(&ui.l_name)),
                        Row::narrow(
                            t("Enabled"),
                            form.switch(
                                "loop-enabled",
                                ui.l_enabled,
                                true,
                                cx.listener(|this, on: &bool, _w, cx| {
                                    this.settings_ui.l_enabled = *on;
                                    cx.notify();
                                }),
                            ),
                        ),
                        form.choice_row(t("Agent"), "loop-agent", agents),
                        form.choice_row(t("Project"), "loop-project", projects),
                    ],
                ),
                &[],
            ))
            .child(form.group(
                Group::new(
                    t("Schedule"),
                    vec![
                        form.choice_row(t("Repeat"), "loop-schedule", schedule),
                        when,
                    ],
                ),
                &[],
            ))
            .child(form.group(
                Group::new(
                    t("Each run"),
                    vec![
                        Row::stacked(t("Prompt"), form.text_area(&ui.l_prompt, 6.0)),
                        self.presses_row(
                            form,
                            "loop-presses",
                            ui.l_presses,
                            cx.listener(|this, _e, _w, cx| this.adjust_loop_presses(-1, cx)),
                            cx.listener(|this, _e, _w, cx| this.adjust_loop_presses(1, cx)),
                        ),
                        Row::narrow(
                            t("Exit the agent after each run"),
                            form.switch(
                                "loop-exit",
                                ui.l_exit,
                                true,
                                cx.listener(|this, on: &bool, _w, cx| {
                                    this.settings_ui.l_exit = *on;
                                    cx.notify();
                                }),
                            ),
                        )
                        .desc(t("Closes the pane once it finishes.")),
                    ],
                ),
                &[],
            ))
            .into_any_element()
    }

    // ===== Hosts =====

    pub(super) fn render_settings_remotes(
        &self,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let items = self
            .remotes
            .iter()
            .enumerate()
            .map(|(idx, host)| {
                let selected = self.settings_ui.selected_remote == Some(idx);
                self.list_item(
                    format!("remote-row-{idx}"),
                    Icon::new(IconName::Network)
                        .small()
                        .text_color(form.item_fg(selected, false))
                        .into_any_element(),
                    &host.name,
                    selected,
                    cx.listener(move |this, _e, window, cx| {
                        this.open_remote_editor(idx, window, cx)
                    }),
                )
            })
            .collect();
        let detail = form.detail();
        let editor = match self.settings_ui.selected_remote {
            Some(idx) if idx < self.remotes.len() => self.render_remote_editor(idx, &detail, cx),
            _ => detail.empty(t(
                "Select a host to edit, or add one. Hosts are used when creating remote projects.",
            )),
        };
        form.master_detail(
            items,
            Some(
                add_button("add-remote", t("Add host"))
                    .on_click(cx.listener(|this, _e, window, cx| this.add_remote(window, cx))),
            ),
            None,
            editor,
        )
        .into_any_element()
    }

    /// The ssh-agent / key file / password choice hosts and identities share.
    fn auth_segs(
        &self,
        auth: SshAuth,
        cx: &mut Context<Self>,
        set: fn(&mut Self, SshAuth, &mut Context<Self>),
    ) -> Vec<Seg> {
        [
            (SshAuth::Agent, SharedString::from("ssh-agent")),
            (SshAuth::Key, t("Key file")),
            (SshAuth::Password, t("Password")),
        ]
        .into_iter()
        .map(|(value, label)| {
            Seg::new(
                label,
                auth == value,
                cx.listener(move |this, _e, _w, cx| set(this, value, cx)),
            )
        })
        .collect()
    }

    /// A key-file field with its Browse button, sized for a row's control.
    fn key_file_control(
        &self,
        form: &Form,
        input: &Entity<InputState>,
        id: &'static str,
        browse: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Div {
        let button_w = form.rem * 6.5;
        let w = form.control_w() - button_w;
        div()
            .flex()
            .items_center()
            .gap_1()
            .child(form.input_w(input, if w < px(90.0) { px(90.0) } else { w }))
            .child(
                Button::new(id)
                    .outline()
                    .small()
                    .icon(IconName::Folder)
                    .label(t("Browse"))
                    .on_click(browse),
            )
    }

    fn render_remote_editor(&self, idx: usize, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let ui = &self.settings_ui;
        let pal = form.pal;
        let auth = ui.s_auth;

        // Credentials: inline, or drawn from a shared login identity.
        let mut creds = vec![Seg::new(
            t("Inline"),
            ui.s_identity_id.is_none(),
            cx.listener(|this, _e, _w, cx| {
                this.settings_ui.s_identity_id = None;
                cx.notify();
            }),
        )];
        for identity in &self.identities {
            let iid = identity.id;
            creds.push(
                Seg::new(
                    unnamed_or(&identity.name),
                    ui.s_identity_id == Some(iid),
                    cx.listener(move |this, _e, _w, cx| {
                        this.settings_ui.s_identity_id = Some(iid);
                        cx.notify();
                    }),
                )
                .icon(Icon::new(IconName::CircleUser)),
            );
        }
        let creds_row = form.choice_row(t("Credentials"), "remote-creds", creds);

        let mut connection = vec![
            Row::new(t("Name"), form.input(&ui.s_name)),
            Row::new(t("Host"), form.input(&ui.s_host)).desc(t("Or a ~/.ssh/config alias.")),
            Row::new(t("Port"), form.input_w(&ui.s_port, form.rem * 6.0)),
        ];
        if ui.s_identity_id.is_none() {
            // Inline credentials: user + auth + key/password on the host itself.
            connection.push(creds_row);
            connection.push(Row::new(t("User"), form.input(&ui.s_user)));
            connection.push(form.choice_row(
                t("Authentication"),
                "remote-auth",
                self.auth_segs(auth, cx, |this, value, cx| this.set_remote_auth(value, cx)),
            ));
            if auth == SshAuth::Key {
                connection.push(Row::new(
                    t("Identity file"),
                    self.key_file_control(
                        form,
                        &ui.s_identity,
                        "remote-browse-key",
                        cx.listener(|this, _e, window, cx| this.browse_identity_file(window, cx)),
                    ),
                ));
            } else if auth == SshAuth::Password {
                // Password auth feeds the secret to ssh via `sshpass`. Warn if
                // it's unavailable (Windows has no sshpass — use a key or
                // ssh-agent there).
                let warn = (!self.sshpass_available).then(|| {
                    SharedString::from(if cfg!(target_os = "windows") {
                        "Password auth needs `sshpass`, which isn't available on Windows. Use a key file or ssh-agent instead."
                    } else {
                        "`sshpass` not found on PATH — install it for password auth, or use a key file / ssh-agent. (Windows can't use password auth.)"
                    })
                });
                connection.push(
                    Row::new(t("Password"), form.input(&ui.s_password))
                        .desc(if ui.s_has_password {
                            t("A password is saved in the OS keychain. Type a new one to replace it.")
                        } else {
                            t("Stored securely in the OS keychain — never in muxel's config.")
                        })
                        .warn(warn),
                );
            }
        } else {
            // Credentials come from a shared identity — say which, or warn if
            // it's gone.
            let shared = self
                .identities
                .iter()
                .find(|i| Some(i.id) == ui.s_identity_id)
                .map(|i| i.name.clone());
            connection.push(match shared {
                Some(name) => creds_row.desc(tf(
                    "User, authentication & key/password come from the “{name}” identity.",
                    &[("name", &name)],
                )),
                None => creds_row.warn(Some(t(
                    "The selected identity no longer exists — this host falls back to ssh-agent until you pick another or switch to Inline.",
                ))),
            });
        }

        // Persistence and the pane shell are the two places the families
        // really differ, so the form asks a different question for each.
        let remote_os = ui.s_remote_os;
        let os_seg = |label: &'static str, value: RemoteOs| {
            Seg::new(
                label,
                remote_os == value,
                cx.listener(move |this, _e, _w, cx| {
                    this.settings_ui.s_remote_os = value;
                    cx.notify();
                }),
            )
        };
        let mut system = vec![form.choice_row(
            t("Operating system"),
            "remote-os",
            vec![
                os_seg("Unix (Linux/macOS)", RemoteOs::Unix),
                os_seg("Windows", RemoteOs::Windows),
            ],
        )];
        if remote_os == RemoteOs::Windows {
            let shell = ui.s_windows_shell;
            let shell_seg = |label: &'static str, value: WindowsShell| {
                Seg::new(
                    label,
                    shell == value,
                    cx.listener(move |this, _e, _w, cx| {
                        this.settings_ui.s_windows_shell = value;
                        cx.notify();
                    }),
                )
            };
            system.push(
                form.choice_row(t("Pane shell"), "remote-win-shell", vec![
                            shell_seg("PowerShell", WindowsShell::PowerShell),
                            shell_seg("pwsh 7+", WindowsShell::Pwsh),
                            shell_seg("cmd.exe", WindowsShell::Cmd),
                        ])
                .desc(t(
                    "Windows has no tmux, so a pane here lasts as long as its SSH connection. On reconnect muxel relaunches the pane and the agent resumes its saved conversation.",
                )),
            );
        } else {
            system.push(
                Row::narrow(
                    t("Persistent tmux session"),
                    form.switch(
                        "remote-tmux",
                        ui.s_use_tmux,
                        true,
                        cx.listener(|this, on: &bool, _w, cx| {
                            this.settings_ui.s_use_tmux = *on;
                            cx.notify();
                        }),
                    ),
                )
                .desc(t("Remote panes run in tmux, so they survive a disconnect.")),
            );
        }

        let options = vec![
            Row::new(t("Jump host"), form.input(&ui.s_jump)).desc(t("ProxyJump. Optional.")),
            Row::narrow(
                t("Forward the ssh-agent"),
                form.switch(
                    "remote-forward-agent",
                    ui.s_forward_agent,
                    true,
                    cx.listener(|this, on: &bool, _w, cx| {
                        this.settings_ui.s_forward_agent = *on;
                        cx.notify();
                    }),
                ),
            )
            .desc("-A"),
            Row::narrow(
                t("Compression"),
                form.switch(
                    "remote-compression",
                    ui.s_compression,
                    true,
                    cx.listener(|this, on: &bool, _w, cx| {
                        this.settings_ui.s_compression = *on;
                        cx.notify();
                    }),
                ),
            )
            .desc(t("-C. Helps on slow or high-latency links.")),
            Row::new("StrictHostKeyChecking", form.input(&ui.s_strict))
                .desc(t("Blank uses accept-new.")),
            Row::new(t("Keepalive"), form.input(&ui.s_keepalive)).desc(t(
                "ServerAliveInterval, in seconds. Blank uses 20; 0 turns it off.",
            )),
            Row::stacked(t("Extra ssh -o options"), form.text_area(&ui.s_extra, 2.0))
                .desc(t("One per line.")),
        ];

        let name = self
            .remotes
            .get(idx)
            .map(|h| unnamed_or(&h.name))
            .unwrap_or_default();
        let actions = vec![
            title_button("test-remote", t("Test connection"))
                .icon(IconName::Network)
                .on_click(cx.listener(move |this, _e, window, cx| {
                    this.save_remote(window, cx);
                    this.test_remote_connection(idx, window, cx);
                }))
                .into_any_element(),
            self.delete_action(
                "del-remote",
                t("Delete host?"),
                tf(
                    "The “{name}” SSH host and its saved password will be removed.",
                    &[("name", &name)],
                ),
                ConfirmAction::DeleteRemote(idx),
                cx,
            ),
        ];
        // The result of the last Test connection, right under the title.
        let test = match &ui.s_test {
            RemoteTestState::Idle => None,
            RemoteTestState::Testing => Some(form.status(t("Connecting…"), pal.dim)),
            RemoteTestState::Ok(msg) => Some(form.status(format!("✓ {msg}"), pal.success)),
            RemoteTestState::Failed(msg) => Some(form.status(format!("✗ {msg}"), pal.danger)),
        };
        v_flex()
            .w(form.w)
            .gap(form.rem)
            .child(form.editor_title(None, name, actions))
            .children(test)
            .child(form.group(Group::new(t("Connection"), connection), &[]))
            .child(form.group(Group::new(t("Remote system"), system), &[]))
            .child(form.group(Group::new(t("SSH options"), options), &[]))
            .into_any_element()
    }

    // ===== Identities =====

    pub(super) fn render_settings_identities(
        &self,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let items = self
            .identities
            .iter()
            .enumerate()
            .map(|(idx, identity)| {
                let selected = self.settings_ui.selected_identity == Some(idx);
                self.list_item(
                    format!("identity-row-{idx}"),
                    Icon::new(IconName::CircleUser)
                        .small()
                        .text_color(form.item_fg(selected, false))
                        .into_any_element(),
                    &identity.name,
                    selected,
                    cx.listener(move |this, _e, window, cx| {
                        this.open_identity_editor(idx, window, cx)
                    }),
                )
            })
            .collect();
        let detail = form.detail();
        let editor = match self.settings_ui.selected_identity {
            Some(idx) if idx < self.identities.len() => {
                self.render_identity_editor(idx, &detail, cx)
            }
            _ => detail.empty(t(
                "Select an identity to edit, or add one. An identity is a reusable login (user + auth + key/password) that multiple hosts can share.",
            )),
        };
        form.master_detail(
            items,
            Some(
                add_button("add-identity", t("Add identity"))
                    .on_click(cx.listener(|this, _e, window, cx| this.add_identity(window, cx))),
            ),
            None,
            editor,
        )
        .into_any_element()
    }

    fn render_identity_editor(
        &self,
        idx: usize,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let ui = &self.settings_ui;
        let auth = ui.id_auth;
        let mut rows = vec![
            Row::new(t("Name"), form.input(&ui.id_name)),
            Row::new(t("User"), form.input(&ui.id_user)),
            form.choice_row(
                t("Authentication"),
                "identity-auth",
                self.auth_segs(auth, cx, |this, value, cx| {
                    this.set_identity_auth(value, cx)
                }),
            ),
        ];
        if auth == SshAuth::Key {
            rows.push(Row::new(
                t("Identity file"),
                self.key_file_control(
                    form,
                    &ui.id_identity,
                    "id-browse-key",
                    cx.listener(|this, _e, window, cx| this.browse_identity_key_file(window, cx)),
                ),
            ));
        } else if auth == SshAuth::Password {
            rows.push(Row::new(t("Password"), form.input(&ui.id_password)).desc(
                if ui.id_has_password {
                    t("A password is saved in the OS keychain. Type a new one to replace it.")
                } else {
                    t("Stored securely in the OS keychain — never in muxel's config.")
                },
            ));
        }
        let name = self
            .identities
            .get(idx)
            .map(|i| unnamed_or(&i.name))
            .unwrap_or_default();
        let delete = self.delete_action(
            "del-identity",
            t("Delete identity?"),
            tf(
                "The “{name}” login identity and its saved password will be removed; hosts using it fall back to inline credentials.",
                &[("name", &name)],
            ),
            ConfirmAction::DeleteIdentity(idx),
            cx,
        );
        v_flex()
            .w(form.w)
            .gap(form.rem)
            .child(form.editor_title(None, name, vec![delete]))
            .child(form.group(Group::new("", rows), &[]))
            .into_any_element()
    }
}
