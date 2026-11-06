//! The pane strip's icon buttons: split, diff, maximize, pop out and close per
//! pane, close and read-aloud per tab, and `+` for a new tab.
//!
//! The strips draw every frame, one of these per tab plus up to seven per pane.
//! gpui-component's `Button` builds a nested label element and a managed tooltip
//! around each icon, which made the buttons most of the strips' frame cost. This
//! draws the same xsmall ghost icon button as a single element. It keeps the
//! colours, size, focus ring, tab stop and mouse-down handling.

use super::*;
use gpui::accesskit::Role;
use gpui_component::GlobalState;
use gpui_component::tooltip::Tooltip;

/// Matches gpui-component's focus ring width.
const RING_WIDTH: Pixels = px(1.5);

type ClickHandler = Box<dyn Fn(&ClickEvent, &mut Window, &mut App)>;
type MouseDownHandler = Box<dyn Fn(&MouseDownEvent, &mut Window, &mut App)>;

#[derive(IntoElement)]
pub(super) struct StripIconButton {
    id: ElementId,
    icon: Icon,
    tooltip: SharedString,
    on_click: Option<ClickHandler>,
    on_right_mouse_down: Option<MouseDownHandler>,
}

impl StripIconButton {
    pub(super) fn new(
        id: impl Into<ElementId>,
        icon: impl Into<Icon>,
        tooltip: impl Into<SharedString>,
    ) -> Self {
        Self {
            id: id.into(),
            icon: icon.into(),
            tooltip: tooltip.into(),
            on_click: None,
            on_right_mouse_down: None,
        }
    }

    pub(super) fn on_click(
        mut self,
        handler: impl Fn(&ClickEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_click = Some(Box::new(handler));
        self
    }

    pub(super) fn on_right_mouse_down(
        mut self,
        handler: impl Fn(&MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_right_mouse_down = Some(Box::new(handler));
        self
    }
}

impl RenderOnce for StripIconButton {
    fn render(self, window: &mut Window, cx: &mut App) -> impl IntoElement {
        let theme = cx.theme();
        // gpui-component's ghost variant.
        let (hover, active) = if theme.mode.is_dark() {
            (theme.secondary.lighten(0.1), theme.secondary.lighten(0.2))
        } else {
            (theme.secondary.darken(0.1), theme.secondary.darken(0.2))
        };
        let (radius, fg, ring) = (theme.radius, theme.secondary_foreground, theme.ring);
        let focus = window
            .use_keyed_state(self.id.clone(), cx, |_, cx| cx.focus_handle())
            .read(cx)
            .clone();
        let focused = focus.is_focused(window);
        let tooltip = self.tooltip;
        // No accessible name, like `Button` without a label. UAT drivers find
        // the title bar's Close by role and name, and a pane's Close named the
        // same would make that lookup ambiguous.
        let mut button = div()
            .id(self.id)
            .role(Role::Button)
            .track_focus(&focus.tab_stop(true))
            .flex()
            .flex_shrink_0()
            .items_center()
            .justify_center()
            .size_5()
            .rounded(radius)
            .cursor_default()
            .text_color(fg)
            .hover(move |s| s.bg(hover.opacity(0.8)))
            .active(move |s| s.bg(active.opacity(0.8)))
            .on_mouse_down(MouseButton::Left, |_, window, cx| {
                // As `Button` does: a click doesn't take focus or start a
                // text selection. Stopping here also keeps the strip under
                // the button from starting a tab or pane drag.
                window.prevent_default();
                GlobalState::suppress_text_selection(cx);
                cx.stop_propagation();
            })
            .tooltip(move |window, cx| Tooltip::new(tooltip.clone()).build(window, cx))
            .child(self.icon.xsmall());
        // Plain `if`s rather than FluentBuilder's `when`, so this file needn't
        // import a trait that `super::*` brings in on some GPUI pins and not
        // on others (a redundant import fails clippy).
        if let Some(on_click) = self.on_click {
            button = button.on_click(move |event, window, cx| on_click(event, window, cx));
        }
        if let Some(handler) = self.on_right_mouse_down {
            button = button.on_mouse_down(MouseButton::Right, move |event, window, cx| {
                handler(event, window, cx)
            });
        }
        if focused {
            button = button.child(
                div()
                    .absolute()
                    .top(-RING_WIDTH)
                    .left(-RING_WIDTH)
                    .right(-RING_WIDTH)
                    .bottom(-RING_WIDTH)
                    .border(RING_WIDTH)
                    .border_color(ring.alpha(0.2))
                    .rounded(radius + RING_WIDTH),
            );
        }
        button
    }
}

#[cfg(test)]
mod tests {
    use super::StripIconButton;
    use gpui::{
        Context, InteractiveElement, IntoElement, KeyDownEvent, KeyUpEvent, Keystroke, Modifiers,
        MouseButton, ParentElement, Render, Styled, TestAppContext, VisualTestContext, Window, div,
        point, px,
    };
    use gpui_component::IconName;
    use std::cell::Cell;
    use std::rc::Rc;

    /// Stands in for a pill or strip: it counts the left presses that reach
    /// it, which is what starts a tab or pane drag.
    struct Strip {
        clicks: Rc<Cell<usize>>,
        strip_presses: Rc<Cell<usize>>,
    }

    impl Render for Strip {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            let (clicks, presses) = (self.clicks.clone(), self.strip_presses.clone());
            div()
                .size_full()
                .flex()
                .on_mouse_down(MouseButton::Left, move |_, _, _| {
                    presses.set(presses.get() + 1)
                })
                .child(
                    StripIconButton::new("tabx", IconName::Close, "Close tab")
                        .on_click(move |_, _, _| clicks.set(clicks.get() + 1)),
                )
        }
    }

    /// Runs `check` on a window whose top-left 20 px square is the button,
    /// with the click and strip-press counters.
    fn with_strip(check: impl FnOnce(&mut VisualTestContext, &Cell<usize>, &Cell<usize>)) {
        let mut cx = TestAppContext::single();
        cx.update(gpui_component::init);
        let clicks = Rc::new(Cell::new(0));
        let strip_presses = Rc::new(Cell::new(0));
        let window = cx.add_window({
            let (clicks, strip_presses) = (clicks.clone(), strip_presses.clone());
            move |_, _| Strip {
                clicks,
                strip_presses,
            }
        });
        let mut cx = VisualTestContext::from_window(window.into(), &cx);
        cx.run_until_parked();
        check(&mut cx, &clicks, &strip_presses);
    }

    #[test]
    fn a_click_presses_the_button_and_goes_no_further() {
        with_strip(|cx, clicks, strip_presses| {
            cx.simulate_click(point(px(10.), px(10.)), Modifiers::none());
            assert_eq!(clicks.get(), 1, "the click reaches the button");
            assert_eq!(
                strip_presses.get(),
                0,
                "the strip under the button must not see the press, or closing a tab drags it"
            );
        });
    }

    #[test]
    fn the_button_is_a_tab_stop_that_enter_presses() {
        with_strip(|cx, clicks, _| {
            cx.update(|window, cx| window.focus_next(cx));
            cx.run_until_parked();
            let enter = Keystroke::parse("enter").unwrap();
            cx.simulate_event(KeyDownEvent {
                keystroke: enter.clone(),
                is_held: false,
                prefer_character_input: false,
            });
            cx.simulate_event(KeyUpEvent { keystroke: enter });
            assert_eq!(
                clicks.get(),
                1,
                "Tab focuses the button and Enter clicks it"
            );
        });
    }
}
