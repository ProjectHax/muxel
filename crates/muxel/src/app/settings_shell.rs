//! The Settings window itself: the grouped section list with its search field,
//! the section header, the scrolling page and the footer. What each section
//! shows lives in `settings_pages`, `settings_lists` and `settings_keys`.

use super::settings_kit::{Block, Form, Group, Row};
use super::*;
use crate::settings_model as model;

/// A text field of Settings that applies when it is left or Enter is pressed,
/// rather than through a button of its own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum TextSetting {
    TerminalFont,
    EditorFont,
    AutoContinue,
    SttProvider,
    SttWakePhrase,
    TtsSystemVoice,
    TtsProvider,
    Passthrough,
}

impl TextSetting {
    pub(super) const ALL: [TextSetting; 8] = [
        Self::TerminalFont,
        Self::EditorFont,
        Self::AutoContinue,
        Self::SttProvider,
        Self::SttWakePhrase,
        Self::TtsSystemVoice,
        Self::TtsProvider,
        Self::Passthrough,
    ];
}

/// What the footer saves or discards while a list item is being edited.
struct EditorContext {
    /// "Editing “Claude”".
    status: SharedString,
    save_label: SharedString,
}

impl MuxelApp {
    /// Width of the section list.
    fn settings_nav_w(&self, window: &Window) -> Pixels {
        window.rem_size() * 13.0
    }

    /// Definite inner width of the settings content pane. The modal card is
    /// `w(settings_size).max_w(relative(0.95))`, so its real width is clamped to
    /// 95% of the window. Subtract the section list and a scrollbar margin. This
    /// is the width content must be sized against absolutely, since the
    /// `overflow_y_scroll` pane gives children no definite width of their own.
    pub(super) fn settings_content_w(&self, window: &mut Window) -> Pixels {
        // The viewport includes the shadow margin drawn around a client-decorated
        // window; the card's 95% is of what is inside it.
        let shadow = gpui_component::window_paddings(window);
        let cap = (window.viewport_size().width - shadow.left - shadow.right) * 0.95;
        let modal_w = if self.settings_size.width < cap {
            self.settings_size.width
        } else {
            cap
        };
        let raw = modal_w - self.settings_nav_w(window) - px(20.0);
        if raw < px(320.0) { px(320.0) } else { raw }
    }

    /// The search query, trimmed; empty when Settings is showing a section.
    pub(super) fn settings_query(&self, cx: &App) -> String {
        self.settings_ui.search.read(cx).value().trim().to_string()
    }

    /// Show `section`, from the top, leaving any search or key recording.
    pub(super) fn set_section(&mut self, section: SettingsSection, cx: &mut Context<Self>) {
        self.settings_ui.section = section;
        self.stop_key_recording(cx);
        self.settings_scroll.set_offset(point(px(0.0), px(0.0)));
        cx.notify();
    }

    /// Open a section from the list. That ends a search: the list is how you
    /// leave the results.
    fn pick_section(
        &mut self,
        section: SettingsSection,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.settings_ui
            .search
            .update(cx, |search, cx| search.set_value("", window, cx));
        self.set_section(section, cx);
    }

    /// How many items a list section holds, shown beside it.
    fn section_count(&self, section: SettingsSection) -> Option<usize> {
        Some(match section {
            SettingsSection::Agents => self.presets.len(),
            SettingsSection::Runners => self.runners.len(),
            SettingsSection::Snippets => self.snippets.len(),
            SettingsSection::Loops => self.loops.len(),
            SettingsSection::Libraries => self.library_hub.configs.len(),
            SettingsSection::Remotes => self.remotes.len(),
            SettingsSection::Identities => self.identities.len(),
            SettingsSection::Projects => self.workspace.projects.len(),
            _ => return None,
        })
    }

    /// The list item being edited, if the footer should save it.
    fn settings_editor(&self) -> Option<EditorContext> {
        let ui = &self.settings_ui;
        let named = |name: Option<String>, save_label: SharedString| {
            let name = name.filter(|name| !name.is_empty())?;
            Some(EditorContext {
                status: tf("Editing “{name}”", &[("name", &name)]).into(),
                save_label,
            })
        };
        match ui.section {
            SettingsSection::Agents => named(
                self.presets
                    .get(ui.selected_preset?)
                    .map(|p| unnamed_or(&p.name)),
                t("Save preset"),
            ),
            SettingsSection::Runners => named(
                self.runners
                    .get(ui.selected_runner?)
                    .map(|r| unnamed_or(&r.name)),
                t("Save runner"),
            ),
            SettingsSection::Snippets => named(
                self.snippets
                    .get(ui.selected_snippet?)
                    .map(|s| unnamed_or(&s.name)),
                t("Save snippet"),
            ),
            SettingsSection::Loops => named(
                self.loops
                    .get(ui.selected_loop?)
                    .map(|l| unnamed_or(&l.name)),
                t("Save loop"),
            ),
            SettingsSection::Remotes => named(
                self.remotes
                    .get(ui.selected_remote?)
                    .map(|h| unnamed_or(&h.name)),
                t("Save host"),
            ),
            SettingsSection::Identities => named(
                self.identities
                    .get(ui.selected_identity?)
                    .map(|i| unnamed_or(&i.name)),
                t("Save identity"),
            ),
            SettingsSection::Projects => named(
                self.workspace
                    .project(ui.selected_project?)
                    .map(|p| unnamed_or(&p.name)),
                t("Save project"),
            ),
            _ => None,
        }
    }

    /// Save the list item the footer is offering to save.
    fn save_settings_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.settings_ui.section {
            SettingsSection::Agents => self.save_preset(cx),
            SettingsSection::Runners => self.save_runner(cx),
            SettingsSection::Snippets => self.save_snippet(cx),
            SettingsSection::Loops => self.save_loop(cx),
            SettingsSection::Remotes => self.save_remote(window, cx),
            SettingsSection::Identities => self.save_identity(window, cx),
            SettingsSection::Projects => self.save_project(cx),
            _ => {}
        }
    }

    /// Put the editor's fields back to what the item was last saved as.
    fn discard_settings_editor(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let ui = &self.settings_ui;
        match ui.section {
            SettingsSection::Agents => {
                if let Some(idx) = ui.selected_preset {
                    self.open_preset_editor(idx, window, cx);
                }
            }
            SettingsSection::Runners => {
                if let Some(idx) = ui.selected_runner {
                    self.open_runner_editor(idx, window, cx);
                }
            }
            SettingsSection::Snippets => {
                if let Some(idx) = ui.selected_snippet {
                    self.open_snippet_editor(idx, window, cx);
                }
            }
            SettingsSection::Loops => {
                if let Some(idx) = ui.selected_loop {
                    self.open_loop_editor(idx, window, cx);
                }
            }
            SettingsSection::Remotes => {
                if let Some(idx) = ui.selected_remote {
                    self.open_remote_editor(idx, window, cx);
                }
            }
            SettingsSection::Identities => {
                if let Some(idx) = ui.selected_identity {
                    self.open_identity_editor(idx, window, cx);
                }
            }
            SettingsSection::Projects => {
                if let Some(pid) = ui.selected_project {
                    self.open_project_editor(pid, window, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    /// Everything Settings can revert, as text: two snapshots are equal exactly
    /// when Revert would change nothing.
    pub(super) fn settings_fingerprint(&self) -> String {
        // Where Settings is shown is not a setting Revert should undo.
        let mut settings = self.settings.clone();
        settings.settings_floating = false;
        let settings = serde_json::to_string(&settings).unwrap_or_default();
        let presets = serde_json::to_string(&self.presets).unwrap_or_default();
        format!(
            "{settings}\n{presets}\n{}\n{}\n{}\n{}\n{}",
            self.theme,
            self.theme_mode,
            self.use_tmux,
            self.use_worktree,
            self.notifications_enabled
        )
    }

    /// Note whether anything differs from when Settings opened. Called after
    /// each save, which is when a setting can have changed.
    pub(super) fn refresh_settings_dirty(&mut self) {
        if !self.show_settings {
            return;
        }
        let Some(opened) = self
            .settings_snapshot
            .as_ref()
            .map(|s| s.fingerprint.clone())
        else {
            return;
        };
        self.settings_dirty = self.settings_fingerprint() != opened;
    }

    /// Put every setting back to how it was when Settings opened, and stay open.
    pub(super) fn revert_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(snap) = self.settings_snapshot.as_ref() else {
            return;
        };
        let before = self.settings.clone();
        self.settings = snap.settings.clone();
        self.settings.settings_floating = before.settings_floating;
        self.presets = snap.presets.clone();
        self.theme = snap.theme.clone();
        self.theme_mode = snap.theme_mode.clone();
        self.use_tmux = snap.use_tmux;
        self.use_worktree = snap.use_worktree;
        self.notifications_enabled = snap.notifications;
        // Re-apply the live visual state that editing may have changed.
        let zoom = self.settings.zoom;
        let ui_font = self.settings.ui_font_size;
        theme::apply_initial_theme(&self.theme.clone(), cx);
        theme::set_ui_font_size(ui_font, cx);
        theme::set_ui_scale(zoom, cx);
        self.refresh_terminal_config(cx);
        self.refresh_terminal_palettes(cx);
        self.apply_editor_config(window, cx);
        rebind_keys(&before, &self.settings.clone(), cx);
        self.persist_settings();
        // The editors and text fields still show what was reverted.
        self.stop_key_recording(cx);
        if self
            .settings_ui
            .selected_preset
            .is_some_and(|idx| idx >= self.presets.len())
        {
            self.settings_ui.selected_preset = None;
        } else if let Some(idx) = self.settings_ui.selected_preset {
            self.open_preset_editor(idx, window, cx);
        }
        self.load_settings_inputs(window, cx);
        self.settings_dirty = false;
        cx.notify();
    }

    /// Seed every text field of Settings from the current settings.
    pub(super) fn load_settings_inputs(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.load_appearance_inputs(window, cx);
        self.load_behavior_inputs(window, cx);
        self.load_speech_inputs(window, cx);
        self.load_read_aloud_inputs(window, cx);
        self.load_keybinding_inputs(window, cx);
    }

    /// Apply one text field if what is typed differs from the saved setting.
    pub(super) fn apply_text_setting(
        &mut self,
        which: TextSetting,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if !self.show_settings {
            return;
        }
        let ui = &self.settings_ui;
        let typed = |input: &Entity<InputState>| input.read(cx).value().trim().to_string();
        match which {
            TextSetting::TerminalFont => {
                if ui.font_family.read(cx).value().as_ref() != self.settings.font_family {
                    self.apply_font_family(cx);
                }
            }
            TextSetting::EditorFont => {
                if ui.editor_font_family.read(cx).value().as_ref()
                    != self.settings.editor_font_family
                {
                    self.apply_editor_font_family(window, cx);
                }
            }
            TextSetting::AutoContinue => {
                if ui.auto_continue_message.read(cx).value().as_ref()
                    != self.settings.auto_continue_message
                {
                    self.apply_auto_continue_message(window, cx);
                }
            }
            TextSetting::SttProvider => {
                if typed(&ui.stt_provider_url) != self.settings.stt_provider_url
                    || typed(&ui.stt_provider_model) != self.settings.stt_provider_model
                    || typed(&ui.stt_language) != self.settings.stt_language
                {
                    self.apply_stt_provider(cx);
                    // A blank URL saves as the default; show what was saved.
                    let url = self.settings.stt_provider_url.clone();
                    self.settings_ui
                        .stt_provider_url
                        .update(cx, |input, cx| input.set_value(url, window, cx));
                }
            }
            TextSetting::SttWakePhrase => {
                if typed(&ui.stt_wake_phrase) != self.settings.stt_wake_phrase {
                    self.apply_stt_wake_phrase(window, cx);
                }
            }
            TextSetting::TtsSystemVoice => {
                // Only where the OS can't list its voices is the field shown;
                // otherwise the picker owns this setting and the field is stale.
                let listed = ui
                    .tts_system_voices
                    .as_ref()
                    .is_some_and(|voices| !voices.is_empty());
                if !listed && typed(&ui.tts_system_voice) != self.settings.tts_system_voice {
                    self.apply_tts_system_voice(cx);
                }
            }
            TextSetting::TtsProvider => {
                if typed(&ui.tts_provider_voice) != self.settings.tts_provider_voice
                    || typed(&ui.tts_provider_model) != self.settings.tts_provider_model
                {
                    self.apply_tts_provider(cx);
                    let voice = self.settings.tts_provider_voice.clone();
                    let model = self.settings.tts_provider_model.clone();
                    self.settings_ui
                        .tts_provider_voice
                        .update(cx, |input, cx| input.set_value(voice, window, cx));
                    self.settings_ui
                        .tts_provider_model
                        .update(cx, |input, cx| input.set_value(model, window, cx));
                }
            }
            TextSetting::Passthrough => {
                let typed = parse_passthrough(&ui.passthrough_keys.read(cx).value());
                if typed != self.settings.terminal_passthrough_keys {
                    self.apply_passthrough_keys(cx);
                }
            }
        }
    }

    /// Apply whatever is still typed in a text field. A field applies when it
    /// loses focus, but closing the window doesn't take focus away first.
    pub(super) fn flush_settings_text(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        for which in TextSetting::ALL {
            self.apply_text_setting(which, window, cx);
        }
    }

    /// Close Settings, keeping every change.
    pub(super) fn dismiss_settings(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.flush_settings_text(window, cx);
        self.persist_settings();
        self.close_settings(cx);
    }

    pub(super) fn render_settings_modal(
        &self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let form = self.form(cx);
        let pal = form.pal;
        modal_backdrop()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, window, cx| this.dismiss_settings(window, cx)),
            )
            .on_mouse_move(cx.listener(|this, ev: &MouseMoveEvent, _w, cx| {
                if let Some((start, base)) = this.settings_resize {
                    let w = (f32::from(base.width) + f32::from(ev.position.x - start.x)).max(560.0);
                    let h =
                        (f32::from(base.height) + f32::from(ev.position.y - start.y)).max(360.0);
                    this.settings_size = size(px(w), px(h));
                    cx.notify();
                } else if let Some((start, base)) = this.settings_move {
                    this.settings_offset = point(
                        base.x + (ev.position.x - start.x),
                        base.y + (ev.position.y - start.y),
                    );
                    cx.notify();
                }
            }))
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, _cx| {
                    this.settings_resize = None;
                    this.settings_move = None;
                }),
            )
            .child(
                div()
                    .w(self.settings_size.width)
                    .h(self.settings_size.height)
                    .relative()
                    // Shift from the centred position by the drag offset.
                    .left(self.settings_offset.x)
                    .top(self.settings_offset.y)
                    .max_w(relative(0.95))
                    .max_h(relative(0.9))
                    .flex()
                    .flex_row()
                    .bg(pal.background)
                    .text_color(pal.text)
                    .border_1()
                    .border_color(pal.card_border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .overflow_hidden()
                    // Clicks inside the card must not reach the backdrop.
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(self.render_settings_nav(&form, window, cx))
                    .child(self.render_settings_main(&form, window, cx))
                    .child(
                        // Bottom-right corner: drag to resize the modal.
                        div()
                            .absolute()
                            .bottom_0()
                            .right_0()
                            .size(px(18.0))
                            .cursor(CursorStyle::ResizeUpLeftDownRight)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                                    cx.stop_propagation();
                                    this.settings_resize = Some((ev.position, this.settings_size));
                                }),
                            )
                            .child(
                                div()
                                    .absolute()
                                    .bottom(px(3.0))
                                    .right(px(3.0))
                                    .size(px(9.0))
                                    .border_b_2()
                                    .border_r_2()
                                    .border_color(pal.dim),
                            ),
                    ),
            )
            .into_any_element()
    }

    /// Everything right of the section list: header, scrolling page, footer.
    fn render_settings_main(
        &self,
        form: &Form,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Div {
        v_flex()
            .flex_1()
            .min_w_0()
            .h_full()
            .child(self.render_settings_header(form, cx))
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .child(self.render_settings_content(form, window, cx)),
            )
            .child(self.render_settings_footer(form, cx))
    }

    /// Settings in a window of its own: the same section list and page under a
    /// title bar, filling the window. The OS moves and resizes it, so it can
    /// sit outside the main window, on another monitor, or beside the panes a
    /// change affects.
    pub(super) fn render_settings_window(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // The viewport includes the shadow margin drawn around a client-decorated
        // window; the page is laid out inside it.
        let shadow = gpui_component::window_paddings(window);
        let raw = window.viewport_size().width
            - shadow.left
            - shadow.right
            - self.settings_nav_w(window)
            - px(20.0);
        self.settings_pane_w = if raw < px(320.0) { px(320.0) } else { raw };
        self.settings_rem = window.rem_size();
        let form = self.form(cx);
        let pal = form.pal;
        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(pal.background)
            .text_color(pal.text)
            .child(
                div()
                    .flex_none()
                    .on_mouse_down(MouseButton::Left, |_e, window, cx| {
                        // Claim the title-bar press so dragging the window
                        // doesn't start a text selection in the page below.
                        window.prevent_default();
                        gpui_component::GlobalState::suppress_text_selection(cx);
                    })
                    .child(
                        TitleBar::new()
                            .on_close_window(
                                cx.listener(|this, _e, window, cx| {
                                    this.dismiss_settings(window, cx)
                                }),
                            )
                            .child(
                                div()
                                    .w_full()
                                    .px_3()
                                    .text_sm()
                                    .font_weight(FontWeight::MEDIUM)
                                    .child(t("Settings")),
                            ),
                    ),
            )
            .child(
                div()
                    .flex_1()
                    .min_h_0()
                    .flex()
                    .flex_row()
                    .child(self.render_settings_nav(&form, window, cx))
                    .child(self.render_settings_main(&form, window, cx)),
            )
            // A confirmation asked for from here ("Delete agent?") shows here,
            // not on a main window that may be on another monitor.
            .children(
                self.confirm_in_settings()
                    .then(|| self.render_confirm_modal(cx)),
            )
            .into_any_element()
    }

    /// Whether the pending confirmation belongs to Settings' own window: it is
    /// floating and is the window the user is in.
    pub(super) fn confirm_in_settings(&self) -> bool {
        self.confirm.is_some() && self.settings_window.is_some() && self.settings_window_active
    }

    /// Remake Settings' text fields in `window`. A field only receives focus
    /// and blur in the window it was made in, so they are remade when Settings
    /// moves between the main window and a window of its own. What was open
    /// stays open; editors are refilled from what was last saved.
    pub(super) fn rebind_settings_ui(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let query = self.settings_ui.search.read(cx).value().to_string();
        let filter = self.settings_ui.kb_filter.read(cx).value().to_string();
        self.stop_key_recording(cx);
        let fresh = SettingsUi::new(window, cx);
        let old = std::mem::replace(&mut self.settings_ui, fresh);
        self.settings_ui.adopt(old);
        self.settings_input_subs = settings_input_subscriptions(&self.settings_ui, window, cx);
        self.settings_ui_window = Some(window.window_handle().window_id());
        self.load_settings_inputs(window, cx);
        self.settings_ui
            .search
            .update(cx, |input, cx| input.set_value(query, window, cx));
        self.settings_ui
            .kb_filter
            .update(cx, |input, cx| input.set_value(filter, window, cx));
        let ui = &self.settings_ui;
        let (preset, runner, snippet, lp) = (
            ui.selected_preset,
            ui.selected_runner,
            ui.selected_snippet,
            ui.selected_loop,
        );
        let (remote, identity, project, library) = (
            ui.selected_remote,
            ui.selected_identity,
            ui.selected_project,
            ui.selected_library,
        );
        if let Some(idx) = preset {
            self.open_preset_editor(idx, window, cx);
        }
        if let Some(idx) = runner {
            self.open_runner_editor(idx, window, cx);
        }
        if let Some(idx) = snippet {
            self.open_snippet_editor(idx, window, cx);
        }
        if let Some(idx) = lp {
            self.open_loop_editor(idx, window, cx);
        }
        if let Some(idx) = remote {
            self.open_remote_editor(idx, window, cx);
        }
        if let Some(idx) = identity {
            self.open_identity_editor(idx, window, cx);
        }
        if let Some(pid) = project {
            self.open_project_editor(pid, window, cx);
        }
        if let Some(id) = library {
            self.open_library_details(id, window, cx);
        }
        cx.notify();
    }

    /// Open Settings' own window. Deferred: `open_window` draws the new window
    /// at once and its render delegates back into this entity, which the
    /// current update still holds.
    pub(super) fn float_settings(&mut self, cx: &mut Context<Self>) {
        let app = cx.weak_entity();
        let scale = (self.settings.ui_font_size * self.settings.zoom / 16.0).clamp(1.0, 2.5);
        let window_size = size(px(980.0 * scale), px(720.0 * scale));
        cx.defer(move |cx| {
            let opened = cx.open_window(
                gpui::WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(Bounds::centered(
                        None,
                        window_size,
                        cx,
                    ))),
                    app_id: Some("muxel".to_string()),
                    window_min_size: Some(size(px(640.0), px(420.0))),
                    ..crate::window_options()
                },
                {
                    let app = app.clone();
                    move |window, cx| {
                        ui_profile::register_profile_window(
                            ui_profile::ProfileWindowKind::Auxiliary,
                            window,
                        );
                        window.set_window_title("muxel — Settings");
                        // The fields have to belong to this window before it
                        // draws them.
                        if let Some(app) = app.upgrade() {
                            app.update(cx, |this, cx| this.rebind_settings_ui(window, cx));
                        }
                        // Closed by the OS rather than a button of ours: keep
                        // what is still typed in a field, as Done would.
                        let closing = app.clone();
                        window.on_window_should_close(cx, move |window, cx| {
                            if let Some(app) = closing.upgrade() {
                                app.update(cx, |this, cx| this.flush_settings_text(window, cx));
                            }
                            true
                        });
                        let view = cx.new(|cx| SettingsWindow::new(app.clone(), window, cx));
                        cx.new(|cx| {
                            gpui_component::Root::new(view, window, cx).bg(cx.theme().background)
                        })
                    }
                },
            );
            let Some(app) = app.upgrade() else { return };
            app.update(cx, |this, cx| {
                match opened {
                    // Settings may have been closed or docked again meanwhile.
                    Ok(handle) if this.show_settings && this.settings.settings_floating => {
                        this.settings_window = Some(handle);
                        this.settings_window_active = true;
                    }
                    Ok(handle) => {
                        let _ = handle.update(cx, |_, window, _| window.remove_window());
                    }
                    Err(e) => {
                        log::warn!("failed to open the Settings window: {e:#}");
                        // Fall back to the dialog rather than showing nothing.
                        this.settings.settings_floating = false;
                        this.persist_settings();
                    }
                }
                cx.notify();
            });
        });
    }

    /// Close Settings' own window, if it has one. Deferred, since the request
    /// may come from a click inside that window.
    pub(super) fn close_settings_window(&mut self, cx: &mut Context<Self>) {
        if let Some(handle) = self.settings_window.take() {
            // The fields belong to the window that is going away; stop
            // listening to them before it does. They are remade, and listened
            // to again, in whichever window shows Settings next.
            self.settings_input_subs.clear();
            cx.defer(move |cx| {
                let _ = handle.update(cx, |_, window, _| window.remove_window());
            });
        }
    }

    /// Move the open Settings between a dialog over the main window and a
    /// window of its own, and remember the choice for next time.
    fn set_settings_floating(
        &mut self,
        floating: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.settings.settings_floating == floating {
            return;
        }
        // The fields are remade for the other window; keep what is typed.
        self.flush_settings_text(window, cx);
        self.settings.settings_floating = floating;
        self.persist_settings();
        if floating {
            self.settings_move = None;
            self.settings_resize = None;
            self.float_settings(cx);
        } else {
            self.settings_window_active = false;
            self.close_settings_window(cx);
        }
        cx.notify();
    }

    /// Begin dragging the window by one of its two title strips.
    fn settings_drag_start(&mut self, ev: &MouseDownEvent, cx: &mut Context<Self>) {
        cx.stop_propagation();
        self.settings_move = Some((ev.position, self.settings_offset));
    }

    fn render_settings_nav(
        &self,
        form: &Form,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let pal = form.pal;
        let rem = form.rem;
        let nav_w = self.settings_nav_w(window);
        let item_w = nav_w - rem;
        let current = self.settings_ui.section;
        let words = model::query_words(&self.settings_query(cx));
        let searching = !words.is_empty();
        let hits = if searching {
            self.search_hits(form, &words, cx)
        } else {
            Vec::new()
        };

        let mut items = v_flex().w(nav_w).px(rem * 0.5).pb_2();
        for (group, sections) in SettingsSection::groups() {
            items = items.child(
                div()
                    .w(item_w)
                    .px_2()
                    .pt_3()
                    .pb_1()
                    .text_xs()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(pal.dim)
                    .child(group),
            );
            for &section in sections {
                let found = hits
                    .iter()
                    .find(|(hit, _)| *hit == section)
                    .map(|(_, count)| *count);
                let selected = !searching && section == current;
                let fg = if selected {
                    pal.nav_selected_text
                } else if searching && found.is_none() {
                    pal.dim
                } else {
                    pal.text
                };
                let icon = match section.icon() {
                    settings_view::SectionIcon::Named(name) => Icon::new(name),
                    settings_view::SectionIcon::Path(path) => Icon::empty().path(path),
                };
                // While searching the number is how many settings matched; a
                // list section otherwise shows how many items it holds.
                let (count, count_fg) = match (searching, found) {
                    (true, Some(found)) => (Some(found), pal.accent),
                    (true, None) => (None, pal.dim),
                    (false, _) => (
                        self.section_count(section),
                        if selected { fg } else { pal.dim },
                    ),
                };
                let mut row = div()
                    .id(SharedString::from(format!("settings-nav-{section:?}")))
                    .w(item_w)
                    .h(rem * 1.75)
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded(form.radius)
                    .cursor_pointer()
                    .text_color(fg)
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.pick_section(section, window, cx)
                    }))
                    .child(icon.small().text_color(fg))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .child(section.title()),
                    )
                    .children(count.map(|count| {
                        div()
                            .flex_none()
                            .text_xs()
                            .text_color(count_fg)
                            .child(count.to_string())
                    }));
                row = if selected {
                    row.bg(pal.nav_selected).font_weight(FontWeight::MEDIUM)
                } else {
                    let hover = pal.track.opacity(0.6);
                    row.hover(move |style| style.bg(hover))
                };
                items = items.child(row);
            }
        }

        v_flex()
            .w(nav_w)
            .flex_none()
            .h_full()
            .bg(pal.nav)
            .border_r_1()
            .border_color(pal.card_border)
            // As a dialog this strip names it and drags it; in its own window
            // the title bar does both.
            .child(if self.settings.settings_floating {
                div().w(nav_w).h(rem * 0.75).flex_none()
            } else {
                div()
                    .w(nav_w)
                    .flex_none()
                    .px(rem)
                    .pt(rem * 0.875)
                    .pb(rem * 0.5)
                    .font_weight(FontWeight::SEMIBOLD)
                    .cursor(CursorStyle::OpenHand)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, ev: &MouseDownEvent, _w, cx| {
                            this.settings_drag_start(ev, cx)
                        }),
                    )
                    .child(t("Settings"))
            })
            .child(
                div().w(nav_w).flex_none().px(rem * 0.75).pb_1().child(
                    div().w(nav_w - rem * 1.5).child(
                        Input::new(&self.settings_ui.search)
                            .prefix(Icon::new(IconName::Search).small().text_color(pal.dim))
                            .cleanable(true),
                    ),
                ),
            )
            .child(
                // The list scrolls on its own, so its last entries stay
                // reachable however short the window is.
                div()
                    .id("settings-nav")
                    .flex_1()
                    .min_h_0()
                    .overflow_y_scroll()
                    .child(items),
            )
    }

    fn render_settings_header(&self, form: &Form, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = form.pal;
        let query = self.settings_query(cx);
        let words = model::query_words(&query);
        let (title, description): (SharedString, SharedString) = if words.is_empty() {
            let section = self.settings_ui.section;
            (section.title(), section.description())
        } else {
            let hits = self.search_hits(form, &words, cx);
            let total: usize = hits.iter().map(|(_, count)| count).sum();
            let found = match total {
                0 => t("Nothing matches. Try a shorter word."),
                _ => tn(
                    "{count} setting found. Change it here, or open its section.",
                    "{count} settings found. Change them here, or open a section.",
                    total,
                    &[("count", &total.to_string())],
                )
                .into(),
            };
            (
                tf("Results for “{query}”", &[("query", &query)]).into(),
                found,
            )
        };
        let floating = self.settings.settings_floating;
        let bar = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_3()
            .pl(self.settings_gutter())
            .pr(form.rem)
            .py(form.rem * 0.75)
            .border_b_1()
            .border_color(pal.divider);
        // As a dialog the header drags it; a window has its title bar for that.
        let bar = if floating {
            bar
        } else {
            bar.cursor(CursorStyle::OpenHand).on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, ev: &MouseDownEvent, _w, cx| this.settings_drag_start(ev, cx)),
            )
        };
        let (float_icon, float_tip) = if floating {
            (IconName::Minimize, t("Dock Settings into the main window"))
        } else {
            (
                IconName::ExternalLink,
                t("Float Settings in a window of its own"),
            )
        };
        bar.child(
            v_flex()
                .flex_1()
                .min_w_0()
                .gap_0p5()
                .child(
                    div()
                        .text_lg()
                        .font_weight(FontWeight::SEMIBOLD)
                        .truncate()
                        .child(title),
                )
                .child(
                    div()
                        .text_xs()
                        .truncate()
                        .text_color(pal.dim)
                        .child(description),
                ),
        )
        .child(
            // Pressing a button here must not begin a drag.
            div()
                .flex_none()
                .flex()
                .items_center()
                .gap_1()
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    Button::new("settings-float")
                        .ghost()
                        .icon(float_icon)
                        .tooltip(float_tip)
                        .on_click(cx.listener(move |this, _ev, window, cx| {
                            this.set_settings_floating(!floating, window, cx)
                        })),
                )
                // The window's title bar has its own close button.
                .children((!floating).then(|| {
                    Button::new("close-settings")
                        .ghost()
                        .icon(IconName::Close)
                        .tooltip(t("Close"))
                        .on_click(
                            cx.listener(|this, _ev, window, cx| this.dismiss_settings(window, cx)),
                        )
                })),
        )
    }

    fn render_settings_footer(&self, form: &Form, cx: &mut Context<Self>) -> impl IntoElement {
        let pal = form.pal;
        let bar = div()
            .flex_none()
            .flex()
            .items_center()
            .gap_2()
            .pl(self.settings_gutter())
            .pr(form.rem * 1.75)
            .py_2()
            .border_t_1()
            .border_color(pal.divider);
        let status = |text: SharedString| {
            div()
                .flex_1()
                .min_w_0()
                .truncate()
                .text_xs()
                .text_color(pal.dim)
                .child(text)
        };
        // A list item's fields are read back on Save, so while one is open the
        // footer saves that item; settings themselves apply as they change.
        let searching = !self.settings_query(cx).is_empty();
        if let Some(editor) = self.settings_editor().filter(|_| !searching) {
            return bar
                .child(status(editor.status))
                .child(
                    Button::new("settings-editor-discard")
                        .ghost()
                        .label(t("Discard changes"))
                        .tooltip(t("Put the fields back to what was last saved"))
                        .on_click(cx.listener(|this, _e, window, cx| {
                            this.discard_settings_editor(window, cx)
                        })),
                )
                .child(
                    Button::new("settings-editor-save")
                        .primary()
                        .label(editor.save_label)
                        .on_click(cx.listener(|this, _e, window, cx| {
                            this.save_settings_editor(window, cx)
                        })),
                );
        }
        bar.child(status(if self.settings_dirty {
            t("Changes are applied and saved as you make them.")
        } else {
            t("Changes apply as you make them.")
        }))
        .child(
            Button::new("settings-revert")
                .ghost()
                .label(t("Revert changes"))
                .tooltip(t(
                    "Put settings back to how they were when you opened this window",
                ))
                .disabled(!self.settings_dirty)
                .on_click(cx.listener(|this, _e, window, cx| this.revert_settings(window, cx))),
        )
        .child(
            Button::new("settings-done")
                .primary()
                .label(t("Done"))
                .on_click(cx.listener(|this, _e, window, cx| this.dismiss_settings(window, cx))),
        )
    }

    fn render_settings_content(
        &self,
        form: &Form,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let words = model::query_words(&self.settings_query(cx));
        let content = if words.is_empty() {
            self.render_settings_section(form, window, cx)
        } else {
            self.render_search_results(form, &words, cx)
        };
        // Centre the page when the window is wider than the page's cap.
        let gutter = self.settings_gutter();
        let spare = self.settings_pane_w - gutter * 2.0 - form.w;
        let inset = if spare > px(0.0) {
            spare * 0.5
        } else {
            px(0.0)
        };
        div()
            .relative()
            .size_full()
            .child(
                div()
                    .id("settings-content")
                    .size_full()
                    .overflow_y_scroll()
                    .track_scroll(&self.settings_scroll)
                    // The scroll container itself doesn't give children a
                    // definite width (percentage widths collapse), so put
                    // the content in an absolute-width inner block.
                    .child(
                        div()
                            .w(self.settings_pane_w)
                            .pl(gutter + inset)
                            .pr(gutter)
                            .pt(form.rem * 1.25)
                            .pb(form.rem * 1.75)
                            .child(content),
                    ),
            )
            .child(
                div().absolute().inset_0().child(
                    Scrollbar::new(&self.settings_scroll)
                        .id("settings-sb")
                        .axis(ScrollbarAxis::Vertical),
                ),
            )
    }

    fn render_settings_section(
        &self,
        form: &Form,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        match self.settings_ui.section {
            SettingsSection::Agents => self.render_settings_agents(form, cx),
            SettingsSection::OutsideControl => self.render_settings_outside_control(form, cx),
            SettingsSection::Runners => self.render_settings_runners(form, cx),
            SettingsSection::Snippets => self.render_settings_snippets(form, cx),
            SettingsSection::Loops => self.render_settings_loops(form, cx),
            SettingsSection::Libraries => self.render_settings_libraries(form, cx),
            SettingsSection::Remotes => self.render_settings_remotes(form, cx),
            SettingsSection::Identities => self.render_settings_identities(form, cx),
            SettingsSection::Keybindings => self.render_settings_keybindings(form, cx),
            section => {
                let mut blocks = self.settings_blocks(section, form, false, cx);
                if section == SettingsSection::Projects {
                    blocks.push(Block::Custom(self.render_project_list(form, cx)));
                }
                form.page(blocks).into_any_element()
            }
        }
    }

    /// The rows of a section that is made of rows, for its page or (with
    /// `for_search`) for the search to pick from. Sections that are list
    /// editors or a walkthrough have none; the search finds their items by name.
    fn settings_blocks(
        &self,
        section: SettingsSection,
        form: &Form,
        for_search: bool,
        cx: &mut Context<Self>,
    ) -> Vec<Block> {
        match section {
            SettingsSection::Appearance => self.appearance_blocks(form, for_search, cx),
            SettingsSection::Behavior => self.general_blocks(form, cx),
            SettingsSection::Editor => self.editor_blocks(form, cx),
            SettingsSection::Sounds => self.notification_blocks(form, cx),
            SettingsSection::Speech => self.dictation_blocks(form, cx),
            SettingsSection::ReadAloud => self.read_aloud_blocks(form, cx),
            SettingsSection::Projects => self.project_blocks(form, cx),
            _ => Vec::new(),
        }
    }

    /// The settings a search found, grouped the way the results show them:
    /// one group per section heading that had a match.
    fn search_groups(
        &self,
        form: &Form,
        words: &[String],
        cx: &mut Context<Self>,
    ) -> Vec<(SettingsSection, Group)> {
        let mut found: Vec<(SettingsSection, Group)> = Vec::new();
        for section in SettingsSection::all() {
            let title = section.title();
            for block in self.settings_blocks(section, form, true, cx) {
                let Block::Group(group) = block else {
                    continue;
                };
                let rows: Vec<Row> = group
                    .rows
                    .into_iter()
                    .filter(|row| row.matches(words, &[group.title.as_ref(), title.as_ref()]))
                    .collect();
                if !rows.is_empty() {
                    let heading: SharedString = if group.title.is_empty() {
                        title.clone()
                    } else {
                        format!("{title} · {}", group.title).into()
                    };
                    found.push((section, Group::new(heading, rows)));
                }
            }
            let rows = match section {
                SettingsSection::Keybindings => self.keybinding_rows(form, words, cx),
                _ => self.search_item_rows(section, words, cx),
            };
            if !rows.is_empty() {
                found.push((section, Group::new(title, rows)));
            }
        }
        found
    }

    /// How many settings matched in each section, in list order.
    fn search_hits(
        &self,
        form: &Form,
        words: &[String],
        cx: &mut Context<Self>,
    ) -> Vec<(SettingsSection, usize)> {
        let mut hits: Vec<(SettingsSection, usize)> = Vec::new();
        for (section, group) in self.search_groups(form, words, cx) {
            match hits.last_mut() {
                Some((last, count)) if *last == section => *count += group.rows.len(),
                _ => hits.push((section, group.rows.len())),
            }
        }
        hits
    }

    /// The items of a list section whose name matches: each opens in its editor.
    fn search_item_rows(
        &self,
        section: SettingsSection,
        words: &[String],
        cx: &mut Context<Self>,
    ) -> Vec<Row> {
        let title = section.title();
        let open = |ix: usize, name: &str| -> Option<Row> {
            if !model::matches(words, &[name, title.as_ref()]) {
                return None;
            }
            let button = Button::new(SharedString::from(format!("search-open-{section:?}-{ix}")))
                .outline()
                .small()
                .label(t("Open"))
                .on_click(cx.listener(move |this, _e, window, cx| {
                    this.open_search_item(section, ix, window, cx)
                }));
            Some(Row::new(unnamed_or(name), button))
        };
        let names: Vec<String> = match section {
            SettingsSection::Agents => self.presets.iter().map(|p| p.name.clone()).collect(),
            SettingsSection::Runners => self.runners.iter().map(|r| r.name.clone()).collect(),
            SettingsSection::Snippets => self.snippets.iter().map(|s| s.name.clone()).collect(),
            SettingsSection::Loops => self.loops.iter().map(|l| l.name.clone()).collect(),
            SettingsSection::Remotes => self.remotes.iter().map(|h| h.name.clone()).collect(),
            SettingsSection::Identities => self.identities.iter().map(|i| i.name.clone()).collect(),
            SettingsSection::Projects => self
                .workspace
                .projects
                .iter()
                .map(|p| p.name.clone())
                .collect(),
            _ => Vec::new(),
        };
        names
            .iter()
            .enumerate()
            .filter_map(|(ix, name)| open(ix, name))
            .collect()
    }

    /// Open the `ix`th item of a list section in its editor, leaving the search.
    fn open_search_item(
        &mut self,
        section: SettingsSection,
        ix: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.pick_section(section, window, cx);
        match section {
            SettingsSection::Agents => self.open_preset_editor(ix, window, cx),
            SettingsSection::Runners => self.open_runner_editor(ix, window, cx),
            SettingsSection::Snippets => self.open_snippet_editor(ix, window, cx),
            SettingsSection::Loops => self.open_loop_editor(ix, window, cx),
            SettingsSection::Remotes => self.open_remote_editor(ix, window, cx),
            SettingsSection::Identities => self.open_identity_editor(ix, window, cx),
            SettingsSection::Projects => {
                if let Some(pid) = self.workspace.projects.get(ix).map(|p| p.id) {
                    self.open_project_editor(pid, window, cx);
                }
            }
            _ => {}
        }
        cx.notify();
    }

    fn render_search_results(
        &self,
        form: &Form,
        words: &[String],
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let pal = form.pal;
        let groups = self.search_groups(form, words, cx);
        if groups.is_empty() {
            return form.empty(t(
                "No setting, shortcut or saved item matches. Clear the search to browse the sections.",
            ));
        }
        let mut col = v_flex().w(form.w).gap(form.rem * 1.25);
        for (section, group) in groups {
            let heading = div()
                .w(form.w)
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .truncate()
                        .text_xs()
                        .font_weight(FontWeight::SEMIBOLD)
                        .text_color(pal.dim)
                        .child(group.title.clone()),
                )
                .child(
                    Button::new(SharedString::from(format!(
                        "search-section-{section:?}-{}",
                        group.title
                    )))
                    .ghost()
                    .xsmall()
                    .label(tf("Open {section}", &[("section", &section.title())]))
                    .on_click(cx.listener(move |this, _e, window, cx| {
                        this.pick_section(section, window, cx)
                    })),
                );
            col = col.child(
                v_flex()
                    .gap_2()
                    .child(heading)
                    .child(form.group(Group::new("", group.rows), words)),
            );
        }
        col.into_any_element()
    }
}

/// Root view of Settings' own window. All state lives in [`MuxelApp`]; render
/// delegates into it, so every `cx.listener` handler works unchanged here.
pub(super) struct SettingsWindow {
    app: WeakEntity<MuxelApp>,
}

impl SettingsWindow {
    fn new(app: WeakEntity<MuxelApp>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        if let Some(entity) = app.upgrade() {
            // Settings change underneath it (a theme, a status tick's counts).
            cx.observe(&entity, |_, _, cx| cx.notify()).detach();
        }
        // Tell the app which window the user is in, so a confirmation asked
        // for from Settings appears in that window.
        cx.observe_window_activation(window, |this, window, cx| {
            let active = window.is_window_active();
            if let Some(app) = this.app.upgrade() {
                app.update(cx, |app, cx| {
                    app.settings_window_active = active;
                    cx.notify();
                });
            }
        })
        .detach();
        Self { app }
    }
}

impl Render for SettingsWindow {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        match self.app.upgrade() {
            Some(app) => app.update(cx, |app, cx| app.render_settings_window(window, cx)),
            None => div().into_any_element(),
        }
    }
}

/// Wire up Settings' text fields: the search and the shortcut filter re-render
/// as you type, and a text setting applies when its field is left or Enter is
/// pressed. Returned so they can be dropped when the fields are remade.
pub(super) fn settings_input_subscriptions(
    ui: &SettingsUi,
    window: &mut Window,
    cx: &mut Context<MuxelApp>,
) -> Vec<Subscription> {
    let mut subs = Vec::new();
    for input in [&ui.search, &ui.kb_filter] {
        subs.push(cx.subscribe_in(
            input,
            window,
            |this, _input, ev: &InputEvent, _window, cx| {
                if matches!(ev, InputEvent::Change) {
                    // Results and filtered shortcuts show from the top.
                    this.settings_scroll.set_offset(point(px(0.0), px(0.0)));
                    cx.notify();
                }
            },
        ));
    }
    for (input, which) in [
        (&ui.font_family, TextSetting::TerminalFont),
        (&ui.editor_font_family, TextSetting::EditorFont),
        (&ui.auto_continue_message, TextSetting::AutoContinue),
        (&ui.stt_provider_url, TextSetting::SttProvider),
        (&ui.stt_provider_model, TextSetting::SttProvider),
        (&ui.stt_language, TextSetting::SttProvider),
        (&ui.stt_wake_phrase, TextSetting::SttWakePhrase),
        (&ui.tts_system_voice, TextSetting::TtsSystemVoice),
        (&ui.tts_provider_voice, TextSetting::TtsProvider),
        (&ui.tts_provider_model, TextSetting::TtsProvider),
        (&ui.passthrough_keys, TextSetting::Passthrough),
    ] {
        subs.push(cx.subscribe_in(
            input,
            window,
            move |this, _input, ev: &InputEvent, window, cx| {
                if matches!(ev, InputEvent::PressEnter { .. } | InputEvent::Blur) {
                    this.apply_text_setting(which, window, cx);
                }
            },
        ));
    }
    subs
}

/// A list item's name as Settings shows it: something even when it has none.
pub(super) fn unnamed_or(name: &str) -> String {
    if name.trim().is_empty() {
        t("(unnamed)").to_string()
    } else {
        name.to_string()
    }
}

/// The chords typed into the passthrough field: comma, space or newline
/// separated.
pub(super) fn parse_passthrough(text: &str) -> Vec<String> {
    text.split([',', ' ', '\n'])
        .map(str::trim)
        .filter(|chord| !chord.is_empty())
        .map(str::to_string)
        .collect()
}
