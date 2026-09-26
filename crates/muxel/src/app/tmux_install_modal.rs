//! The offer to install tmux, and the dialog that runs the install.
//!
//! Made once, on the first launch (macOS/Linux) that finds tmux missing —
//! `Settings::tmux_install_offered` remembers it was — and again whenever the
//! user asks from Settings. The install runs on a thread of its own
//! (`crate::tmux_install`), streaming its output here; the dialog can be hidden
//! while it runs and comes back when it's done or needs a password.

use super::*;
use crate::tmux_install::{self as runner, Event, Outcome, Probe};
use muxel_core::tmux_install::{Install, Manual, Plan};

/// Output lines kept (the dialog shows the tail).
const LOG_KEEP: usize = 200;
/// Lines shown while installing, and after a failure.
const LOG_SHOWN_RUNNING: usize = 6;
const LOG_SHOWN_FAILED: usize = 12;

pub(super) struct TmuxInstall {
    probe: Probe,
    stage: Stage,
    log: Vec<String>,
    /// Hidden while the install carries on; reopened from Settings, or by the
    /// install finishing.
    hidden: bool,
    /// The sudo password, when no graphical prompt could be shown.
    password: Entity<InputState>,
    /// Focus the password field on the next render (the outcome that asks for
    /// it arrives without a window).
    focus_password: bool,
}

enum Stage {
    /// The offer; `note` says why it's back (a dismissed system prompt).
    Offer {
        note: Option<SharedString>,
    },
    Running,
    /// Waiting for a sudo password; `wrong` after one was refused.
    Password {
        wrong: bool,
    },
    /// tmux works; its `tmux -V`, when known.
    Installed(String),
    Failed(String),
}

impl TmuxInstall {
    fn install(&self) -> Option<&Install> {
        match &self.probe.plan {
            Plan::Install(install) => Some(install),
            Plan::Manual(_) => None,
        }
    }
}

impl MuxelApp {
    /// A fresh offer for this computer, or `None` where muxel doesn't use tmux.
    pub(super) fn new_tmux_install(
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<TmuxInstall> {
        let probe = runner::probe()?;
        let password = cx.new(|cx| {
            InputState::new(window, cx)
                .masked(true)
                .placeholder(t("Your password"))
        });
        cx.subscribe_in(
            &password,
            window,
            |this, _input, ev: &InputEvent, window, cx| {
                if let InputEvent::PressEnter { .. } = ev {
                    this.submit_tmux_password(window, cx);
                }
            },
        )
        .detach();
        Some(TmuxInstall {
            probe,
            stage: Stage::Offer { note: None },
            log: Vec::new(),
            hidden: false,
            password,
            focus_password: false,
        })
    }

    /// Whether the dialog is on screen (see `any_overlay_open`).
    pub(super) fn tmux_install_shown(&self) -> bool {
        self.tmux_install.as_ref().is_some_and(|m| !m.hidden)
    }

    /// Open the dialog from Settings — showing an install already under way.
    pub(super) fn open_tmux_install(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match &mut self.tmux_install {
            Some(m) => m.hidden = false,
            None => self.tmux_install = Self::new_tmux_install(window, cx),
        }
        cx.notify();
    }

    /// Close the dialog: an install under way carries on, hidden.
    fn close_tmux_install(&mut self, cx: &mut Context<Self>) {
        self.mark_tmux_offered();
        match &mut self.tmux_install {
            Some(m) if matches!(m.stage, Stage::Running) => m.hidden = true,
            _ => self.tmux_install = None,
        }
        cx.notify();
    }

    /// The first-launch offer has been answered; don't make it again.
    fn mark_tmux_offered(&mut self) {
        if !self.settings.tmux_install_offered {
            self.settings.tmux_install_offered = true;
            self.persist_settings();
        }
    }

    fn start_tmux_install(&mut self, password: Option<String>, cx: &mut Context<Self>) {
        self.mark_tmux_offered();
        let Some(m) = self.tmux_install.as_mut() else {
            return;
        };
        let Some(install) = m.install().cloned() else {
            return;
        };
        if matches!(m.stage, Stage::Running) {
            return;
        }
        m.stage = Stage::Running;
        m.log.clear();
        let (tx, rx) = async_channel::unbounded();
        std::thread::spawn(move || runner::run(&install, password, tx));
        cx.spawn(async move |view: WeakEntity<Self>, cx| {
            while let Ok(event) = rx.recv().await {
                let done = matches!(event, Event::Done(_));
                let alive = view.update(cx, |this, cx| {
                    this.on_tmux_install_event(event, cx);
                });
                if done || alive.is_err() {
                    break;
                }
            }
        })
        .detach();
        cx.notify();
    }

    fn on_tmux_install_event(&mut self, event: Event, cx: &mut Context<Self>) {
        let Some(m) = self.tmux_install.as_mut() else {
            return;
        };
        match event {
            Event::Line(line) => {
                if m.log.len() >= LOG_KEEP {
                    m.log.remove(0);
                }
                m.log.push(line);
            }
            Event::Done(outcome) => {
                m.hidden = false;
                m.stage = match outcome {
                    Outcome::Installed(version) => {
                        self.tmux_available = true;
                        Stage::Installed(version)
                    }
                    Outcome::Dismissed => Stage::Offer {
                        note: Some(t(
                            "The password prompt was closed, so nothing was installed.",
                        )),
                    },
                    Outcome::NeedPassword { wrong } => {
                        m.focus_password = true;
                        Stage::Password { wrong }
                    }
                    Outcome::Failed(message) => {
                        log::warn!("tmux install failed: {message}");
                        Stage::Failed(message)
                    }
                };
            }
        }
        cx.notify();
    }

    fn submit_tmux_password(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(m) = self.tmux_install.as_ref() else {
            return;
        };
        if !matches!(m.stage, Stage::Password { .. }) {
            return;
        }
        let password = m.password.read(cx).value().to_string();
        if password.is_empty() {
            return;
        }
        // Don't leave it sitting in the widget.
        m.password.update(cx, |s, cx| s.set_value("", window, cx));
        self.start_tmux_install(Some(password), cx);
    }

    /// After the user installed tmux (or Homebrew) themselves.
    fn recheck_tmux(&mut self, cx: &mut Context<Self>) {
        let Some(m) = self.tmux_install.as_mut() else {
            return;
        };
        if program_on_path("tmux") {
            self.tmux_available = true;
            m.stage = Stage::Installed(String::new());
        } else if let Some(probe) = runner::probe() {
            m.probe = probe;
            m.stage = Stage::Offer { note: None };
        }
        cx.notify();
    }

    pub(super) fn render_tmux_install(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let use_tmux = self.use_tmux;
        let Some(m) = self.tmux_install.as_mut() else {
            return div().into_any_element();
        };
        if std::mem::take(&mut m.focus_password) {
            let handle = m.password.read(cx).focus_handle(cx);
            window.focus(&handle, cx);
        }
        let m = &*m;
        let muted = cx.theme().muted_foreground;
        let mono = cx.theme().mono_font_family.clone();
        let code = |text: String| {
            v_flex()
                .p_2()
                .rounded(cx.theme().radius)
                .bg(cx.theme().secondary)
                .font_family(mono.clone())
                .text_xs()
                .children(
                    text.lines()
                        .map(|l| div().child(l.to_string()))
                        .collect::<Vec<_>>(),
                )
        };
        let para = |text: SharedString| div().text_sm().child(text);
        let hint = |text: SharedString| div().text_xs().text_color(muted).child(text);
        let why = t(
            "muxel runs each agent in a tmux session, so it keeps going when you close the window and reattaches when you reopen it. tmux isn’t installed on this computer.",
        );
        let manager = m.install().map(|i| i.manager.label()).unwrap_or_default();

        let mut body = v_flex().gap_3();
        let mut footer = h_flex().justify_end().gap_2().pt_2();
        let close = |label: SharedString| {
            Button::new("tmux-close")
                .ghost()
                .label(label)
                .on_click(cx.listener(|this, _e, _w, cx| this.close_tmux_install(cx)))
        };
        let title = match &m.stage {
            Stage::Offer { note } => {
                body = body.child(para(why));
                match &m.probe.plan {
                    Plan::Install(install) => {
                        body = body
                            .child(para(match &m.probe.system {
                                Some(system) => tf(
                                    "muxel can install it with {manager} on {system}. It will run:",
                                    &[("manager", manager), ("system", system)],
                                )
                                .into(),
                                None => tf(
                                    "muxel can install it with {manager}. It will run:",
                                    &[("manager", manager)],
                                )
                                .into(),
                            }))
                            .child(code(install.display()))
                            .child(hint(if install.needs_root() {
                                t("Your system may ask for your password. Only this install runs as administrator.")
                            } else {
                                t("Homebrew installs it for your account — no administrator password needed.")
                            }));
                        footer = footer.child(close(t("Not now"))).child(
                            Button::new("tmux-install")
                                .primary()
                                .label(t("Install tmux"))
                                .on_click(cx.listener(|this, _e, _w, cx| {
                                    this.start_tmux_install(None, cx)
                                })),
                        );
                    }
                    Plan::Manual(manual) => {
                        body = body.child(para(match manual {
                            Manual::NoHomebrew => t(
                                "muxel installs tmux with Homebrew, which isn’t installed. Install Homebrew from brew.sh, then choose Check again.",
                            ),
                            Manual::NixOs => t(
                                "On NixOS, add tmux to environment.systemPackages (or home.packages) in your configuration and rebuild, then choose Check again.",
                            ),
                            Manual::Immutable => t(
                                "This system’s packages are read-only, so muxel can’t install tmux directly. Layer it with rpm-ostree and restart, then choose Check again:",
                            ),
                            Manual::Unsupported => t(
                                "muxel doesn’t recognise this system’s package manager. Install tmux with it, then choose Check again.",
                            ),
                        }));
                        if *manual == Manual::Immutable {
                            body = body.child(code("sudo rpm-ostree install tmux".into()));
                        }
                        footer = footer.child(close(t("Not now")));
                        if *manual == Manual::NoHomebrew {
                            footer = footer.child(
                                Button::new("tmux-brew")
                                    .label(t("Open brew.sh"))
                                    .on_click(|_e, _w, cx| cx.open_url("https://brew.sh")),
                            );
                        }
                        footer = footer.child(
                            Button::new("tmux-recheck")
                                .primary()
                                .label(t("Check again"))
                                .on_click(cx.listener(|this, _e, _w, cx| this.recheck_tmux(cx))),
                        );
                    }
                }
                if let Some(note) = note {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().warning)
                            .child(note.clone()),
                    );
                }
                match m.probe.plan {
                    Plan::Install(_) => t("Install tmux?"),
                    Plan::Manual(_) => t("tmux isn’t installed"),
                }
            }
            Stage::Running => {
                body = body.child(
                    h_flex()
                        .gap_2()
                        .items_center()
                        .child(Spinner::new().small())
                        .child(para(
                            tf("Installing with {manager}…", &[("manager", manager)]).into(),
                        )),
                );
                if m.install().is_some_and(Install::needs_root) {
                    body = body.child(hint(t(
                        "Approve the system’s password prompt if one appears.",
                    )));
                }
                let tail = m.log.len().saturating_sub(LOG_SHOWN_RUNNING);
                if tail < m.log.len() {
                    body = body.child(code(m.log[tail..].join("\n")));
                }
                footer = footer.child(close(t("Hide")));
                t("Installing tmux")
            }
            Stage::Password { wrong } => {
                body = body.child(para(t(
                    "No system password prompt is available, so enter your password for sudo. It’s used for this install only and isn’t kept.",
                )));
                if *wrong {
                    body = body.child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().danger)
                            .child(t("That password didn’t work. Try again.")),
                    );
                }
                body = body.child(Input::new(&m.password));
                footer = footer
                    .child(
                        Button::new("tmux-pw-cancel")
                            .ghost()
                            .label(t("Cancel"))
                            .on_click(cx.listener(|this, _e, window, cx| {
                                if let Some(m) = this.tmux_install.as_mut() {
                                    m.password.update(cx, |s, cx| s.set_value("", window, cx));
                                    m.stage = Stage::Offer { note: None };
                                }
                                cx.notify();
                            })),
                    )
                    .child(
                        Button::new("tmux-pw-ok")
                            .primary()
                            .label(t("Install"))
                            .on_click(cx.listener(|this, _e, window, cx| {
                                this.submit_tmux_password(window, cx)
                            })),
                    );
                t("Administrator password")
            }
            Stage::Installed(version) => {
                body = body.child(para(if version.is_empty() {
                    t("tmux is ready.")
                } else {
                    tf("{version} is ready.", &[("version", version)]).into()
                }));
                body = body.child(hint(if use_tmux {
                    t("New agents will run in tmux sessions. Panes already open keep running as they are.")
                } else {
                    t("Turn on “New agents run in a tmux session” in Settings to use it.")
                }));
                footer = footer.child(
                    Button::new("tmux-done")
                        .primary()
                        .label(t("Done"))
                        .on_click(cx.listener(|this, _e, _w, cx| this.close_tmux_install(cx))),
                );
                t("tmux is installed")
            }
            Stage::Failed(message) => {
                body = body.child(para(message.clone().into()));
                let tail = m.log.len().saturating_sub(LOG_SHOWN_FAILED);
                if tail < m.log.len() {
                    body = body.child(code(m.log[tail..].join("\n")));
                }
                if let Some(install) = m.install() {
                    let commands = runner::terminal_commands(install);
                    body = body
                        .child(hint(t("You can also run it yourself in a terminal:")))
                        .child(code(commands.clone()));
                    footer = footer
                        .child(close(t("Close")))
                        .child(Button::new("tmux-copy").label(t("Copy command")).on_click(
                            move |_e, _w, cx| {
                                cx.write_to_clipboard(ClipboardItem::new_string(commands.clone()))
                            },
                        ))
                        .child(
                            Button::new("tmux-retry")
                                .primary()
                                .label(t("Try again"))
                                .on_click(cx.listener(|this, _e, _w, cx| {
                                    this.start_tmux_install(None, cx)
                                })),
                        );
                } else {
                    footer = footer.child(close(t("Close")));
                }
                t("Couldn’t install tmux")
            }
        };

        modal_backdrop()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _ev, _w, cx| this.close_tmux_install(cx)),
            )
            .child(
                div()
                    .w(px(480.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .p_5()
                    .bg(cx.theme().background)
                    .border_1()
                    .border_color(cx.theme().border)
                    .rounded(cx.theme().radius_lg)
                    .shadow_lg()
                    .on_mouse_down(MouseButton::Left, |_ev, _w, cx| cx.stop_propagation())
                    .child(div().text_lg().font_semibold().child(title))
                    .child(body)
                    .child(footer),
            )
            .into_any_element()
    }
}
