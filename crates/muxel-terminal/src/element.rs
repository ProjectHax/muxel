//! The custom gpui [`Element`] that paints a terminal grid, plus the
//! [`InputHandler`] that routes typed text to the PTY.
//!
//! Rendering mirrors Zed/okena's approach: adjacent same-style cells are
//! batched into a single shaped text run, and runs of the same background color
//! are merged into rectangles, to keep per-frame draw calls low.

use crate::colors::{TerminalPalette, brighten};
use crate::session::TerminalSession;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::cell::Flags;
use gpui::*;
use std::sync::Arc;

/// gpui element rendering one [`TerminalSession`].
pub struct TerminalElement {
    session: Arc<TerminalSession>,
    focus_handle: FocusHandle,
    palette: TerminalPalette,
    font_family: SharedString,
    font_size: Pixels,
}

impl TerminalElement {
    pub fn new(
        session: Arc<TerminalSession>,
        focus_handle: FocusHandle,
        palette: TerminalPalette,
        font_family: SharedString,
        font_size: Pixels,
    ) -> Self {
        Self {
            session,
            focus_handle,
            palette,
            font_family,
            font_size,
        }
    }
}

impl IntoElement for TerminalElement {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

/// Font metrics computed during layout and reused while painting.
pub struct TermLayout {
    cell_width: Pixels,
    line_height: Pixels,
    font_size: Pixels,
    font: Font,
    font_bold: Font,
    font_italic: Font,
    font_bold_italic: Font,
}

impl Element for TerminalElement {
    type RequestLayoutState = TermLayout;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let font_size = self.font_size;
        // Use a concrete, installed monospace face. The generic "monospace"
        // family does not resolve in gpui's font database and silently falls
        // back to a proportional font, which breaks fixed-width cell layout
        // (glyphs get force-spread to a wrong advance width).
        #[cfg(target_os = "macos")]
        let default_family = "Menlo";
        #[cfg(target_os = "windows")]
        let default_family = "Consolas";
        #[cfg(all(not(target_os = "macos"), not(target_os = "windows")))]
        let default_family = "DejaVu Sans Mono";
        let family: SharedString = if self.font_family.is_empty() {
            default_family.into()
        } else {
            self.font_family.clone()
        };
        let font = Font {
            family,
            features: FontFeatures::disable_ligatures(),
            fallbacks: Some(FontFallbacks::from_fonts(vec![
                "DejaVu Sans Mono".into(),
                "Liberation Mono".into(),
                "Noto Sans Mono".into(),
                "Source Code Pro".into(),
                "Menlo".into(),
                "Consolas".into(),
            ])),
            weight: FontWeight::NORMAL,
            style: FontStyle::Normal,
        };
        let font_bold = Font {
            weight: FontWeight::BOLD,
            ..font.clone()
        };
        let font_italic = Font {
            style: FontStyle::Italic,
            ..font.clone()
        };
        let font_bold_italic = Font {
            weight: FontWeight::BOLD,
            style: FontStyle::Italic,
            ..font.clone()
        };

        let text_system = window.text_system();
        let font_id = text_system.resolve_font(&font);
        let cell_width = text_system
            .advance(font_id, font_size, 'm')
            .map(|s| s.width)
            .unwrap_or(font_size * 0.6);
        let line_height = font_size * 1.3;

        let style = Style {
            size: Size {
                width: relative(1.0).into(),
                height: relative(1.0).into(),
            },
            ..Default::default()
        };
        let layout_id = window.request_layout(style, [], cx);

        (
            layout_id,
            TermLayout {
                cell_width,
                line_height,
                font_size,
                font,
                font_bold,
                font_italic,
                font_bold_italic,
            },
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _state: &mut Self::RequestLayoutState,
        window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        window.insert_hitbox(bounds, HitboxBehavior::Normal)
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        state: &mut Self::RequestLayoutState,
        hitbox: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        // Route typed text (and IME) to this terminal while it's focused.
        window.handle_input(
            &self.focus_handle,
            TerminalInputHandler {
                session: self.session.clone(),
            },
            cx,
        );

        let cell_width = state.cell_width;
        let line_height = state.line_height;
        let font_size = state.font_size;
        let cell_w = f32::from(cell_width);
        let line_h = f32::from(line_height);

        // Size the PTY/grid to the available area.
        let cols = (((f32::from(bounds.size.width) - 0.5) / cell_w)
            .floor()
            .max(1.0)) as u16;
        let rows = (((f32::from(bounds.size.height) - 0.5) / line_h)
            .floor()
            .max(1.0)) as u16;
        // Debounced: while a resize is still settling, keep requesting frames so
        // it lands even if nothing else repaints (a close/drag burst coalesces
        // into one resize, avoiding repeated agent redraws).
        if self.session.resize(cols, rows) {
            window.refresh();
        }

        window.paint_quad(fill(bounds, self.palette.background_hsla()));

        let origin = bounds.origin;
        let palette = self.palette.clone();
        let focused = self.focus_handle.is_focused(window);

        // Overlay scrollbar geometry (right edge of the terminal area).
        let bar_x = f32::from(bounds.origin.x) + f32::from(bounds.size.width) - SCROLLBAR_WIDTH;
        let track_top = f32::from(bounds.origin.y);
        let track_h = f32::from(bounds.size.height);

        // ---- Ctrl/Cmd+click: open a URL under the cursor ----
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || e.button != MouseButton::Left
                    || !e.modifiers.secondary()
                {
                    return;
                }
                if !hitbox.is_hovered(window) {
                    return;
                }
                let url = session.with_term(|term| {
                    let off = term.grid().display_offset() as i32;
                    let (point, _side) = grid_point(
                        e.position - origin,
                        cell_width,
                        line_height,
                        cols,
                        rows,
                        off,
                    );
                    let grid = term.grid();
                    let chars: Vec<char> = (0..grid.columns())
                        .map(|c| grid[GridPoint::new(point.line, Column(c))].c)
                        .collect();
                    crate::links::find_url_at(&chars, point.column.0)
                });
                if let Some(url) = url {
                    cx.open_url(&url);
                }
            });
        }
        // ---- Mouse text selection (drag to select; copy via the view's keys) ----
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || e.button != MouseButton::Left
                    || e.modifiers.secondary()
                    || cx.has_active_drag()
                {
                    return;
                }
                if !hitbox.is_hovered(window) {
                    return;
                }
                // A press on the scrollbar starts a scrollbar drag, not a selection.
                if f32::from(e.position.x) >= bar_x && session.grid_metrics().0 > 0 {
                    return;
                }
                session.with_term_mut(|term| {
                    let off = term.grid().display_offset() as i32;
                    let (point, side) = grid_point(
                        e.position - origin,
                        cell_width,
                        line_height,
                        cols,
                        rows,
                        off,
                    );
                    term.selection = Some(Selection::new(SelectionType::Simple, point, side));
                });
                session.start_selecting();
                window.refresh();
            });
        }
        {
            let session = self.session.clone();
            window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || cx.has_active_drag() {
                    return;
                }
                if e.pressed_button != Some(MouseButton::Left) || !session.is_selecting() {
                    return;
                }
                session.with_term_mut(|term| {
                    let off = term.grid().display_offset() as i32;
                    let (point, side) = grid_point(
                        e.position - origin,
                        cell_width,
                        line_height,
                        cols,
                        rows,
                        off,
                    );
                    if let Some(sel) = term.selection.as_mut() {
                        sel.update(point, side);
                    }
                });
                window.refresh();
            });
        }
        {
            let session = self.session.clone();
            window.on_mouse_event(move |e: &MouseUpEvent, phase, _window, _cx| {
                if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                    session.stop_selecting();
                }
            });
        }
        // ---- Mouse wheel: scroll history, or forward to a mouse-aware app ----
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |e: &ScrollWheelEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble || !hitbox.is_hovered(window) {
                    return;
                }
                let dy = f32::from(e.delta.pixel_delta(line_height).y);
                // Cell under the pointer — only used when the wheel is forwarded
                // to a mouse-reporting app as a mouse event.
                let local = e.position - origin;
                let col = ((f32::from(local.x).max(0.0) / cell_w) as usize)
                    .min(cols.saturating_sub(1) as usize);
                let row = ((f32::from(local.y).max(0.0) / line_h) as usize)
                    .min(rows.saturating_sub(1) as usize);
                if session.scroll_wheel(dy, line_h, col, row) {
                    window.refresh();
                }
            });
        }
        // ---- Draggable vertical scrollbar (right edge) ----
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || e.button != MouseButton::Left
                    || cx.has_active_drag()
                {
                    return;
                }
                if !hitbox.is_hovered(window) || f32::from(e.position.x) < bar_x {
                    return;
                }
                let (history, offset, screen_lines) = session.grid_metrics();
                let Some(g) = scrollbar_geom(track_h, history, offset, screen_lines) else {
                    return;
                };
                let y = f32::from(e.position.y) - track_top;
                // Grab the thumb where clicked; clicking the track jumps it under
                // the cursor (grab = half the thumb).
                let grab = if y >= g.thumb_top && y <= g.thumb_top + g.thumb_height {
                    y - g.thumb_top
                } else {
                    g.thumb_height / 2.0
                };
                session.scrollbar_drag_start(grab);
                apply_scrollbar_drag(&session, track_h, y, grab);
                window.refresh();
            });
        }
        {
            let session = self.session.clone();
            window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let Some(grab) = session.scrollbar_grab() else {
                    return;
                };
                apply_scrollbar_drag(&session, track_h, f32::from(e.position.y) - track_top, grab);
                window.refresh();
            });
        }
        {
            let session = self.session.clone();
            window.on_mouse_event(move |e: &MouseUpEvent, phase, _window, _cx| {
                if phase == DispatchPhase::Bubble && e.button == MouseButton::Left {
                    session.scrollbar_drag_end();
                }
            });
        }
        // Right-click copies the current selection to the clipboard.
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || e.button != MouseButton::Right
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                if let Some(text) = session.selection_to_string()
                    && !text.is_empty()
                {
                    cx.write_to_clipboard(ClipboardItem::new_string(text));
                    session.clear_selection();
                    window.refresh();
                }
            });
        }

        let scrollbar_dragging = self.session.scrollbar_grab().is_some();
        let search_needle = self.session.search_needle();
        self.session.with_term(|term| {
            let grid = term.grid();
            let screen_lines = grid.screen_lines();
            let columns = grid.columns();
            let history = grid.history_size();
            let display_offset = grid.display_offset() as i32;
            let sel_range = term.selection.as_ref().and_then(|s| s.to_range(term));
            let sel_color = palette.selection_hsla();
            let search_color = hsla(0.13, 0.9, 0.5, 0.42); // amber match highlight

            let mut runs: Vec<BatchedRun> = Vec::new();
            let mut rects: Vec<BgRect> = Vec::new();
            let mut sel_rects: Vec<BgRect> = Vec::new();
            let mut search_rects: Vec<BgRect> = Vec::new();
            let mut cur_run: Option<BatchedRun> = None;
            let mut cur_rect: Option<BgRect> = None;
            let mut cur_sel: Option<BgRect> = None;
            let mut cur_search: Option<BgRect> = None;

            for row in 0..screen_lines {
                let visual_line = row as i32;
                let buffer_line = visual_line - display_offset;

                if let Some(run) = cur_run.take() {
                    runs.push(run);
                }
                if let Some(rect) = cur_rect.take() {
                    rects.push(rect);
                }
                if let Some(sel) = cur_sel.take() {
                    sel_rects.push(sel);
                }
                if let Some(s) = cur_search.take() {
                    search_rects.push(s);
                }

                // Columns of this row covered by a search match (if searching).
                let row_matched: Vec<bool> = if search_needle.is_empty() {
                    Vec::new()
                } else {
                    let chars: Vec<char> = (0..columns)
                        .map(|c| {
                            grid[GridPoint {
                                line: Line(buffer_line),
                                column: Column(c),
                            }]
                            .c
                        })
                        .collect();
                    let mut m = vec![false; columns];
                    for (s, len) in crate::search::match_ranges(&chars, &search_needle) {
                        for cell in m.iter_mut().take((s + len).min(columns)).skip(s) {
                            *cell = true;
                        }
                    }
                    m
                };

                for col in 0..columns {
                    let cell = &grid[GridPoint {
                        line: Line(buffer_line),
                        column: Column(col),
                    }];
                    let col_i = col as i32;

                    let mut fg = cell.fg;
                    let mut bg = cell.bg;
                    if cell.flags.contains(Flags::BOLD) {
                        fg = brighten(fg);
                    }
                    if cell.flags.contains(Flags::INVERSE) {
                        std::mem::swap(&mut fg, &mut bg);
                    }

                    // Background rectangle (skip the default background).
                    if !palette.is_default_bg(&bg) {
                        let color = palette.color(&bg);
                        let extend = cur_rect.as_ref().is_some_and(|r| {
                            r.line == visual_line
                                && r.start_col + r.num_cells as i32 == col_i
                                && r.color == color
                        });
                        if extend {
                            cur_rect.as_mut().unwrap().num_cells += 1;
                        } else {
                            if let Some(prev) = cur_rect.take() {
                                rects.push(prev);
                            }
                            cur_rect = Some(BgRect {
                                line: visual_line,
                                start_col: col_i,
                                num_cells: 1,
                                color,
                            });
                        }
                    } else if let Some(prev) = cur_rect.take() {
                        rects.push(prev);
                    }

                    // Selection highlight (batched like background rects).
                    let selected = sel_range.as_ref().is_some_and(|r| {
                        r.contains(GridPoint {
                            line: Line(buffer_line),
                            column: Column(col),
                        })
                    });
                    if selected {
                        let extend = cur_sel.as_ref().is_some_and(|s| {
                            s.line == visual_line && s.start_col + s.num_cells as i32 == col_i
                        });
                        if extend {
                            cur_sel.as_mut().unwrap().num_cells += 1;
                        } else {
                            if let Some(prev) = cur_sel.take() {
                                sel_rects.push(prev);
                            }
                            cur_sel = Some(BgRect {
                                line: visual_line,
                                start_col: col_i,
                                num_cells: 1,
                                color: sel_color,
                            });
                        }
                    } else if let Some(prev) = cur_sel.take() {
                        sel_rects.push(prev);
                    }

                    // Search-match highlight (batched like the selection).
                    if row_matched.get(col).copied().unwrap_or(false) {
                        let extend = cur_search.as_ref().is_some_and(|s| {
                            s.line == visual_line && s.start_col + s.num_cells as i32 == col_i
                        });
                        if extend {
                            cur_search.as_mut().unwrap().num_cells += 1;
                        } else {
                            if let Some(prev) = cur_search.take() {
                                search_rects.push(prev);
                            }
                            cur_search = Some(BgRect {
                                line: visual_line,
                                start_col: col_i,
                                num_cells: 1,
                                color: search_color,
                            });
                        }
                    } else if let Some(prev) = cur_search.take() {
                        search_rects.push(prev);
                    }

                    if cell.flags.contains(Flags::WIDE_CHAR_SPACER) {
                        continue;
                    }
                    // Blank cells with no decoration produce no glyph.
                    if cell.c == ' ' && !cell.flags.intersects(Flags::UNDERLINE | Flags::STRIKEOUT)
                    {
                        continue;
                    }

                    let mut fg_color = palette.color(&fg);
                    if cell.flags.contains(Flags::DIM) && !cell.flags.contains(Flags::BOLD) {
                        fg_color.l = (fg_color.l * 0.66).clamp(0.0, 1.0);
                    }

                    let font = match (
                        cell.flags.contains(Flags::BOLD),
                        cell.flags.contains(Flags::ITALIC),
                    ) {
                        (true, true) => state.font_bold_italic.clone(),
                        (true, false) => state.font_bold.clone(),
                        (false, true) => state.font_italic.clone(),
                        (false, false) => state.font.clone(),
                    };

                    let underline = if cell.flags.intersects(Flags::ALL_UNDERLINES) {
                        Some(UnderlineStyle {
                            color: Some(fg_color),
                            thickness: px(1.0),
                            wavy: cell.flags.contains(Flags::UNDERCURL),
                        })
                    } else {
                        None
                    };
                    let strikethrough = if cell.flags.contains(Flags::STRIKEOUT) {
                        Some(StrikethroughStyle {
                            color: Some(fg_color),
                            thickness: px(1.0),
                        })
                    } else {
                        None
                    };

                    let style = TextRun {
                        len: cell.c.len_utf8(),
                        font,
                        color: fg_color,
                        background_color: None,
                        underline,
                        strikethrough,
                    };

                    let append = cur_run
                        .as_ref()
                        .is_some_and(|r| r.can_append(&style, visual_line, col_i));
                    if append {
                        cur_run.as_mut().unwrap().append(cell.c);
                    } else {
                        if let Some(prev) = cur_run.take() {
                            runs.push(prev);
                        }
                        cur_run = Some(BatchedRun::new(visual_line, col_i, cell.c, style));
                    }
                }
            }
            if let Some(run) = cur_run.take() {
                runs.push(run);
            }
            if let Some(rect) = cur_rect.take() {
                rects.push(rect);
            }
            if let Some(sel) = cur_sel.take() {
                sel_rects.push(sel);
            }
            if let Some(s) = cur_search.take() {
                search_rects.push(s);
            }

            for rect in &rects {
                rect.paint(origin, cell_width, line_height, window);
            }
            for rect in &sel_rects {
                rect.paint(origin, cell_width, line_height, window);
            }
            for rect in &search_rects {
                rect.paint(origin, cell_width, line_height, window);
            }
            for run in &runs {
                run.paint(origin, cell_width, line_height, font_size, window, cx);
            }

            // Cursor.
            let cursor = term.grid().cursor.point;
            let cursor_visual = cursor.line.0 + display_offset;
            if cursor_visual >= 0 && cursor_visual < screen_lines as i32 {
                let x = px((f32::from(origin.x) + cursor.column.0 as f32 * cell_w).floor());
                let y = px((f32::from(origin.y) + cursor_visual as f32 * line_h).floor());
                let mut color = palette.cursor_hsla();
                color.a = if focused { 0.85 } else { 0.4 };
                window.paint_quad(fill(
                    Bounds::new(point(x, y), size(cell_width, line_height)),
                    color,
                ));
            }

            // Vertical scrollbar — only when there's scrollback to show.
            if let Some(g) = scrollbar_geom(track_h, history, display_offset as usize, screen_lines)
            {
                let x = px(bar_x);
                let mut track_c = palette.foreground_hsla();
                track_c.a = 0.05;
                window.paint_quad(fill(
                    Bounds::new(
                        point(x, origin.y),
                        size(px(SCROLLBAR_WIDTH), bounds.size.height),
                    ),
                    track_c,
                ));
                let mut thumb_c = palette.foreground_hsla();
                thumb_c.a = if scrollbar_dragging { 0.5 } else { 0.3 };
                window.paint_quad(fill(
                    Bounds::new(
                        point(x + px(2.0), px(f32::from(origin.y) + g.thumb_top)),
                        size(px(SCROLLBAR_WIDTH - 4.0), px(g.thumb_height)),
                    ),
                    thumb_c,
                ));
            }
        });
    }
}

/// Width of the overlay scrollbar in pixels.
const SCROLLBAR_WIDTH: f32 = 12.0;
/// Minimum thumb height so it stays grabbable even with deep scrollback.
const SCROLLBAR_MIN_THUMB: f32 = 24.0;

/// Thumb position + size for the current scroll state, or `None` when there's
/// no scrollback history to represent.
struct ScrollbarGeom {
    thumb_top: f32,
    thumb_height: f32,
}

fn scrollbar_geom(
    track_h: f32,
    history: usize,
    offset: usize,
    screen_lines: usize,
) -> Option<ScrollbarGeom> {
    if history == 0 || track_h <= 0.0 {
        return None;
    }
    let total = (history + screen_lines) as f32;
    let thumb_height =
        (track_h * screen_lines as f32 / total).clamp(SCROLLBAR_MIN_THUMB.min(track_h), track_h);
    // Fraction from the top: 0 when fully scrolled up, 1 at the live bottom.
    let frac = (history - offset) as f32 / history as f32;
    let thumb_top = (track_h - thumb_height) * frac;
    Some(ScrollbarGeom {
        thumb_top,
        thumb_height,
    })
}

/// Scroll so the thumb's top edge lands at `cursor_y - grab` within the track.
fn apply_scrollbar_drag(session: &TerminalSession, track_h: f32, cursor_y: f32, grab: f32) {
    let (history, _offset, screen_lines) = session.grid_metrics();
    let Some(g) = scrollbar_geom(track_h, history, 0, screen_lines) else {
        return;
    };
    let denom = (track_h - g.thumb_height).max(1.0);
    let thumb_top = (cursor_y - grab).clamp(0.0, denom);
    let frac = thumb_top / denom; // 0 top .. 1 bottom
    let offset = ((1.0 - frac) * history as f32).round() as usize;
    session.set_display_offset(offset.min(history));
}

/// Map a pixel position (relative to the terminal origin) to a grid point +
/// cell side, clamped to the visible grid.
fn grid_point(
    pos: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    cols: u16,
    rows: u16,
    display_offset: i32,
) -> (GridPoint, Side) {
    let cw = f32::from(cell_width).max(1.0);
    let lh = f32::from(line_height).max(1.0);
    let x = f32::from(pos.x).max(0.0);
    let y = f32::from(pos.y).max(0.0);
    let col = ((x / cw) as usize).min(cols.saturating_sub(1) as usize);
    let row = ((y / lh) as i32).clamp(0, rows.saturating_sub(1) as i32);
    let side = if x % cw > cw / 2.0 {
        Side::Right
    } else {
        Side::Left
    };
    (
        GridPoint::new(Line(row - display_offset), Column(col)),
        side,
    )
}

/// A run of adjacent cells sharing the same style, shaped and painted together.
struct BatchedRun {
    start_line: i32,
    start_col: i32,
    text: String,
    cell_count: usize,
    style: TextRun,
}

impl BatchedRun {
    fn new(start_line: i32, start_col: i32, c: char, style: TextRun) -> Self {
        let mut text = String::with_capacity(64);
        text.push(c);
        Self {
            start_line,
            start_col,
            text,
            cell_count: 1,
            style,
        }
    }

    fn can_append(&self, other: &TextRun, line: i32, col: i32) -> bool {
        self.start_line == line
            && self.start_col + self.cell_count as i32 == col
            && self.style.font == other.font
            && self.style.color == other.color
            && self.style.background_color == other.background_color
            && self.style.underline == other.underline
            && self.style.strikethrough == other.strikethrough
    }

    fn append(&mut self, c: char) {
        self.text.push(c);
        self.cell_count += 1;
    }

    fn paint(
        &self,
        origin: Point<Pixels>,
        cell_width: Pixels,
        line_height: Pixels,
        font_size: Pixels,
        window: &mut Window,
        cx: &mut App,
    ) {
        let pos = Point::new(
            origin.x + self.start_col as f32 * cell_width,
            origin.y + self.start_line as f32 * line_height,
        );
        let run = TextRun {
            len: self.text.len(),
            font: self.style.font.clone(),
            color: self.style.color,
            background_color: self.style.background_color,
            underline: self.style.underline,
            strikethrough: self.style.strikethrough,
        };
        let _ = window
            .text_system()
            .shape_line(
                self.text.clone().into(),
                font_size,
                &[run],
                Some(cell_width),
            )
            .paint(pos, line_height, TextAlign::Left, None, window, cx);
    }
}

/// A run of adjacent cells sharing a non-default background color.
struct BgRect {
    line: i32,
    start_col: i32,
    num_cells: usize,
    color: Hsla,
}

impl BgRect {
    fn paint(
        &self,
        origin: Point<Pixels>,
        cell_width: Pixels,
        line_height: Pixels,
        window: &mut Window,
    ) {
        let pos = point(
            px((f32::from(origin.x) + self.start_col as f32 * f32::from(cell_width)).floor()),
            origin.y + line_height * self.line as f32,
        );
        let sz = size(
            px((f32::from(cell_width) * self.num_cells as f32).ceil()),
            line_height,
        );
        window.paint_quad(fill(Bounds::new(pos, sz), self.color));
    }
}

/// Routes committed text (including IME) to the PTY. Special keys are handled
/// separately via the view's `on_key_down` + `keymap::key_to_bytes`.
pub struct TerminalInputHandler {
    session: Arc<TerminalSession>,
}

impl InputHandler for TerminalInputHandler {
    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<UTF16Selection> {
        Some(UTF16Selection {
            range: 0..0,
            reversed: false,
        })
    }

    fn marked_text_range(
        &mut self,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<std::ops::Range<usize>> {
        None
    }

    fn text_for_range(
        &mut self,
        _range: std::ops::Range<usize>,
        _adjusted: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<String> {
        None
    }

    fn replace_text_in_range(
        &mut self,
        _replacement_range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        if !text.is_empty() {
            self.session.write_input(text.as_bytes());
        }
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        _range_utf16: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut App,
    ) {
        if !new_text.is_empty() {
            self.session.write_input(new_text.as_bytes());
        }
    }

    fn unmark_text(&mut self, _window: &mut Window, _cx: &mut App) {}

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<Bounds<Pixels>> {
        None
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Option<usize> {
        None
    }

    fn accepts_text_input(&mut self, _window: &mut Window, _cx: &mut App) -> bool {
        true
    }
}
