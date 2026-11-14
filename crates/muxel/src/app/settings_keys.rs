//! Settings → Keybindings: every rebindable action by name, its keys as key
//! caps, and click-to-record. Which rows exist, how a 1–9 range folds into one
//! row and what counts as a conflict are decided in `settings_model`.

use super::settings_kit::{Form, Group, Row};
use super::*;
use crate::settings_model::{self as model, KEY_ROWS, KeyRow};

/// The keystroke an action has when the user hasn't changed it.
fn default_keystroke(action: &str) -> &'static str {
    settings_view::DEFAULT_KEYBINDINGS
        .iter()
        .find(|(name, _, _)| *name == action)
        .map(|(_, default, _)| *default)
        .unwrap_or("")
}

/// What an action is called, in the user's language.
fn action_title(action: &str) -> SharedString {
    match model::row_for_action(action) {
        Some((row, None)) => t(row.label),
        Some((row, Some(digit))) => {
            let n = digit.to_string();
            match row.id {
                "JumpToTab" => tf("Tab {n}", &[("n", &n)]).into(),
                "JumpToProject" => tf("Project {n}", &[("n", &n)]).into(),
                _ => tf("New agent from preset {n}", &[("n", &n)]).into(),
            }
        }
        None => model::action_label(action).into(),
    }
}

/// One row as Keybindings draws it.
struct KeyLine {
    group: &'static str,
    /// The row's id: an action, or the prefix of a folded range.
    id: String,
    label: SharedString,
    caps: Vec<String>,
    /// The caps of the default, when the row differs from it.
    default_caps: Option<Vec<String>>,
    context: Option<String>,
    /// Other actions on the same keys.
    clashes: Vec<String>,
    /// A folded 1–9 range: recording takes the modifiers of a number chord.
    range: bool,
}

impl MuxelApp {
    /// The keystroke an action is bound to: the user's override, else the default.
    pub(super) fn effective_keystroke(&self, action: &str) -> String {
        self.settings
            .keybindings
            .iter()
            .find(|k| k.action == action)
            .map(|k| k.keystroke.clone())
            .filter(|keystroke| !keystroke.trim().is_empty())
            .unwrap_or_else(|| default_keystroke(action).to_string())
    }

    /// Bind `action` to `keystroke`. Binding it back to its default drops the
    /// override, so a later change of the default reaches it.
    fn set_keystroke(&mut self, action: &str, keystroke: &str) {
        self.settings.keybindings.retain(|k| k.action != action);
        if model::normalize_keystroke(keystroke)
            != model::normalize_keystroke(default_keystroke(action))
        {
            self.settings.keybindings.push(muxel_core::KeyBindingCfg {
                action: action.to_string(),
                keystroke: keystroke.to_string(),
            });
        }
    }

    /// Make the keymap match the settings after a change, and save them.
    fn commit_keybindings(&mut self, before: &muxel_core::Settings, cx: &mut Context<Self>) {
        rebind_keys(before, &self.settings.clone(), cx);
        self.persist_settings();
        cx.notify();
    }

    /// Seed the Keybindings text field from the current settings.
    pub(super) fn load_keybinding_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let pass = self.settings.terminal_passthrough_keys.join(", ");
        self.settings_ui
            .passthrough_keys
            .update(cx, |s, cx| s.set_value(pass, window, cx));
    }

    /// Read the terminal passthrough chords and rebind around them.
    pub(super) fn apply_passthrough_keys(&mut self, cx: &mut Context<Self>) {
        let before = self.settings.clone();
        let typed = self.settings_ui.passthrough_keys.read(cx).value().clone();
        self.settings.terminal_passthrough_keys = settings_shell::parse_passthrough(&typed);
        self.commit_keybindings(&before, cx);
    }

    /// Wait for the next key press and bind it to row `id`. Every keystroke is
    /// intercepted before the keymap sees it, so pressing a chord that is
    /// already a shortcut records it rather than running it.
    fn start_key_recording(&mut self, id: String, cx: &mut Context<Self>) {
        self.settings_ui.kb_recording = Some(id);
        if self.key_recorder.is_none() {
            let app = cx.weak_entity();
            self.key_recorder = Some(cx.intercept_keystrokes(move |event, _window, cx| {
                let keystroke = event.keystroke.clone();
                let handled = app
                    .update(cx, |this, cx| this.record_keystroke(&keystroke, cx))
                    .unwrap_or(false);
                if handled {
                    cx.stop_propagation();
                }
            }));
        }
        cx.notify();
    }

    /// Stop waiting for a key press, if a row was.
    pub(super) fn stop_key_recording(&mut self, cx: &mut Context<Self>) {
        self.settings_ui.kb_recording = None;
        // The interceptor may be the caller; let it return before it is dropped.
        if let Some(recorder) = self.key_recorder.take() {
            cx.defer(move |_| drop(recorder));
        }
        cx.notify();
    }

    /// A key pressed while a row is recording. Returns whether it was consumed.
    fn record_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) -> bool {
        let Some(id) = self.settings_ui.kb_recording.clone() else {
            return false;
        };
        if !self.show_settings {
            self.stop_key_recording(cx);
            return false;
        }
        // A modifier on its own is the start of a chord, not a shortcut.
        if model::is_modifier_key(&keystroke.key) {
            return true;
        }
        let mods = keystroke.modifiers;
        let bare = !(mods.control || mods.alt || mods.shift || mods.platform || mods.function);
        if bare && keystroke.key == "escape" {
            self.stop_key_recording(cx);
            return true;
        }
        let typed = keystroke.unparse();
        let before = self.settings.clone();
        let is_range = KEY_ROWS.iter().any(|row| row.range && row.id == id);
        if is_range {
            // A range takes its modifiers from any number chord; anything else
            // is ignored, and the row keeps waiting.
            let Some((modifiers, _)) = model::digit_chord(&typed) else {
                return true;
            };
            for digit in 1..=9u8 {
                self.set_keystroke(
                    &format!("{id}{digit}"),
                    &model::range_keystroke(&modifiers, digit),
                );
            }
        } else {
            self.set_keystroke(&id, &typed);
        }
        self.stop_key_recording(cx);
        self.commit_keybindings(&before, cx);
        true
    }

    /// Put a row back on its default keys.
    fn reset_key_row(&mut self, id: &str, cx: &mut Context<Self>) {
        let before = self.settings.clone();
        let actions: Vec<String> = match KEY_ROWS.iter().find(|row| row.id == id) {
            Some(row) => model::row_actions(row),
            // One digit of a range that isn't folded.
            None => vec![id.to_string()],
        };
        self.settings
            .keybindings
            .retain(|k| !actions.contains(&k.action));
        self.stop_key_recording(cx);
        self.commit_keybindings(&before, cx);
    }

    /// Put every shortcut back on its default keys.
    fn reset_all_keybindings(&mut self, cx: &mut Context<Self>) {
        let before = self.settings.clone();
        self.settings.keybindings.clear();
        self.stop_key_recording(cx);
        self.commit_keybindings(&before, cx);
    }

    /// "Press Ctrl+Alt and a preset's number", when the nine new-agent
    /// shortcuts still share their modifiers.
    pub(super) fn new_agent_shortcut_hint(&self) -> Option<SharedString> {
        let keys: Vec<String> = (1..=9)
            .map(|n| self.effective_keystroke(&format!("NewAgent{n}")))
            .collect();
        let modifiers = model::range_modifiers(&keys)?;
        let mut caps = model::key_caps(&model::range_keystroke(&modifiers, 1));
        caps.pop();
        if caps.is_empty() {
            return None;
        }
        Some(
            tf(
                "Press {keys} and a preset's number to launch it.",
                &[("keys", &caps.join("+"))],
            )
            .into(),
        )
    }

    /// Every row of Keybindings as it stands: folded where a range still
    /// shares its modifiers, spelled out digit by digit where it doesn't.
    fn key_lines(&self) -> Vec<KeyLine> {
        let bound = bound_actions(&self.settings);
        let clashes = model::conflicts(&bound);
        let context_of = |action: &str| {
            bound
                .iter()
                .find(|b| b.action == action)
                .and_then(|b| b.context.clone())
        };
        let clashes_of = |action: &str| -> Vec<String> {
            clashes
                .get(action)
                .map(|others| {
                    others
                        .iter()
                        .map(|other| action_title(other).to_string())
                        .collect()
                })
                .unwrap_or_default()
        };
        let single = |row: &KeyRow, action: &str, label: SharedString| {
            let keystroke = self.effective_keystroke(action);
            let default = default_keystroke(action);
            let changed =
                model::normalize_keystroke(&keystroke) != model::normalize_keystroke(default);
            KeyLine {
                group: row.group,
                id: action.to_string(),
                label,
                caps: model::key_caps(&keystroke),
                default_caps: changed.then(|| model::key_caps(default)),
                context: context_of(action),
                clashes: clashes_of(action),
                range: false,
            }
        };
        let mut lines = Vec::new();
        for row in KEY_ROWS {
            if !row.range {
                lines.push(single(row, row.id, t(row.label)));
                continue;
            }
            let actions = model::row_actions(row);
            let keys: Vec<String> = actions
                .iter()
                .map(|a| self.effective_keystroke(a))
                .collect();
            let Some(modifiers) = model::range_modifiers(&keys) else {
                for action in &actions {
                    lines.push(single(row, action, action_title(action)));
                }
                continue;
            };
            let folded = |modifiers: &str| {
                let mut caps = model::key_caps(&model::range_keystroke(modifiers, 1));
                caps.pop();
                caps.push("1 – 9".to_string());
                caps
            };
            let defaults: Vec<String> = actions
                .iter()
                .map(|a| default_keystroke(a).to_string())
                .collect();
            let default_modifiers = model::range_modifiers(&defaults);
            let mut all_clashes: Vec<String> = actions.iter().flat_map(|a| clashes_of(a)).collect();
            all_clashes.sort();
            all_clashes.dedup();
            lines.push(KeyLine {
                group: row.group,
                id: row.id.to_string(),
                label: t(row.label),
                caps: folded(&modifiers),
                default_caps: default_modifiers
                    .filter(|default| *default != modifiers)
                    .map(|default| folded(&default)),
                context: context_of(&actions[0]),
                clashes: all_clashes,
                range: true,
            });
        }
        lines
    }

    fn key_line_row(&self, line: KeyLine, form: &Form, cx: &mut Context<Self>) -> Row {
        let pal = form.pal;
        let recording = self.settings_ui.kb_recording.as_deref() == Some(line.id.as_str());
        let id = line.id.clone();
        let keys = if recording {
            div()
                .id(SharedString::from(format!("kb-rec-{id}")))
                .h(form.rem * 1.75)
                .px_2()
                .flex()
                .items_center()
                .gap_2()
                .rounded(form.radius)
                .bg(pal.track)
                .border_1()
                .border_color(pal.accent)
                .text_xs()
                .cursor_pointer()
                .on_click(cx.listener(|this, _e, _w, cx| this.stop_key_recording(cx)))
                .child(div().size(px(7.0)).rounded_full().bg(pal.accent))
                .child(if line.range {
                    t("Hold the modifiers and press a number…")
                } else {
                    t("Press the new shortcut…")
                })
                .child(div().text_color(pal.dim).child(t("Esc cancels")))
        } else {
            let hover = pal.track;
            div()
                .id(SharedString::from(format!("kb-keys-{id}")))
                .p_1()
                .rounded(form.radius)
                .cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(
                    cx.listener(move |this, _e, _w, cx| this.start_key_recording(id.clone(), cx)),
                )
                .child(form.key_caps(line.caps.clone()))
        };
        let reset = line.default_caps.as_ref().map(|default| {
            let id = line.id.clone();
            Button::new(SharedString::from(format!("kb-reset-{id}")))
                .ghost()
                .xsmall()
                .icon(Icon::empty().path("icons/rotate-ccw.svg"))
                .tooltip(tf("Reset to {keys}", &[("keys", &default.join("+"))]))
                .on_click(cx.listener(move |this, _e, _w, cx| this.reset_key_row(&id, cx)))
        });

        let mut notes: Vec<String> = Vec::new();
        match line.context.as_deref() {
            Some("Terminal") => notes.push(t("Only while a terminal is focused.").to_string()),
            Some("!Terminal") => notes.push(t("Not while a terminal is focused.").to_string()),
            _ => {}
        }
        if let Some(default) = &line.default_caps {
            notes.push(tf(
                "Changed. The default is {keys}.",
                &[("keys", &default.join("+"))],
            ));
        }
        let mut row = Row::new(
            line.label,
            div()
                .flex()
                .items_center()
                .gap_1()
                .children(reset)
                .child(keys),
        )
        // Lets the filter and the search find a row by its keys.
        .keywords(format!("{} {}", line.caps.join(" "), line.caps.join("+")));
        if !notes.is_empty() {
            row = row.desc(notes.join(" "));
        }
        row.warn((!line.clashes.is_empty()).then(|| {
            tf(
                "Also bound to {actions}. Only one of them will fire.",
                &[("actions", &line.clashes.join(", "))],
            )
            .into()
        }))
    }

    /// The shortcut rows a search found.
    pub(super) fn keybinding_rows(
        &self,
        form: &Form,
        words: &[String],
        cx: &mut Context<Self>,
    ) -> Vec<Row> {
        let section = SettingsSection::Keybindings.title();
        self.key_lines()
            .into_iter()
            .filter_map(|line| {
                let group = t(line.group);
                let row = self.key_line_row(line, form, cx);
                row.matches(words, &[group.as_ref(), section.as_ref()])
                    .then_some(row)
            })
            .collect()
    }

    pub(super) fn render_settings_keybindings(
        &self,
        form: &Form,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = form.pal;
        let filter = model::query_words(&self.settings_ui.kb_filter.read(cx).value());
        let lines = self.key_lines();
        let any_changed = !self.settings.keybindings.is_empty();

        let reset_w = form.rem * 11.0;
        let toolbar = div()
            .w(form.w)
            .flex()
            .items_center()
            .gap_2()
            .child(
                div().w(form.w - reset_w - px(8.0)).child(
                    Input::new(&self.settings_ui.kb_filter)
                        .prefix(Icon::new(IconName::Search).small().text_color(pal.dim))
                        .cleanable(true),
                ),
            )
            .child(
                div().w(reset_w).flex().justify_end().child(
                    Button::new("kb-reset-all")
                        .outline()
                        .label(t("Reset all"))
                        .tooltip(t("Put every shortcut back on its default keys"))
                        .disabled(!any_changed)
                        .on_click(cx.listener(|this, _e, _w, cx| this.reset_all_keybindings(cx))),
                ),
            );

        let mut col = v_flex().w(form.w).gap(form.rem * 1.25).child(toolbar);
        let mut shown = 0;
        let mut group: Option<(&'static str, Vec<Row>)> = None;
        let flush = |col: Div, group: Option<(&'static str, Vec<Row>)>| match group {
            Some((title, rows)) if !rows.is_empty() => {
                col.child(form.group(Group::new(t(title), rows), &filter))
            }
            _ => col,
        };
        for line in lines {
            let title = line.group;
            let row = self.key_line_row(line, form, cx);
            if !filter.is_empty() && !row.matches(&filter, &[t(title).as_ref()]) {
                continue;
            }
            shown += 1;
            match &mut group {
                Some((current, rows)) if *current == title => rows.push(row),
                _ => {
                    col = flush(col, group.take());
                    group = Some((title, vec![row]));
                }
            }
        }
        col = flush(col, group.take());
        if shown == 0 {
            col = col.child(form.empty(t("No shortcut matches that filter.")));
        }

        if filter.is_empty() {
            col = col.child(form.group(
                Group::new(
                    t("Terminal passthrough"),
                    vec![
                        Row::new(
                            t("Keys muxel leaves to the focused terminal"),
                            form.input(&self.settings_ui.passthrough_keys),
                        )
                        .desc(t(
                            "Extra chords, comma-separated, such as ctrl-shift-p. Plain Ctrl+letter shortcuts already yield to a terminal, so agents receive them (Ctrl+S stash, and so on). Applies when you leave the field.",
                        )),
                    ],
                ),
                &[],
            ));
        }
        col.into_any_element()
    }
}
