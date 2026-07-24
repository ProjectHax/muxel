//! The custom gpui [`Element`] that paints a terminal grid, plus the
//! [`InputHandler`] that routes typed text to the PTY.
//!
//! Rendering mirrors Zed/okena's approach: adjacent same-style cells are
//! batched into a single shaped text run, and runs of the same background color
//! are merged into rectangles, to keep per-frame draw calls low.

use crate::colors::{TerminalPalette, brighten};
use crate::profile::{self, PaintMode};
use crate::session::{CachedRect, CachedRun, PaintDrawList, PaintMetrics, TerminalSession};
use crate::view::TerminalMouseMode;
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line, Point as GridPoint, Side};
use alacritty_terminal::selection::{Selection, SelectionType};
use alacritty_terminal::term::TermMode;
use alacritty_terminal::term::cell::Flags;
use gpui::*;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

/// gpui element rendering one [`TerminalSession`].
pub struct TerminalElement {
    session: Arc<TerminalSession>,
    /// The owning [`crate::view::TerminalView`] entity. Event handlers repaint by
    /// `cx.notify(view_id)` — NOT `window.request_animation_frame()`, which reads
    /// `current_view()`; that is only set during draw phases and **panics** (empty
    /// entity stack) when called from mouse/modifier dispatch. And not
    /// `window.refresh()`, which bypasses `AnyView::cached` for every terminal.
    view_id: EntityId,
    focus_handle: FocusHandle,
    palette: TerminalPalette,
    font_family: SharedString,
    font_size: Pixels,
    mouse_mode: TerminalMouseMode,
}

impl TerminalElement {
    pub fn new(
        session: Arc<TerminalSession>,
        view_id: EntityId,
        focus_handle: FocusHandle,
        palette: TerminalPalette,
        font_family: SharedString,
        font_size: Pixels,
        mouse_mode: TerminalMouseMode,
    ) -> Self {
        Self {
            session,
            view_id,
            focus_handle,
            palette,
            font_family,
            font_size,
            mouse_mode,
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
        let paint_t0 = Instant::now();
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
        // Resize the PTY/grid to fit — but NOT while a pane/tab is being dragged.
        // A drag churns the layout through many intermediate sizes; resizing on
        // each one floods the agent with SIGWINCHes, making it redraw (and often
        // garble) its UI repeatedly. Defer until the drag drops and the layout
        // settles — the post-drop repaint applies the final size. Divider/window
        // resizes don't set an active drag, so they stay live. The debounce still
        // coalesces a settling burst into one resize (frames are re-requested
        // while it settles).
        if !cx.has_active_drag() && self.session.resize(cols, rows) {
            // Per-view frame (not window.refresh — that bypasses AnyView::cached for all terminals).
            window.request_animation_frame();
        }

        window.paint_quad(fill(bounds, self.palette.background_hsla()));

        let origin = bounds.origin;
        let palette = self.palette.clone();
        let focused = self.focus_handle.is_focused(window);

        // Overlay scrollbar geometry (right edge of the terminal area).
        let bar_x = f32::from(bounds.origin.x) + f32::from(bounds.size.width) - SCROLLBAR_WIDTH;
        let track_h = f32::from(bounds.size.height);

        self.register_input_handlers(bounds, hitbox, state, cols, rows, window);
        let scrollbar_dragging = self.session.scrollbar_grab().is_some();
        let hovered_link = self.session.hovered_link();
        let link_hitbox = hitbox.clone();
        let bg = palette.background_hsla();
        let metrics = PaintMetrics {
            cell_w,
            line_h,
            font_size: f32::from(font_size),
            cols,
            rows,
            bg: hsla_to_arr(bg),
        };
        let content_gen = self.session.content_generation();

        // Grid baseline from the BASE font. gpui centers each ShapedLine on its
        // own ascent/descent, so a run whose shaping pulled in a fallback face
        // (e.g. "❯", box drawing) or a bold variant would drift its whole text
        // off the row baseline. Terminals have exactly one baseline per row;
        // every run is pinned to this one at paint. (shape_line is cached by
        // the text system, so this reference shape is ~free per frame.)
        let base_ref = window.text_system().shape_line(
            "M".into(),
            font_size,
            &[TextRun {
                len: 1,
                font: state.font.clone(),
                color: gpui::black(),
                background_color: None,
                underline: None,
                strikethrough: None,
            }],
            Some(cell_width),
        );
        let base_half = (base_ref.ascent - base_ref.descent) * 0.5;

        // Draw-list cache: gen+metrics hit → replay. Miss → full walk with
        // shape retention (keep ShapedLines for unchanged runs).
        let mut paint_mode = PaintMode::Full;
        let mut phases = profile::PaintPhases::default();
        let hit = self
            .session
            .with_paint_list_if_valid(content_gen, metrics, |list| {
                paint_draw_list(list, origin, cell_width, line_height, base_half, window, cx);
            });
        if hit {
            paint_mode = PaintMode::Replay;
        } else {
            let damage = self.session.take_pending_damage();
            let prev = self.session.take_paint_list();
            let t0 = Instant::now();
            let mut list = match (
                damage.prefer_partial_rebuild(rows as usize),
                prev.as_ref().filter(|p| p.metrics.same_font(&metrics)),
            ) {
                (Some(lines), Some(prev_list)) => patch_draw_list(
                    prev_list,
                    &self.session,
                    &palette,
                    content_gen,
                    metrics,
                    lines,
                ),
                _ => build_draw_list(&self.session, &palette, content_gen, metrics),
            };
            phases.build = t0.elapsed();
            phases.runs = list.runs.len() as u64;
            // Reuse shapes only from a list with the same font geometry — a
            // ShapedLine embeds glyph advances, so retaining across a zoom
            // would paint old-size glyphs (and static content would keep them
            // until the text itself changed).
            if let Some(prev) = prev.as_ref().filter(|p| p.metrics.same_font(&metrics)) {
                phases.runs_reused = retain_shaped_runs(&mut list, prev);
            }
            let t1 = Instant::now();
            shape_draw_list(&mut list, cell_width, font_size, state, window);
            phases.shape = t1.elapsed();
            let t2 = Instant::now();
            // Full submit still required under gpui (element paints whole bounds).
            // Damage cuts rebuild cost; priority scheduling cuts how often we get here.
            paint_draw_list(
                &list,
                origin,
                cell_width,
                line_height,
                base_half,
                window,
                cx,
            );
            phases.submit = t2.elapsed();
            self.session.store_paint_list(list);
        }

        // Cursor, scrollbar, and link hover stay live (not in the draw-list gen).
        self.session.with_term(|term| {
            let grid = term.grid();
            let screen_lines = grid.screen_lines();
            let history = grid.history_size();
            let display_offset = grid.display_offset() as i32;

            if let Some(link) = hovered_link.as_ref() {
                let visual = link.line + display_offset;
                if visual >= 0 && visual < screen_lines as i32 {
                    let x = px((f32::from(origin.x) + link.start as f32 * cell_w).floor());
                    let y =
                        px((f32::from(origin.y) + (visual as f32 + 1.0) * line_h - 2.0).floor());
                    let w = px((cell_w * (link.end - link.start) as f32).ceil());
                    window.paint_quad(fill(
                        Bounds::new(point(x, y), size(w, px(1.5))),
                        hsla(0.58, 0.9, 0.62, 0.95),
                    ));
                }
                window.set_cursor_style(CursorStyle::PointingHand, &link_hitbox);
            }

            // Cursor — only when the app hasn't hidden it (DECTCEM / CSI ?25l).
            let cursor = term.grid().cursor.point;
            let cursor_visual = cursor.line.0 + display_offset;
            if term.mode().contains(TermMode::SHOW_CURSOR)
                && cursor_visual >= 0
                && cursor_visual < screen_lines as i32
            {
                let x = px((f32::from(origin.x) + cursor.column.0 as f32 * cell_w).floor());
                let y = px((f32::from(origin.y) + cursor_visual as f32 * line_h).floor());
                let mut color = palette.cursor_hsla();
                color.a = if focused { 0.85 } else { 0.4 };
                window.paint_quad(fill(
                    Bounds::new(point(x, y), size(cell_width, line_height)),
                    color,
                ));
            }

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
        profile::paint_with_phases(paint_t0.elapsed(), focused, paint_mode, phases);
    }
}

/// Steal `shaped` layouts from the previous draw list for runs with identical
/// text + style. Returns how many runs reused a prior shape (shape_line skipped
/// for those).
///
/// Keyed by *content*, not grid position: a `ShapedLine` is position-independent
/// (the paint position is supplied at paint time), so when streaming output
/// scrolls every line up a row, the shifted lines still reuse last frame's
/// shaping. Position-keyed retention got ~0% reuse in exactly that case — the
/// hottest one. Duplicate texts (borders, blank runs) share one entry; the
/// clone is cheap (the glyph layout is refcounted).
fn retain_shaped_runs(list: &mut PaintDrawList, prev: &PaintDrawList) -> u64 {
    #[derive(PartialEq, Eq, Hash)]
    struct ShapeKey<'a> {
        text: &'a str,
        /// bold | italic | underline | wavy | strike, one bit each.
        style: u8,
        /// Hsla as raw bits (f32 isn't Hash; palette colors are exact-stable).
        color: [u32; 4],
    }
    fn key(run: &CachedRun) -> ShapeKey<'_> {
        ShapeKey {
            text: &run.text,
            style: run.bold as u8
                | (run.italic as u8) << 1
                | (run.underline as u8) << 2
                | (run.wavy as u8) << 3
                | (run.strike as u8) << 4,
            color: run.color.map(f32::to_bits),
        }
    }
    let mut shaped: HashMap<ShapeKey<'_>, &gpui::ShapedLine> =
        HashMap::with_capacity(prev.runs.len());
    for old in &prev.runs {
        if let Some(s) = old.shaped.as_ref() {
            shaped.entry(key(old)).or_insert(s);
        }
    }
    let mut reused = 0u64;
    for run in &mut list.runs {
        let hit = shaped.get(&key(run)).map(|s| (*s).clone());
        if let Some(s) = hit {
            run.shaped = Some(s);
            reused += 1;
        }
    }
    reused
}

fn hsla_to_arr(c: Hsla) -> [f32; 4] {
    [c.h, c.s, c.l, c.a]
}

fn arr_to_hsla(c: [f32; 4]) -> Hsla {
    hsla(c[0], c[1], c[2], c[3])
}

fn cached_rect_from_bg(r: &BgRect) -> CachedRect {
    CachedRect {
        line: r.line,
        start_col: r.start_col,
        num_cells: r.num_cells,
        color: hsla_to_arr(r.color),
    }
}

fn paint_cached_rect(
    r: &CachedRect,
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    window: &mut Window,
) {
    let pos = point(
        px((f32::from(origin.x) + r.start_col as f32 * f32::from(cell_width)).floor()),
        origin.y + line_height * r.line as f32,
    );
    let sz = size(
        px((f32::from(cell_width) * r.num_cells as f32).ceil()),
        line_height,
    );
    window.paint_quad(fill(Bounds::new(pos, sz), arr_to_hsla(r.color)));
}

fn paint_draw_list(
    list: &PaintDrawList,
    origin: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    base_half: Pixels,
    window: &mut Window,
    cx: &mut App,
) {
    for rect in &list.bg_rects {
        paint_cached_rect(rect, origin, cell_width, line_height, window);
    }
    for rect in &list.sel_rects {
        paint_cached_rect(rect, origin, cell_width, line_height, window);
    }
    for rect in &list.search_rects {
        paint_cached_rect(rect, origin, cell_width, line_height, window);
    }
    for run in &list.runs {
        let Some(shaped) = run.shaped.as_ref() else {
            continue;
        };
        // gpui places this run's baseline at y + line_height/2 + (asc-desc)/2
        // using the RUN's own metrics; shift y so every run lands on the base
        // font's baseline instead (one baseline per row, like every terminal).
        let run_half = (shaped.ascent - shaped.descent) * 0.5;
        let pos = Point::new(
            origin.x + run.start_col as f32 * cell_width,
            origin.y + run.start_line as f32 * line_height + (base_half - run_half),
        );
        let _ = shaped.paint(pos, line_height, TextAlign::Left, None, window, cx);
    }
}

/// Shape every run once after a full grid walk. Stored for replay paints.
fn shape_draw_list(
    list: &mut PaintDrawList,
    cell_width: Pixels,
    font_size: Pixels,
    state: &TermLayout,
    window: &mut Window,
) {
    let text_system = window.text_system();
    for run in &mut list.runs {
        if run.shaped.is_some() {
            continue;
        }
        let color = arr_to_hsla(run.color);
        let font = match (run.bold, run.italic) {
            (true, true) => state.font_bold_italic.clone(),
            (true, false) => state.font_bold.clone(),
            (false, true) => state.font_italic.clone(),
            (false, false) => state.font.clone(),
        };
        let underline = if run.underline {
            Some(UnderlineStyle {
                color: Some(color),
                thickness: px(1.0),
                wavy: run.wavy,
            })
        } else {
            None
        };
        let strikethrough = if run.strike {
            Some(StrikethroughStyle {
                color: Some(color),
                thickness: px(1.0),
            })
        } else {
            None
        };
        let style = TextRun {
            len: run.text.len(),
            font,
            color,
            background_color: None,
            underline,
            strikethrough,
        };
        run.shaped = Some(text_system.shape_line(
            run.text.clone().into(),
            font_size,
            &[style],
            Some(cell_width),
        ));
    }
}

/// Rebuild only `visual_lines` into a clone of `prev`, dropping old runs/rects
/// on those lines. Used when alacritty reports partial damage.
fn patch_draw_list(
    prev: &PaintDrawList,
    session: &TerminalSession,
    palette: &TerminalPalette,
    content_gen: u64,
    metrics: PaintMetrics,
    visual_lines: &[i32],
) -> PaintDrawList {
    let mut list = prev.clone();
    list.content_gen = content_gen;
    list.metrics = metrics;
    let damaged: std::collections::HashSet<i32> = visual_lines.iter().copied().collect();
    list.runs.retain(|r| !damaged.contains(&r.start_line));
    list.bg_rects.retain(|r| !damaged.contains(&r.line));
    list.sel_rects.retain(|r| !damaged.contains(&r.line));
    list.search_rects.retain(|r| !damaged.contains(&r.line));

    let patch = build_draw_list_rows(session, palette, content_gen, metrics, visual_lines);
    list.runs.extend(patch.runs);
    list.bg_rects.extend(patch.bg_rects);
    list.sel_rects.extend(patch.sel_rects);
    list.search_rects.extend(patch.search_rects);
    // Stable paint order: top-to-bottom, left-to-right.
    list.runs.sort_by_key(|a| (a.start_line, a.start_col));
    list.bg_rects.sort_by_key(|a| (a.line, a.start_col));
    list.sel_rects.sort_by_key(|a| (a.line, a.start_col));
    list.search_rects.sort_by_key(|a| (a.line, a.start_col));
    list
}

fn build_draw_list(
    session: &TerminalSession,
    palette: &TerminalPalette,
    content_gen: u64,
    metrics: PaintMetrics,
) -> PaintDrawList {
    let rows: Vec<i32> = (0..metrics.rows as i32).collect();
    build_draw_list_rows(session, palette, content_gen, metrics, &rows)
}

fn build_draw_list_rows(
    session: &TerminalSession,
    palette: &TerminalPalette,
    content_gen: u64,
    metrics: PaintMetrics,
    visual_rows: &[i32],
) -> PaintDrawList {
    let search_needle = session.search_needle();
    session.with_term(|term| {
        let grid = term.grid();
        let screen_lines = grid.screen_lines() as i32;
        let columns = grid.columns();
        let display_offset = grid.display_offset() as i32;
        let sel_range = term.selection.as_ref().and_then(|s| s.to_range(term));
        let sel_color = palette.selection_hsla();
        let search_color = hsla(0.13, 0.9, 0.5, 0.42);

        let mut runs: Vec<BatchedRun> = Vec::new();
        let mut rects: Vec<BgRect> = Vec::new();
        let mut sel_rects: Vec<BgRect> = Vec::new();
        let mut search_rects: Vec<BgRect> = Vec::new();
        let mut cur_run: Option<BatchedRun> = None;
        let mut cur_rect: Option<BgRect> = None;
        let mut cur_sel: Option<BgRect> = None;
        let mut cur_search: Option<BgRect> = None;

        for &visual_line in visual_rows {
            if visual_line < 0 || visual_line >= screen_lines {
                continue;
            }
            let row = visual_line as usize;
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
                if cell.c == ' ' && !cell.flags.intersects(Flags::UNDERLINE | Flags::STRIKEOUT) {
                    continue;
                }

                let mut fg_color = palette.color(&fg);
                if cell.flags.contains(Flags::DIM) && !cell.flags.contains(Flags::BOLD) {
                    fg_color.l = (fg_color.l * 0.66).clamp(0.0, 1.0);
                }

                let style = RunStyle {
                    bold: cell.flags.contains(Flags::BOLD),
                    italic: cell.flags.contains(Flags::ITALIC),
                    color: fg_color,
                    underline: cell.flags.intersects(Flags::ALL_UNDERLINES),
                    wavy: cell.flags.contains(Flags::UNDERCURL),
                    strike: cell.flags.contains(Flags::STRIKEOUT),
                };

                let append = cur_run
                    .as_ref()
                    .is_some_and(|r| r.can_append(style, visual_line, col_i));
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

        let cached_runs: Vec<CachedRun> = runs
            .into_iter()
            .map(|r| CachedRun {
                start_line: r.start_line,
                start_col: r.start_col,
                text: r.text,
                bold: r.style.bold,
                italic: r.style.italic,
                color: hsla_to_arr(r.style.color),
                underline: r.style.underline,
                wavy: r.style.wavy,
                strike: r.style.strike,
                shaped: None,
            })
            .collect();

        PaintDrawList {
            content_gen,
            metrics,
            runs: cached_runs,
            bg_rects: rects.iter().map(cached_rect_from_bg).collect(),
            sel_rects: sel_rects.iter().map(cached_rect_from_bg).collect(),
            search_rects: search_rects.iter().map(cached_rect_from_bg).collect(),
        }
    })
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

/// The clickable link under a pixel position (relative to the terminal origin):
/// an OSC 8 hyperlink on the cell (highest priority), an `http(s)` URL in the
/// line text, or a file path that resolves to an existing local file (relative
/// paths resolve against the session's spawn cwd — remote panes have none, so
/// their paths never match).
fn link_at(
    session: &TerminalSession,
    local: Point<Pixels>,
    cell_width: Pixels,
    line_height: Pixels,
    cols: u16,
    rows: u16,
) -> Option<crate::session::HoveredLink> {
    use crate::session::HoveredLink;
    session.with_term(|term| {
        let grid = term.grid();
        let off = grid.display_offset() as i32;
        // Clamp against the LIVE grid. `cols`/`rows` may be a stale cached
        // `PointerHit` — Ctrl pressed without a mouse move after the pane resized
        // — and `grid[point]` below is a raw `Vec` index that panics if the
        // cached width exceeds the current grid.
        let cols = cols.min(grid.columns() as u16);
        let rows = rows.min(grid.screen_lines() as u16);
        let (point, _side) = grid_point(local, cell_width, line_height, cols, rows, off);
        let columns = grid.columns();

        // OSC 8 hyperlink: underline the contiguous run of cells carrying the
        // same URI (the hyperlink id/uri, not the visible text, is the link).
        // Agents sometimes put a bare relative path in the URI; resolve those
        // the same way as plain path text so the open target is a real file://.
        if let Some(link) = grid[point].hyperlink() {
            let raw_uri = link.uri().to_string();
            let same = |c: usize| {
                grid[GridPoint::new(point.line, Column(c))]
                    .hyperlink()
                    .is_some_and(|h| h.uri() == raw_uri)
            };
            let mut start = point.column.0;
            while start > 0 && same(start - 1) {
                start -= 1;
            }
            let mut end = point.column.0 + 1;
            while end < columns && same(end) {
                end += 1;
            }
            let url = normalize_link_uri(&raw_uri, session);
            return Some(HoveredLink {
                line: point.line.0,
                start,
                end,
                url,
            });
        }

        let chars: Vec<char> = (0..columns)
            .map(|c| grid[GridPoint::new(point.line, Column(c))].c)
            .collect();
        if let Some((start, end, url)) = crate::links::url_span_at(&chars, point.column.0) {
            return Some(HoveredLink {
                line: point.line.0,
                start,
                end,
                url,
            });
        }
        if let Some((start, end, raw)) = crate::links::path_span_at(&chars, point.column.0) {
            let home = std::env::var_os("HOME")
                .or_else(|| std::env::var_os("USERPROFILE"))
                .map(std::path::PathBuf::from);
            if let Some(abs) = crate::links::resolve_path(&raw, session.cwd(), home.as_deref())
                && abs.exists()
            {
                return Some(HoveredLink {
                    line: point.line.0,
                    start,
                    end,
                    url: crate::links::file_uri(&abs),
                });
            }
        }
        None
    })
}

/// Turn a raw OSC 8 / pasted URI into something `OpenLink` can open: leave
/// `http(s)`/`file://` alone; resolve bare filesystem paths against the pane cwd.
fn normalize_link_uri(uri: &str, session: &TerminalSession) -> String {
    if uri.starts_with("http://")
        || uri.starts_with("https://")
        || uri.starts_with("file://")
        || uri.starts_with("mailto:")
    {
        return uri.to_string();
    }
    let home = std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(std::path::PathBuf::from);
    if let Some(abs) = crate::links::resolve_path(uri, session.cwd(), home.as_deref())
        && abs.exists()
    {
        return crate::links::file_uri(&abs);
    }
    uri.to_string()
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

/// Everything that distinguishes one text run from the next for batching and
/// shaping. The full gpui `Font`/`TextRun` is only built once per run at shape
/// time (see `shape_draw_list`), not per cell.
#[derive(Clone, Copy, PartialEq)]
struct RunStyle {
    bold: bool,
    italic: bool,
    color: Hsla,
    underline: bool,
    wavy: bool,
    strike: bool,
}

/// A run of adjacent cells sharing the same style, shaped and painted together.
struct BatchedRun {
    start_line: i32,
    start_col: i32,
    text: String,
    cell_count: usize,
    style: RunStyle,
}

impl BatchedRun {
    fn new(start_line: i32, start_col: i32, c: char, style: RunStyle) -> Self {
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

    fn can_append(&self, other: RunStyle, line: i32, col: i32) -> bool {
        self.start_line == line
            && self.start_col + self.cell_count as i32 == col
            && self.style == other
    }

    fn append(&mut self, c: char) {
        self.text.push(c);
        self.cell_count += 1;
    }
}

/// A run of adjacent cells sharing a non-default background color.
struct BgRect {
    line: i32,
    start_col: i32,
    num_cells: usize,
    color: Hsla,
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
            let t0 = Instant::now();
            self.session.write_input(text.as_bytes());
            // InputHandler has no is_held; key-repeat for letters often lands here.
            profile::key_handled(false, t0.elapsed());
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

impl TerminalElement {
    /// Register this frame's mouse and modifier handlers. gpui re-registers
    /// element listeners on every paint; splitting them out keeps
    /// [`Element::paint`] focused on drawing.
    fn register_input_handlers(
        &self,
        bounds: Bounds<Pixels>,
        hitbox: &Hitbox,
        state: &TermLayout,
        cols: u16,
        rows: u16,
        window: &mut Window,
    ) {
        let cell_width = state.cell_width;
        let line_height = state.line_height;
        let cell_w = f32::from(cell_width);
        let line_h = f32::from(line_height);
        let origin = bounds.origin;
        // Overlay scrollbar geometry (right edge of the terminal area).
        let bar_x = f32::from(bounds.origin.x) + f32::from(bounds.size.width) - SCROLLBAR_WIDTH;
        let track_top = f32::from(bounds.origin.y);
        let track_h = f32::from(bounds.size.height);

        // File drag-and-drop from Explorer is handled on the TerminalView wrapper
        // via InteractiveElement::on_drop::<ExternalPaths> — gpui rewrites OS
        // FileDrop into an internal drag, so FileDropEvent mouse listeners never
        // see Entered/Submit.

        // ---- Ctrl/Cmd+click: open the link (OSC 8 / URL / file path) under the
        // cursor. Dispatched as an action so the app decides where it opens
        // (built-in browser vs the OS).
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            let focus = self.focus_handle.clone();
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
                if let Some(link) = link_at(
                    &session,
                    e.position - origin,
                    cell_width,
                    line_height,
                    cols,
                    rows,
                ) {
                    focus.dispatch_action(&crate::view::OpenLink(link.url), window, cx);
                }
            });
        }
        // ---- Ctrl/Cmd+hover: underline the link under the cursor ----
        // Always remember the pointer so Ctrl pressed *without* a move still
        // hit-tests (users often park the mouse, then press Ctrl).
        let view_id = self.view_id;
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            let cw = f32::from(cell_width);
            let lh = f32::from(line_height);
            window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let local = e.position - origin;
                if hitbox.is_hovered(window) {
                    session.set_pointer_hit(Some(crate::session::PointerHit {
                        local_x: f32::from(local.x),
                        local_y: f32::from(local.y),
                        cell_width: cw,
                        line_height: lh,
                        cols,
                        rows,
                    }));
                } else {
                    session.set_pointer_hit(None);
                }
                let link = if e.modifiers.secondary()
                    && e.pressed_button.is_none()
                    && hitbox.is_hovered(window)
                {
                    link_at(&session, local, cell_width, line_height, cols, rows)
                } else {
                    None
                };
                if session.set_hovered_link(link) {
                    cx.notify(view_id);
                }
            });
        }
        // Ctrl/Cmd pressed or released: re-hit-test without waiting for a move.
        {
            let session = self.session.clone();
            window.on_modifiers_changed(move |e: &ModifiersChangedEvent, _window, cx| {
                if !e.modifiers.secondary() {
                    if session.set_hovered_link(None) {
                        cx.notify(view_id);
                    }
                    return;
                }
                // Secondary just went down (or is held through another mod change).
                let Some(hit) = session.pointer_hit() else {
                    return;
                };
                let link = link_at(
                    &session,
                    point(px(hit.local_x), px(hit.local_y)),
                    px(hit.cell_width),
                    px(hit.line_height),
                    hit.cols,
                    hit.rows,
                );
                if session.set_hovered_link(link) {
                    cx.notify(view_id);
                }
            });
        }
        // ---- Mouse: forward to the app when it enabled mouse reporting
        // (Grok/Claude/vim); otherwise (or with Shift held) do local selection. ----
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            let mouse_mode = self.mouse_mode;
            let cw = f32::from(cell_width);
            let lh = f32::from(line_height);
            window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || e.modifiers.secondary() || cx.has_active_drag()
                {
                    return;
                }
                if !hitbox.is_hovered(window) {
                    return;
                }
                // Scrollbar owns left-click in its strip.
                if e.button == MouseButton::Left
                    && f32::from(e.position.x) >= bar_x
                    && session.grid_metrics().0 > 0
                {
                    return;
                }
                let local = e.position - origin;
                let col = ((f32::from(local.x).max(0.0) / cw) as usize)
                    .min(cols.saturating_sub(1) as usize);
                let row = ((f32::from(local.y).max(0.0) / lh) as usize)
                    .min(rows.saturating_sub(1) as usize);
                let button = match e.button {
                    MouseButton::Left => 0u8,
                    MouseButton::Middle => 1,
                    MouseButton::Right => 2,
                    _ => return,
                };
                // Mouse-reporting apps get the click (Shift forces local select).
                // In RightClickMenu mode, right-click is reserved for the local
                // Copy/Paste menu (the muxel crate's context-menu wrapper owns it),
                // so don't *also* forward it to the app — that double-fires.
                let right_click_owned_by_menu =
                    button == 2 && mouse_mode == TerminalMouseMode::RightClickMenu;
                if session.mouse_reporting() && !e.modifiers.shift && !right_click_owned_by_menu {
                    let _ = session.clear_selection();
                    session.report_mouse_button(
                        col,
                        row,
                        button,
                        true,
                        e.modifiers.shift,
                        e.modifiers.alt,
                        e.modifiers.control,
                    );
                    return;
                }
                // Local text selection (left button only).
                if e.button != MouseButton::Left {
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
                cx.notify(view_id);
            });
        }
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            let cw = f32::from(cell_width);
            let lh = f32::from(line_height);
            window.on_mouse_event(move |e: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || cx.has_active_drag() {
                    return;
                }
                // Forward motion to mouse-aware apps (drag / any-event).
                if session.mouse_reporting()
                    && session.mouse_motion_reporting()
                    && hitbox.is_hovered(window)
                    && !e.modifiers.shift
                    && !session.is_selecting()
                {
                    let local = e.position - origin;
                    let col = ((f32::from(local.x).max(0.0) / cw) as usize)
                        .min(cols.saturating_sub(1) as usize);
                    let row = ((f32::from(local.y).max(0.0) / lh) as usize)
                        .min(rows.saturating_sub(1) as usize);
                    let button = match e.pressed_button {
                        Some(MouseButton::Left) => Some(0u8),
                        Some(MouseButton::Middle) => Some(1),
                        Some(MouseButton::Right) => Some(2),
                        _ => None,
                    };
                    session.report_mouse_motion(
                        col,
                        row,
                        button,
                        e.modifiers.shift,
                        e.modifiers.alt,
                        e.modifiers.control,
                    );
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
                cx.notify(view_id);
            });
        }
        {
            let session = self.session.clone();
            let mouse_mode = self.mouse_mode;
            let cw = f32::from(cell_width);
            let lh = f32::from(line_height);
            window.on_mouse_event(move |e: &MouseUpEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                // Release report: if we forwarded this button's *press* to the app,
                // always send the release — even if the pointer has left the pane or
                // Shift is now held — or the app is stranded with a phantom held
                // button (e.g. a vim visual-drag that ends in a sibling split).
                if let Some(button) = session.mouse_press_pending() {
                    let local = e.position - origin;
                    let col = ((f32::from(local.x).max(0.0) / cw) as usize)
                        .min(cols.saturating_sub(1) as usize);
                    let row = ((f32::from(local.y).max(0.0) / lh) as usize)
                        .min(rows.saturating_sub(1) as usize);
                    session.report_mouse_button(
                        col,
                        row,
                        button,
                        false,
                        e.modifiers.shift,
                        e.modifiers.alt,
                        e.modifiers.control,
                    );
                }
                if e.button == MouseButton::Left {
                    session.stop_selecting();
                    // "Copy on select": the moment a drag-selection ends, put it on
                    // the clipboard (the highlight stays so it's clear what copied).
                    if mouse_mode == TerminalMouseMode::CopyOnSelect
                        && let Some(text) = session.selection_to_string()
                        && !text.is_empty()
                    {
                        cx.write_to_clipboard(ClipboardItem::new_string(text));
                    }
                }
            });
        }
        // ---- Mouse wheel: scroll history, or forward to a mouse-aware app ----
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            window.on_mouse_event(move |e: &ScrollWheelEvent, phase, window, cx| {
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
                    cx.notify(view_id);
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
                cx.notify(view_id);
            });
        }
        {
            let session = self.session.clone();
            window.on_mouse_event(move |e: &MouseMoveEvent, phase, _window, cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let Some(grab) = session.scrollbar_grab() else {
                    return;
                };
                apply_scrollbar_drag(&session, track_h, f32::from(e.position.y) - track_top, grab);
                cx.notify(view_id);
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
        // Right-click behavior depends on the mouse mode. (In RightClickMenu mode
        // this handler is a no-op — the muxel crate wraps the view in a Copy/Paste
        // context menu, which owns right-click. We must not consume the event here.)
        {
            let session = self.session.clone();
            let hitbox = hitbox.clone();
            let mouse_mode = self.mouse_mode;
            window.on_mouse_event(move |e: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || e.button != MouseButton::Right
                    || !hitbox.is_hovered(window)
                {
                    return;
                }
                // When the app owns the mouse, don't steal right-click for paste
                // (already reported as a mouse event by the click handler).
                if session.mouse_reporting() && !e.modifiers.shift {
                    return;
                }
                match mouse_mode {
                    // Copy the selection if there is one, otherwise paste.
                    TerminalMouseMode::CopyPaste => {
                        if let Some(text) = session.selection_to_string().filter(|t| !t.is_empty())
                        {
                            cx.write_to_clipboard(ClipboardItem::new_string(text));
                            session.clear_selection();
                            cx.notify(view_id);
                        } else {
                            crate::view::paste_clipboard_into_session(&session, cx);
                        }
                    }
                    // The selection already auto-copied; right-click pastes and
                    // drops the (now stale) selection highlight.
                    TerminalMouseMode::CopyOnSelect => {
                        crate::view::paste_clipboard_into_session(&session, cx);
                        if session.clear_selection() {
                            cx.notify(view_id);
                        }
                    }
                    // Handled by the context menu in the muxel crate.
                    TerminalMouseMode::RightClickMenu => {}
                }
            });
        }
    }
}
