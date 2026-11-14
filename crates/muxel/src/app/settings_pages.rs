//! The Settings sections that are plain groups of rows: Appearance, General,
//! Editor, Dictation, Read aloud and the top of Projects. Each returns its
//! blocks, so the search can pick rows out of them as well as the page.

use super::settings_kit::{Block, Click, Form, Group, ListItem, Row, Seg, delete_button};
use super::settings_shell::unnamed_or;
use super::*;

/// A stepper's − or + handler.
fn step(on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static) -> Option<Click> {
    Some(Box::new(on_click))
}

impl MuxelApp {
    /// A switch bound to one boolean setting.
    pub(super) fn setting_switch(
        &self,
        form: &Form,
        id: &'static str,
        on: bool,
        cx: &mut Context<Self>,
        set: impl Fn(&mut muxel_core::Settings, bool) + 'static,
    ) -> Stateful<Div> {
        form.switch(
            id,
            on,
            true,
            cx.listener(move |this, on: &bool, _w, cx| {
                set(&mut this.settings, *on);
                this.persist_settings();
                cx.notify();
            }),
        )
    }

    /// One choice of a segmented control that sets a setting when picked.
    pub(super) fn setting_seg(
        &self,
        label: impl Into<SharedString>,
        selected: bool,
        cx: &mut Context<Self>,
        pick: impl Fn(&mut muxel_core::Settings) + 'static,
    ) -> Seg {
        Seg::new(
            label,
            selected,
            cx.listener(move |this, _e, _w, cx| {
                pick(&mut this.settings);
                this.persist_settings();
                cx.notify();
            }),
        )
    }

    // ===== Appearance =====

    /// The theme chooser as a menu: what the search shows, where the swatch
    /// grid would not fit in a row.
    fn theme_menu(&self, cx: &mut Context<Self>) -> AnyElement {
        let current_theme = cx.theme().theme_name().clone();
        DropdownButton::new("settings-theme")
            .small()
            .button(
                Button::new("settings-theme-btn")
                    .outline()
                    .icon(IconName::Palette)
                    .label(current_theme),
            )
            .dropdown_menu(move |mut menu, _window, cx| {
                for name in theme::theme_names(cx) {
                    menu = menu.menu(name.clone(), Box::new(crate::theme::SwitchTheme(name)));
                }
                menu.scrollable(true)
            })
            .into_any_element()
    }

    /// A pane drawn in the active theme, terminal font and border style, so a
    /// change shows before the window is closed.
    fn render_theme_preview(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let th = cx.theme();
        let border = match self.settings.pane_border.as_str() {
            "off" => th.background,
            "bold" => th.muted_foreground,
            _ => th.border,
        };
        let family: SharedString = if self.settings.font_family.trim().is_empty() {
            th.mono_font_family.clone()
        } else {
            self.settings.font_family.clone().into()
        };
        let size = px(self.settings.font_size * self.settings.zoom);
        let (fg, dim) = (th.foreground, th.muted_foreground);
        let (c_blue, c_green, c_yellow, c_red) = (th.blue, th.green, th.yellow, th.red);
        let line = || div().flex().gap_2();
        div()
            .w(form.w)
            .rounded(th.radius_lg)
            .overflow_hidden()
            .bg(th.background)
            .border_1()
            .border_color(border)
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_2()
                    .py_1()
                    .bg(th.title_bar)
                    .border_b_1()
                    .border_color(th.border)
                    .text_xs()
                    .child(
                        div()
                            .px_2()
                            .py_0p5()
                            .rounded(th.radius)
                            .bg(th.background)
                            .text_color(fg)
                            .child(t("agent")),
                    )
                    .child(div().px_2().text_color(fg.opacity(0.7)).child(t("shell")))
                    .child(div().flex_1())
                    .child(div().px_1().text_color(fg.opacity(0.7)).child(t("Preview"))),
            )
            .child(
                v_flex()
                    .px_3()
                    .py_2()
                    .font_family(family)
                    .text_size(size)
                    .text_color(fg)
                    .child(
                        line()
                            .child(div().text_color(c_blue).child("~/code/api"))
                            .child("on")
                            .child(div().text_color(c_yellow).child("main")),
                    )
                    .child(
                        line()
                            .child(div().text_color(c_green).child("❯"))
                            .child("cargo test --workspace"),
                    )
                    .child(
                        line()
                            .child(
                                div()
                                    .text_color(dim)
                                    .child("test pane::split_keeps_sizes ..."),
                            )
                            .child(div().text_color(c_green).child("ok")),
                    )
                    .child(
                        line()
                            .child(
                                div()
                                    .text_color(dim)
                                    .child("test worktree::name_collision ..."),
                            )
                            .child(div().text_color(c_red).child("FAILED")),
                    ),
            )
            .into_any_element()
    }

    /// Every theme as a swatch; click one to apply it.
    fn render_theme_picker(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let pal = form.pal;
        let rem = form.rem;
        let swatches = &self.settings_ui.theme_swatches;
        let current = cx.theme().theme_name().clone();
        let gap = rem * 0.75;
        // As many columns as fit at a comfortable swatch width.
        let cols = (((form.w + gap) / (rem * 10.0 + gap)).floor() as usize).clamp(2, 6);
        let cell_w = (form.w - gap * (cols as f32 - 1.0)) / cols as f32;
        let expanded = self.settings_ui.themes_expanded;

        // Collapsed: two rows, the theme in use first so it is always in view.
        let mut order: Vec<usize> = (0..swatches.len()).collect();
        if !expanded {
            if let Some(at) = swatches.iter().position(|s| s.name == current) {
                order.remove(at);
                order.insert(0, at);
            }
            order.truncate(cols * 2);
        }

        let mut grid = div().w(form.w).flex().flex_wrap().gap(gap);
        for ix in order {
            let swatch = &swatches[ix];
            let selected = swatch.name == current;
            let name = swatch.name.clone();
            let pad = rem * 0.625;
            let bar_w = cell_w - pad * 2.0 - px(4.0);
            let mut face = v_flex()
                .w(cell_w)
                .h(rem * 3.875)
                .p(pad)
                .gap(px(5.0))
                .rounded(form.radius)
                .bg(swatch.background)
                .border_2()
                .border_color(if selected { pal.accent } else { swatch.border });
            for (color, share) in swatch.bars.iter().zip([0.6_f32, 0.4, 0.72, 0.3]) {
                face = face.child(div().w(bar_w * share).h(px(5.0)).rounded_full().bg(*color));
            }
            let mut label = div().w(cell_w).truncate().text_xs().child(name.clone());
            label = if selected {
                label.text_color(pal.text).font_weight(FontWeight::MEDIUM)
            } else {
                label.text_color(pal.dim)
            };
            grid = grid.child(
                v_flex()
                    .id(SharedString::from(format!("theme-swatch-{ix}")))
                    .w(cell_w)
                    .gap_1()
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _e, _w, cx| this.set_theme(name.clone(), cx)))
                    .child(face)
                    .child(label),
            );
        }

        let toggle_label = if expanded {
            t("Show fewer")
        } else {
            tf(
                "Browse all {count}",
                &[("count", &swatches.len().to_string())],
            )
            .into()
        };
        v_flex()
            .w(form.w)
            .gap_2()
            .child(
                div()
                    .w(form.w)
                    .flex()
                    .items_center()
                    .child(
                        div()
                            .flex_1()
                            .text_xs()
                            .font_weight(FontWeight::SEMIBOLD)
                            .text_color(pal.dim)
                            .child(t("Theme")),
                    )
                    .child(
                        Button::new("themes-expand")
                            .ghost()
                            .xsmall()
                            .label(toggle_label)
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.settings_ui.themes_expanded =
                                    !this.settings_ui.themes_expanded;
                                cx.notify();
                            })),
                    ),
            )
            .child(grid)
            .into_any_element()
    }

    pub(super) fn appearance_blocks(
        &self,
        form: &Form,
        for_search: bool,
        cx: &mut Context<Self>,
    ) -> Vec<Block> {
        let mut blocks = Vec::new();
        if for_search {
            blocks.push(Block::Group(Group::new(
                "",
                vec![
                    Row::new(t("Theme"), self.theme_menu(cx))
                        .keywords("color colour colors colours dark light scheme"),
                ],
            )));
        } else {
            blocks.push(Block::Custom(self.render_theme_preview(form, cx)));
            blocks.push(Block::Custom(self.render_theme_picker(form, cx)));
        }

        let ui_font = self.settings.ui_font_size.round() as i32;
        let zoom = (self.settings.zoom * 100.0).round() as i32;
        let term_font = self.settings.font_size.round() as i32;
        blocks.push(Block::Group(Group::new(
            t("Text size"),
            vec![
                Row::new(
                    t("Interface text"),
                    form.stepper(
                        "ui-font",
                        format!("{ui_font} px"),
                        (ui_font > 10)
                            .then(|| step(cx.listener(|this, _e, _w, cx| this.adjust_ui_font(-1.0, cx))))
                            .flatten(),
                        (ui_font < 28)
                            .then(|| step(cx.listener(|this, _e, _w, cx| this.adjust_ui_font(1.0, cx))))
                            .flatten(),
                    ),
                )
                .desc(t("Everything except the terminal."))
                .keywords("font size"),
                Row::new(
                    t("Zoom"),
                    form.stepper(
                        "ui-zoom",
                        format!("{zoom}%"),
                        (zoom > 50)
                            .then(|| step(cx.listener(|this, _e, _w, cx| this.adjust_zoom(-0.1, cx))))
                            .flatten(),
                        (zoom < 400)
                            .then(|| step(cx.listener(|this, _e, _w, cx| this.adjust_zoom(0.1, cx))))
                            .flatten(),
                    ),
                )
                .desc(t("Scales the whole app, spacing included."))
                .keywords("scale size"),
                Row::new(
                    t("Terminal text"),
                    form.stepper(
                        "term-font",
                        format!("{term_font} px"),
                        (term_font > 6)
                            .then(|| {
                                step(cx.listener(|this, _e, _w, cx| {
                                    this.adjust_terminal_font(-1.0, cx)
                                }))
                            })
                            .flatten(),
                        (term_font < 72)
                            .then(|| {
                                step(cx.listener(|this, _e, _w, cx| {
                                    this.adjust_terminal_font(1.0, cx)
                                }))
                            })
                            .flatten(),
                    ),
                )
                .desc(t("Independent of zoom."))
                .keywords("font size"),
                Row::new(t("Terminal font"), form.input(&self.settings_ui.font_family))
                    .desc(t(
                        "Any installed monospace family. Blank uses the built-in default. Applies when you leave the field.",
                    ))
                    .keywords("family typeface"),
            ],
        )));

        let border = self.settings.pane_border.clone();
        let border_seg = |value: &'static str, label: SharedString| {
            Seg::new(
                label,
                border == value,
                cx.listener(move |this, _e, _w, cx| this.set_pane_border(value, cx)),
            )
        };
        let language = DropdownButton::new("settings-language")
            .small()
            .button(
                Button::new("settings-language-btn")
                    .outline()
                    .label(crate::i18n::display_name(&crate::i18n::current_language())),
            )
            .dropdown_menu(move |mut menu, _window, _cx| {
                for entry in crate::i18n::available_languages() {
                    menu = menu.menu(
                        SharedString::from(entry.1),
                        Box::new(crate::i18n::SetLanguage(entry.0.to_string())),
                    );
                }
                menu.scrollable(true)
            });
        blocks.push(Block::Group(Group::new(
            t("Panes and language"),
            vec![
                form.choice_row(
                    t("Pane border"),
                    "pane-border",
                    vec![
                        border_seg("off", t("Off")),
                        border_seg("subtle", t("Subtle")),
                        border_seg("bold", t("Bold")),
                    ],
                )
                .desc(t("The outline around panes you aren't focused on.")),
                Row::new(t("Language"), language).keywords("locale translation"),
            ],
        )));
        blocks
    }

    // ===== General =====

    pub(super) fn general_blocks(&self, form: &Form, cx: &mut Context<Self>) -> Vec<Block> {
        let settings = &self.settings;
        let as_panes = settings.new_agents_open_as_panes();
        let place_seg = |value: &'static str, label: SharedString, selected: bool| {
            Seg::new(
                label,
                selected,
                cx.listener(move |this, _e, _w, cx| {
                    this.settings.new_agent_placement = value.to_string();
                    this.persist_settings();
                    cx.notify();
                }),
            )
        };

        // The default preset: a menu, since there can be many.
        let default_id = self
            .presets
            .iter()
            .find(|p| p.id.to_string() == settings.default_preset)
            .map(|p| p.id);
        let default_name: SharedString = default_id
            .and_then(|id| self.presets.iter().find(|p| p.id == id))
            .map(|p| unnamed_or(&p.name).into())
            .unwrap_or_else(|| t("None"));
        let choices: Vec<(Uuid, String)> = self
            .presets
            .iter()
            .map(|p| (p.id, unnamed_or(&p.name)))
            .collect();
        let app = cx.entity();
        let default_preset = DropdownButton::new("general-default-preset")
            .small()
            .button(
                Button::new("general-default-preset-btn")
                    .outline()
                    .label(default_name),
            )
            .dropdown_menu(move |mut menu, window, _cx| {
                for (id, name) in &choices {
                    let id = *id;
                    menu = menu.item(
                        PopupMenuItem::new(name.clone())
                            .checked(Some(id) == default_id)
                            .on_click(window.listener_for(&app, move |this, _e, _w, cx| {
                                this.set_global_default_preset(id, cx)
                            })),
                    );
                }
                menu.scrollable(true)
            });

        let tmux_ok = self.tmux_available;
        let tmux_switch = form.switch(
            "b-tmux",
            // Effectively off (and greyed) when tmux isn't installed, even
            // though the preference stays saved.
            self.use_tmux && tmux_ok,
            tmux_ok,
            cx.listener(|this, on: &bool, _w, cx| {
                this.use_tmux = *on;
                this.persist_settings();
                cx.notify();
            }),
        );
        let tmux_row = if tmux_ok {
            Row::narrow(t("Run in a tmux session"), tmux_switch).desc(t(
                "Keeps the session alive if muxel closes, so you can attach to it again.",
            ))
        } else {
            let can_install = cfg!(any(target_os = "linux", target_os = "macos"));
            Row::new(
                t("Run in a tmux session"),
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .children(can_install.then(|| {
                        Button::new("b-tmux-install")
                            .outline()
                            .small()
                            .label(t("Install tmux…"))
                            .on_click(cx.listener(|this, _e, window, cx| {
                                this.open_tmux_install(window, cx)
                            }))
                    }))
                    .child(tmux_switch),
            )
            .warn(Some(t("tmux isn't installed on this computer.")))
        };

        let mut blocks = vec![Block::Group(Group::new(
            t("New agents"),
            vec![
                form.choice_row(t("Open in"), "new-agent-place", vec![
                            place_seg(muxel_core::NEW_AGENT_TAB, t("New tab"), !as_panes),
                            place_seg(muxel_core::NEW_AGENT_PANE, t("New pane"), as_panes),
                        ])
                .desc(t(
                    "For the toolbar's agent button and Ctrl+Alt+1–9. A new tab joins the focused pane; a new pane opens to its right.",
                ))
                .keywords("placement"),
                Row::new(t("Default preset"), default_preset)
                    .desc(t("The agent launched when you don't pick one."))
                    .keywords("agent"),
                tmux_row,
                Row::narrow(
                    t("Use a git worktree"),
                    form.switch(
                        "b-worktree",
                        self.use_worktree,
                        true,
                        cx.listener(|this, on: &bool, _w, cx| {
                            this.use_worktree = *on;
                            this.persist_settings();
                            cx.notify();
                        }),
                    ),
                )
                .desc(t("Each new agent works on its own branch, in its own folder."))
                .keywords("branch"),
                Row::new(
                    t("Auto-continue message"),
                    form.input(&self.settings_ui.auto_continue_message),
                )
                .desc(t(
                    "What a pane's Auto toggle types, then presses Enter, to resume an agent that stalls with work left. Right-click a pane's Auto button to give that pane its own.",
                )),
            ],
        ))];

        let mut pane_rows = vec![
            Row::narrow(
                t("Close a pane when its process exits"),
                form.switch(
                    "b-close",
                    settings.close_on_exit,
                    true,
                    cx.listener(|this, on: &bool, _w, cx| this.set_close_on_exit(*on, cx)),
                ),
            )
            .desc(t("Only after a clean exit. A crash leaves the pane open.")),
            Row::narrow(
                t("Ask before closing a busy terminal"),
                self.setting_switch(
                    form,
                    "b-confirm-close-term",
                    settings.confirm_close_terminal,
                    cx,
                    |s, on| s.confirm_close_terminal = on,
                ),
            )
            .keywords("confirm"),
        ];
        // Job objects and cgroups: Windows and Linux. Elsewhere it does nothing.
        if cfg!(any(windows, target_os = "linux")) {
            pane_rows.push(self.memory_limit_row(form, cx));
        }
        blocks.push(Block::Group(Group::new(t("Panes"), pane_rows)));

        let mouse = settings.terminal_mouse.clone();
        let mouse_seg = |value: &'static str, label: SharedString| {
            Seg::new(
                label,
                mouse == value,
                cx.listener(move |this, _e, _w, cx| this.set_terminal_mouse(value, cx)),
            )
        };
        let mouse_help = match mouse.as_str() {
            "menu" => t("Right-click opens a menu with Copy and Paste."),
            "copy_on_select" => t("Selecting text copies it. Right-click pastes."),
            _ => t("Right-click copies the selection, or pastes when nothing is selected."),
        };
        blocks.push(Block::Group(Group::new(
            t("Terminal"),
            vec![
                form.choice_row(
                    t("Copy and paste"),
                    "terminal-mouse",
                    vec![
                        mouse_seg("copy_paste", t("Right-click")),
                        mouse_seg("menu", t("Menu")),
                        mouse_seg("copy_on_select", t("On select")),
                    ],
                )
                .desc(mouse_help)
                .keywords("mouse clipboard selection"),
                Row::narrow(
                    t("Open links in the built-in browser"),
                    form.switch(
                        "b-browser",
                        settings.browser_enabled,
                        true,
                        cx.listener(|this, on: &bool, _w, cx| {
                            this.settings.browser_enabled = *on;
                            this.persist_settings();
                            // Windows only: the embedded webview needs a
                            // compositor flag set before gpui starts (see
                            // main.rs), so a mid-session change takes full
                            // effect on the next launch.
                            #[cfg(target_os = "windows")]
                            this.add_event(
                                NotifKind::Success,
                                t("Browser"),
                                t("Restart muxel to fully apply the browser change.").to_string(),
                            );
                            cx.notify();
                        }),
                    ),
                )
                .desc(t("Ctrl+click a link. Off uses your system browser.")),
            ],
        )));

        blocks.push(Block::Group(Group::new(
            t("Window"),
            vec![
                Row::narrow(
                    t("Minimize to the system tray on close"),
                    form.switch(
                        "b-minimize-tray",
                        settings.minimize_to_tray,
                        true,
                        cx.listener(|this, on: &bool, _w, cx| this.set_minimize_to_tray(*on, cx)),
                    ),
                ),
                Row::narrow(
                    t("Developer console"),
                    self.setting_switch(
                        form,
                        "b-dev-console",
                        settings.dev_console_enabled,
                        cx,
                        |s, on| s.dev_console_enabled = on,
                    ),
                )
                .desc(t("Toggle it with F12.")),
            ],
        )));
        blocks
    }

    /// The per-process (Windows) or per-pane (Linux) memory limit.
    fn memory_limit_row(&self, form: &Form, cx: &mut Context<Self>) -> Row {
        let current = self.settings.pane_memory_limit_gb;
        let mut choices = vec![0, 8, 16, 32, 64];
        // A hand-edited value still shows as the selected choice.
        if !choices.contains(&current) {
            choices.push(current);
            choices.sort_unstable();
        }
        let options = choices
            .into_iter()
            .map(|gb| {
                let label: SharedString = if gb == 0 {
                    t("Off")
                } else {
                    tf("{gb} GB", &[("gb", &gb.to_string())]).into()
                };
                self.setting_seg(label, current == gb, cx, move |s| {
                    s.pane_memory_limit_gb = gb
                })
            })
            .collect();
        // Windows limits each process (a job object); Linux limits the pane's
        // processes together (a cgroup), which is all a cgroup can do.
        let (label, description) = if cfg!(windows) {
            (
                t("Memory limit for each process in a pane"),
                t(
                    "Limits how much memory each process a local pane starts can commit. A process that asks for more is refused, which usually stops it, so one runaway doesn't take the computer down in the common case; it can't help when less memory is free than the limit. The 16 GB default fits typical developer tools; large Java heaps, optimized links and local model runners can need more. Applies to panes started after a change. Windows reports a process that reaches the limit on a best-effort basis. Programs launched with their own breakaway flag, and processes a tmux server or remote host runs, aren't covered.",
                ),
            )
        } else {
            (
                t("Memory limit for each pane"),
                t(
                    "Limits how much memory a local pane's processes can use together, plus up to a quarter of that in swap. When they reach it, Linux stops the largest of them and the rest of the pane, its agent included, keeps running; the notifications feed says so. Each local pane runs in a systemd scope of its own, so even with the limit off, a process stopped because the computer ran out of memory takes nothing else with it. The 16 GB default fits typical developer tools; large builds, links and local model runners can need more. Applies to panes started after a change. Needs a systemd user session. Panes on a remote host get a scope of their own there, but no limit.",
                ),
            )
        };
        form.choice_row(label, "pane-memory", options)
            .desc(description)
            .keywords("ram oom")
    }

    // ===== Editor =====

    pub(super) fn editor_blocks(&self, form: &Form, cx: &mut Context<Self>) -> Vec<Block> {
        let settings = &self.settings;
        let font = settings.editor_font_size.round() as i32;
        let tab = settings.editor_tab_size;
        let editor_switch =
            |id: &'static str, on: bool, set: fn(&mut muxel_core::Settings, bool)| {
                form.switch(
                    id,
                    on,
                    true,
                    cx.listener(move |this, on: &bool, window, cx| {
                        set(&mut this.settings, *on);
                        this.persist_settings();
                        this.apply_editor_config(window, cx);
                        cx.notify();
                    }),
                )
            };
        vec![
            Block::Group(Group::new(
                t("Text"),
                vec![
                    Row::new(
                        t("Font size"),
                        form.stepper(
                            "ed-font",
                            format!("{font} px"),
                            (font > 6)
                                .then(|| {
                                    step(cx.listener(|this, _e, window, cx| {
                                        this.adjust_editor_font(-1.0, window, cx)
                                    }))
                                })
                                .flatten(),
                            (font < 48)
                                .then(|| {
                                    step(cx.listener(|this, _e, window, cx| {
                                        this.adjust_editor_font(1.0, window, cx)
                                    }))
                                })
                                .flatten(),
                        ),
                    )
                    .desc(t("The code editor and git-diff panes."))
                    .keywords("code diff"),
                    Row::new(
                        t("Font family"),
                        form.input(&self.settings_ui.editor_font_family),
                    )
                    .desc(t(
                        "Blank uses the theme's monospace font. Applies when you leave the field.",
                    ))
                    .keywords("typeface"),
                    Row::new(
                        t("Tab width"),
                        form.stepper(
                            "ed-tab",
                            tn(
                                "{count} space",
                                "{count} spaces",
                                tab as usize,
                                &[("count", &tab.to_string())],
                            ),
                            (tab > 1)
                                .then(|| {
                                    step(cx.listener(|this, _e, window, cx| {
                                        this.adjust_editor_tab(-1, window, cx)
                                    }))
                                })
                                .flatten(),
                            (tab < 16)
                                .then(|| {
                                    step(cx.listener(|this, _e, window, cx| {
                                        this.adjust_editor_tab(1, window, cx)
                                    }))
                                })
                                .flatten(),
                        ),
                    )
                    .desc(t("Applies to files opened from now on."))
                    .keywords("indent"),
                ],
            )),
            Block::Group(Group::new(
                t("Display"),
                vec![
                    Row::narrow(
                        t("Soft wrap long lines"),
                        editor_switch("ed-wrap", settings.editor_soft_wrap, |s, on| {
                            s.editor_soft_wrap = on
                        }),
                    ),
                    Row::narrow(
                        t("Show line numbers"),
                        editor_switch("ed-lines", settings.editor_line_numbers, |s, on| {
                            s.editor_line_numbers = on
                        }),
                    ),
                    Row::narrow(
                        t("Show indent guides"),
                        editor_switch("ed-guides", settings.editor_indent_guides, |s, on| {
                            s.editor_indent_guides = on
                        }),
                    ),
                ],
            )),
        ]
    }

    // ===== Dictation =====

    pub(super) fn dictation_blocks(&self, form: &Form, cx: &mut Context<Self>) -> Vec<Block> {
        use muxel_core::SttEngine;
        let settings = &self.settings;
        let ui = &self.settings_ui;
        let is_local = settings.stt_engine == SttEngine::Local;
        let engine_seg = |engine: SttEngine, label: SharedString| {
            Seg::new(
                label,
                settings.stt_engine == engine,
                cx.listener(move |this, _e, _w, cx| this.set_stt_engine(engine, cx)),
            )
        };
        let mut engine_rows = vec![
            form.choice_row(t("Dictation engine"), "stt-engine", vec![
                        engine_seg(SttEngine::Local, t("Local")),
                        engine_seg(SttEngine::Provider, t("Provider")),
                    ])
            .desc(t(
                "Local runs entirely on this machine. Provider uploads your recorded audio to the endpoint below.",
            ))
            .keywords("speech to text stt whisper microphone"),
        ];
        if is_local {
            let models = ["tiny", "base", "small", "medium"]
                .into_iter()
                .map(|m| {
                    Seg::new(
                        m,
                        settings.stt_model == m,
                        cx.listener(move |this, _e, _w, cx| this.set_stt_model(m, cx)),
                    )
                })
                .collect();
            engine_rows.push(
                form.choice_row(t("Local model"), "stt-model", models)
                    .desc(t("whisper.cpp. Downloaded the first time it is used."))
                    .keywords("whisper"),
            );
        } else {
            let key_w = form.inner_w() - form.rem * 13.0;
            let key_controls = div()
                .flex()
                .items_center()
                .gap_2()
                .child(form.input_w(
                    &ui.stt_api_key,
                    if key_w < px(160.0) { px(160.0) } else { key_w },
                ))
                .child(
                    Button::new("stt-save-key")
                        .primary()
                        .small()
                        .label(t("Save key"))
                        .on_click(
                            cx.listener(|this, _e, window, cx| this.save_stt_api_key(window, cx)),
                        ),
                )
                .children(ui.stt_has_key.then(|| {
                    Button::new("stt-clear-key")
                        .ghost()
                        .small()
                        .label(t("Remove"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.clear_stt_api_key(cx)))
                }));
            engine_rows.extend([
                Row::new(t("Provider base URL"), form.input(&ui.stt_provider_url))
                    .desc(t(
                        "Any OpenAI-compatible endpoint. Applies when you leave the field.",
                    ))
                    .keywords("openai endpoint"),
                Row::new(t("Provider model"), form.input(&ui.stt_provider_model)),
                Row::stacked(t("API key"), key_controls)
                    .desc(if ui.stt_has_key {
                        t("A key is stored in the OS keychain. Type a new one to replace it.")
                    } else {
                        t("Stored in the OS keychain, never in muxel's config.")
                    })
                    .keywords("secret token"),
            ]);
        }

        let mut transcript_rows = vec![
            Row::new(t("Spoken language"), form.input(&ui.stt_language))
                .desc(t("A BCP-47 code such as en. Blank detects it."))
                .keywords("locale"),
            Row::narrow(
                t("Press Enter after the transcript"),
                self.setting_switch(form, "stt-autosubmit", settings.stt_autosubmit, cx, |s, on| {
                    s.stt_autosubmit = on
                }),
            )
            .desc(t("Sends what you said as soon as it is inserted."))
            .keywords("submit"),
            Row::narrow(
                t("Wake command"),
                self.setting_switch(
                    form,
                    "stt-wake-command",
                    settings.stt_wake_command,
                    cx,
                    |s, on| s.stt_wake_command = on,
                ),
            )
            .desc(t(
                "Dictating the wake phrase sweeps every pane and relaunches the ones that aren't running.",
            )),
        ];
        if settings.stt_wake_command {
            transcript_rows.push(
                Row::new(t("Wake phrase"), form.input(&ui.stt_wake_phrase)).desc(t(
                    "Matched on whole words, ignoring case and punctuation. The transcript that triggers it is not typed into the pane.",
                )),
            );
        }
        vec![
            Block::Group(Group::new(t("Engine"), engine_rows)),
            Block::Group(Group::new(t("Transcript"), transcript_rows)),
        ]
    }

    // ===== Read aloud =====

    pub(super) fn read_aloud_blocks(&self, form: &Form, cx: &mut Context<Self>) -> Vec<Block> {
        use muxel_core::{ReadAloudAuto, ReadAloudScope, TtsEngine};
        let settings = &self.settings;
        let ui = &self.settings_ui;

        // A shortcut as the user has it bound.
        let chord = |action: &str| {
            settings
                .keybindings
                .iter()
                .find(|k| k.action == action)
                .map(|k| k.keystroke.clone())
                .or_else(|| {
                    settings_view::DEFAULT_KEYBINDINGS
                        .iter()
                        .find(|(name, _, _)| *name == action)
                        .map(|(_, default, _)| default.to_string())
                })
                .map(|ks| prettify_keys(&ks))
                .unwrap_or_default()
        };
        let intro: SharedString = tf(
            "Press {keys}, or a speaker (in the toolbar for the focused pane, or on any pane), to hear the agent's last reply. Press it again to pause, and again to resume — each pane keeps its own place, picking up at the start of the sentence it paused in. {restart} starts over; {stop} stops.",
            &[
                ("keys", &chord("ReadAloud")),
                ("restart", &chord("ReadAloudRestart")),
                ("stop", &chord("ReadAloudStop")),
            ],
        )
        .into();

        let scope = settings.read_aloud_scope;
        let auto = settings.read_aloud_auto;
        let max = settings.read_aloud_max_chars;
        let engine = settings.tts_engine;

        let limits = [
            (0, t("No limit")),
            (500, t("30 s")),
            (1000, t("1 min")),
            (3000, t("3 min")),
        ]
        .into_iter()
        .map(|(chars, label)| {
            self.setting_seg(label, max == chars, cx, move |s| {
                s.read_aloud_max_chars = chars
            })
        })
        .collect();

        let reading = Group::new(
            t("Reading"),
            vec![
                Row::narrow(
                    t("Show read-aloud buttons"),
                    self.setting_switch(form, "ra-button", settings.read_aloud_button, cx, |s, on| {
                        s.read_aloud_button = on
                    }),
                )
                .desc(t("In the toolbar and on each pane."))
                .keywords("speaker tts text to speech"),
                form.choice_row(t("What to read"), "ra-scope", vec![
                            self.setting_seg(
                                t("Final message"),
                                scope == ReadAloudScope::FinalMessage,
                                cx,
                                |s| s.read_aloud_scope = ReadAloudScope::FinalMessage,
                            ),
                            self.setting_seg(
                                t("Whole last turn"),
                                scope == ReadAloudScope::WholeTurn,
                                cx,
                                |s| s.read_aloud_scope = ReadAloudScope::WholeTurn,
                            ),
                        ])
                .desc(t(
                    "The final message is what the agent wrote after its last tool call — usually its summary. Code, diffs, commands and tool output are never read, only the agent's own words.",
                )),
                form.choice_row(t("Read automatically"), "ra-auto", vec![
                            self.setting_seg(t("Off"), auto == ReadAloudAuto::Off, cx, |s| {
                                s.read_aloud_auto = ReadAloudAuto::Off
                            }),
                            self.setting_seg(
                                t("Focused pane"),
                                auto == ReadAloudAuto::Focused,
                                cx,
                                |s| s.read_aloud_auto = ReadAloudAuto::Focused,
                            ),
                            self.setting_seg(
                                t("Any agent"),
                                auto == ReadAloudAuto::All,
                                cx,
                                |s| s.read_aloud_auto = ReadAloudAuto::All,
                            ),
                        ])
                .desc(t("When an agent finishes.")),
                Row::narrow(
                    t("Say the agent's name before its reply"),
                    self.setting_switch(
                        form,
                        "ra-announce",
                        settings.read_aloud_announce,
                        cx,
                        |s, on| s.read_aloud_announce = on,
                    ),
                ),
                form.choice_row(t("Length limit"), "ra-max", limits).desc(t(
                    "A long reply stops at the end of a sentence once the limit is reached.",
                )),
            ],
        );

        let cleanup = Group::new(
            t("Clean-up"),
            vec![
                Row::narrow(
                    t("Say \u{201c}link\u{201d} instead of web addresses"),
                    self.setting_switch(
                        form,
                        "ra-urls",
                        settings.read_aloud_skip_urls,
                        cx,
                        |s, on| s.read_aloud_skip_urls = on,
                    ),
                )
                .keywords("url"),
                Row::narrow(
                    t("Say only the file name of a path"),
                    self.setting_switch(
                        form,
                        "ra-paths",
                        settings.read_aloud_short_paths,
                        cx,
                        |s, on| s.read_aloud_short_paths = on,
                    ),
                )
                .desc(t("src/app.rs:120 is read as app.rs.")),
                Row::narrow(
                    t("Read tables row by row"),
                    self.setting_switch(
                        form,
                        "ra-tables",
                        settings.read_aloud_tables,
                        cx,
                        |s, on| s.read_aloud_tables = on,
                    ),
                )
                .desc(t("Off skips them.")),
            ],
        );

        let mut engines =
            vec![
                self.setting_seg(t("System"), engine == TtsEngine::System, cx, |s| {
                    s.tts_engine = TtsEngine::System
                }),
            ];
        if crate::tts::local_voice_supported() || engine == TtsEngine::Local {
            engines.push(self.setting_seg(
                t("Local (Kokoro)"),
                engine == TtsEngine::Local,
                cx,
                |s| s.tts_engine = TtsEngine::Local,
            ));
        }
        engines.push(
            self.setting_seg(t("Provider"), engine == TtsEngine::Provider, cx, |s| {
                s.tts_engine = TtsEngine::Provider
            }),
        );
        let mut voice_rows = vec![
            form.choice_row(t("Voice"), "ra-engine", engines)
                .keywords("synthesizer engine tts"),
        ];
        match engine {
            TtsEngine::System => {
                let listed = ui
                    .tts_system_voices
                    .as_ref()
                    .filter(|voices| !voices.is_empty());
                match listed {
                    Some(voices) => {
                        let current = if settings.tts_system_voice.is_empty() {
                            t("OS default voice")
                        } else {
                            SharedString::from(settings.tts_system_voice.clone())
                        };
                        let choices: Vec<(String, String)> = muxel_core::tts::voices_for_language(
                            voices,
                            &crate::i18n::current_language(),
                        )
                        .into_iter()
                        .cloned()
                        .collect();
                        let picker = DropdownButton::new("ra-system-voice")
                            .small()
                            .button(
                                Button::new("ra-system-voice-btn")
                                    .outline()
                                    .icon(Icon::empty().path("icons/volume-2.svg"))
                                    .label(current),
                            )
                            .dropdown_menu(move |mut menu, _window, _cx| {
                                menu = menu.menu(
                                    t("OS default voice"),
                                    Box::new(SetSystemVoice(String::new())),
                                );
                                for (name, locale) in &choices {
                                    menu = menu.menu(
                                        format!("{name} ({locale})"),
                                        Box::new(SetSystemVoice(name.clone())),
                                    );
                                }
                                menu.scrollable(true)
                            });
                        voice_rows.push(Row::new(t("System voice"), picker));
                    }
                    None => voice_rows.push(
                        Row::new(t("System voice name"), form.input(&ui.tts_system_voice))
                            .desc(t("Blank uses the OS default voice.")),
                    ),
                }
            }
            TtsEngine::Local => {
                let voices = muxel_core::tts::KOKORO_VOICES
                    .iter()
                    .map(|voice| {
                        self.setting_seg(*voice, settings.tts_local_voice == *voice, cx, move |s| {
                            s.tts_local_voice = voice.to_string()
                        })
                    })
                    .collect();
                voice_rows.push(
                    form.choice_row(t("Kokoro voice"), "ra-kokoro", voices).desc(t(
                        "Runs offline on this machine. The model (about 90 MB) downloads the first time it speaks. Speaks at its own pace.",
                    )),
                );
            }
            TtsEngine::Provider => {
                voice_rows.extend([
                    Row::new(t("Provider voice"), form.input(&ui.tts_provider_voice))
                        .desc(t("alloy, echo, nova, onyx, shimmer, …")),
                    Row::new(t("Provider model"), form.input(&ui.tts_provider_model)),
                    Row::new(
                        t("Endpoint and API key"),
                        Button::new("ra-open-speech")
                            .outline()
                            .small()
                            .label(t("Dictation settings"))
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.set_section(SettingsSection::Speech, cx)
                            })),
                    )
                    .desc(t(
                        "Uses the provider URL and API key from Dictation. The reply's text is sent to that endpoint.",
                    )),
                ]);
            }
        }
        let rate = settings.tts_rate;
        voice_rows.extend([
            Row::new(
                if engine == TtsEngine::Local {
                    t("Speaking rate (System and Provider voices)")
                } else {
                    t("Speaking rate")
                },
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        Button::new("ra-rate-reset")
                            .ghost()
                            .small()
                            .label(t("Normal"))
                            .disabled((rate - 1.0).abs() < 0.05)
                            .on_click(cx.listener(|this, _e, _w, cx| {
                                this.settings.tts_rate = 1.0;
                                this.persist_settings();
                                cx.notify();
                            })),
                    )
                    .child(form.stepper(
                        "ra-rate",
                        format!("{rate:.1}×"),
                        step(cx.listener(|this, _e, _w, cx| this.adjust_tts_rate(-0.1, cx))),
                        step(cx.listener(|this, _e, _w, cx| this.adjust_tts_rate(0.1, cx))),
                    )),
            )
            .keywords("speed"),
            Row::new(
                t("Hear a sample"),
                Button::new("ra-test-voice")
                    .outline()
                    .small()
                    .icon(Icon::empty().path("icons/volume-2.svg"))
                    .label(t("Test voice"))
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.say_read_aloud_notice(
                            t("This is how muxel reads an agent's reply aloud."),
                            cx,
                        )
                    })),
            )
            .desc(t("Speaks one sentence with these settings.")),
        ]);

        vec![
            Block::Note(intro),
            Block::Group(reading),
            Block::Group(cleanup),
            Block::Group(Group::new(t("Voice"), voice_rows)),
        ]
    }

    // ===== Projects =====

    /// What applies to every project; the per-project editor follows it.
    pub(super) fn project_blocks(&self, form: &Form, cx: &mut Context<Self>) -> Vec<Block> {
        let settings = &self.settings;
        let days = accomplishments::clamp_retention(settings.accomplishment_retention_days);
        let retention = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                Button::new("acc-days-reset")
                    .ghost()
                    .small()
                    .label(t("Default"))
                    .disabled(days == accomplishments::DEFAULT_RETENTION_DAYS)
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        this.set_accomplishment_retention(
                            accomplishments::DEFAULT_RETENTION_DAYS,
                            cx,
                        )
                    })),
            )
            .child(
                form.stepper(
                    "acc-days",
                    tf("{days} days", &[("days", &days.to_string())]),
                    (days > 1)
                        .then(|| {
                            step(cx.listener(|this, _e, _w, cx| {
                                this.adjust_accomplishment_retention(-1, cx)
                            }))
                        })
                        .flatten(),
                    (days < accomplishments::MAX_RETENTION_DAYS)
                        .then(|| {
                            step(cx.listener(|this, _e, _w, cx| {
                                this.adjust_accomplishment_retention(1, cx)
                            }))
                        })
                        .flatten(),
                ),
            );
        vec![
            Block::Group(Group::new(
                t("New projects"),
                vec![
                    Row::narrow(
                        t("Turn on shared memory"),
                        self.setting_switch(
                            form,
                            "new-project-memory",
                            settings.new_project_memory,
                            cx,
                            |s, on| s.new_project_memory = on,
                        ),
                    )
                    .desc(t(
                        "For projects added from now on. Change an existing project below, or from its menu in the sidebar.",
                    )),
                    Row::narrow(
                        t("Turn on the accomplishment log"),
                        self.setting_switch(
                            form,
                            "new-project-accomplishments",
                            settings.new_project_accomplishments,
                            cx,
                            |s, on| s.new_project_accomplishments = on,
                        ),
                    ),
                ],
            )),
            Block::Group(Group::new(
                t("All projects"),
                vec![
                    Row::new(t("Accomplishment log history"), retention).desc(t(
                        "Older entries are pruned from each project's .muxel/ACCOMPLISHMENTS.md.",
                    )),
                ],
            )),
        ]
    }

    /// The projects, each with its own editor.
    pub(super) fn render_project_list(&self, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let items = self
            .workspace
            .projects
            .iter()
            .map(|project| {
                let pid = project.id;
                let selected = self.settings_ui.selected_project == Some(pid);
                ListItem {
                    id: SharedString::from(format!("proj-item-{pid}")),
                    icon: Some(
                        Icon::new(IconName::Folder)
                            .small()
                            .text_color(form.item_fg(selected, false))
                            .into_any_element(),
                    ),
                    label: unnamed_or(&project.name).into(),
                    sub: None,
                    trailing: None,
                    selected,
                    dim: false,
                    on_click: Box::new(cx.listener(move |this, _e, window, cx| {
                        this.open_project_editor(pid, window, cx)
                    })),
                }
            })
            .collect();
        let detail = form.detail();
        let editor = match self.settings_ui.selected_project {
            Some(pid) if self.workspace.project(pid).is_some() => {
                self.render_project_editor(pid, &detail, cx)
            }
            _ => detail.empty(t("Select a project to edit.")),
        };
        v_flex()
            .w(form.w)
            .gap_2()
            .child(form.heading(t("Each project")))
            .child(form.master_detail(items, None, None, editor))
            .into_any_element()
    }

    fn render_project_editor(&self, pid: Uuid, form: &Form, cx: &mut Context<Self>) -> AnyElement {
        let Some(project) = self.workspace.project(pid) else {
            return div().into_any_element();
        };
        let root = project.root_path.display().to_string();
        let default_id = project.default_preset;
        let presets = self
            .presets
            .iter()
            .map(|p| {
                let id = p.id;
                Seg::new(
                    unnamed_or(&p.name),
                    Some(id) == default_id,
                    cx.listener(move |this, _e, _w, cx| this.set_project_default_preset(id, cx)),
                )
            })
            .collect();
        let folder = div()
            .flex()
            .items_center()
            .gap_2()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_xs()
                    .text_color(form.pal.dim)
                    .child(root),
            )
            .child(
                Button::new("change-folder")
                    .outline()
                    .small()
                    .label(t("Change…"))
                    .on_click(
                        cx.listener(|this, _e, window, cx| this.change_project_folder(window, cx)),
                    ),
            );
        let delete = delete_button("del-project", t("Delete project…")).on_click(cx.listener(
            move |this, _e, _w, cx| {
                this.request_confirm(
                    t("Delete project?"),
                    t("The project and its panes will be removed."),
                    t("Delete"),
                    ConfirmAction::DeleteProject(pid),
                    cx,
                )
            },
        ));
        v_flex()
            .w(form.w)
            .gap(form.rem)
            .child(form.editor_title(
                None,
                unnamed_or(&project.name),
                vec![delete.into_any_element()],
            ))
            .child(form.group(
                Group::new(
                    "",
                    vec![
                        Row::new(t("Name"), form.input(&self.settings_ui.proj_name)),
                        Row::stacked(t("Folder"), folder),
                        form.choice_row(t("Default preset"), "project-preset", presets)
                        .desc(t("The agent this project launches when you don't pick one.")),
                        Row::narrow(
                            t("Shared agent memory"),
                            form.switch(
                                "proj-memory",
                                project.memory_enabled,
                                true,
                                cx.listener(move |this, _on: &bool, _w, cx| {
                                    this.toggle_project_memory(pid, cx)
                                }),
                            ),
                        )
                        .desc(t(
                            "Agents read and append lessons in .muxel/MEMORY.md across runs.",
                        )),
                        Row::narrow(
                            t("Accomplishment log"),
                            form.switch(
                                "proj-accomplishments",
                                project.accomplishments_enabled,
                                true,
                                cx.listener(move |this, _on: &bool, _w, cx| {
                                    this.toggle_project_accomplishments(pid, cx)
                                }),
                            ),
                        )
                        .desc(t(
                            "Agents append each finished feature or fix to .muxel/ACCOMPLISHMENTS.md. They never read it.",
                        )),
                    ],
                ),
                &[],
            ))
            .into_any_element()
    }
}
