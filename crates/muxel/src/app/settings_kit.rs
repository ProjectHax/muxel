//! The building blocks of the Settings window: its theme-derived palette, the
//! titled groups of rows every section is made of, and the controls a row holds.
//!
//! Everything here is sized absolutely from the content pane's width. The pane
//! scrolls, so it gives its children no definite width of their own: a
//! percentage or flex width collapses an `Input` to a square and mis-measures
//! the height of a wrapped label (the next row then overlaps it).

use super::*;
use crate::settings_model as model;

/// What a control does when clicked.
pub(super) type Click = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;

/// The colours Settings paints with, derived from the active theme so each one
/// stays legible whatever the theme chose (see [`model::palette`]).
#[derive(Clone, Copy)]
pub(super) struct Pal {
    /// The window's own surface: the theme's background, made opaque.
    pub background: Hsla,
    pub nav: Hsla,
    pub card: Hsla,
    pub card_border: Hsla,
    pub divider: Hsla,
    pub track: Hsla,
    pub track_selected: Hsla,
    pub text: Hsla,
    pub dim: Hsla,
    pub nav_selected: Hsla,
    pub nav_selected_text: Hsla,
    pub accent: Hsla,
    pub on_accent: Hsla,
    pub switch_off: Hsla,
    pub switch_off_thumb: Hsla,
    pub warning: Hsla,
    pub danger: Hsla,
    pub success: Hsla,
    pub highlight: Hsla,
}

impl Pal {
    pub(super) fn of(cx: &App) -> Self {
        let p = model::palette(&theme::settings_theme_inputs(cx.theme()));
        let c = theme::hsla;
        Self {
            background: c(p.background),
            nav: c(p.nav),
            card: c(p.card),
            card_border: c(p.card_border),
            divider: c(p.divider),
            track: c(p.track),
            track_selected: c(p.track_selected),
            text: c(p.text),
            dim: c(p.dim),
            nav_selected: c(p.nav_selected),
            nav_selected_text: c(p.nav_selected_text),
            accent: c(p.accent),
            on_accent: c(p.on_accent),
            switch_off: c(p.switch_off),
            switch_off_thumb: c(p.switch_off_thumb),
            warning: c(p.warning),
            danger: c(p.danger),
            success: c(p.success),
            highlight: c(p.highlight),
        }
    }
}

/// Where a row puts its control.
#[derive(Clone, Copy, PartialEq)]
pub(super) enum RowLayout {
    /// Beside the label, in the right-hand column.
    Side,
    /// Beside the label, in a column just wide enough for a switch.
    Narrow,
    /// Under the label, the full width of the card.
    Stacked,
}

/// One setting: what it is called, one line about it, and its control.
pub(super) struct Row {
    pub label: SharedString,
    pub desc: Option<SharedString>,
    /// Shown under the description in the theme's warning colour.
    pub warn: Option<SharedString>,
    /// Extra words the search matches that the label and description lack.
    pub keywords: SharedString,
    pub control: AnyElement,
    pub layout: RowLayout,
}

impl Row {
    fn with(label: impl Into<SharedString>, control: impl IntoElement, layout: RowLayout) -> Self {
        Self {
            label: label.into(),
            desc: None,
            warn: None,
            keywords: SharedString::default(),
            control: control.into_any_element(),
            layout,
        }
    }

    pub(super) fn new(label: impl Into<SharedString>, control: impl IntoElement) -> Self {
        Self::with(label, control, RowLayout::Side)
    }

    /// A row whose control is a switch.
    pub(super) fn narrow(label: impl Into<SharedString>, control: impl IntoElement) -> Self {
        Self::with(label, control, RowLayout::Narrow)
    }

    /// A row whose control needs the card's full width: a prompt, a picker with
    /// as many choices as there are presets.
    pub(super) fn stacked(label: impl Into<SharedString>, control: impl IntoElement) -> Self {
        Self::with(label, control, RowLayout::Stacked)
    }

    pub(super) fn desc(mut self, desc: impl Into<SharedString>) -> Self {
        self.desc = Some(desc.into());
        self
    }

    pub(super) fn warn(mut self, warn: Option<SharedString>) -> Self {
        self.warn = warn;
        self
    }

    pub(super) fn keywords(mut self, keywords: impl Into<SharedString>) -> Self {
        self.keywords = keywords.into();
        self
    }

    /// Whether the search `words` find this row, given the headings it is under.
    pub(super) fn matches(&self, words: &[String], headings: &[&str]) -> bool {
        let mut haystacks: Vec<&str> = vec![self.label.as_ref(), self.keywords.as_ref()];
        if let Some(desc) = &self.desc {
            haystacks.push(desc.as_ref());
        }
        haystacks.extend_from_slice(headings);
        model::matches(words, &haystacks)
    }
}

/// A titled card of rows.
pub(super) struct Group {
    /// Empty for a card with no heading.
    pub title: SharedString,
    pub rows: Vec<Row>,
}

impl Group {
    pub(super) fn new(title: impl Into<SharedString>, rows: Vec<Row>) -> Self {
        Self {
            title: title.into(),
            rows,
        }
    }
}

/// A piece of a section's page, top to bottom.
pub(super) enum Block {
    Group(Group),
    /// Secondary text between groups.
    Note(SharedString),
    /// Anything that is not rows: the theme picker, a setup walkthrough.
    Custom(AnyElement),
}

/// One choice of a segmented control.
pub(super) struct Seg {
    label: SharedString,
    icon: Option<Icon>,
    selected: bool,
    disabled: bool,
    on_click: Click,
}

impl Seg {
    pub(super) fn new(
        label: impl Into<SharedString>,
        selected: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        Self {
            label: label.into(),
            icon: None,
            selected,
            disabled: false,
            on_click: Box::new(on_click),
        }
    }

    pub(super) fn icon(mut self, icon: Icon) -> Self {
        self.icon = Some(icon);
        self
    }

    pub(super) fn disabled(mut self, disabled: bool) -> Self {
        self.disabled = disabled;
        self
    }
}

/// One entry of a list editor's left-hand list.
pub(super) struct ListItem {
    pub id: SharedString,
    pub icon: Option<AnyElement>,
    pub label: SharedString,
    /// A second, smaller line.
    pub sub: Option<SharedString>,
    /// Right-aligned: a state marker or a shortcut.
    pub trailing: Option<AnyElement>,
    pub selected: bool,
    /// Drawn in secondary text: switched off, or not installed.
    pub dim: bool,
    pub on_click: Click,
}

/// Sizes and colours for laying out one column of settings. Cheap to build;
/// a render makes one for the page and one for a list editor's detail side.
#[derive(Clone, Copy)]
pub(super) struct Form {
    pub pal: Pal,
    /// Definite width of a card.
    pub w: Pixels,
    /// The window's rem, so sizes follow the interface font size and zoom.
    pub rem: Pixels,
    pub radius: Pixels,
    pub radius_lg: Pixels,
    /// How much of a card's width a row's control column takes.
    pub control_share: f32,
}

fn clamp_px(value: Pixels, min: Pixels, max: Pixels) -> Pixels {
    if value < min {
        min
    } else if value > max {
        max
    } else {
        value
    }
}

impl Form {
    fn pad_x(&self) -> Pixels {
        self.rem * 0.875
    }

    fn pad_y(&self) -> Pixels {
        self.rem * 0.625
    }

    /// Width inside a card's border and padding.
    pub(super) fn inner_w(&self) -> Pixels {
        let w = self.w - self.pad_x() * 2.0 - px(2.0);
        if w < px(120.0) { px(120.0) } else { w }
    }

    /// Width of the right-hand column a row's control sits in.
    pub(super) fn control_w(&self) -> Pixels {
        clamp_px(
            self.inner_w() * self.control_share,
            self.rem * 11.0,
            self.rem * 20.0,
        )
    }

    fn narrow_w(&self) -> Pixels {
        self.rem * 2.5
    }

    /// Width of a list editor's left-hand list.
    pub(super) fn list_w(&self) -> Pixels {
        self.rem * 11.5
    }

    fn list_gap(&self) -> Pixels {
        self.rem * 1.25
    }

    /// The form for a list editor's detail side: this page, less its list.
    pub(super) fn detail(&self) -> Form {
        let w = self.w - self.list_w() - self.list_gap();
        Form {
            w: if w < px(260.0) { px(260.0) } else { w },
            // A narrower card: give the control the same room it has on a page.
            control_share: 0.5,
            ..*self
        }
    }

    fn label_text(&self, text: SharedString, words: &[String]) -> AnyElement {
        let ranges = model::highlight_ranges(&text, words);
        if ranges.is_empty() {
            return text.into_any_element();
        }
        let style = HighlightStyle {
            background_color: Some(self.pal.highlight),
            ..Default::default()
        };
        StyledText::new(text)
            .with_highlights(ranges.into_iter().map(|range| (range, style)))
            .into_any_element()
    }

    fn render_row(&self, row: Row, first: bool, words: &[String]) -> Div {
        let pal = self.pal;
        let gap = self.rem;
        let label_w = match row.layout {
            RowLayout::Side => self.inner_w() - self.control_w() - gap,
            RowLayout::Narrow => self.inner_w() - self.narrow_w() - gap,
            RowLayout::Stacked => self.inner_w(),
        };
        let label_w = if label_w < px(100.0) {
            px(100.0)
        } else {
            label_w
        };
        let label = v_flex()
            .w(label_w)
            .flex_none()
            .gap_0p5()
            .child(
                div()
                    .w(label_w)
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(pal.text)
                    .child(self.label_text(row.label, words)),
            )
            .children(row.desc.map(|desc| {
                div()
                    .w(label_w)
                    .text_xs()
                    .text_color(pal.dim)
                    .child(self.label_text(desc, words))
            }))
            .children(row.warn.map(|warn| {
                div()
                    .w(label_w)
                    .text_xs()
                    .text_color(pal.warning)
                    .child(warn)
            }));
        let body = match row.layout {
            RowLayout::Stacked => v_flex()
                .gap_2()
                .child(label)
                // A flex row, so a control narrower than the card keeps its
                // own width rather than stretching across it.
                .child(div().w(self.inner_w()).flex().child(row.control)),
            layout => {
                let control_w = if layout == RowLayout::Narrow {
                    self.narrow_w()
                } else {
                    self.control_w()
                };
                div().flex().items_center().gap(gap).child(label).child(
                    div()
                        .w(control_w)
                        .flex_none()
                        .flex()
                        .flex_wrap()
                        .items_center()
                        .justify_end()
                        .gap_1()
                        .child(row.control),
                )
            }
        };
        let mut body = body.w(self.w - px(2.0)).px(self.pad_x()).py(self.pad_y());
        if !first {
            body = body.border_t_1().border_color(pal.divider);
        }
        body
    }

    /// A group heading on its own, for a group whose body is not rows.
    pub(super) fn heading(&self, title: impl Into<SharedString>) -> Div {
        div()
            .w(self.w)
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(self.pal.dim)
            .child(title.into())
    }

    /// An empty card to fill with anything.
    pub(super) fn card(&self) -> Div {
        v_flex()
            .w(self.w)
            .bg(self.pal.card)
            .border_1()
            .border_color(self.pal.card_border)
            .rounded(self.radius_lg)
    }

    /// A card of rows under its heading. `words` highlights search matches.
    pub(super) fn group(&self, group: Group, words: &[String]) -> Div {
        let mut card = self.card();
        for (ix, row) in group.rows.into_iter().enumerate() {
            card = card.child(self.render_row(row, ix == 0, words));
        }
        let mut col = v_flex().w(self.w).gap_2();
        if !group.title.is_empty() {
            col = col.child(self.heading(group.title));
        }
        col.child(card)
    }

    /// Secondary text the width of the page.
    pub(super) fn note(&self, text: impl Into<SharedString>) -> Div {
        div()
            .w(self.w)
            .text_xs()
            .text_color(self.pal.dim)
            .child(text.into())
    }

    /// A section's page: its blocks, top to bottom.
    pub(super) fn page(&self, blocks: Vec<Block>) -> Div {
        let mut col = v_flex().w(self.w).gap(self.rem * 1.375);
        for block in blocks {
            col = match block {
                Block::Group(group) => col.child(self.group(group, &[])),
                Block::Note(text) => col.child(self.note(text)),
                Block::Custom(element) => col.child(element),
            };
        }
        col
    }

    /// An on/off switch. `on_toggle` receives the new state.
    pub(super) fn switch(
        &self,
        id: impl Into<ElementId>,
        on: bool,
        enabled: bool,
        on_toggle: impl Fn(&bool, &mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        let pal = self.pal;
        let thumb = self.rem;
        let track = div()
            .id(id)
            .w(self.rem * 2.125)
            .h(self.rem * 1.25)
            .flex_none()
            .p(px(2.0))
            .flex()
            .items_center()
            .rounded_full()
            .child(div().size(thumb).rounded_full().bg(if on {
                pal.on_accent
            } else {
                pal.switch_off_thumb
            }));
        let track = if on {
            track.justify_end().bg(pal.accent)
        } else {
            track.justify_start().bg(pal.switch_off)
        };
        if enabled {
            track
                .cursor_pointer()
                .on_click(move |_, window, cx| on_toggle(&!on, window, cx))
        } else {
            track.opacity(0.45)
        }
    }

    /// Roughly how wide a segmented control with these choices draws. An
    /// estimate from the label lengths: the layout needs a definite width
    /// before any text is measured.
    fn segmented_w(&self, options: &[Seg]) -> Pixels {
        let mut w = px(6.0);
        for seg in options {
            let chars: f32 = seg
                .label
                .chars()
                .map(|c| if c.is_ascii() { 1.0 } else { 1.8 })
                .sum();
            w += self.rem * (chars * 0.45 + 1.25) + px(2.0);
            if seg.icon.is_some() {
                w += self.rem * 1.1;
            }
        }
        w
    }

    /// A row whose control is a segmented control: beside the label when the
    /// choices fit the control column, under it when they don't (many presets,
    /// a narrow editor, a wordier language).
    pub(super) fn choice_row(
        &self,
        label: impl Into<SharedString>,
        id: &'static str,
        options: Vec<Seg>,
    ) -> Row {
        if self.segmented_w(&options) <= self.control_w() {
            Row::new(label, self.segmented(id, options))
        } else {
            Row::stacked(label, self.segmented(id, options))
        }
    }

    /// One-of-a-few choices drawn as a single control.
    pub(super) fn segmented(&self, id: &'static str, options: Vec<Seg>) -> Div {
        let pal = self.pal;
        let mut track = div()
            .flex()
            .flex_wrap()
            .gap(px(2.0))
            .p(px(2.0))
            .rounded(self.radius)
            .bg(pal.track)
            .border_1()
            .border_color(pal.card_border);
        for (ix, seg) in options.into_iter().enumerate() {
            let mut cell = div()
                .id(SharedString::from(format!("{id}-{ix}")))
                .h(self.rem * 1.5)
                .px(self.rem * 0.625)
                .flex()
                .items_center()
                .gap_1()
                .rounded(self.radius)
                .text_xs()
                .children(seg.icon.map(|icon| icon.xsmall()))
                .child(seg.label);
            cell = if seg.selected {
                cell.bg(pal.track_selected)
                    .text_color(pal.text)
                    .font_weight(FontWeight::MEDIUM)
            } else {
                cell.text_color(pal.dim)
            };
            cell = if seg.disabled {
                cell.opacity(0.5)
            } else {
                let on_click = seg.on_click;
                let hover = pal.track_selected.opacity(0.5);
                let cell = cell.cursor_pointer().on_click(on_click);
                if seg.selected {
                    cell
                } else {
                    cell.hover(move |style| style.bg(hover))
                }
            };
            track = track.child(cell);
        }
        track
    }

    fn step_button(
        &self,
        id: SharedString,
        glyph: &'static str,
        enabled: bool,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Stateful<Div> {
        let hover = self.pal.track_selected;
        let cell = div()
            .id(id)
            .size(self.rem * 1.625)
            .flex()
            .items_center()
            .justify_center()
            .rounded(self.radius)
            .text_color(self.pal.text)
            .child(glyph);
        if enabled {
            cell.cursor_pointer()
                .hover(move |style| style.bg(hover))
                .on_click(on_click)
        } else {
            cell.opacity(0.4)
        }
    }

    /// A number with a − and a + either side. `dec`/`inc` are `None` at a limit.
    pub(super) fn stepper(
        &self,
        id: &'static str,
        value: impl Into<SharedString>,
        dec: Option<Click>,
        inc: Option<Click>,
    ) -> Div {
        let noop = || -> Click { Box::new(|_, _, _| {}) };
        let (can_dec, can_inc) = (dec.is_some(), inc.is_some());
        div()
            .flex()
            .items_center()
            .p(px(2.0))
            .rounded(self.radius)
            .bg(self.pal.track)
            .border_1()
            .border_color(self.pal.card_border)
            .child(self.step_button(
                SharedString::from(format!("{id}-dec")),
                "−",
                can_dec,
                dec.unwrap_or_else(noop),
            ))
            .child(
                div()
                    .min_w(self.rem * 3.5)
                    .px_1()
                    .text_center()
                    .text_sm()
                    .text_color(self.pal.text)
                    .child(value.into()),
            )
            .child(self.step_button(
                SharedString::from(format!("{id}-inc")),
                "+",
                can_inc,
                inc.unwrap_or_else(noop),
            ))
    }

    /// A text field the width of a row's control column.
    pub(super) fn input(&self, state: &Entity<InputState>) -> Div {
        self.input_w(state, self.control_w())
    }

    pub(super) fn input_w(&self, state: &Entity<InputState>, w: Pixels) -> Div {
        div().w(w).flex_none().child(Input::new(state))
    }

    /// A multi-line field the full width of a card, `lines` tall.
    pub(super) fn text_area(&self, state: &Entity<InputState>, lines: f32) -> Div {
        div()
            .w(self.inner_w())
            .child(Input::new(state).h(self.rem * (lines * 1.4 + 1.0)))
    }

    /// A keystroke as key caps.
    pub(super) fn key_caps(&self, caps: Vec<String>) -> Div {
        let pal = self.pal;
        let mut row = div().flex().items_center().gap_1();
        for cap in caps {
            row = row.child(if cap == "then" {
                div().text_xs().text_color(pal.dim).child(t("then"))
            } else {
                div()
                    .min_w(self.rem * 1.5)
                    .h(self.rem * 1.375)
                    .px(self.rem * 0.375)
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(self.radius)
                    .bg(pal.track)
                    .border_1()
                    .border_color(pal.card_border)
                    .text_xs()
                    .text_color(pal.text)
                    .child(cap)
            });
        }
        row
    }

    /// The colour a list item's icon and text take.
    pub(super) fn item_fg(&self, selected: bool, dim: bool) -> Hsla {
        if selected {
            self.pal.nav_selected_text
        } else if dim {
            self.pal.dim
        } else {
            self.pal.text
        }
    }

    /// A list editor: its items and an add button on the left, the selected
    /// item's editor (laid out with [`Form::detail`]) on the right.
    pub(super) fn master_detail(
        &self,
        items: Vec<ListItem>,
        add: Option<Button>,
        hint: Option<SharedString>,
        detail: AnyElement,
    ) -> Div {
        let pal = self.pal;
        let list_w = self.list_w();
        let mut list = v_flex().w(list_w).flex_none().gap_0p5();
        for item in items {
            let fg = self.item_fg(item.selected, item.dim);
            let mut row = div()
                .id(item.id)
                .w(list_w)
                .min_h(self.rem * 1.875)
                .px_2()
                .py_1()
                .flex()
                .items_center()
                .gap_2()
                .rounded(self.radius)
                .cursor_pointer()
                .text_color(fg)
                .on_click(item.on_click)
                .children(item.icon)
                .child(
                    v_flex()
                        .flex_1()
                        .min_w_0()
                        .overflow_hidden()
                        .child(div().text_sm().truncate().child(item.label))
                        .children(item.sub.map(|sub| {
                            div()
                                .text_xs()
                                .truncate()
                                .text_color(if item.selected { fg } else { pal.dim })
                                .child(sub)
                        })),
                )
                .children(item.trailing);
            row = if item.selected {
                row.bg(pal.nav_selected)
            } else {
                let hover = pal.track.opacity(0.6);
                row.hover(move |style| style.bg(hover))
            };
            list = list.child(row);
        }
        if let Some(add) = add {
            list = list.child(div().pt_1().flex().child(add));
        }
        if let Some(hint) = hint {
            list = list.child(
                div()
                    .w(list_w)
                    .pt_2()
                    .px_2()
                    .text_xs()
                    .text_color(pal.dim)
                    .child(hint),
            );
        }
        div()
            .w(self.w)
            .flex()
            .items_start()
            .gap(self.list_gap())
            .child(list)
            .child(div().w(self.detail().w).flex_none().child(detail))
    }

    /// What a list editor's detail side says when nothing is selected.
    pub(super) fn empty(&self, text: impl Into<SharedString>) -> AnyElement {
        div()
            .w(self.w)
            .py_6()
            .text_sm()
            .text_color(self.pal.dim)
            .child(text.into())
            .into_any_element()
    }

    /// The top of an editor: what is being edited, and what can be done to it.
    pub(super) fn editor_title(
        &self,
        icon: Option<AnyElement>,
        title: impl Into<SharedString>,
        actions: Vec<AnyElement>,
    ) -> Div {
        div()
            .w(self.w)
            .flex()
            .items_center()
            .gap_2()
            .children(icon)
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(self.pal.text)
                    .child(title.into()),
            )
            .children(actions)
    }

    /// A line of status text: a test's result, a saved-password note.
    pub(super) fn status(&self, text: impl Into<SharedString>, color: Hsla) -> Div {
        div()
            .w(self.w)
            .text_xs()
            .text_color(color)
            .child(text.into())
    }
}

/// The add button under a list editor's list.
pub(super) fn add_button(id: &'static str, label: SharedString) -> Button {
    Button::new(id)
        .ghost()
        .small()
        .icon(IconName::Plus)
        .label(label)
}

/// A button that deletes what is being edited (after a confirmation).
pub(super) fn delete_button(id: &'static str, label: SharedString) -> Button {
    Button::new(id).danger().outline().small().label(label)
}

/// A quiet secondary action in an editor's title.
pub(super) fn title_button(id: &'static str, label: SharedString) -> Button {
    Button::new(id).outline().small().label(label)
}

impl MuxelApp {
    /// The form a section's page is laid out with: the content pane's width,
    /// capped so a wide window doesn't stretch a label away from its control.
    pub(super) fn form(&self, cx: &App) -> Form {
        let rem = self.settings_rem;
        let page = self.settings_pane_w - self.settings_gutter() * 2.0;
        let cap = rem * 54.0;
        let w = if page > cap { cap } else { page };
        Form {
            pal: Pal::of(cx),
            w: if w < px(280.0) { px(280.0) } else { w },
            rem,
            radius: cx.theme().radius,
            radius_lg: cx.theme().radius_lg,
            control_share: 0.42,
        }
    }

    /// Space either side of a section's page.
    pub(super) fn settings_gutter(&self) -> Pixels {
        self.settings_rem * 1.5
    }
}
