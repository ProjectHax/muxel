//! Controlled split layout. Persistent sizes are preferences; measured bounds
//! and temporary minimum clamps never become preferences without a user drag.

use gpui::{
    Along, AnyElement, App, AppContext, AvailableSpace, Axis, Bounds, Context, Element, ElementId,
    Empty, Entity, GlobalElementId, InspectorElementId, InteractiveElement, IntoElement, LayoutId,
    MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, ParentElement, Pixels, Render,
    StatefulInteractiveElement, Style, Styled, Window, div, point, px, relative, size,
};
use gpui_component::ActiveTheme;
use muxel_core::geometry::{fixed_panel_size, proportional_sizes};
use std::{ops::Range, rc::Rc};

type ResizeCallback = Rc<dyn Fn(&Entity<SplitState>, &mut Window, &mut App)>;
type ResetCallback = Rc<dyn Fn(&mut Window, &mut App)>;

pub fn h_resizable(id: impl Into<ElementId>) -> Split {
    Split::new(id.into(), Axis::Horizontal)
}

pub fn v_resizable(id: impl Into<ElementId>) -> Split {
    Split::new(id.into(), Axis::Vertical)
}

pub fn resizable_panel() -> Panel {
    Panel {
        preferred: px(1.0),
        fixed: false,
        range: px(100.0)..Pixels::MAX,
        children: vec![],
    }
}

pub struct Panel {
    preferred: Pixels,
    fixed: bool,
    range: Range<Pixels>,
    children: Vec<AnyElement>,
}

impl Panel {
    pub fn size(mut self, size: Pixels) -> Self {
        self.preferred = size;
        self
    }
    pub fn fixed_size(mut self, size: Pixels) -> Self {
        self.preferred = size;
        self.fixed = true;
        self
    }
    pub fn size_range(mut self, range: Range<Pixels>) -> Self {
        self.range = range;
        self
    }
}

impl ParentElement for Panel {
    fn extend(&mut self, elements: impl IntoIterator<Item = AnyElement>) {
        self.children.extend(elements);
    }
}

#[derive(Clone)]
struct Drag;
impl Render for Drag {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        Empty
    }
}

struct DragStart {
    index: usize,
    pointer: Pixels,
    sizes: Vec<Pixels>,
    preferred: Vec<Pixels>,
    moved: bool,
}

#[derive(Default)]
pub struct SplitState {
    preferred: Vec<Pixels>,
    measured: Vec<Pixels>,
    pressed_at: Option<Pixels>,
    drag: Option<DragStart>,
}

impl SplitState {
    /// Drag result, only read by the persistence callback after a real drag.
    pub fn sizes(&self) -> &[Pixels] {
        &self.preferred
    }

    fn start_drag(&mut self, index: usize, handle_origin: Pixels, click_offset: Pixels) {
        self.drag = Some(DragStart {
            index,
            pointer: self
                .pressed_at
                .take()
                .unwrap_or(handle_origin + click_offset),
            sizes: self.measured.clone(),
            preferred: self.preferred.clone(),
            moved: false,
        });
    }

    fn begin_drag(
        &mut self,
        index: usize,
        handle_origin: Pixels,
        click_offset: Pixels,
        ranges: &[Range<Pixels>],
        fixed: bool,
    ) {
        self.start_drag(index, handle_origin, click_offset);
        // The group's move listener runs before GPUI starts a drag. Apply this
        // threshold-crossing move now, including gestures with only one move.
        self.drag_to(handle_origin + click_offset, ranges, fixed);
    }

    fn finish_drag(&mut self, pointer: Pixels, ranges: &[Range<Pixels>], fixed: bool) -> bool {
        self.pressed_at = None;
        self.drag_to(pointer, ranges, fixed);
        self.drag.take().is_some_and(|drag| drag.moved)
    }

    fn drag_to(&mut self, pointer: Pixels, ranges: &[Range<Pixels>], fixed: bool) -> bool {
        let Some(drag) = &mut self.drag else {
            return false;
        };
        let ix = drag.index;
        let delta = pointer - drag.pointer;
        if delta == px(0.0) && !drag.moved {
            return false;
        }
        let pair = drag.sizes[ix] + drag.sizes[ix + 1];
        let lower = ranges[ix].start.max(pair - ranges[ix + 1].end);
        let upper = ranges[ix].end.min(pair - ranges[ix + 1].start).max(lower);
        let left = (drag.sizes[ix] + delta).clamp(lower, upper);
        if !drag.moved && left == drag.sizes[ix] {
            return false;
        }
        self.preferred.clone_from(&drag.preferred);
        if fixed {
            self.preferred[ix] = left;
            self.preferred[ix + 1] = pair - left;
        } else {
            // Unclamped children share a weight/pixel scale. Minimum-clamped
            // children have a smaller ratio, so the maximum recovers that scale.
            // Convert this pair's desired pixels back to weights at that scale;
            // retaining its old total weight would move untouched siblings when
            // one of the dragged children was previously minimum-clamped.
            let scale = drag
                .preferred
                .iter()
                .zip(&drag.sizes)
                .map(|(&preferred, &measured)| {
                    let preferred = f32::from(preferred);
                    let weight = if preferred.is_finite() && preferred > 0.0 {
                        preferred
                    } else {
                        1.0
                    };
                    weight / f32::from(measured).max(f32::MIN_POSITIVE)
                })
                .fold(0.0_f32, f32::max);
            self.preferred[ix] = left * scale;
            self.preferred[ix + 1] = (pair - left) * scale;
        }
        drag.moved = true;
        true
    }
}

pub struct Split {
    id: ElementId,
    axis: Axis,
    panels: Vec<Panel>,
    on_resize: ResizeCallback,
    on_reset: Option<ResetCallback>,
}

impl Split {
    fn new(id: ElementId, axis: Axis) -> Self {
        Self {
            id,
            axis,
            panels: vec![],
            on_resize: Rc::new(|_, _, _| {}),
            on_reset: None,
        }
    }
    pub fn child(mut self, panel: Panel) -> Self {
        self.panels.push(panel);
        self
    }
    pub fn on_resize(
        mut self,
        callback: impl Fn(&Entity<SplitState>, &mut Window, &mut App) + 'static,
    ) -> Self {
        self.on_resize = Rc::new(callback);
        self
    }
    pub fn on_reset(mut self, callback: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_reset = Some(Rc::new(callback));
        self
    }
}

impl IntoElement for Split {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

impl Element for Split {
    type RequestLayoutState = Entity<SplitState>;
    type PrepaintState = Vec<AnyElement>;

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }
    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Entity<SplitState>) {
        let state = window.use_keyed_state(self.id.clone(), cx, |_, _| SplitState::default());
        state.update(cx, |state, cx| {
            // A cancelled drag must not leave unsaved transient sizes behind.
            if !cx.has_active_drag() {
                state.drag = None;
            }
            if state.drag.is_none() {
                state.preferred = self.panels.iter().map(|panel| panel.preferred).collect();
            }
        });
        let style = Style {
            size: size(relative(1.0).into(), relative(1.0).into()),
            ..Style::default()
        };
        (window.request_layout(style, None, cx), state)
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Entity<SplitState>,
        window: &mut Window,
        cx: &mut App,
    ) -> Vec<AnyElement> {
        let extent = f32::from(bounds.size.along(self.axis));
        let preferred: Vec<f32> = state
            .read(cx)
            .preferred
            .iter()
            .copied()
            .map(f32::from)
            .collect();
        let minimums: Vec<f32> = self
            .panels
            .iter()
            .map(|panel| f32::from(panel.range.start))
            .collect();
        let sizes = if let Some(index) = self.panels.iter().position(|panel| panel.fixed) {
            // Side-panel groups are exactly [fixed, center] or [center, fixed].
            debug_assert_eq!(self.panels.len(), 2);
            let panel = &self.panels[index];
            let fixed = fixed_panel_size(
                extent,
                preferred[index],
                minimums[index],
                f32::from(panel.range.end),
                minimums[1 - index],
            );
            let mut sizes = vec![0.0; 2];
            sizes[index] = fixed;
            sizes[1 - index] = (extent - fixed).max(minimums[1 - index]);
            sizes
        } else {
            proportional_sizes(extent, &preferred, &minimums)
        };
        state.update(cx, |state, _| {
            state.measured = sizes.iter().copied().map(px).collect()
        });

        let mut elements = Vec::with_capacity(self.panels.len() * 2);
        let drag_ranges: Rc<Vec<_>> = Rc::new(
            self.panels
                .iter()
                .map(|panel| panel.range.clone())
                .collect(),
        );
        let fixed = self.panels.iter().any(|panel| panel.fixed);
        let mut offset = px(0.0);
        for (index, panel) in self.panels.iter_mut().enumerate() {
            let panel_size = match self.axis {
                Axis::Horizontal => size(px(sizes[index]), bounds.size.height),
                Axis::Vertical => size(bounds.size.width, px(sizes[index])),
            };
            let origin = bounds.origin
                + match self.axis {
                    Axis::Horizontal => point(offset, px(0.0)),
                    Axis::Vertical => point(px(0.0), offset),
                };
            let mut content = div()
                .id(("panel", index))
                .w(panel_size.width)
                .h(panel_size.height)
                .children(std::mem::take(&mut panel.children))
                .into_any_element();
            content.layout_as_root(panel_size.map(AvailableSpace::Definite), window, cx);
            content.prepaint_at(origin, window, cx);
            elements.push(content);

            if index > 0 {
                let drag_state = state.clone();
                let press_state = state.clone();
                let drag_ranges = drag_ranges.clone();
                let axis = self.axis;
                let reset = self.on_reset.clone();
                let handle_size = match axis {
                    Axis::Horizontal => size(px(8.0), bounds.size.height),
                    Axis::Vertical => size(bounds.size.width, px(8.0)),
                };
                let mut handle = div()
                    .id(("divider", index))
                    .w(handle_size.width)
                    .h(handle_size.height)
                    .occlude()
                    .on_mouse_down(
                        MouseButton::Left,
                        move |event: &MouseDownEvent, window, cx| {
                            let resetting = event.click_count >= 2 && reset.is_some();
                            press_state.update(cx, |state, _| {
                                state.pressed_at =
                                    (!resetting).then_some(event.position.along(axis));
                            });
                            if event.click_count >= 2
                                && let Some(reset) = &reset
                            {
                                reset(window, cx);
                                cx.stop_propagation();
                            }
                        },
                    )
                    .on_drag(Drag, move |_, position, _, cx| {
                        drag_state.update(cx, |state, _| {
                            state.begin_drag(
                                index - 1,
                                origin.along(axis) - px(4.0),
                                position.along(axis),
                                &drag_ranges,
                                fixed,
                            );
                        });
                        cx.stop_propagation();
                        cx.new(|_| Drag)
                    });
                handle = match axis {
                    Axis::Horizontal => handle
                        .cursor_col_resize()
                        .child(div().ml(px(3.5)).w(px(1.0)).h_full().bg(cx.theme().border)),
                    Axis::Vertical => handle
                        .cursor_row_resize()
                        .child(div().mt(px(3.5)).h(px(1.0)).w_full().bg(cx.theme().border)),
                };
                let mut handle = handle.into_any_element();
                handle.layout_as_root(handle_size.map(AvailableSpace::Definite), window, cx);
                let handle_origin = origin
                    - match axis {
                        Axis::Horizontal => point(px(4.0), px(0.0)),
                        Axis::Vertical => point(px(0.0), px(4.0)),
                    };
                handle.prepaint_at(handle_origin, window, cx);
                elements.push(handle);
            }
            offset += px(sizes[index]);
        }
        elements
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        _: Bounds<Pixels>,
        state: &mut Entity<SplitState>,
        elements: &mut Vec<AnyElement>,
        window: &mut Window,
        cx: &mut App,
    ) {
        for element in elements {
            element.paint(window, cx);
        }
        let moving = state.clone();
        let axis = self.axis;
        let fixed = self.panels.iter().any(|panel| panel.fixed);
        let ranges: Vec<_> = self
            .panels
            .iter()
            .map(|panel| panel.range.clone())
            .collect();
        let finishing_ranges = ranges.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, window, cx| {
            if !phase.bubble() {
                return;
            }
            moving.update(cx, |state, cx| {
                if state.drag_to(event.position.along(axis), &ranges, fixed) {
                    cx.notify();
                    window.refresh();
                }
            });
        });
        let finishing = state.clone();
        let on_resize = self.on_resize.clone();
        window.on_mouse_event(move |event: &MouseUpEvent, phase, window, cx| {
            if !phase.bubble() || event.button != MouseButton::Left {
                return;
            }
            let dragged = finishing.update(cx, |state, _| {
                state.finish_drag(event.position.along(axis), &finishing_ranges, fixed)
            });
            if dragged {
                on_resize(&finishing, window, cx);
                window.refresh();
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::SplitState;
    use gpui::px;

    #[test]
    fn drag_includes_movement_before_the_gpui_drag_threshold() {
        let mut state = SplitState {
            preferred: vec![px(232.0), px(1.0)],
            measured: vec![px(232.0), px(968.0)],
            pressed_at: Some(px(232.0)),
            drag: None,
        };
        // GPUI creates the drag on a later MouseMove, passing that event's
        // offset from the handle. The original MouseDown was at 232.
        let ranges = [px(160.0)..px(900.0), px(100.0)..px(10000.0)];
        state.begin_drag(0, px(228.0), px(19.0), &ranges, true);
        assert_eq!(state.sizes()[0], px(247.0));
        assert!(state.drag_to(
            px(300.0),
            &[px(160.0)..px(900.0), px(100.0)..px(10000.0)],
            true,
        ));
        assert_eq!(state.sizes()[0], px(300.0));
        assert!(state.finish_drag(px(300.0), &ranges, true));
        assert!(state.pressed_at.is_none());
    }

    #[test]
    fn one_threshold_move_then_release_commits_but_a_click_does_not() {
        let ranges = [px(160.0)..px(900.0), px(100.0)..px(10000.0)];
        let mut state = SplitState {
            preferred: vec![px(232.0), px(1.0)],
            measured: vec![px(232.0), px(968.0)],
            pressed_at: Some(px(232.0)),
            drag: None,
        };
        assert!(!state.finish_drag(px(232.0), &ranges, true));
        assert!(state.pressed_at.is_none());
        assert_eq!(state.sizes()[0], px(232.0));
        state.pressed_at = Some(px(232.0));
        state.begin_drag(0, px(228.0), px(19.0), &ranges, true);
        assert!(state.finish_drag(px(247.0), &ranges, true));
        assert_eq!(state.sizes()[0], px(247.0));
    }

    #[test]
    fn dragging_a_minimum_clamped_pane_tracks_pointer_without_moving_other_siblings() {
        let mut split = SplitState {
            preferred: vec![px(100.0), px(900.0), px(500.0)],
            measured: vec![px(340.0), px(810.0), px(450.0)],
            pressed_at: None,
            drag: None,
        };
        split.start_drag(0, px(336.0), px(4.0));
        assert!(split.drag_to(px(400.0), &vec![px(340.0)..px(10000.0); 3], false));
        let weights: Vec<f32> = split.sizes().iter().copied().map(f32::from).collect();
        let allocated = muxel_core::geometry::proportional_sizes(1600.0, &weights, &[340.0; 3]);
        for (actual, expected) in allocated.iter().zip([400.0, 750.0, 450.0]) {
            assert!((actual - expected).abs() < 0.001, "{allocated:?}");
        }
        assert_eq!(split.sizes()[2], px(500.0));
    }

    #[test]
    fn a_clamp_or_click_does_not_save_a_new_sidebar_preference() {
        let mut state = SplitState {
            preferred: vec![px(420.0), px(1.0)],
            measured: vec![px(250.0), px(100.0)],
            pressed_at: None,
            drag: None,
        };
        state.start_drag(0, px(246.0), px(4.0));
        assert!(!state.drag_to(
            px(250.0),
            &[px(160.0)..px(900.0), px(100.0)..px(10000.0)],
            true
        ));
        assert_eq!(state.sizes()[0], px(420.0));
        assert!(!state.drag.unwrap().moved);
    }

    #[test]
    fn sidebar_drag_sets_pixels_and_split_drag_keeps_unaffected_preferences() {
        let mut sidebar = SplitState {
            preferred: vec![px(232.0), px(1.0)],
            measured: vec![px(232.0), px(968.0)],
            pressed_at: None,
            drag: None,
        };
        sidebar.start_drag(0, px(228.0), px(4.0));
        assert!(sidebar.drag_to(
            px(300.0),
            &[px(160.0)..px(900.0), px(100.0)..px(10000.0)],
            true
        ));
        assert_eq!(sidebar.sizes()[0], px(300.0));

        let mut split = SplitState {
            preferred: vec![px(900.0), px(300.0), px(50.0)],
            measured: vec![px(600.0), px(200.0), px(100.0)],
            pressed_at: None,
            drag: None,
        };
        split.start_drag(0, px(596.0), px(4.0));
        assert!(split.drag_to(
            px(500.0),
            &[
                px(100.0)..px(10000.0),
                px(100.0)..px(10000.0),
                px(100.0)..px(10000.0)
            ],
            false
        ));
        assert_eq!(split.sizes(), &[px(750.0), px(450.0), px(50.0)]);
    }
}
