//! Countdown controls and the shared close confirmation in a popout window.

use super::*;

fn countdown_label(deadline: Instant) -> SharedString {
    let remaining = deadline.saturating_duration_since(Instant::now());
    let seconds = remaining.as_secs() + u64::from(remaining.subsec_nanos() != 0);
    tf("Undo {seconds}", &[("seconds", &seconds.to_string())]).into()
}

pub(super) fn close_alternative_label(action: &ConfirmAction) -> Option<SharedString> {
    match action {
        ConfirmAction::CloseBatch(ids) if ids.len() > 1 => Some(t("Close only the idle ones")),
        ConfirmAction::SaveEditorBeforeClose { .. } => Some(t("Don't Save")),
        _ => None,
    }
}

impl MuxelApp {
    fn close_group_name(&self, group: &CloseGroup) -> SharedString {
        if group.members.len() == 1 {
            group
                .members
                .first()
                .and_then(|&iid| self.workspace.instance(iid))
                .map(|instance| SharedString::from(instance.display_name().to_string()))
                .unwrap_or_else(|| t("Terminal"))
        } else {
            tn(
                "{n} terminal",
                "{n} terminals",
                group.members.len(),
                &[("n", &group.members.len().to_string())],
            )
            .into()
        }
    }

    fn close_decision_label(&self, group: &CloseGroup) -> SharedString {
        if group.members.len() == 1 {
            tf(
                "Review before closing {name}",
                &[("name", self.close_group_name(group).as_ref())],
            )
            .into()
        } else {
            t("Review before closing these terminals")
        }
    }

    /// Drop every close request still being checked. Late replies find no
    /// token and do nothing; terminals already counting down keep their Undo.
    pub(super) fn cancel_close_checks(&mut self, cx: &mut Context<Self>) {
        self.close_requests.clear();
        cx.notify();
    }

    /// The newest group's chip text and the label of its button, which acts on
    /// that group. A request still being checked has a segment of its own.
    pub(super) fn close_chip_text(&self, latest: &CloseGroup) -> (SharedString, SharedString) {
        let several = self.close_groups.len() > 1;
        let panes: usize = self
            .close_groups
            .iter()
            .map(|group| group.members.len())
            .sum();
        let multiple = panes > 1;
        let decision = !several && latest.needs_decision && !latest.undoing;
        let label: SharedString = if latest.checking && !latest.undoing {
            t("Checking before close…")
        } else if decision {
            self.close_decision_label(latest)
        } else if multiple {
            tf("{n} closing", &[("n", &panes.to_string())]).into()
        } else if let Some(caller) = latest.caller.as_deref() {
            tf(
                "Closed {name} by {caller}",
                &[
                    ("name", self.close_group_name(latest).as_ref()),
                    ("caller", caller),
                ],
            )
            .into()
        } else {
            tf(
                "Closed {name}",
                &[("name", self.close_group_name(latest).as_ref())],
            )
            .into()
        };
        let button = if decision {
            t("Keep")
        } else if latest.undoing {
            t("Restoring…")
        } else if latest.checking || multiple {
            t("Undo")
        } else {
            countdown_label(latest.deadline)
        };
        (label, button)
    }

    fn render_close_check(&self, cx: &mut Context<Self>) -> AnyElement {
        div()
            .id("recoverable-close-checking")
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_0p5()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(div().text_xs().child(t("Checking before close…")))
            .child(
                Button::new("close-check-cancel")
                    .ghost()
                    .xsmall()
                    .label(t("Cancel"))
                    .on_click(cx.listener(|this, _, _, cx| this.cancel_close_checks(cx))),
            )
            .into_any_element()
    }

    /// The newest countdown, and beside it any close request still being
    /// checked, each with the button that acts on it.
    pub(super) fn render_close_chip(&self, cx: &mut Context<Self>) -> AnyElement {
        let check = (!self.close_requests.is_empty()).then(|| self.render_close_check(cx));
        let Some(latest) = self.close_groups.last() else {
            return check.unwrap_or_else(|| div().into_any_element());
        };
        let group = self.render_close_group_chip(latest, cx);
        match check {
            Some(check) => div()
                .flex()
                .items_center()
                .gap_1()
                .min_w_0()
                .child(group)
                .child(check)
                .into_any_element(),
            None => group,
        }
    }

    fn render_close_group_chip(&self, latest: &CloseGroup, cx: &mut Context<Self>) -> AnyElement {
        let several = self.close_groups.len() > 1;
        let multiple = self
            .close_groups
            .iter()
            .map(|group| group.members.len())
            .sum::<usize>()
            > 1;
        let (label, button) = self.close_chip_text(latest);
        let gid = latest.id;
        let decision = !several && latest.needs_decision && !latest.undoing;
        let needs_decision = self
            .close_groups
            .iter()
            .any(|group| group.needs_decision && !group.undoing);
        div()
            .id("recoverable-close-chip")
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py_0p5()
            .max_w(px(440.0))
            .min_w_0()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(if needs_decision {
                cx.theme().warning
            } else {
                cx.theme().border
            })
            // The entire titlebar is a drag region. Claim presses over both the
            // text and its buttons so Undo does not start a window move.
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
            .child(
                div()
                    .min_w_0()
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .text_xs()
                    .child(label),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("·"),
            )
            .children(decision.then(|| {
                Button::new("close-chip-finalize")
                    .xsmall()
                    .danger()
                    .label(t("Close"))
                    .on_click(cx.listener(move |this, _, _, cx| this.finalize_close_group(gid, cx)))
            }))
            .child(
                Button::new("close-chip-undo")
                    .ghost()
                    .xsmall()
                    .label(button)
                    .disabled(latest.undoing)
                    .tooltip(t("Restore the closed terminals (Ctrl+Alt+Z)"))
                    .on_click(cx.listener(move |this, _, window, cx| {
                        if several {
                            this.undo_latest_close(window, cx);
                        } else {
                            this.undo_close(gid, window, cx);
                        }
                    })),
            )
            .children(multiple.then(|| {
                Button::new("close-chip-list")
                    .ghost()
                    .xsmall()
                    .icon(IconName::ChevronDown)
                    .selected(self.show_close_list)
                    .tooltip(t("Review closing terminals"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.show_close_list = !this.show_close_list;
                        cx.notify();
                    }))
            }))
            .into_any_element()
    }

    pub(super) fn render_close_list(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.show_close_list || self.close_groups.is_empty() {
            return div().into_any_element();
        }
        let rows = self
            .close_groups
            .iter()
            .rev()
            .map(|group| {
                let gid = group.id;
                let name = self.close_group_name(group);
                let names = group
                    .members
                    .iter()
                    .filter_map(|&iid| self.workspace.instance(iid))
                    .map(|instance| instance.display_name())
                    .collect::<Vec<_>>()
                    .join(", ");
                let status = if group.undoing {
                    t("Restoring…")
                } else if group.needs_decision {
                    self.close_decision_label(group)
                } else if group.checking {
                    t("Checking before close…")
                } else {
                    countdown_label(group.deadline)
                };
                div()
                    .flex()
                    .flex_col()
                    .gap_2()
                    .p_3()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(div().font_semibold().child(name))
                    .children((group.members.len() > 1).then(|| div().text_sm().child(names)))
                    .children(group.caller.as_deref().map(|caller| {
                        div()
                            .text_xs()
                            .text_color(cx.theme().muted_foreground)
                            .child(tf("Closed by {caller}", &[("caller", caller)]))
                    }))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_2()
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .text_xs()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(status),
                            )
                            .children((group.needs_decision && !group.undoing).then(|| {
                                Button::new(SharedString::from(format!(
                                    "close-list-finalize-{gid}"
                                )))
                                .small()
                                .danger()
                                .label(t("Close"))
                                .on_click(cx.listener(
                                    move |this, _, _, cx| {
                                        this.finalize_close_group(gid, cx);
                                        if this.close_groups.is_empty() {
                                            this.show_close_list = false;
                                        }
                                    },
                                ))
                            }))
                            .children((!group.undoing).then(|| {
                                Button::new(SharedString::from(format!("close-list-undo-{gid}")))
                                    .small()
                                    .ghost()
                                    .label(if group.needs_decision {
                                        t("Keep")
                                    } else {
                                        t("Undo")
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.undo_close(gid, window, cx)
                                    }))
                            })),
                    )
            })
            .collect::<Vec<_>>();
        modal_backdrop()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, _, cx| {
                    this.show_close_list = false;
                    cx.notify();
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                if event.keystroke.key == "escape" {
                    this.show_close_list = false;
                    cx.stop_propagation();
                    cx.notify();
                }
            }))
            .child(
                div()
                    .w(px(480.0))
                    .max_w(relative(0.95))
                    .max_h(relative(0.9))
                    .flex()
                    .flex_col()
                    .p_4()
                    .gap_3()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(
                        div()
                            .text_lg()
                            .font_semibold()
                            .child(t("Closing terminals")),
                    )
                    .child(
                        div()
                            .id("recoverable-close-list")
                            .min_h_0()
                            .max_h(px(360.0))
                            .overflow_y_scroll()
                            .children(rows),
                    )
                    .child(
                        div().flex().justify_end().child(
                            Button::new("close-list-dismiss")
                                .ghost()
                                .label(t("Done"))
                                .on_click(cx.listener(|this, _, _, cx| {
                                    this.show_close_list = false;
                                    cx.notify();
                                })),
                        ),
                    ),
            )
            .into_any_element()
    }

    pub(super) fn run_close_alternative(&mut self, cx: &mut Context<Self>) {
        let Some(action) = self.confirm.as_ref().map(|pending| pending.action.clone()) else {
            return;
        };
        match action {
            ConfirmAction::CloseBatch(ids) if ids.len() > 1 => {
                self.confirm = None;
                self.accept_close_request(ids, true, cx);
            }
            ConfirmAction::SaveEditorBeforeClose { iid, rest } => {
                self.confirm = None;
                self.close_instance_inner(iid, "close (without saving)", cx);
                self.request_recoverable_close(
                    rest.into_iter().filter(|&other| other != iid).collect(),
                    cx,
                );
            }
            _ => return,
        }
        cx.notify();
    }
}

fn main_app(cx: &App) -> Option<Entity<MuxelApp>> {
    cx.try_global::<MuxelHandle>()?.0.upgrade()
}

impl PopoutView {
    pub(super) fn central_close_confirm(&self, cx: &App) -> Option<PendingConfirm> {
        let app = main_app(cx)?;
        app.read(cx)
            .confirm
            .as_ref()
            .filter(|pending| pending.action.pane_instance() == Some(self.iid))
            .cloned()
    }

    fn cancel_central_close_confirm(&self, cx: &mut Context<Self>) {
        if let Some(app) = main_app(cx) {
            app.update(cx, |app, cx| {
                if app
                    .confirm
                    .as_ref()
                    .is_some_and(|pending| pending.action.pane_instance() == Some(self.iid))
                {
                    app.cancel_confirm(cx);
                }
            });
        }
        cx.notify();
    }

    pub(super) fn render_central_close_confirm(
        &self,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let pending = self.central_close_confirm(cx)?;
        let alternate = close_alternative_label(&pending.action);
        Some(
            modal_backdrop()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| this.cancel_central_close_confirm(cx)),
                )
                .on_key_down(cx.listener(|this, event: &KeyDownEvent, _, cx| {
                    if event.keystroke.key == "escape" {
                        this.cancel_central_close_confirm(cx);
                        cx.stop_propagation();
                    }
                }))
                .child(
                    div()
                        .w(px(480.0))
                        .max_w(relative(0.95))
                        .max_h(relative(0.9))
                        .flex()
                        .flex_col()
                        .gap_3()
                        .p_5()
                        .bg(cx.theme().background)
                        .border_1()
                        .border_color(cx.theme().border)
                        .rounded(cx.theme().radius_lg)
                        .shadow_lg()
                        .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                        .child(div().text_lg().font_semibold().child(pending.title))
                        .child(
                            div()
                                .id("popout-close-message")
                                .min_h_0()
                                .max_h(px(260.0))
                                .overflow_y_scroll()
                                .text_sm()
                                .text_color(cx.theme().muted_foreground)
                                .child(pending.message),
                        )
                        .child(
                            div()
                                .flex()
                                .flex_wrap()
                                .justify_end()
                                .gap_2()
                                .pt_2()
                                .child(
                                    Button::new("popout-close-cancel")
                                        .ghost()
                                        .label(t("Cancel"))
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            this.cancel_central_close_confirm(cx)
                                        })),
                                )
                                .children(alternate.map(|label| {
                                    Button::new("popout-close-alternative")
                                        .ghost()
                                        .label(label)
                                        .on_click(cx.listener(|this, _, _, cx| {
                                            if let Some(app) = main_app(cx) {
                                                app.update(cx, |app, cx| {
                                                    if app.confirm.as_ref().is_some_and(|pending| {
                                                        pending.action.pane_instance()
                                                            == Some(this.iid)
                                                    }) {
                                                        app.run_close_alternative(cx);
                                                    }
                                                });
                                            }
                                        }))
                                }))
                                .child(
                                    Button::new("popout-close-ok")
                                        .danger()
                                        .label(pending.confirm_label)
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            if let Some(app) = main_app(cx) {
                                                app.update(cx, |app, cx| {
                                                    if app.confirm.as_ref().is_some_and(|pending| {
                                                        pending.action.pane_instance()
                                                            == Some(this.iid)
                                                    }) {
                                                        app.run_confirm(window, cx);
                                                    }
                                                });
                                            }
                                        })),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }
}
