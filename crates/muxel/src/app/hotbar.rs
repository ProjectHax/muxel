//! The bottom hotbar: one-click, full-size views of things that otherwise live in
//! side panels or windows — the agent dashboard, the active project's git changes,
//! shared memory and files, and every project's accomplishments.
//!
//! A view fills everything right of the project sidebar (the docked panels and the
//! git-diff panel step aside while it's up); clicking its button again returns to
//! the panes. The project sidebar stays, and switching projects there retargets the
//! per-project views. The active button is drawn filled (the primary style), with
//! an accent bar above it, and named at the end of the hotbar, so it's always clear
//! what's showing.

use super::*;

/// Height of the hotbar; bottom-anchored floating bars sit above it.
pub(super) const HOTBAR_HEIGHT: f32 = 36.0;

/// What fills the main window's pane area.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(super) enum MainView {
    /// The active project's panes — the normal layout.
    #[default]
    Panes,
    Dashboard,
    Changes,
    Memory,
    Files,
    Accomplishments,
}

impl MainView {
    /// The hotbar's views, in button order.
    pub(super) const HOTBAR: [Self; 5] = [
        Self::Dashboard,
        Self::Changes,
        Self::Memory,
        Self::Files,
        Self::Accomplishments,
    ];

    /// The view after clicking `target`'s button: `target`, or back to the panes
    /// when it's already showing.
    pub(super) fn toggled(self, target: Self) -> Self {
        if self == target { Self::Panes } else { target }
    }

    /// Whether the view shows the active project, and so needs one.
    pub(super) fn per_project(self) -> bool {
        matches!(self, Self::Changes | Self::Memory | Self::Files)
    }

    fn label(self) -> SharedString {
        match self {
            Self::Panes => t("Panes"),
            Self::Dashboard => t("Dashboard"),
            Self::Changes => t("Changes"),
            Self::Memory => t("Memory"),
            Self::Files => t("Files"),
            Self::Accomplishments => t("Accomplishments"),
        }
    }

    fn tooltip(self) -> SharedString {
        match self {
            Self::Panes => t("Panes"),
            Self::Dashboard => t("Every agent's status, full view"),
            Self::Changes => t("Git changes in the active project, full view"),
            Self::Memory => t("Shared memory for the active project, full view"),
            Self::Files => t("Files in the active project, full view"),
            Self::Accomplishments => t("Accomplishments across all projects, full view"),
        }
    }

    fn icon(self) -> Icon {
        match self {
            Self::Panes => IconName::LayoutDashboard.into(),
            Self::Dashboard => IconName::LayoutDashboard.into(),
            Self::Changes => Icon::empty().path("icons/diff.svg"),
            Self::Memory => IconName::Star.into(),
            Self::Files => Icon::empty().path("icons/files.svg"),
            Self::Accomplishments => IconName::CircleCheck.into(),
        }
    }

    fn id(self) -> &'static str {
        match self {
            Self::Panes => "hotbar-panes",
            Self::Dashboard => "hotbar-dashboard",
            Self::Changes => "hotbar-changes",
            Self::Memory => "hotbar-memory",
            Self::Files => "hotbar-files",
            Self::Accomplishments => "hotbar-accomplishments",
        }
    }
}

impl MuxelApp {
    /// A hotbar click: show `target`, or return to the panes if it's showing.
    pub(super) fn toggle_main_view(
        &mut self,
        target: MainView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let next = self.main_view.toggled(target);
        self.set_main_view(next, window, cx);
    }

    /// Show `view` in the pane area, loading what it needs.
    pub(super) fn set_main_view(
        &mut self,
        view: MainView,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.main_view = view;
        let active = self.workspace.active_project;
        match view {
            MainView::Panes | MainView::Dashboard => {}
            MainView::Changes => match self.git_diff_tab {
                GitDiffTab::Files => self.refresh_git_diff_panel(cx),
                GitDiffTab::Worktrees => self.refresh_git_diff_worktrees(cx),
            },
            MainView::Memory => {
                if let Some(pid) = active {
                    self.point_memory_at(pid, window, cx);
                }
            }
            MainView::Files => {
                if let Some(pid) = active {
                    self.load_file_browser(pid, cx);
                }
            }
            MainView::Accomplishments => {
                self.ensure_accomplishments_overview(window, cx);
                self.refresh_all_accomplishments(cx);
            }
        }
        cx.notify();
    }

    /// The active project became `pid`: keep a per-project view on it. (The
    /// file browser follows it in `select_project`, and git changes refresh on
    /// the status tick.)
    pub(super) fn main_view_follow_project(
        &mut self,
        pid: Uuid,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match self.main_view {
            MainView::Memory if self.memory_pid != Some(pid) => {
                self.point_memory_at(pid, window, cx)
            }
            MainView::Changes => self.refresh_git_diff_panel(cx),
            _ => {}
        }
    }

    /// The hotbar along the bottom of the main window.
    pub(super) fn render_hotbar(&self, cx: &mut Context<Self>) -> AnyElement {
        let muted = cx.theme().muted_foreground;
        let accent = cx.theme().primary;
        let project = self.workspace.active().map(|p| p.name.clone());
        let mut bar = div()
            .flex_none()
            .h(px(HOTBAR_HEIGHT))
            .w_full()
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().sidebar);
        for view in MainView::HOTBAR {
            let showing = self.main_view == view;
            let disabled = view.per_project() && project.is_none();
            bar = bar.child(
                // The accent bar above the active button, tab-style.
                div()
                    .flex_none()
                    .h_full()
                    .flex()
                    .items_center()
                    .border_t_2()
                    .border_color(if showing {
                        accent
                    } else {
                        gpui::transparent_black()
                    })
                    .child(
                        Button::new(view.id())
                            .with_variant(if showing {
                                ButtonVariant::Primary
                            } else {
                                ButtonVariant::Ghost
                            })
                            .small()
                            .icon(view.icon())
                            .label(view.label())
                            .disabled(disabled)
                            .tooltip(if disabled {
                                t("Select a project first")
                            } else if showing {
                                t("Back to panes")
                            } else {
                                view.tooltip()
                            })
                            .on_click(cx.listener(move |this, _e, window, cx| {
                                this.toggle_main_view(view, window, cx)
                            })),
                    ),
            );
        }
        // Say what's up, and how to get back.
        if self.main_view != MainView::Panes {
            let label = self.main_view.label().to_string();
            let text = match (&project, self.main_view.per_project()) {
                (Some(project), true) => tf(
                    "Showing {view} for {project} — click it again to return to your panes",
                    &[("view", &label), ("project", project)],
                ),
                _ => tf(
                    "Showing {view} — click it again to return to your panes",
                    &[("view", &label)],
                ),
            };
            bar = bar.child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_right()
                    .text_xs()
                    .text_color(muted)
                    .overflow_hidden()
                    .whitespace_nowrap()
                    .text_ellipsis()
                    .child(text),
            );
        }
        bar.into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::MainView;

    #[test]
    fn a_button_shows_its_view_and_a_second_click_returns_to_the_panes() {
        for view in MainView::HOTBAR {
            let shown = MainView::Panes.toggled(view);
            assert_eq!(shown, view);
            assert_eq!(shown.toggled(view), MainView::Panes, "{view:?}");
        }
    }

    #[test]
    fn another_button_switches_views_directly() {
        assert_eq!(
            MainView::Memory.toggled(MainView::Changes),
            MainView::Changes
        );
    }

    #[test]
    fn only_project_views_need_an_active_project() {
        let per_project: Vec<_> = MainView::HOTBAR
            .into_iter()
            .filter(|v| v.per_project())
            .collect();
        assert_eq!(
            per_project,
            [MainView::Changes, MainView::Memory, MainView::Files]
        );
    }
}
