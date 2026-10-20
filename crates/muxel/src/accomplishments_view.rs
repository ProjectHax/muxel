//! The accomplishment log's all-projects overview, plus the small rendering
//! helpers the per-project panel in `app.rs` shares with it.
//!
//! One [`AccomplishmentsOverview`] entity backs both places the overview appears:
//! embedded in the main window (in place of the pane area, like the dashboard) and
//! as the root of its own pop-out window. It renders from a local
//! [`OverviewSnapshot`] that it refreshes by observing [`MuxelApp`] — never by
//! reading the app in `render`, which for a freshly opened window runs inside the
//! app update that opened it (see `DevConsoleView`).

use crate::app::MuxelApp;
use crate::editor::DiskStamp;
use crate::i18n::{t, tf};
use gpui::*;
use gpui_component::input::{Input, InputEvent, InputState};
use gpui_component::tag::Tag;
use gpui_component::{button::*, *};
use muxel_core::accomplishments::{self, Accomplishment, AccomplishmentKind};
use muxel_core::memory::fmt_date;
use std::collections::HashSet;
use uuid::Uuid;

/// One project's loaded log, as cached by the app for every view.
#[derive(Clone, Debug, Default)]
pub(crate) struct ProjectLog {
    /// Maintained entries, oldest first.
    pub entries: Vec<Accomplishment>,
    /// Why the last load failed (host unreachable, unreadable file), if it did.
    pub error: Option<String>,
    /// A load has completed at least once.
    pub loaded: bool,
    /// The local file's stamp at the last load, for cheap change polling.
    pub stamp: Option<DiskStamp>,
}

/// What the overview shows for one project.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OverviewProject {
    pub id: Uuid,
    pub name: String,
    pub entries: Vec<Accomplishment>,
    pub error: Option<String>,
    pub loading: bool,
}

/// Everything the overview renders, copied out of the app.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct OverviewSnapshot {
    /// The app's `accomplishments_generation` this was built from.
    pub generation: u64,
    pub retention_days: u32,
    /// Projects with the log enabled, in sidebar order.
    pub projects: Vec<OverviewProject>,
}

/// Local wall-clock seconds (unix seconds + the local UTC offset): the clock the
/// log's dates are written in.
pub(crate) fn local_now_secs() -> i64 {
    let now = chrono::Local::now();
    now.timestamp() + i64::from(now.offset().local_minus_utc())
}

/// The translated name of a kind.
pub(crate) fn kind_label(kind: AccomplishmentKind) -> SharedString {
    match kind {
        AccomplishmentKind::Feature => t("Feature"),
        AccomplishmentKind::Fix => t("Fix"),
        AccomplishmentKind::Refactor => t("Refactor"),
        AccomplishmentKind::Perf => t("Performance"),
        AccomplishmentKind::Docs => t("Docs"),
        AccomplishmentKind::Test => t("Test"),
        AccomplishmentKind::Chore => t("Chore"),
        AccomplishmentKind::Other => t("Other"),
    }
}

/// A small colored tag naming `kind`.
pub(crate) fn kind_tag(kind: AccomplishmentKind) -> Tag {
    let color = match kind {
        AccomplishmentKind::Feature => ColorName::Green,
        AccomplishmentKind::Fix => ColorName::Blue,
        AccomplishmentKind::Refactor => ColorName::Violet,
        AccomplishmentKind::Perf => ColorName::Orange,
        AccomplishmentKind::Docs => ColorName::Cyan,
        AccomplishmentKind::Test => ColorName::Amber,
        AccomplishmentKind::Chore => ColorName::Gray,
        AccomplishmentKind::Other => ColorName::Neutral,
    };
    Tag::color(color).small().child(kind_label(kind))
}

/// "Today", "Yesterday", or the date, for a day heading.
pub(crate) fn day_heading(day: i64, today: i64) -> SharedString {
    match (accomplishments::day_start(today) - day) / 86_400 {
        0 => t("Today"),
        1 => t("Yesterday"),
        _ => fmt_date(day).into(),
    }
}

/// A row of kind tags with counts, e.g. `[Feature] 3  [Fix] 5`.
pub(crate) fn kind_counts_row<'a>(entries: impl IntoIterator<Item = &'a Accomplishment>) -> Div {
    let mut row = div().flex().flex_wrap().items_center().gap_2();
    for (kind, n) in accomplishments::kind_counts(entries) {
        row = row.child(
            div()
                .flex()
                .items_center()
                .gap_1()
                .child(kind_tag(kind))
                .child(div().text_xs().child(n.to_string())),
        );
    }
    row
}

/// One entry: kind tag, summary, and the agent that did it.
pub(crate) fn entry_row(e: &Accomplishment, muted: Hsla) -> Div {
    let mut row = div()
        .flex()
        .items_start()
        .gap_2()
        .child(div().flex_none().child(kind_tag(e.kind)))
        .child(div().flex_1().min_w_0().text_sm().child(e.summary.clone()));
    // Attribution, and when an agent corrected the entry on a later day.
    let mut meta: Vec<String> = e.agent.iter().cloned().collect();
    if let Some(updated) = e.updated {
        meta.push(tf("updated {date}", &[("date", &fmt_date(updated))]));
    }
    if !meta.is_empty() {
        row = row.child(
            div()
                .flex_none()
                .text_xs()
                .text_color(muted)
                .child(meta.join(" · ")),
        );
    }
    row
}

/// The all-projects accomplishment overview (embedded or popped out).
pub(crate) struct AccomplishmentsOverview {
    app: WeakEntity<MuxelApp>,
    snapshot: OverviewSnapshot,
    /// Whether this instance is its own window (no pop-out/close buttons then).
    popped_out: bool,
    search: Entity<InputState>,
    /// Projects the user filtered out (chips toggled off).
    hidden_projects: HashSet<Uuid>,
    /// Show only this kind, when set.
    kind_filter: Option<AccomplishmentKind>,
    focus_handle: FocusHandle,
}

impl AccomplishmentsOverview {
    pub(crate) fn new(
        app: WeakEntity<MuxelApp>,
        snapshot: OverviewSnapshot,
        popped_out: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        // Observe callbacks run after the app's update, so reading it there is safe.
        // They fire on every app notify (each tick included), so only rebuild when
        // the log data or the set of projects actually changed.
        if let Some(entity) = app.upgrade() {
            cx.observe(&entity, |this: &mut Self, app, cx| {
                let app = app.read(cx);
                if app.accomplishments_snapshot_is_current(&this.snapshot) {
                    return;
                }
                this.snapshot = app.accomplishments_snapshot();
                cx.notify();
            })
            .detach();
        }
        let search =
            cx.new(|cx| InputState::new(window, cx).placeholder(t("Search accomplishments…")));
        cx.subscribe(&search, |_this, _input, ev: &InputEvent, cx| {
            if matches!(ev, InputEvent::Change) {
                cx.notify();
            }
        })
        .detach();
        Self {
            app,
            snapshot,
            popped_out,
            search,
            hidden_projects: HashSet::new(),
            kind_filter: None,
            focus_handle: cx.focus_handle(),
        }
    }

    /// Run `f` on the app with the **main** window — the overview may live in its
    /// own window, but project navigation happens in the main one. `window` is the
    /// one handling the click: when that *is* the main window it must be used as is,
    /// since a window can't be re-entered with `update` while it's dispatching.
    fn with_main_window(
        &self,
        window: &mut Window,
        cx: &mut App,
        f: impl FnOnce(&mut MuxelApp, &mut Window, &mut Context<MuxelApp>) + 'static,
    ) {
        let Some(app) = self.app.upgrade() else {
            return;
        };
        let Some(main) = app.read(cx).main_window_handle() else {
            return;
        };
        if window.window_handle() == main {
            app.update(cx, |app, cx| f(app, window, cx));
            return;
        }
        let _ = main.update(cx, |_, window, cx| {
            window.activate_window();
            app.update(cx, |app, cx| f(app, window, cx));
        });
    }
}

impl Focusable for AccomplishmentsOverview {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for AccomplishmentsOverview {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let muted = cx.theme().muted_foreground;
        let border = cx.theme().border;
        let today = local_now_secs();
        let query = self.search.read(cx).value().trim().to_lowercase();
        let snap = &self.snapshot;

        // Visible entries, tagged with their project, after every filter.
        let shown_projects: Vec<&OverviewProject> = snap
            .projects
            .iter()
            .filter(|p| !self.hidden_projects.contains(&p.id))
            .collect();
        let in_window: Vec<(&OverviewProject, &Accomplishment)> = shown_projects
            .iter()
            .flat_map(|p| p.entries.iter().map(move |e| (*p, e)))
            .collect();
        let visible: Vec<(&OverviewProject, &Accomplishment)> = in_window
            .iter()
            .copied()
            .filter(|(_, e)| self.kind_filter.is_none_or(|k| e.kind == k) && e.matches(&query))
            .collect();

        // Header: title, window, and the window's controls.
        let mut header = div()
            .w_full()
            .flex()
            .items_center()
            .gap_2()
            .px_3()
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .child(div().font_semibold().child(t("Accomplishments")))
                    .child(div().text_xs().text_color(muted).child(tf(
                        "All projects · last {days} days",
                        &[("days", &snap.retention_days.to_string())],
                    ))),
            )
            .child(
                Button::new("acc-ov-refresh")
                    .ghost()
                    .xsmall()
                    .icon(Icon::empty().path("icons/refresh.svg"))
                    .tooltip(t("Refresh"))
                    .on_click(cx.listener(|this, _e, _w, cx| {
                        if let Some(app) = this.app.upgrade() {
                            app.update(cx, |app, cx| app.refresh_all_accomplishments(cx));
                        }
                    })),
            );
        if !self.popped_out {
            header = header
                .child(
                    Button::new("acc-ov-popout")
                        .ghost()
                        .xsmall()
                        .icon(IconName::ExternalLink)
                        .tooltip(t("Pop out into its own window"))
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            if let Some(app) = this.app.upgrade() {
                                app.update(cx, |app, cx| {
                                    app.close_accomplishments_overview(cx);
                                    app.pop_out_accomplishments(cx);
                                });
                            }
                        })),
                )
                .child(
                    Button::new("acc-ov-close")
                        .ghost()
                        .xsmall()
                        .icon(IconName::Close)
                        .tooltip(t("Close"))
                        .on_click(cx.listener(|this, _e, _w, cx| {
                            if let Some(app) = this.app.upgrade() {
                                app.update(cx, |app, cx| app.close_accomplishments_overview(cx));
                            }
                        })),
                );
        }
        // Popped out, the header is the window's title bar.
        let header: AnyElement = if self.popped_out {
            TitleBar::new().child(header).into_any_element()
        } else {
            header
                .py_2()
                .border_b_1()
                .border_color(border)
                .into_any_element()
        };

        // Filters: per-kind totals (click to show only that kind) and project chips.
        let mut kinds = div().flex().flex_wrap().items_center().gap_2();
        for (kind, n) in accomplishments::kind_counts(in_window.iter().map(|(_, e)| *e)) {
            let selected = self.kind_filter == Some(kind);
            kinds = kinds.child(
                div()
                    .id(SharedString::from(format!("acc-ov-kind-{}", kind.as_str())))
                    .flex()
                    .items_center()
                    .gap_1()
                    .px_1()
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .bg(if selected {
                        cx.theme().secondary
                    } else {
                        gpui::transparent_black()
                    })
                    .child(kind_tag(kind))
                    .child(div().text_xs().child(n.to_string()))
                    .on_click(cx.listener(move |this, _e, _w, cx| {
                        this.kind_filter = (this.kind_filter != Some(kind)).then_some(kind);
                        cx.notify();
                    })),
            );
        }
        let mut filters = div()
            .flex()
            .flex_col()
            .gap_2()
            .px_3()
            .py_2()
            .border_b_1()
            .border_color(border)
            .child(kinds);
        if snap.projects.len() > 1 {
            let mut chips = div().flex().flex_wrap().gap_1();
            for p in &snap.projects {
                let id = p.id;
                let on = !self.hidden_projects.contains(&id);
                chips = chips.child(
                    Button::new(SharedString::from(format!("acc-ov-proj-{}", id.simple())))
                        .xsmall()
                        .outline()
                        .selected(on)
                        .label(format!("{} · {}", p.name, p.entries.len()))
                        .on_click(cx.listener(move |this, _e, _w, cx| {
                            if !this.hidden_projects.remove(&id) {
                                this.hidden_projects.insert(id);
                            }
                            cx.notify();
                        })),
                );
            }
            filters = filters.child(chips);
        }
        filters = filters.child(Input::new(&self.search).w_full());

        // Body: projects that couldn't load, then days → projects → entries.
        let mut body = div()
            .id("acc-ov-body")
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .px_3()
            .py_2()
            .flex()
            .flex_col()
            .gap_3();
        for p in &shown_projects {
            if let Some(err) = &p.error {
                body = body.child(div().text_xs().text_color(muted).child(tf(
                    "Couldn't load {project}: {error}",
                    &[("project", &p.name), ("error", err)],
                )));
            } else if p.loading {
                body = body.child(
                    div()
                        .text_xs()
                        .text_color(muted)
                        .child(tf("Loading {project}…", &[("project", &p.name)])),
                );
            }
        }
        if snap.projects.is_empty() {
            body = body.child(div().text_sm().text_color(muted).child(t(
                "No project has its accomplishment log turned on. Enable it per project \
                 from the sidebar's right-click menu or Settings → Projects.",
            )));
        } else if in_window.is_empty() {
            body = body.child(div().text_sm().text_color(muted).child(tf(
                "Nothing logged in the last {days} days yet. Agents add entries here as they finish work.",
                &[("days", &snap.retention_days.to_string())],
            )));
        } else if visible.is_empty() {
            body = body.child(
                div()
                    .text_sm()
                    .text_color(muted)
                    .child(t("No entries match your filters.")),
            );
        }
        for (day, items) in accomplishments::group_by_day(visible, |(_, e)| e.date) {
            let mut day_col = div().flex().flex_col().gap_2().child(
                div()
                    .flex()
                    .items_baseline()
                    .gap_2()
                    .border_b_1()
                    .border_color(border)
                    .pb_1()
                    .child(div().font_semibold().child(day_heading(day, today)))
                    .child(
                        div()
                            .text_xs()
                            .text_color(muted)
                            .child(items.len().to_string()),
                    ),
            );
            // Within the day, keep projects in sidebar order.
            for p in &shown_projects {
                let mine: Vec<&Accomplishment> = items
                    .iter()
                    .filter(|(owner, _)| owner.id == p.id)
                    .map(|(_, e)| *e)
                    .collect();
                if mine.is_empty() {
                    continue;
                }
                let pid = p.id;
                let mut block = div().flex().flex_col().gap_1().pl_1().child(
                    div()
                        .id(SharedString::from(format!(
                            "acc-ov-open-{day}-{}",
                            pid.simple()
                        )))
                        .text_xs()
                        .font_semibold()
                        .text_color(muted)
                        .cursor_pointer()
                        .hover(|s| s.text_color(cx.theme().foreground))
                        .child(p.name.clone())
                        .on_click(cx.listener(move |this, _e, window, cx| {
                            this.with_main_window(window, cx, move |app, window, cx| {
                                app.show_project_accomplishments(pid, window, cx)
                            });
                        })),
                );
                for e in mine {
                    block = block.child(entry_row(e, muted));
                }
                day_col = day_col.child(block);
            }
            body = body.child(day_col);
        }

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .bg(cx.theme().background)
            .text_color(cx.theme().foreground)
            .child(header)
            .child(filters)
            .child(body)
    }
}
