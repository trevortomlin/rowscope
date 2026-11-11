//! The spreadsheet grid as one custom element: headers, row numbers, cells,
//! selection and scrollbars are painted directly, and only the visible cells
//! are ever read. A frame costs the same for ten rows or a hundred million.

use std::sync::Arc;

use gpui::{
    App, BorderStyle, Bounds, CursorStyle, Element, ElementId, Entity, Font, FontWeight,
    GlobalElementId, Hitbox, HitboxBehavior, Hsla, InspectorElementId, IntoElement, LayoutId,
    MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, ScrollEvent, SharedString, Style,
    TextAlign, TextRun, Window, fill, point, px, quad, relative, size,
};

use crate::data;
use crate::data::value::{col_name, parse_number};
use crate::sheet::{Geom, Hit, SCROLLBAR, Sheet};
use crate::theme;

pub struct Grid {
    sheet: Entity<Sheet>,
}

impl Grid {
    pub fn new(sheet: Entity<Sheet>) -> Self {
        Grid { sheet }
    }
}

impl IntoElement for Grid {
    type Element = Self;
    fn into_element(self) -> Self {
        self
    }
}

/// One cell's text, ready to shape.
struct CellText {
    x: f32,
    w: f32,
    y: f32,
    text: SharedString,
    right: bool,
    color: Hsla,
}

/// A visible column header.
struct HeaderCell {
    c: usize,
    x: f32,
    w: f32,
    name: SharedString,
    badge: &'static str,
    selected: bool,
    filtered: bool,
    sorted: Option<bool>,
    menu_open: bool,
    hovered: bool,
    button_hover: bool,
    edge_hover: bool,
}

/// Everything paint needs, read from the sheet in prepaint so painting can
/// borrow the app mutably.
pub struct Prepaint {
    hitbox: Hitbox,
    geom: Geom,
    first_row: u64,
    rows: u64,
    sx: f32,
    sy: f64,
    zoom: f32,
    font_size: f32,
    focused: bool,
    has_header: bool,
    letter_h: f32,
    content_w: f32,
    grid_h: f32,
    col_lines: Vec<f32>,
    cells: Vec<CellText>,
    row_labels: Vec<(f32, SharedString, bool)>,
    headers: Vec<HeaderCell>,
    whole_cols: bool,
    whole_rows: bool,
    multi: bool,
    sel: Option<Bounds<Pixels>>,
    cursor: Option<Bounds<Pixels>>,
    filtered_view: bool,
    v_thumb: Option<(f32, f32)>,
    h_thumb: Option<(f32, f32)>,
    thumb_hover: (bool, bool),
    resize_cursor: bool,
    pointer_cursor: bool,
}

/// Cell text for display: one line, at most a couple of hundred characters.
fn display(v: &[u8]) -> SharedString {
    let s = data::lossy(&v[..v.len().min(1024)]);
    let s = s.trim_end();
    let needs = s.chars().count() > 240 || s.contains(['\n', '\r', '\t']);
    if !needs {
        return SharedString::from(s.to_string());
    }
    let mut out: String = s
        .chars()
        .take(240)
        .map(|c| {
            if c == '\n' || c == '\r' || c == '\t' {
                ' '
            } else {
                c
            }
        })
        .collect();
    if s.chars().count() > 240 {
        out.push('…');
    }
    out.into()
}

impl Element for Grid {
    type RequestLayoutState = ();
    type PrepaintState = Prepaint;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, ()) {
        let mut style = Style::default();
        style.size.width = relative(1.).into();
        style.size.height = relative(1.).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        window: &mut Window,
        cx: &mut App,
    ) -> Prepaint {
        let hitbox = window.insert_hitbox(bounds, HitboxBehavior::Normal);
        let s = self.sheet.read(cx);
        let geom = Geom {
            bounds,
            gutter: s.gutter_w(),
            head: s.head_h(),
            row_h: s.row_h(),
        };
        let prev = s.geom.replace(geom);
        let vp = geom.viewport();
        let (vw, vh) = (f32::from(vp.size.width), f32::from(vp.size.height));

        // Keep the scroll position valid when the window or data changes.
        let clamped_y = s.scroll_y.clamp(0., s.max_scroll_y(&geom));
        let clamped_x = s.scroll_x.clamp(0., s.max_scroll_x(&geom));
        let (sy, sx) = (clamped_y, clamped_x);

        let rh = geom.row_h as f64;
        let first_row = (sy / rh).floor() as u64;
        let view_len = s.view_len();
        let rows = (((vh as f64) / rh).ceil() as u64 + 1).min(view_len.saturating_sub(first_row));
        let c0 = s.col_at(sx).unwrap_or(s.ncols);
        let c1 = s.col_at(sx + vw).map_or(s.ncols, |c| (c + 1).min(s.ncols));
        let cols = c0..c1.max(c0);

        let mut cells = Vec::with_capacity((rows as usize) * cols.len());
        let mut row_labels = Vec::with_capacity(rows as usize);
        let filtered = s.is_filtered() || s.query.sort.is_some();
        let first = s.first();
        let mut widest = s.seen_cols.get();
        for i in 0..rows {
            let vr = first_row + i;
            let y = (vr as f64 * rh - sy) as f32;
            let drow = s.data_row(vr);
            let phys = drow + first;
            row_labels.push((
                y,
                SharedString::from(data::value::count(drow + 1)),
                s.sel_rows().contains(&vr),
            ));
            if phys >= s.data.rows {
                continue;
            }
            let edited = s.edits.rows.contains(&phys);
            let mut push = |c: usize, v: &[u8]| {
                if !cols.contains(&c) {
                    return;
                }
                let (v, color) = match edited
                    .then(|| s.edits.cells.get(&(phys, c as u32)))
                    .flatten()
                {
                    Some(e) => (e.as_bytes(), theme::warning()),
                    None => (v, theme::text_soft()),
                };
                if v.is_empty() {
                    return;
                }
                let numeric = s.types.get(c).is_some_and(|t| t.numeric());
                let right = numeric && parse_number(v).is_some();
                cells.push(CellText {
                    x: s.col_x[c] - sx,
                    w: s.col_x[c + 1] - s.col_x[c],
                    y,
                    text: display(v),
                    right,
                    color,
                });
            };
            let mut n = 0;
            s.data.for_each_field(phys, cols.end.max(1), |c, v| {
                n = c + 1;
                push(c, &v);
            });
            if edited {
                // Edited cells past the end of a short row.
                for c in n.max(cols.start)..cols.end {
                    if let Some(e) = s.edits.cells.get(&(phys, c as u32)) {
                        let e: Arc<str> = e.clone();
                        push(c, e.as_bytes());
                    }
                }
            }
            // A row reaching the last known column may have more.
            if n == cols.end && cols.end == s.ncols {
                let mut all = 0;
                s.data.for_each_field(phys, 16_384, |c, _| all = c + 1);
                widest = widest.max(all);
            }
        }
        if widest > s.ncols {
            s.seen_cols.set(widest);
            let sheet = self.sheet.clone();
            window.on_next_frame(move |_, cx| sheet.update(cx, |_, cx| cx.notify()));
        }
        // The first frame at a new size: let the sheet clamp its scroll.
        if prev.bounds.size != geom.bounds.size || sy != s.scroll_y || sx != s.scroll_x {
            let sheet = self.sheet.clone();
            window.on_next_frame(move |_, cx| {
                sheet.update(cx, |s, _| {
                    let g = s.geom.get();
                    s.scroll_y = s.scroll_y.clamp(0., s.max_scroll_y(&g));
                    s.scroll_x = s.scroll_x.clamp(0., s.max_scroll_x(&g));
                })
            });
        }
        let sel_cols = s.sel_cols();
        let menu_col = s.menu.as_ref().map(|(m, _)| m.read(cx).col);
        let headers = cols
            .clone()
            .map(|c| HeaderCell {
                c,
                x: s.col_x[c] - sx,
                w: s.col_x[c + 1] - s.col_x[c],
                name: s.header.get(c).cloned().unwrap_or_default(),
                badge: s.types.get(c).copied().unwrap_or_default().badge(),
                selected: sel_cols.contains(&c),
                filtered: s.has_filter(c),
                sorted: s.query.sort.filter(|so| so.col == c).map(|so| so.desc),
                menu_open: menu_col == Some(c),
                hovered: matches!(s.hover, Hit::ColHeader { col, .. } if col == c),
                button_hover: matches!(s.hover, Hit::ColHeader { col, button: true } if col == c),
                edge_hover: matches!(s.hover, Hit::ColEdge(e) if e == c),
            })
            .collect();
        let grid_h = ((view_len as f64 * rh - sy) as f32).min(vh).max(0.);
        let sel = selection_rect(s, &geom, sx, sy);
        let rows_sel = s.sel_rows();
        Prepaint {
            hitbox,
            geom,
            first_row,
            rows,
            sx,
            sy,
            zoom: s.zoom,
            font_size: s.font_size(),
            focused: s.focus.contains_focused(window, cx),
            has_header: s.has_header,
            letter_h: s.letter_h(),
            content_w: s.content_w(),
            grid_h,
            col_lines: cols.clone().map(|c| s.col_x[c + 1] - sx).collect(),
            cells,
            row_labels,
            headers,
            whole_cols: s.is_whole_cols(),
            whole_rows: s.is_whole_rows(),
            multi: rows_sel.end - rows_sel.start > 1 || sel_cols.len() > 1,
            sel,
            cursor: cell_rect(s, &geom, s.cursor.0, s.cursor.1, sx, sy),
            filtered_view: filtered,
            v_thumb: s.v_thumb(&geom),
            h_thumb: s.h_thumb(&geom),
            thumb_hover: (
                matches!(s.hover, Hit::VThumb(_)),
                matches!(s.hover, Hit::HThumb(_)),
            ),
            resize_cursor: matches!(s.hover, Hit::ColEdge(_)) || s.resizing(),
            pointer_cursor: matches!(s.hover, Hit::ColHeader { button: true, .. }),
        }
    }

    fn paint(
        &mut self,
        _: Option<&GlobalElementId>,
        _: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _: &mut (),
        p: &mut Prepaint,
        window: &mut Window,
        cx: &mut App,
    ) {
        let g = p.geom;
        let vp = g.viewport();
        let rh = g.row_h;
        let z = p.zoom;
        let font: Font = window.text_style().font();
        let bold = Font {
            weight: FontWeight::SEMIBOLD,
            ..font.clone()
        };
        let fs = px(p.font_size);
        let small = px((p.font_size - 2.).max(9.));
        let line_h = px((p.font_size * 1.35).round());

        let shape =
            |window: &mut Window, text: SharedString, font: &Font, size: Pixels, color: Hsla| {
                let run = TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color,
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                };
                window.text_system().shape_line(text, size, &[run], None)
            };
        let mask = |b: Bounds<Pixels>| Some(gpui::ContentMask { bounds: b });

        window.paint_quad(fill(bounds, theme::bg()));

        // ── Cells ──
        window.with_content_mask(mask(vp), |window| {
            let right_edge = (p.content_w - p.sx).min(f32::from(vp.size.width)).max(0.);
            for i in 0..p.rows {
                let vr = p.first_row + i;
                if vr % 2 == 1 {
                    let y = (vr as f64 * rh as f64 - p.sy) as f32;
                    window.paint_quad(fill(
                        Bounds::new(
                            point(vp.left(), vp.top() + px(y)),
                            size(px(right_edge), px(rh)),
                        ),
                        theme::zebra(),
                    ));
                }
            }
            if let Some(r) = p.sel {
                window.paint_quad(fill(r, theme::selection()));
            }
            for &x in &p.col_lines {
                window.paint_quad(fill(
                    Bounds::new(
                        point(vp.left() + px(x - 1.), vp.top()),
                        size(px(1.), px(p.grid_h)),
                    ),
                    theme::grid_line(),
                ));
            }
            for i in 0..p.rows {
                let vr = p.first_row + i;
                let y = ((vr + 1) as f64 * rh as f64 - p.sy) as f32;
                window.paint_quad(fill(
                    Bounds::new(
                        point(vp.left(), vp.top() + px(y - 1.)),
                        size(px(right_edge), px(1.)),
                    ),
                    theme::grid_line(),
                ));
            }
            let pad = 7. * z;
            for c in p.cells.drain(..) {
                let line = shape(window, c.text, &font, fs, c.color);
                let cell = Bounds::new(
                    point(vp.left() + px(c.x), vp.top() + px(c.y)),
                    size(px(c.w - 1.), px(rh - 1.)),
                );
                let w = f32::from(line.width());
                let fits = w <= c.w - 2. * pad;
                let x = if c.right && fits {
                    c.x + c.w - pad - w
                } else {
                    c.x + pad
                };
                let origin = point(vp.left() + px(x), cell.top() + (px(rh) - line_h) / 2.);
                if fits {
                    line.paint(origin, line_h, TextAlign::Left, None, window, cx)
                        .ok();
                } else {
                    window.with_content_mask(mask(cell), |window| {
                        line.paint(origin, line_h, TextAlign::Left, None, window, cx)
                            .ok();
                    });
                }
            }
            if let Some(r) = p.sel
                && p.multi
            {
                window.paint_quad(quad(
                    r,
                    px(0.),
                    gpui::transparent_black(),
                    px(1.),
                    theme::accent().opacity(0.8),
                    BorderStyle::Solid,
                ));
            }
            if let Some(cb) = p.cursor {
                let b = Bounds::new(
                    cb.origin - point(px(1.), px(1.)),
                    cb.size + size(px(1.), px(1.)),
                );
                window.paint_quad(quad(
                    b,
                    px(1.),
                    gpui::transparent_black(),
                    px(2.),
                    if p.focused {
                        theme::accent()
                    } else {
                        theme::muted()
                    },
                    BorderStyle::Solid,
                ));
            }
        });

        // ── Column headers ──
        let head = Bounds::new(
            point(bounds.left() + px(g.gutter), bounds.top()),
            size(bounds.size.width - px(g.gutter), px(g.head)),
        );
        let letter_h = p.letter_h;
        window.paint_quad(fill(head, theme::header_bg()));
        window.with_content_mask(mask(head), |window| {
            for h in p.headers.drain(..) {
                let x0 = head.left() + px(h.x);
                let w = h.w;
                let col_b = Bounds::new(point(x0, head.top()), size(px(w), px(g.head)));
                let whole = h.selected && p.whole_cols;
                if whole {
                    window.paint_quad(fill(col_b, theme::accent_soft()));
                } else if h.selected {
                    window.paint_quad(fill(
                        Bounds::new(col_b.origin, size(col_b.size.width, px(letter_h))),
                        theme::accent_soft().opacity(0.7),
                    ));
                }
                if h.hovered && !whole {
                    window.paint_quad(fill(col_b, theme::hover()));
                }
                let letter = shape(
                    window,
                    col_name(h.c).into(),
                    &font,
                    small,
                    if h.selected {
                        theme::accent()
                    } else {
                        theme::faint()
                    },
                );
                let lw = letter.width();
                letter
                    .paint(
                        point(
                            x0 + (px(w) - lw) / 2.,
                            head.top() + (px(letter_h) - line_h) / 2.,
                        ),
                        line_h,
                        TextAlign::Left,
                        None,
                        window,
                        cx,
                    )
                    .ok();
                let active = h.filtered || h.sorted.is_some();
                if p.has_header {
                    let name_top = head.top() + px(letter_h);
                    let name_h = g.head - letter_h;
                    window.paint_quad(fill(
                        Bounds::new(point(x0, name_top), size(px(w), px(1.))),
                        theme::grid_line(),
                    ));
                    let pad = 8. * z;
                    let badge = shape(window, h.badge.into(), &bold, small, theme::faint());
                    badge
                        .paint(
                            point(x0 + px(pad), name_top + (px(name_h) - line_h) / 2.),
                            line_h,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        )
                        .ok();
                    let name_x = pad + f32::from(badge.width()) + 6. * z;
                    let button_w = 26. * z;
                    let name_w = (w - name_x - button_w - 2.).max(0.);
                    let line = shape(window, h.name, &bold, fs, theme::text());
                    let nb = Bounds::new(
                        point(x0 + px(name_x), name_top),
                        size(px(name_w), px(name_h)),
                    );
                    window.with_content_mask(mask(nb), |window| {
                        line.paint(
                            point(nb.left(), name_top + (px(name_h) - line_h) / 2.),
                            line_h,
                            TextAlign::Left,
                            None,
                            window,
                            cx,
                        )
                        .ok();
                    });
                    if f32::from(line.width()) > name_w && name_w > 10. {
                        let fade = Bounds::new(
                            point(nb.right() - px(14.), name_top + px(2.)),
                            size(px(14.), px(name_h - 4.)),
                        );
                        window.paint_quad(fill(
                            fade,
                            gpui::linear_gradient(
                                90.,
                                gpui::linear_color_stop(theme::header_bg().opacity(0.), 0.),
                                gpui::linear_color_stop(theme::header_bg(), 1.),
                            ),
                        ));
                    }
                    let bh = 20. * z;
                    let bb = Bounds::new(
                        point(
                            x0 + px(w - button_w - 2.),
                            name_top + px((name_h - bh) / 2.),
                        ),
                        size(px(button_w - 2.), px(bh)),
                    );
                    if active || h.menu_open || h.button_hover {
                        window.paint_quad(
                            fill(
                                bb,
                                if active || h.menu_open {
                                    theme::accent_soft()
                                } else {
                                    theme::hover()
                                },
                            )
                            .corner_radii(px(4.)),
                        );
                    }
                    let icon = match (h.filtered, h.sorted) {
                        (true, _) => "filter-fill",
                        (false, Some(true)) => "arrow-down",
                        (false, Some(false)) => "arrow-up",
                        _ => "chevron-down",
                    };
                    let isz = 13. * z;
                    let ib = Bounds::new(
                        point(
                            bb.left() + (bb.size.width - px(isz)) / 2.,
                            bb.top() + (bb.size.height - px(isz)) / 2.,
                        ),
                        size(px(isz), px(isz)),
                    );
                    let icolor = if active || h.menu_open {
                        theme::accent()
                    } else if h.hovered {
                        theme::text_soft()
                    } else {
                        theme::faint()
                    };
                    let _ = window.paint_svg(
                        ib,
                        crate::assets::icon(icon),
                        None,
                        Default::default(),
                        icolor,
                        cx,
                    );
                } else if active {
                    let isz = 11. * z;
                    let ib = Bounds::new(
                        point(
                            x0 + px(w - isz - 6.),
                            head.top() + (px(letter_h) - px(isz)) / 2.,
                        ),
                        size(px(isz), px(isz)),
                    );
                    let icon = if h.filtered { "filter-fill" } else { "sort" };
                    let _ = window.paint_svg(
                        ib,
                        crate::assets::icon(icon),
                        None,
                        Default::default(),
                        theme::accent(),
                        cx,
                    );
                }
                window.paint_quad(fill(
                    Bounds::new(point(x0 + px(w - 1.), head.top()), size(px(1.), px(g.head))),
                    if h.edge_hover {
                        theme::accent()
                    } else {
                        theme::grid_line_strong()
                    },
                ));
            }
        });
        window.paint_quad(fill(
            Bounds::new(
                point(bounds.left(), bounds.top() + px(g.head - 1.)),
                size(bounds.size.width, px(1.)),
            ),
            theme::grid_line_strong(),
        ));

        // ── Row numbers ──
        let gutter = Bounds::new(
            point(bounds.left(), vp.top()),
            size(px(g.gutter), vp.size.height),
        );
        window.paint_quad(fill(gutter, theme::header_bg()));
        window.with_content_mask(mask(gutter), |window| {
            for (y, label, selected) in p.row_labels.drain(..) {
                let rb = Bounds::new(
                    point(gutter.left(), vp.top() + px(y)),
                    size(px(g.gutter), px(rh)),
                );
                if selected {
                    window.paint_quad(fill(
                        rb,
                        if p.whole_rows {
                            theme::accent_soft()
                        } else {
                            theme::accent_soft().opacity(0.6)
                        },
                    ));
                }
                let color = if selected {
                    theme::accent()
                } else if p.filtered_view {
                    theme::accent().opacity(0.7)
                } else {
                    theme::faint()
                };
                let line = shape(window, label, &font, small, color);
                let lw = line.width();
                line.paint(
                    point(rb.right() - px(8.) - lw, rb.top() + (px(rh) - line_h) / 2.),
                    line_h,
                    TextAlign::Left,
                    None,
                    window,
                    cx,
                )
                .ok();
                window.paint_quad(fill(
                    Bounds::new(
                        point(rb.left(), rb.bottom() - px(1.)),
                        size(px(g.gutter), px(1.)),
                    ),
                    theme::grid_line(),
                ));
            }
        });
        window.paint_quad(fill(
            Bounds::new(
                point(bounds.left() + px(g.gutter - 1.), bounds.top()),
                size(px(1.), bounds.size.height),
            ),
            theme::grid_line_strong(),
        ));

        // ── Corner ──
        let corner = Bounds::new(bounds.origin, size(px(g.gutter - 1.), px(g.head - 1.)));
        window.paint_quad(fill(corner, theme::header_bg()));
        let tri = 8. * z;
        let _ = window.paint_svg(
            Bounds::new(
                point(
                    corner.right() - px(tri + 5.),
                    corner.bottom() - px(tri + 5.),
                ),
                size(px(tri), px(tri)),
            ),
            crate::assets::icon("corner"),
            None,
            Default::default(),
            theme::faint(),
            cx,
        );

        // ── Scrollbars ──
        if let Some((off, len)) = p.v_thumb {
            let b = Bounds::new(
                point(vp.right() - px(SCROLLBAR - 2.), vp.top() + px(off + 2.)),
                size(px(SCROLLBAR - 4.), px(len - 4.)),
            );
            let c = if p.thumb_hover.0 {
                theme::muted()
            } else {
                theme::scrollbar()
            };
            window.paint_quad(fill(b, c).corner_radii(px(3.)));
        }
        if let Some((off, len)) = p.h_thumb {
            let b = Bounds::new(
                point(vp.left() + px(off + 2.), vp.bottom() - px(SCROLLBAR - 2.)),
                size(px(len - 4.), px(SCROLLBAR - 4.)),
            );
            let c = if p.thumb_hover.1 {
                theme::muted()
            } else {
                theme::scrollbar()
            };
            window.paint_quad(fill(b, c).corner_radii(px(3.)));
        }

        // ── Cursor shape and input ──
        if p.resize_cursor {
            window.set_cursor_style(CursorStyle::ResizeLeftRight, &p.hitbox);
        } else if p.pointer_cursor {
            window.set_cursor_style(CursorStyle::PointingHand, &p.hitbox);
        }

        let weak = self.sheet.downgrade();
        let hitbox = p.hitbox.clone();
        window.on_mouse_event({
            let weak = weak.clone();
            let hitbox = hitbox.clone();
            move |e: &MouseDownEvent, phase, window, cx| {
                if phase.bubble() && hitbox.is_hovered(window) {
                    weak.update(cx, |s, cx| s.mouse_down(e, window, cx)).ok();
                }
            }
        });
        window.on_mouse_event({
            let weak = weak.clone();
            move |e: &MouseMoveEvent, phase, _, cx| {
                if phase.bubble() {
                    weak.update(cx, |s, cx| s.mouse_move(e, cx)).ok();
                }
            }
        });
        window.on_mouse_event({
            let weak = weak.clone();
            move |e: &MouseUpEvent, phase, _, cx| {
                if phase.bubble() {
                    weak.update(cx, |s, cx| s.mouse_up(e, cx)).ok();
                }
            }
        });
        window.on_mouse_event(move |e: &ScrollEvent, phase, window, cx| {
            if phase.bubble() && hitbox.should_handle_scroll(window) {
                weak.update(cx, |s, cx| s.scroll(e, cx)).ok();
            }
        });
    }
}

fn cell_rect(s: &Sheet, g: &Geom, vr: u64, col: usize, sx: f32, sy: f64) -> Option<Bounds<Pixels>> {
    if col >= s.ncols || vr >= s.view_len() {
        return None;
    }
    let vp = g.viewport();
    let y = (vr as f64 * g.row_h as f64 - sy) as f32;
    Some(Bounds::new(
        point(vp.left() + px(s.col_x[col] - sx), vp.top() + px(y)),
        size(px(s.col_x[col + 1] - s.col_x[col]), px(g.row_h)),
    ))
}

/// The selected block, clipped to a bit beyond the viewport so huge
/// selections don't produce huge quads.
fn selection_rect(s: &Sheet, g: &Geom, sx: f32, sy: f64) -> Option<Bounds<Pixels>> {
    if s.view_len() == 0 || s.ncols == 0 {
        return None;
    }
    let vp = g.viewport();
    let rows = s.sel_rows();
    let cols = s.sel_cols();
    let cols = cols.start.min(s.ncols - 1)..cols.end.min(s.ncols);
    let top = (rows.start as f64 * g.row_h as f64 - sy).max(-100.) as f32;
    let bottom =
        (rows.end as f64 * g.row_h as f64 - sy).min(f32::from(vp.size.height) as f64 + 100.) as f32;
    let left = (s.col_x[cols.start] - sx).max(-100.);
    let right = (s.col_x[cols.end] - sx).min(f32::from(vp.size.width) + 100.);
    if bottom <= top || right <= left {
        return None;
    }
    Some(Bounds::new(
        point(vp.left() + px(left), vp.top() + px(top)),
        size(px(right - left), px(bottom - top)),
    ))
}
