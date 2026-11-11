//! The drop-down on a column header: sort, a condition (contains, >, …) and
//! a checklist of the column's values with their counts.

use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use gpui::{
    App, Context, Entity, FocusHandle, Focusable, FontWeight, Render, ScrollStrategy, SharedString,
    Subscription, Task, UniformListScrollHandle, WeakEntity, Window, actions, div, prelude::*, px,
    uniform_list,
};
use rustc_hash::FxHashSet;

use crate::data::query::{self, Cond, Distinct, NumOp, TextOp};
use crate::data::value::{self, ColType};
use crate::sheet::Sheet;
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;
use crate::ui::{button, icon_el};

actions!(column_menu, [MenuApply, MenuClose]);

const LIST_ROW_H: f32 = 26.;

/// What the menu needs from the sheet when it opens.
pub struct MenuInit {
    pub name: SharedString,
    pub kind: ColType,
    pub filters: Vec<Cond>,
    pub data: crate::data::Data,
    pub first: u64,
    pub query: crate::data::query::Query,
    pub edits: Arc<crate::data::query::Edits>,
    pub ncols: usize,
}

pub struct ColumnMenu {
    sheet: WeakEntity<Sheet>,
    pub col: usize,
    name: SharedString,
    kind: ColType,
    focus: FocusHandle,
    text_op: TextOp,
    num_op: NumOp,
    input_a: Entity<TextInput>,
    input_b: Entity<TextInput>,
    list_search: Entity<TextInput>,
    values: Option<Distinct>,
    checked: Vec<bool>,
    shown: Vec<usize>,
    /// The filter's value set from before, applied once values load.
    initial: Option<(query::ValueSet, bool)>,
    scroll: UniformListScrollHandle,
    error: Option<String>,
    _task: Option<Task<()>>,
    _subs: Vec<Subscription>,
    cancel: Arc<AtomicBool>,
}

impl Focusable for ColumnMenu {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for ColumnMenu {
    fn drop(&mut self) {
        self.cancel.store(true, Relaxed);
    }
}

impl ColumnMenu {
    pub fn new(
        sheet: WeakEntity<Sheet>,
        col: usize,
        init: MenuInit,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let input_a = cx.new(|cx| TextInput::new("Value", cx));
        let input_b = cx.new(|cx| TextInput::new("and", cx));
        let list_search = cx.new(|cx| TextInput::new("Search values", cx));
        let subs = vec![
            cx.subscribe(&list_search, |this, _, _: &TextInputEvent, cx| {
                this.refilter_list(cx);
            }),
            cx.subscribe(&input_a, |_, _, _: &TextInputEvent, cx| cx.notify()),
        ];
        let mut menu = ColumnMenu {
            sheet: sheet.clone(),
            col,
            name: SharedString::default(),
            kind: ColType::Text,
            focus: cx.focus_handle(),
            text_op: TextOp::Contains,
            num_op: NumOp::Gt,
            input_a,
            input_b,
            list_search,
            values: None,
            checked: Vec::new(),
            shown: Vec::new(),
            initial: None,
            scroll: UniformListScrollHandle::new(),
            error: None,
            _task: None,
            _subs: subs,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        let s = &init;
        menu.name = s.name.clone();
        menu.kind = s.kind;
        // Start from the column's current filters.
        let mut a_text = None;
        let mut b_text = None;
        for f in s.filters.iter() {
            match f {
                Cond::Text { op, needle } => {
                    menu.text_op = *op;
                    a_text = Some(needle.clone());
                }
                Cond::Num { op, a, b } => {
                    menu.num_op = *op;
                    a_text = Some(value::format_number(*a));
                    if *op == NumOp::Between {
                        b_text = Some(value::format_number(*b));
                    }
                }
                Cond::Values { set, exclude } => menu.initial = Some((set.clone(), *exclude)),
            }
        }
        let MenuInit {
            data,
            first,
            query: q,
            edits,
            ncols,
            ..
        } = init;
        let numeric = menu.kind.numeric();
        if let Some(t) = a_text {
            menu.input_a.update(cx, |i, cx| i.set_text(t, None, cx));
        }
        if let Some(t) = b_text {
            menu.input_b.update(cx, |i, cx| i.set_text(t, None, cx));
        }
        let cancel = menu.cancel.clone();
        let job = cx.background_spawn(async move {
            let rows = if q.filters_rows() {
                match query::run(&data, first, &q, &edits, ncols, &cancel) {
                    Ok(Some(r)) => Some(r.rows),
                    Ok(None) => return None,
                    Err(e) => return Some(Err(e)),
                }
            } else {
                None
            };
            let d = query::distinct(&data, first, rows.as_deref(), col, numeric, &edits, &cancel);
            drop(rows);
            crate::data::release_free_memory();
            d.map(Ok)
        });
        menu._task = Some(cx.spawn(async move |this, cx| {
            let r = job.await;
            this.update(cx, |m, cx| {
                match r {
                    Some(Ok(d)) => m.set_values(d),
                    Some(Err(e)) => m.error = Some(e),
                    None => {}
                }
                cx.notify();
            })
            .ok();
        }));
        menu.input_a.read(cx).focus_handle(cx).focus(window, cx);
        menu
    }

    fn set_values(&mut self, d: Distinct) {
        self.checked = match &self.initial {
            Some((set, exclude)) => d
                .values
                .iter()
                .map(|(v, _)| set.contains(v) != *exclude)
                .collect(),
            None => vec![true; d.values.len()],
        };
        self.values = Some(d);
        self.shown = (0..self.checked.len()).collect();
    }

    fn refilter_list(&mut self, cx: &mut Context<Self>) {
        let needle = self.list_search.read(cx).text().to_lowercase();
        if let Some(d) = &self.values {
            self.shown = (0..d.values.len())
                .filter(|&i| {
                    needle.is_empty()
                        || String::from_utf8_lossy(&d.values[i].0)
                            .to_lowercase()
                            .contains(&needle)
                })
                .collect();
        }
        self.scroll.scroll_to_item(0, ScrollStrategy::Top);
        cx.notify();
    }

    fn all_shown_checked(&self) -> bool {
        self.shown.iter().all(|&i| self.checked[i])
    }

    fn toggle_all(&mut self, cx: &mut Context<Self>) {
        let on = !self.all_shown_checked();
        for &i in &self.shown {
            self.checked[i] = on;
        }
        cx.notify();
    }

    fn close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if let Some(s) = self.sheet.upgrade() {
            s.update(cx, |s, cx| s.close_menu(window, cx));
        }
    }

    fn sort(&mut self, desc: bool, window: &mut Window, cx: &mut Context<Self>) {
        let col = self.col;
        if let Some(s) = self.sheet.upgrade() {
            s.update(cx, |s, cx| {
                let same = s
                    .query
                    .sort
                    .is_some_and(|so| so.col == col && so.desc == desc);
                if same {
                    s.set_sort(None, cx);
                } else {
                    s.sort_by(col, desc, cx);
                }
            });
        }
        self.close(window, cx);
    }

    /// The conditions the menu describes, or an error to show.
    fn conditions(&self, cx: &App) -> Result<Vec<Cond>, String> {
        let mut conds = Vec::new();
        let a = self.input_a.read(cx).text().trim().to_string();
        let b = self.input_b.read(cx).text().trim().to_string();
        if !a.is_empty() {
            if self.kind.numeric() {
                let x = value::parse_number(a.as_bytes())
                    .ok_or_else(|| format!("“{a}” isn't a number"))?;
                let y = if self.num_op == NumOp::Between {
                    value::parse_number(b.as_bytes())
                        .ok_or_else(|| "Enter both ends of the range".to_string())?
                } else {
                    0.
                };
                conds.push(Cond::Num {
                    op: self.num_op,
                    a: x,
                    b: y,
                });
            } else {
                if self.text_op == TextOp::Regex {
                    regex::Regex::new(&a).map_err(|e| {
                        let e = e.to_string();
                        e.lines()
                            .last()
                            .unwrap_or("invalid regex")
                            .trim()
                            .to_string()
                    })?;
                }
                conds.push(Cond::Text {
                    op: self.text_op,
                    needle: a,
                });
            }
        }
        if let Some(d) = &self.values {
            let total = self.checked.len();
            let on = self.checked.iter().filter(|&&c| c).count();
            if on == 0 {
                return Err("Pick at least one value".into());
            }
            if on < total {
                let exclude = d.truncated || total - on < on;
                let set: FxHashSet<Box<[u8]>> = d
                    .values
                    .iter()
                    .zip(&self.checked)
                    .filter(|(_, c)| **c != exclude)
                    .map(|((v, _), _)| v.clone())
                    .collect();
                conds.push(Cond::Values {
                    set: Arc::new(set),
                    exclude,
                });
            }
        }
        Ok(conds)
    }

    fn apply(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        match self.conditions(cx) {
            Ok(conds) => {
                let col = self.col;
                if let Some(s) = self.sheet.upgrade() {
                    s.update(cx, |s, cx| s.set_filter(col, conds, cx));
                }
                self.close(window, cx);
            }
            Err(e) => {
                self.error = Some(e);
                cx.notify();
            }
        }
    }

    fn clear(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let col = self.col;
        if let Some(s) = self.sheet.upgrade() {
            s.update(cx, |s, cx| s.clear_filter(col, cx));
        }
        self.close(window, cx);
    }

    fn render_ops(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let chips: Vec<_> = if self.kind.numeric() {
            NumOp::ALL
                .iter()
                .enumerate()
                .map(|(i, &op)| {
                    let on = self.num_op == op;
                    chip(("num-op", i), op.label(), on).on_click(cx.listener(move |m, _, _, cx| {
                        m.num_op = op;
                        m.error = None;
                        cx.notify();
                    }))
                })
                .collect()
        } else {
            TextOp::ALL
                .iter()
                .enumerate()
                .map(|(i, &op)| {
                    let on = self.text_op == op;
                    chip(("text-op", i), op.label(), on).on_click(cx.listener(
                        move |m, _, _, cx| {
                            m.text_op = op;
                            m.error = None;
                            cx.notify();
                        },
                    ))
                })
                .collect()
        };
        div().flex().flex_wrap().gap_1().children(chips)
    }

    fn render_values(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(d) = &self.values else {
            return div()
                .h(px(120.))
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .text_xs()
                .text_color(theme::muted())
                .child(icon_el("spinner", theme::accent(), 14.))
                .child("Counting values…")
                .into_any_element();
        };
        let n = self.shown.len();
        let all = self.all_shown_checked();
        let some = self.shown.iter().any(|&i| self.checked[i]);
        let total: u64 = d.values.iter().map(|(_, c)| c).sum();
        let list_h = (n.clamp(1, 8) as f32) * LIST_ROW_H;
        div()
            .flex()
            .flex_col()
            .gap_1()
            .child(input_box(self.list_search.clone(), "search"))
            .child(
                div()
                    .id("select-all")
                    .h(px(LIST_ROW_H))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover()))
                    .on_click(cx.listener(|m, _, _, cx| m.toggle_all(cx)))
                    .child(checkbox(all, some && !all))
                    .child(
                        div()
                            .flex_1()
                            .text_sm()
                            .font_weight(FontWeight::MEDIUM)
                            .text_color(theme::text())
                            .child(if self.list_search.read(cx).text().is_empty() {
                                "Select all".to_string()
                            } else {
                                format!("Select all {} matches", value::count(n as u64))
                            }),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::faint())
                            .child(value::count(total)),
                    ),
            )
            .child(
                div()
                    .h(px(list_h))
                    .border_t_1()
                    .border_color(theme::border())
                    .child(
                        uniform_list(
                            "values",
                            n,
                            cx.processor(|m, range: Range<usize>, _, cx| {
                                range
                                    .filter_map(|r| {
                                        let i = *m.shown.get(r)?;
                                        Some(m.render_value(i, cx))
                                    })
                                    .collect::<Vec<_>>()
                            }),
                        )
                        .track_scroll(&self.scroll)
                        .size_full(),
                    ),
            )
            .when(d.truncated, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(theme::faint())
                        .child("Showing the most common values"),
                )
            })
            .into_any_element()
    }

    fn render_value(&self, i: usize, cx: &mut Context<Self>) -> gpui::AnyElement {
        let d = self.values.as_ref().unwrap();
        let (v, count) = &d.values[i];
        let on = self.checked[i];
        let label: SharedString = if v.is_empty() {
            "(Blanks)".into()
        } else {
            let s = String::from_utf8_lossy(v);
            let s: String = s.chars().take(120).collect();
            s.replace(['\n', '\r', '\t'], " ").into()
        };
        div()
            .id(("value", i))
            .h(px(LIST_ROW_H))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .rounded_md()
            .cursor_pointer()
            .hover(|s| s.bg(theme::hover()))
            .on_click(cx.listener(move |m, e: &gpui::ClickEvent, _, cx| {
                if e.modifiers().control || e.modifiers().alt {
                    // Only this value.
                    m.checked.iter_mut().for_each(|c| *c = false);
                    m.checked[i] = true;
                } else {
                    m.checked[i] = !m.checked[i];
                }
                m.error = None;
                cx.notify();
            }))
            .child(checkbox(on, false))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_sm()
                    .text_color(if v.is_empty() {
                        theme::muted()
                    } else {
                        theme::text_soft()
                    })
                    .child(label),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(theme::faint())
                    .child(value::count(*count)),
            )
            .into_any_element()
    }
}

fn chip(
    id: impl Into<gpui::ElementId>,
    label: &'static str,
    on: bool,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .px_2()
        .h(px(22.))
        .flex()
        .items_center()
        .rounded_md()
        .text_xs()
        .cursor_pointer()
        .border_1()
        .when(on, |d| {
            d.bg(theme::accent_soft())
                .border_color(theme::accent().opacity(0.5))
                .text_color(theme::accent())
        })
        .when(!on, |d| {
            d.border_color(theme::border())
                .text_color(theme::muted())
                .hover(|s| s.bg(theme::hover()).text_color(theme::text_soft()))
        })
        .child(label)
}

pub fn checkbox(on: bool, partial: bool) -> impl IntoElement {
    div()
        .size(px(15.))
        .flex_none()
        .rounded(px(4.))
        .flex()
        .items_center()
        .justify_center()
        .border_1()
        .when(on || partial, |d| {
            d.bg(theme::accent()).border_color(theme::accent())
        })
        .when(!on && !partial, |d| d.border_color(theme::border_strong()))
        .when(on, |d| d.child(icon_el("check", theme::on_accent(), 11.)))
        .when(partial, |d| {
            d.child(icon_el("minus", theme::on_accent(), 11.))
        })
}

pub fn input_box(input: Entity<TextInput>, icon: &'static str) -> impl IntoElement {
    div()
        .h(px(30.))
        .px_2()
        .flex()
        .items_center()
        .gap_2()
        .rounded_md()
        .bg(theme::bg())
        .border_1()
        .border_color(theme::border_strong())
        .text_sm()
        .text_color(theme::text())
        .child(icon_el(icon, theme::faint(), 13.))
        .child(input)
}

fn section(label: &'static str) -> impl IntoElement {
    div()
        .pt_1()
        .text_xs()
        .font_weight(FontWeight::SEMIBOLD)
        .text_color(theme::faint())
        .child(label)
}

impl Render for ColumnMenu {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let numeric = self.kind.numeric();
        let (sort, filtered) = self
            .sheet
            .upgrade()
            .map(|s| {
                let s = s.read(cx);
                (s.query.sort, s.has_filter(self.col))
            })
            .unwrap_or((None, false));
        let sorted = |desc: bool| sort.is_some_and(|s| s.col == self.col && s.desc == desc);
        let (asc_label, desc_label) = if numeric {
            ("Smallest → largest", "Largest → smallest")
        } else {
            ("A → Z", "Z → A")
        };
        let sort_button = |id: &'static str, icon: &'static str, label: &'static str, on: bool| {
            div()
                .id(id)
                .flex_1()
                .h(px(30.))
                .px_2()
                .flex()
                .items_center()
                .justify_center()
                .gap_2()
                .rounded_md()
                .text_xs()
                .cursor_pointer()
                .border_1()
                .when(on, |d| {
                    d.bg(theme::accent_soft())
                        .border_color(theme::accent().opacity(0.5))
                        .text_color(theme::accent())
                })
                .when(!on, |d| {
                    d.border_color(theme::border())
                        .text_color(theme::text_soft())
                        .hover(|s| s.bg(theme::hover()))
                })
                .child(icon_el(
                    icon,
                    if on { theme::accent() } else { theme::muted() },
                    14.,
                ))
                .child(label)
        };
        let between = numeric && self.num_op == NumOp::Between;
        div()
            .id("column-menu")
            .key_context("ColumnMenu")
            .track_focus(&self.focus)
            .on_action(cx.listener(|m, _: &MenuApply, w, cx| m.apply(w, cx)))
            .on_action(cx.listener(|m, _: &MenuClose, w, cx| m.close(w, cx)))
            .occlude()
            .on_mouse_down_out(cx.listener(|m, _, w, cx| m.close(w, cx)))
            .w(px(310.))
            .p_3()
            .flex()
            .flex_col()
            .gap_2()
            .rounded_lg()
            .bg(theme::raised())
            .border_1()
            .border_color(theme::border_strong())
            .shadow_lg()
            .text_color(theme::text())
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .px_1()
                            .rounded(px(4.))
                            .bg(theme::bg())
                            .text_xs()
                            .font_weight(FontWeight::BOLD)
                            .text_color(theme::muted())
                            .child(self.kind.badge()),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child(self.name.clone()),
                    )
                    .child(
                        div()
                            .id("close")
                            .size(px(22.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .cursor_pointer()
                            .hover(|s| s.bg(theme::hover()))
                            .on_click(cx.listener(|m, _, w, cx| m.close(w, cx)))
                            .child(icon_el("x", theme::muted(), 13.)),
                    ),
            )
            .child(
                div()
                    .flex()
                    .gap_2()
                    .child(
                        sort_button("sort-asc", "sort-asc", asc_label, sorted(false))
                            .on_click(cx.listener(|m, _, w, cx| m.sort(false, w, cx))),
                    )
                    .child(
                        sort_button("sort-desc", "sort-desc", desc_label, sorted(true))
                            .on_click(cx.listener(|m, _, w, cx| m.sort(true, w, cx))),
                    ),
            )
            .child(section("CONDITION"))
            .child(self.render_ops(cx))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(
                        div()
                            .flex_1()
                            .child(input_box(self.input_a.clone(), "filter")),
                    )
                    .when(between, |d| {
                        d.child(
                            div()
                                .flex_1()
                                .child(input_box(self.input_b.clone(), "minus")),
                        )
                    }),
            )
            .child(section("VALUES"))
            .child(self.render_values(cx))
            .when_some(self.error.clone(), |d, e| {
                d.child(
                    div()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_xs()
                        .text_color(theme::danger())
                        .child(icon_el("alert", theme::danger(), 13.))
                        .child(e),
                )
            })
            .child(
                div()
                    .pt_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .when(filtered, |d| {
                        d.child(
                            button("clear", "Clear", Some("filter-x"), false)
                                .on_click(cx.listener(|m, _, w, cx| m.clear(w, cx))),
                        )
                    })
                    .child(div().flex_1())
                    .child(
                        button("cancel", "Cancel", None, false)
                            .on_click(cx.listener(|m, _, w, cx| m.close(w, cx))),
                    )
                    .child(
                        button("apply", "Apply", None, true)
                            .on_click(cx.listener(|m, _, w, cx| m.apply(w, cx))),
                    ),
            )
    }
}
