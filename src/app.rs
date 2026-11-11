//! The window: file tabs, the search box, name box and formula bar, filter
//! chips, the open sheet, workbook sheet tabs and the status bar.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};
use std::time::Duration;

use gpui::{
    App, Context, Entity, ExternalPaths, FocusHandle, Focusable, FontWeight, PathPromptOptions,
    Render, SharedString, Subscription, Task, Window, actions, div, prelude::*, px,
};

use crate::data::query;
use crate::data::value::{self, col_name, parse_col_name};
use crate::sheet::{Sheet, SheetEvent, View};
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;
use crate::ui::{button, icon_el, kbd, switch, tooltip, vsep};

actions!(
    workspace,
    [
        Open,
        CloseTab,
        NextTab,
        PrevTab,
        FocusSearch,
        GoTo,
        Export,
        ToggleHeader,
        ZoomIn,
        ZoomOut,
        ZoomReset,
        SearchConfirm,
        SearchCancel,
        NameBoxConfirm,
        NameBoxCancel,
        Quit,
    ]
);

struct ExportJob {
    path: PathBuf,
    progress: Arc<AtomicU64>,
    total: u64,
    cancel: Arc<AtomicBool>,
    _task: Task<()>,
}

pub struct Workspace {
    focus: FocusHandle,
    sheets: Vec<Entity<Sheet>>,
    active: usize,
    search: Entity<TextInput>,
    name_box: Entity<TextInput>,
    toast: Option<SharedString>,
    toast_gen: u64,
    _toast_task: Option<Task<()>>,
    export: Option<ExportJob>,
    close_armed: Option<usize>,
    recent: Vec<PathBuf>,
    sheet_subs: Vec<Subscription>,
    _subs: Vec<Subscription>,
    _poll: Option<Task<()>>,
    window_title: String,
}

impl Focusable for Workspace {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

fn config_dir() -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config")))
        .unwrap_or_else(|| PathBuf::from("."))
        .join("rowscope")
}

fn load_recent() -> Vec<PathBuf> {
    std::fs::read_to_string(config_dir().join("recent"))
        .unwrap_or_default()
        .lines()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .take(8)
        .collect()
}

fn human_bytes(n: u64) -> String {
    let units = ["B", "KB", "MB", "GB", "TB"];
    let mut v = n as f64;
    let mut u = 0;
    while v >= 1024. && u < units.len() - 1 {
        v /= 1024.;
        u += 1;
    }
    if u == 0 {
        format!("{n} B")
    } else {
        format!("{v:.1} {}", units[u])
    }
}

fn ms(d: Duration) -> String {
    let ms = d.as_secs_f64() * 1e3;
    if ms < 10. {
        format!("{ms:.1} ms")
    } else if ms < 10_000. {
        format!("{ms:.0} ms")
    } else {
        format!("{:.1} s", ms / 1e3)
    }
}

impl Workspace {
    pub fn new(paths: Vec<PathBuf>, window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search = cx.new(|cx| TextInput::new("Search all cells", cx));
        let name_box = cx.new(|cx| TextInput::new("A1", cx));
        let subs = vec![
            cx.subscribe(&search, |this, input, _: &TextInputEvent, cx| {
                let text = input.read(cx).text().to_string();
                if let Some(s) = this.sheet() {
                    s.update(cx, |s, cx| s.set_search(&text, cx));
                }
                cx.notify();
            }),
            cx.observe_window_activation(window, |_, _, cx| cx.notify()),
        ];
        let mut ws = Workspace {
            focus: cx.focus_handle(),
            sheets: Vec::new(),
            active: 0,
            search,
            name_box,
            toast: None,
            toast_gen: 0,
            _toast_task: None,
            export: None,
            close_armed: None,
            recent: load_recent(),
            sheet_subs: Vec::new(),
            _subs: subs,
            _poll: None,
            window_title: String::new(),
        };
        for p in paths {
            ws.open_path(p, window, cx);
        }
        if ws.sheets.is_empty() {
            ws.focus.focus(window, cx);
        }
        ws
    }

    pub fn sheet(&self) -> Option<&Entity<Sheet>> {
        self.sheets.get(self.active)
    }

    pub fn open_path(&mut self, path: PathBuf, window: &mut Window, cx: &mut Context<Self>) {
        let path = std::fs::canonicalize(&path).unwrap_or(path);
        if let Some(i) = self.sheets.iter().position(|s| s.read(cx).path == path) {
            self.activate(i, window, cx);
            return;
        }
        let sheet = cx.new(|cx| Sheet::new(path.clone(), None, cx));
        let sub = cx.subscribe_in(
            &sheet,
            window,
            |this, sheet, ev: &SheetEvent, window, cx| match ev {
                SheetEvent::Changed => {
                    if this.sheet() == Some(sheet) {
                        this.sync_name_box(window, cx);
                    }
                    cx.notify();
                }
                SheetEvent::Toast(t) => this.show_toast(t.clone(), cx),
            },
        );
        self.sheets.push(sheet);
        self.sheet_subs.push(sub);
        self.remember(&path);
        let i = self.sheets.len() - 1;
        self.activate(i, window, cx);
    }

    fn remember(&mut self, path: &Path) {
        self.recent.retain(|p| p != path);
        self.recent.insert(0, path.to_path_buf());
        self.recent.truncate(8);
        let dir = config_dir();
        let _ = std::fs::create_dir_all(&dir);
        let text: String = self
            .recent
            .iter()
            .map(|p| format!("{}\n", p.display()))
            .collect();
        let _ = std::fs::write(dir.join("recent"), text);
    }

    fn activate(&mut self, i: usize, window: &mut Window, cx: &mut Context<Self>) {
        if i >= self.sheets.len() {
            return;
        }
        self.active = i;
        self.close_armed = None;
        let sheet = self.sheets[i].clone();
        let search = sheet.read(cx).query.search.clone();
        self.search.update(cx, |t, cx| t.set_text(search, None, cx));
        {
            let f = sheet.read(cx).focus.clone();
            f.focus(window, cx);
        }
        self.sync_name_box(window, cx);
        cx.notify();
    }

    fn close_tab(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = self.sheet() else {
            return;
        };
        if sheet.read(cx).is_dirty() && self.close_armed != Some(self.active) {
            self.close_armed = Some(self.active);
            self.show_toast(
                "This file has edits that aren't exported. Close again to discard them.".into(),
                cx,
            );
            return;
        }
        self.close_armed = None;
        self.sheets.remove(self.active);
        drop(self.sheet_subs.remove(self.active));
        if self.sheets.is_empty() {
            self.active = 0;
            self.search.update(cx, |t, cx| t.set_text("", None, cx));
            self.focus.focus(window, cx);
            cx.notify();
        } else {
            let i = self.active.min(self.sheets.len() - 1);
            self.activate(i, window, cx);
        }
    }

    fn show_toast(&mut self, text: String, cx: &mut Context<Self>) {
        self.toast = Some(text.into());
        self.toast_gen += 1;
        let generation = self.toast_gen;
        let timer = cx.background_executor().timer(Duration::from_millis(3500));
        self._toast_task = Some(cx.spawn(async move |this, cx| {
            timer.await;
            this.update(cx, |this, cx| {
                if this.toast_gen == generation {
                    this.toast = None;
                    cx.notify();
                }
            })
            .ok();
        }));
        cx.notify();
    }

    fn sync_name_box(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.name_box.read(cx).focus_handle(cx).is_focused(window) {
            return;
        }
        let text = self
            .sheet()
            .map(|s| cell_ref(s.read(cx)))
            .unwrap_or_default();
        self.name_box.update(cx, |t, cx| {
            if t.text() != text {
                t.set_text(text, None, cx);
            }
        });
    }

    fn prompt_open(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let rx = cx.prompt_for_paths(PathPromptOptions {
            files: true,
            directories: false,
            multiple: true,
            prompt: Some("Open".into()),
        });
        cx.spawn_in(window, async move |this, cx| {
            let result = rx.await;
            this.update_in(cx, |this, window, cx| match result {
                Ok(Ok(Some(paths))) => {
                    for p in paths {
                        this.open_path(p, window, cx);
                    }
                }
                Ok(Err(e)) => this.show_toast(format!("Couldn't show the file picker: {e}"), cx),
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    fn prompt_export(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(sheet) = self.sheet().cloned() else {
            return;
        };
        if self.export.is_some() {
            self.show_toast("An export is already running".into(), cx);
            return;
        }
        sheet.update(cx, |s, cx| s.commit_edit(cx));
        let s = sheet.read(cx);
        if s.loading.is_some() {
            self.show_toast("Wait for the file to finish loading".into(), cx);
            return;
        }
        let dir = s
            .path
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."));
        let stem = s
            .path
            .file_stem()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "export".into());
        let suffix = match (&s.sheet_name, s.is_filtered() || s.query.sort.is_some()) {
            (Some(n), _) if s.sheets.len() > 1 => format!("-{n}"),
            (_, true) => "-filtered".into(),
            _ if s.is_dirty() => "-edited".into(),
            _ => String::new(),
        };
        let name = format!("{stem}{suffix}.csv");
        let rx = cx.prompt_for_new_path(&dir, Some(&name));
        cx.spawn_in(window, async move |this, cx| {
            let result = rx.await;
            this.update_in(cx, |this, _, cx| match result {
                Ok(Ok(Some(path))) => this.start_export(sheet, path, cx),
                Ok(Err(e)) => this.show_toast(format!("Couldn't show the file picker: {e}"), cx),
                _ => {}
            })
            .ok();
        })
        .detach();
    }

    pub fn start_export(&mut self, sheet: Entity<Sheet>, path: PathBuf, cx: &mut Context<Self>) {
        let s = sheet.read(cx);
        if path == s.path {
            self.show_toast(
                "Export to a new file; the source stays untouched".into(),
                cx,
            );
            return;
        }
        let data = s.data.clone();
        let first = s.first();
        let header: Option<Vec<String>> = s
            .has_header
            .then(|| s.header.iter().map(|h| h.to_string()).collect());
        let count = s.view_len();
        let view = s.view.clone();
        let ncols = s.ncols;
        let edits = s.edits.clone();
        let progress = Arc::new(AtomicU64::new(0));
        let cancel = Arc::new(AtomicBool::new(false));
        let job = cx.background_spawn({
            let (progress, cancel, path) = (progress.clone(), cancel.clone(), path.clone());
            async move {
                let vf = move |r: u64| match &view {
                    View::All => r,
                    View::Rows(v) => v[r as usize] as u64,
                };
                let t0 = std::time::Instant::now();
                query::export_csv(
                    &path,
                    &data,
                    first,
                    header.as_deref(),
                    count,
                    &vf,
                    ncols,
                    &edits,
                    &progress,
                    &cancel,
                )
                .map(|n| (n, t0.elapsed()))
            }
        });
        let task = cx.spawn(async move |this, cx| {
            let r = job.await;
            this.update(cx, |this, cx| {
                let name = this
                    .export
                    .as_ref()
                    .and_then(|e| e.path.file_name())
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                this.export = None;
                this._poll = None;
                match r {
                    Ok((n, t)) => this.show_toast(
                        format!("Exported {} rows to {name} in {}", value::count(n), ms(t)),
                        cx,
                    ),
                    Err(e) => this.show_toast(format!("Export failed: {e:#}"), cx),
                }
            })
            .ok();
        });
        // Redraw the progress while it runs.
        let timer_exec = cx.background_executor().clone();
        self._poll = Some(cx.spawn(async move |this, cx| {
            loop {
                timer_exec.timer(Duration::from_millis(150)).await;
                if this.update(cx, |_, cx| cx.notify()).is_err() {
                    break;
                }
            }
        }));
        self.export = Some(ExportJob {
            path,
            progress,
            total: count,
            cancel,
            _task: task,
        });
        cx.notify();
    }

    fn go_to_ref(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.name_box.read(cx).text().trim().to_string();
        let Some(sheet) = self.sheet().cloned() else {
            return;
        };
        let target = parse_ref(&text);
        let target = target.and_then(|(a, b)| {
            let s = sheet.read(cx);
            let map = |r: Ref| -> Option<Ref> {
                Some((
                    match r.0 {
                        Some(dr) => Some(s.view_row_of(dr)?),
                        None => None,
                    },
                    r.1,
                ))
            };
            Some((
                map(a)?,
                match b {
                    Some(b) => Some(map(b)?),
                    None => None,
                },
            ))
        });
        sheet.update(cx, |s, cx| match target {
            Some((a, b)) => {
                let a = (a.0.unwrap_or(s.cursor.0), a.1.unwrap_or(s.cursor.1));
                match b {
                    Some(b) => {
                        let b = (b.0.unwrap_or(a.0), b.1.unwrap_or(a.1));
                        s.go_to(a.0, a.1, cx);
                        s.select_range(a, b, cx);
                    }
                    None => s.go_to(a.0, a.1, cx),
                }
            }
            None => cx.emit(SheetEvent::Toast(format!(
                "Can't go to “{text}”: not a cell reference (try B12, 5000 or A1:C10), or that row is filtered out."
            ))),
        });
        {
            let f = sheet.read(cx).focus.clone();
            f.focus(window, cx);
        }
        self.sync_name_box(window, cx);
    }

    fn zoom(&mut self, factor: Option<f32>, cx: &mut Context<Self>) {
        if let Some(s) = self.sheet() {
            s.update(cx, |s, cx| {
                s.zoom = match factor {
                    Some(f) => (s.zoom * f).clamp(0.6, 2.5),
                    None => 1.0,
                };
                s.update_col_x();
                cx.notify();
            });
        }
    }

    // ── Rendering ───────────────────────────────────────────────────────

    fn render_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let tabs: Vec<_> = self
            .sheets
            .iter()
            .enumerate()
            .map(|(i, sheet)| {
                let s = sheet.read(cx);
                let active = i == self.active;
                let loading = s.loading.is_some();
                div()
                    .id(("tab", i))
                    .h(px(30.))
                    .max_w(px(240.))
                    .pl_3()
                    .pr_1()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .when(active, |d| d.bg(theme::raised()).text_color(theme::text()))
                    .when(!active, |d| {
                        d.text_color(theme::muted())
                            .hover(|s| s.bg(theme::hover()).text_color(theme::text_soft()))
                    })
                    .on_click(cx.listener(move |this, _, w, cx| this.activate(i, w, cx)))
                    .on_mouse_down(
                        gpui::MouseButton::Middle,
                        cx.listener(move |this, _, w, cx| {
                            this.active = i;
                            this.close_tab(w, cx);
                        }),
                    )
                    .tooltip(tooltip(s.path.display().to_string()))
                    .child(icon_el(
                        if loading { "spinner" } else { "file-sheet" },
                        if active {
                            theme::success()
                        } else {
                            theme::faint()
                        },
                        14.,
                    ))
                    .child(div().truncate().child(s.title()))
                    .when(s.is_dirty(), |d| {
                        d.child(div().size(px(6.)).rounded_full().bg(theme::warning()))
                    })
                    .child(
                        div()
                            .id(("close", i))
                            .size(px(20.))
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            .rounded_md()
                            .hover(|s| s.bg(theme::border()))
                            .on_click(cx.listener(move |this, _, w, cx| {
                                cx.stop_propagation();
                                this.active = i;
                                this.close_tab(w, cx);
                            }))
                            .child(icon_el("x", theme::faint(), 12.)),
                    )
            })
            .collect();
        div()
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .gap_1()
            .overflow_hidden()
            .children(tabs)
    }

    fn render_top_bar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let has_sheet = self.sheet().is_some();
        let searching = !self.search.read(cx).text().is_empty();
        let (matches, running) = self
            .sheet()
            .map(|s| {
                let s = s.read(cx);
                (s.view_len(), s.query_running)
            })
            .unwrap_or((0, false));
        div()
            .flex_none()
            .h(px(46.))
            .px_3()
            .flex()
            .items_center()
            .gap_2()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex()
                    .flex_none()
                    .items_center()
                    .gap_2()
                    .pr_2()
                    .child(
                        div()
                            .size(px(24.))
                            .rounded_md()
                            .bg(theme::success().opacity(0.16))
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon_el("table", theme::success(), 15.)),
                    )
                    .when(!has_sheet, |d| {
                        d.child(
                            div()
                                .text_sm()
                                .font_weight(FontWeight::SEMIBOLD)
                                .text_color(theme::text())
                                .child("Rowscope"),
                        )
                    }),
            )
            .child(self.render_tabs(cx))
            .child(
                div()
                    .id("open")
                    .size(px(30.))
                    .flex()
                    .flex_none()
                    .items_center()
                    .justify_center()
                    .rounded_md()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover()))
                    .tooltip(tooltip("Open a file (Ctrl+O)"))
                    .on_click(cx.listener(|this, _, w, cx| this.prompt_open(w, cx)))
                    .child(icon_el("plus", theme::text_soft(), 16.)),
            )
            .when(has_sheet, |d| {
                d.child(
                    div()
                        .key_context("SearchBox")
                        .on_action(cx.listener(|this, _: &SearchConfirm, w, cx| {
                            if let Some(s) = this.sheet() {
                                {
                                    let f = s.read(cx).focus.clone();
                                    f.focus(w, cx);
                                }
                            }
                        }))
                        .on_action(cx.listener(|this, _: &SearchCancel, w, cx| {
                            this.search.update(cx, |t, cx| t.clear(w, cx));
                            if let Some(s) = this.sheet() {
                                {
                                    let f = s.read(cx).focus.clone();
                                    f.focus(w, cx);
                                }
                            }
                        }))
                        .w(px(300.))
                        .flex_none()
                        .h(px(32.))
                        .px_2()
                        .flex()
                        .items_center()
                        .gap_2()
                        .rounded_lg()
                        .bg(theme::bg())
                        .border_1()
                        .border_color(if searching {
                            theme::accent().opacity(0.6)
                        } else {
                            theme::border_strong()
                        })
                        .text_sm()
                        .text_color(theme::text())
                        .child(icon_el(
                            "search",
                            if searching {
                                theme::accent()
                            } else {
                                theme::faint()
                            },
                            14.,
                        ))
                        .child(self.search.clone())
                        .when(searching, |d| {
                            d.child(
                                div()
                                    .flex_none()
                                    .text_xs()
                                    .text_color(if running {
                                        theme::faint()
                                    } else {
                                        theme::muted()
                                    })
                                    .child(if running {
                                        "…".to_string()
                                    } else {
                                        format!("{} rows", value::count(matches))
                                    }),
                            )
                            .child(
                                div()
                                    .id("clear-search")
                                    .size(px(20.))
                                    .flex()
                                    .flex_none()
                                    .items_center()
                                    .justify_center()
                                    .rounded_md()
                                    .cursor_pointer()
                                    .hover(|s| s.bg(theme::hover()))
                                    .on_click(cx.listener(|this, _, w, cx| {
                                        this.search.update(cx, |t, cx| t.clear(w, cx));
                                    }))
                                    .child(icon_el("x", theme::muted(), 12.)),
                            )
                        })
                        .when(!searching, |d| d.child(kbd("Ctrl F"))),
                )
                .child(
                    div()
                        .id("export")
                        .child(button("export-btn", "Export", Some("save"), false))
                        .tooltip(tooltip(
                            "Export the rows shown, with edits, as CSV (Ctrl+S)",
                        ))
                        .on_click(cx.listener(|this, _, w, cx| this.prompt_export(w, cx))),
                )
            })
    }

    fn render_formula_bar(
        &self,
        sheet: &Entity<Sheet>,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        let s = sheet.read(cx);
        let (r, c) = s.cursor;
        let content = if s.view_len() > 0 && s.ncols > 0 {
            s.cell_text(r, c)
        } else {
            String::new()
        };
        let edited = s.view_len() > 0 && s.edits.get(s.phys_row(r), c).is_some();
        let header_on = s.has_header;
        let col_name_text = s.header.get(c).cloned().unwrap_or_default();
        div()
            .flex_none()
            .h(px(40.))
            .px_2()
            .flex()
            .items_center()
            .gap_2()
            .bg(theme::panel())
            .border_b_1()
            .border_color(theme::border())
            .child(
                div()
                    .key_context("NameBox")
                    .on_action(cx.listener(|this, _: &NameBoxConfirm, w, cx| this.go_to_ref(w, cx)))
                    .on_action(cx.listener(|this, _: &NameBoxCancel, w, cx| {
                        if let Some(s) = this.sheet() {
                            {
                                let f = s.read(cx).focus.clone();
                                f.focus(w, cx);
                            }
                        }
                        this.sync_name_box(w, cx);
                    }))
                    .w(px(130.))
                    .flex_none()
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .rounded_md()
                    .bg(theme::bg())
                    .border_1()
                    .border_color(theme::border())
                    .text_sm()
                    .font_weight(FontWeight::MEDIUM)
                    .text_color(theme::text())
                    .child(self.name_box.clone()),
            )
            .child(
                div()
                    .flex_none()
                    .px_1()
                    .text_sm()
                    .italic()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::faint())
                    .child("fx"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .h(px(28.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .bg(theme::bg())
                    .border_1()
                    .border_color(theme::border())
                    .text_sm()
                    .when(header_on && !col_name_text.is_empty(), |d| {
                        d.child(
                            div()
                                .flex_none()
                                .px_1()
                                .rounded(px(4.))
                                .bg(theme::raised())
                                .text_xs()
                                .text_color(theme::muted())
                                .child(col_name_text),
                        )
                    })
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .truncate()
                            .text_color(if edited {
                                theme::warning()
                            } else {
                                theme::text()
                            })
                            .child(content.replace(['\n', '\r'], " ⏎ ")),
                    ),
            )
            .child(vsep())
            .child(
                switch("header-toggle", "Header row", header_on)
                    .tooltip(tooltip("Use the first row as column names (Ctrl+Shift+H)"))
                    .on_click(cx.listener(|this, _, _, cx| {
                        if let Some(s) = this.sheet().cloned() {
                            s.update(cx, |s, cx| {
                                let on = !s.has_header;
                                s.set_has_header(on, cx)
                            });
                        }
                    })),
            )
    }

    fn render_chips(
        &self,
        sheet: &Entity<Sheet>,
        cx: &mut Context<Self>,
    ) -> Option<impl IntoElement + use<>> {
        let s = sheet.read(cx);
        if s.query.filters.is_empty() && s.query.sort.is_none() && s.query_error.is_none() {
            return None;
        }
        let mut chips: Vec<gpui::AnyElement> = Vec::new();
        if let Some(so) = s.query.sort {
            let name = s.header.get(so.col).cloned().unwrap_or_default();
            let dir = match (so.numeric, so.desc) {
                (true, false) => "smallest first",
                (true, true) => "largest first",
                (false, false) => "A → Z",
                (false, true) => "Z → A",
            };
            let col = so.col;
            chips.push(
                chip_el(
                    ("sort-chip", 0usize),
                    if so.desc { "arrow-down" } else { "arrow-up" },
                    name,
                    format!("sorted {dir}"),
                )
                .on_click(cx.listener(move |this, _, w, cx| {
                    if let Some(s) = this.sheet().cloned() {
                        s.update(cx, |s, cx| s.open_menu(col, w, cx));
                    }
                }))
                .child(
                    chip_close(("sort-x", 0usize)).on_click(cx.listener(|this, _, _, cx| {
                        cx.stop_propagation();
                        if let Some(s) = this.sheet().cloned() {
                            s.update(cx, |s, cx| s.set_sort(None, cx));
                        }
                    })),
                )
                .into_any_element(),
            );
        }
        for (i, f) in s.query.filters.iter().enumerate() {
            let name = s.header.get(f.col).cloned().unwrap_or_default();
            let col = f.col;
            chips.push(
                chip_el(("filter-chip", i), "filter", name, f.cond.describe())
                    .on_click(cx.listener(move |this, _, w, cx| {
                        if let Some(s) = this.sheet().cloned() {
                            s.update(cx, |s, cx| s.open_menu(col, w, cx));
                        }
                    }))
                    .child(chip_close(("filter-x", i)).on_click(cx.listener(
                        move |this, _, _, cx| {
                            cx.stop_propagation();
                            if let Some(s) = this.sheet().cloned() {
                                s.update(cx, |s, cx| s.remove_filter_at(i, cx));
                            }
                        },
                    )))
                    .into_any_element(),
            );
        }
        let error = s.query_error.clone();
        let many = s.query.filters.len() + usize::from(s.query.sort.is_some()) > 1;
        Some(
            div()
                .flex_none()
                .min_h(px(38.))
                .px_3()
                .py_1()
                .flex()
                .flex_wrap()
                .items_center()
                .gap_2()
                .bg(theme::panel())
                .border_b_1()
                .border_color(theme::border())
                .children(chips)
                .when(many, |d| {
                    d.child(
                        div()
                            .id("clear-all")
                            .px_2()
                            .h(px(26.))
                            .flex()
                            .items_center()
                            .rounded_md()
                            .cursor_pointer()
                            .text_xs()
                            .text_color(theme::muted())
                            .hover(|s| s.bg(theme::hover()).text_color(theme::text()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(s) = this.sheet().cloned() {
                                    s.update(cx, |s, cx| {
                                        s.query.filters.clear();
                                        s.query.sort = None;
                                        s.run_query(cx);
                                    });
                                }
                            }))
                            .child("Clear all"),
                    )
                })
                .when_some(error, |d, e| {
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
                }),
        )
    }

    fn render_status(&self, sheet: &Entity<Sheet>, cx: &mut Context<Self>) -> impl IntoElement {
        let s = sheet.read(cx);
        let mut left: Vec<gpui::AnyElement> = Vec::new();
        // Workbook sheet tabs.
        if s.sheets.len() > 1 {
            for (i, name) in s.sheets.iter().enumerate() {
                let active = s.sheet_name.as_deref() == Some(name.as_str());
                let n = name.clone();
                left.push(
                    div()
                        .id(("wbsheet", i))
                        .h(px(24.))
                        .px_3()
                        .flex()
                        .items_center()
                        .rounded_md()
                        .cursor_pointer()
                        .text_xs()
                        .when(active, |d| {
                            d.bg(theme::raised())
                                .text_color(theme::success())
                                .font_weight(FontWeight::SEMIBOLD)
                                .border_b_2()
                                .border_color(theme::success())
                        })
                        .when(!active, |d| {
                            d.text_color(theme::muted()).hover(|s| s.bg(theme::hover()))
                        })
                        .on_click(cx.listener(move |this, _, w, cx| {
                            if let Some(s) = this.sheet().cloned() {
                                s.update(cx, |s, cx| s.open_sheet(n.clone(), cx));
                                {
                                    let f = s.read(cx).focus.clone();
                                    f.focus(w, cx);
                                }
                            }
                        }))
                        .child(name.clone())
                        .into_any_element(),
                );
            }
            left.push(vsep().into_any_element());
        }
        // Row counts.
        let total = s.data_rows();
        let shown = s.view_len();
        let rows_text = if s.is_filtered() {
            format!("{} of {} rows", value::count(shown), value::count(total))
        } else {
            format!("{} rows", value::count(total))
        };
        left.push(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(icon_el(
                    if s.is_filtered() { "filter" } else { "rows" },
                    if s.is_filtered() {
                        theme::accent()
                    } else {
                        theme::faint()
                    },
                    13.,
                ))
                .child(
                    div()
                        .text_color(if s.is_filtered() {
                            theme::text_soft()
                        } else {
                            theme::muted()
                        })
                        .child(rows_text),
                )
                .child(
                    div()
                        .text_color(theme::faint())
                        .child(format!("× {} columns", value::count(s.ncols as u64))),
                )
                .into_any_element(),
        );
        if let Some(f) = s.loading {
            left.push(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon_el("spinner", theme::accent(), 12.))
                    .child(
                        div()
                            .w(px(80.))
                            .h(px(4.))
                            .rounded_full()
                            .bg(theme::border())
                            .child(
                                div()
                                    .h_full()
                                    .rounded_full()
                                    .bg(theme::accent())
                                    .w(gpui::relative(f.clamp(0.02, 1.))),
                            ),
                    )
                    .child(
                        div()
                            .text_color(theme::muted())
                            .child(format!("Loading {:.0}%", f * 100.)),
                    )
                    .into_any_element(),
            );
        } else if s.query_running {
            left.push(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(icon_el("spinner", theme::accent(), 12.))
                    .child(div().text_color(theme::muted()).child("Filtering…"))
                    .into_any_element(),
            );
        } else {
            let mut timing = Vec::new();
            if let Some(t) = s.load_elapsed {
                timing.push(format!("opened in {}", ms(t)));
            }
            if let Some(t) = s.query_elapsed {
                timing.push(format!("query {}", ms(t)));
            }
            if !timing.is_empty() {
                left.push(
                    div()
                        .text_color(theme::faint())
                        .child(timing.join(" · "))
                        .into_any_element(),
                );
            }
        }
        if s.is_dirty() {
            let n = s.edits.cells.len() as u64;
            left.push(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(div().size(px(6.)).rounded_full().bg(theme::warning()))
                    .child(
                        div()
                            .text_color(theme::warning())
                            .child(format!("{} edited cells", value::count(n))),
                    )
                    .into_any_element(),
            );
        }

        // Selection statistics, on the right.
        let mut right: Vec<gpui::AnyElement> = Vec::new();
        if let Some(job) = &self.export {
            let done = job.progress.load(Relaxed);
            let f = done as f32 / job.total.max(1) as f32;
            right.push(
                div()
                    .flex()
                    .items_center()
                    .gap_2()
                    .child(icon_el("save", theme::accent(), 12.))
                    .child(
                        div()
                            .text_color(theme::muted())
                            .child(format!("Exporting {:.0}%", f * 100.)),
                    )
                    .child(
                        div()
                            .id("cancel-export")
                            .px_1()
                            .rounded_md()
                            .cursor_pointer()
                            .text_color(theme::faint())
                            .hover(|s| s.bg(theme::hover()).text_color(theme::text()))
                            .on_click(cx.listener(|this, _, _, cx| {
                                if let Some(j) = &this.export {
                                    j.cancel.store(true, Relaxed);
                                }
                                cx.notify();
                            }))
                            .child("Cancel"),
                    )
                    .into_any_element(),
            );
        }
        if s.sel_cells() > 1 {
            let stat = |label: &'static str, v: String| {
                div()
                    .flex()
                    .gap_1()
                    .child(div().text_color(theme::faint()).child(label))
                    .child(div().text_color(theme::text_soft()).child(v))
                    .into_any_element()
            };
            match &s.stats {
                Some(st) => {
                    right.push(stat("Count", value::count(st.count)));
                    if st.nums > 0 {
                        let fmt = |v: f64| {
                            if v.abs() >= 1e15 || (v != 0. && v.abs() < 1e-4) {
                                format!("{v:.4e}")
                            } else if v.fract() == 0. {
                                value::format_grouped(v, 0)
                            } else {
                                value::format_grouped(v, 2)
                            }
                        };
                        right.push(stat("Sum", fmt(st.sum)));
                        right.push(stat("Average", fmt(st.avg())));
                        right.push(stat("Min", fmt(st.min)));
                        right.push(stat("Max", fmt(st.max)));
                    }
                }
                None => right.push(
                    div()
                        .flex()
                        .items_center()
                        .gap_1()
                        .child(icon_el("spinner", theme::faint(), 11.))
                        .child(div().text_color(theme::faint()).child("Summing…"))
                        .into_any_element(),
                ),
            }
        }
        div()
            .flex_none()
            .h(px(32.))
            .px_3()
            .flex()
            .items_center()
            .gap_3()
            .bg(theme::panel())
            .border_t_1()
            .border_color(theme::border())
            .text_xs()
            .children(left)
            .child(div().flex_1())
            .children(right)
    }

    fn render_welcome(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let recent: Vec<_> = self
            .recent
            .iter()
            .enumerate()
            .map(|(i, p)| {
                let path = p.clone();
                let name = p
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let dir = p
                    .parent()
                    .map(|d| d.display().to_string())
                    .unwrap_or_default();
                let size = std::fs::metadata(p).map(|m| m.len()).unwrap_or(0);
                div()
                    .id(("recent", i))
                    .h(px(44.))
                    .px_3()
                    .flex()
                    .items_center()
                    .gap_3()
                    .rounded_lg()
                    .cursor_pointer()
                    .hover(|s| s.bg(theme::hover()))
                    .on_click(
                        cx.listener(move |this, _, w, cx| this.open_path(path.clone(), w, cx)),
                    )
                    .child(icon_el("file-sheet", theme::success(), 18.))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .truncate()
                                    .text_sm()
                                    .text_color(theme::text())
                                    .child(name),
                            )
                            .child(
                                div()
                                    .truncate()
                                    .text_xs()
                                    .text_color(theme::faint())
                                    .child(dir),
                            ),
                    )
                    .child(
                        div()
                            .text_xs()
                            .text_color(theme::faint())
                            .child(human_bytes(size)),
                    )
            })
            .collect();
        let hint = |k: &'static str, label: &'static str| {
            div()
                .flex()
                .items_center()
                .gap_2()
                .text_xs()
                .text_color(theme::muted())
                .child(kbd(k))
                .child(label)
        };
        div()
            .flex_1()
            .flex()
            .items_center()
            .justify_center()
            .child(
                div()
                    .w(px(520.))
                    .flex()
                    .flex_col()
                    .gap_5()
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .items_center()
                            .gap_3()
                            .child(
                                div()
                                    .size(px(64.))
                                    .rounded_2xl()
                                    .bg(theme::success().opacity(0.14))
                                    .border_1()
                                    .border_color(theme::success().opacity(0.3))
                                    .flex()
                                    .items_center()
                                    .justify_center()
                                    .child(icon_el("table", theme::success(), 32.)),
                            )
                            .child(
                                div()
                                    .text_xl()
                                    .font_weight(FontWeight::SEMIBOLD)
                                    .text_color(theme::text())
                                    .child("Open a spreadsheet"),
                            )
                            .child(
                                div()
                                    .text_sm()
                                    .text_color(theme::muted())
                                    .child("CSV, TSV, XLSX, XLSB, XLS and ODS — millions of rows, filtered in milliseconds."),
                            )
                            .child(
                                div().pt_2().child(
                                    button("welcome-open", "Open file…", Some("folder-open"), true)
                                        .on_click(cx.listener(|this, _, w, cx| this.prompt_open(w, cx))),
                                ),
                            )
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(theme::faint())
                                    .child("or drop a file anywhere in this window"),
                            ),
                    )
                    .when(!recent.is_empty(), |d| {
                        d.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .p_2()
                                .rounded_xl()
                                .bg(theme::panel())
                                .border_1()
                                .border_color(theme::border())
                                .child(
                                    div()
                                        .px_3()
                                        .pt_1()
                                        .pb_1()
                                        .text_xs()
                                        .font_weight(FontWeight::SEMIBOLD)
                                        .text_color(theme::faint())
                                        .child("RECENT"),
                                )
                                .children(recent),
                        )
                    })
                    .child(
                        div()
                            .flex()
                            .flex_wrap()
                            .justify_center()
                            .gap_4()
                            .child(hint("Ctrl O", "Open"))
                            .child(hint("Ctrl F", "Search"))
                            .child(hint("Ctrl G", "Go to cell"))
                            .child(hint("Ctrl S", "Export CSV")),
                    ),
            )
    }

    fn render_toast(&self) -> Option<impl IntoElement + use<>> {
        let t = self.toast.clone()?;
        Some(
            div()
                .absolute()
                .bottom(px(48.))
                .left_0()
                .right_0()
                .flex()
                .justify_center()
                .child(
                    div()
                        .max_w(px(640.))
                        .px_4()
                        .py_2()
                        .rounded_lg()
                        .bg(theme::card())
                        .border_1()
                        .border_color(theme::border_strong())
                        .shadow_lg()
                        .flex()
                        .items_center()
                        .gap_2()
                        .text_sm()
                        .text_color(theme::text())
                        .child(icon_el("info", theme::accent(), 15.))
                        .child(t),
                ),
        )
    }
}

fn chip_el(
    id: impl Into<gpui::ElementId>,
    icon: &'static str,
    name: SharedString,
    desc: String,
) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .h(px(26.))
        .pl_2()
        .pr_1()
        .flex()
        .items_center()
        .gap_1()
        .rounded_full()
        .bg(theme::accent_soft())
        .border_1()
        .border_color(theme::accent().opacity(0.35))
        .cursor_pointer()
        .text_xs()
        .hover(|s| s.border_color(theme::accent().opacity(0.7)))
        .child(icon_el(icon, theme::accent(), 12.))
        .child(
            div()
                .font_weight(FontWeight::SEMIBOLD)
                .text_color(theme::text())
                .child(name),
        )
        .child(
            div()
                .text_color(theme::text_soft())
                .max_w(px(320.))
                .truncate()
                .child(desc),
        )
}

fn chip_close(id: impl Into<gpui::ElementId>) -> gpui::Stateful<gpui::Div> {
    div()
        .id(id)
        .size(px(18.))
        .flex()
        .items_center()
        .justify_center()
        .rounded_full()
        .hover(|s| s.bg(theme::accent_strong()))
        .child(icon_el("x", theme::accent(), 10.))
}

/// `B12` (cursor) or `B12:D20` (range) for the name box.
fn cell_ref(s: &Sheet) -> String {
    if s.view_len() == 0 || s.ncols == 0 {
        return String::new();
    }
    let one = |(r, c): (u64, usize)| format!("{}{}", col_name(c), s.data_row(r) + 1);
    if s.sel_cells() > 1 {
        let rows = s.sel_rows();
        let cols = s.sel_cols();
        if s.is_whole_cols() {
            if cols.len() == 1 {
                return col_name(cols.start);
            }
            return format!("{}:{}", col_name(cols.start), col_name(cols.end - 1));
        }
        if s.is_filtered() || s.query.sort.is_some() {
            // Row numbers aren't contiguous in a filtered or sorted view.
            return format!("{}R × {}C", value::count(rows.end - rows.start), cols.len());
        }
        return format!(
            "{}:{}",
            one((rows.start, cols.start)),
            one((rows.end - 1, cols.end - 1))
        );
    }
    one(s.cursor)
}

type Ref = (Option<u64>, Option<usize>);

/// Parses `B12`, `12`, `B`, or two of those joined by `:`. Row numbers are
/// 1-based view rows.
fn parse_ref(text: &str) -> Option<(Ref, Option<Ref>)> {
    let one = |t: &str| -> Option<Ref> {
        let t = t.trim().replace(',', "");
        if t.is_empty() {
            return None;
        }
        let split = t.find(|c: char| c.is_ascii_digit()).unwrap_or(t.len());
        let (letters, digits) = t.split_at(split);
        let col = if letters.is_empty() {
            None
        } else {
            Some(parse_col_name(letters)?)
        };
        let row = if digits.is_empty() {
            None
        } else {
            Some(digits.parse::<u64>().ok()?.max(1) - 1)
        };
        if col.is_none() && row.is_none() {
            return None;
        }
        Some((row, col))
    };
    match text.split_once(':') {
        Some((a, b)) => Some((one(a)?, Some(one(b)?))),
        None => Some((one(text)?, None)),
    }
}

impl Render for Workspace {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let sheet = self.sheet().cloned();
        let title = match &sheet {
            Some(s) => format!("{} — Rowscope", s.read(cx).title()),
            None => "Rowscope".to_string(),
        };
        if title != self.window_title {
            window.set_window_title(&title);
            self.window_title = title;
        }
        div()
            .id("workspace")
            .key_context("Workspace")
            .track_focus(&self.focus)
            .on_action(cx.listener(|this, _: &Open, w, cx| this.prompt_open(w, cx)))
            .on_action(cx.listener(|this, _: &CloseTab, w, cx| this.close_tab(w, cx)))
            .on_action(cx.listener(|this, _: &NextTab, w, cx| {
                if !this.sheets.is_empty() {
                    let i = (this.active + 1) % this.sheets.len();
                    this.activate(i, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &PrevTab, w, cx| {
                if !this.sheets.is_empty() {
                    let i = (this.active + this.sheets.len() - 1) % this.sheets.len();
                    this.activate(i, w, cx);
                }
            }))
            .on_action(cx.listener(|this, _: &FocusSearch, w, cx| {
                if this.sheet().is_some() {
                    let f = this.search.read(cx).focus_handle(cx);
                    f.focus(w, cx);
                    this.search.update(cx, |t, cx| {
                        let n = t.text().len();
                        t.set_text(t.text().to_string(), Some(0..n), cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &GoTo, w, cx| {
                if this.sheet().is_some() {
                    let f = this.name_box.read(cx).focus_handle(cx);
                    f.focus(w, cx);
                    this.name_box.update(cx, |t, cx| {
                        let n = t.text().len();
                        t.set_text(t.text().to_string(), Some(0..n), cx);
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &Export, w, cx| this.prompt_export(w, cx)))
            .on_action(cx.listener(|this, _: &ToggleHeader, _, cx| {
                if let Some(s) = this.sheet().cloned() {
                    s.update(cx, |s, cx| {
                        let on = !s.has_header;
                        s.set_has_header(on, cx)
                    });
                }
            }))
            .on_action(cx.listener(|this, _: &ZoomIn, _, cx| this.zoom(Some(1.1), cx)))
            .on_action(cx.listener(|this, _: &ZoomOut, _, cx| this.zoom(Some(1. / 1.1), cx)))
            .on_action(cx.listener(|this, _: &ZoomReset, _, cx| this.zoom(None, cx)))
            .on_drop(cx.listener(|this, paths: &ExternalPaths, w, cx| {
                for p in paths.paths() {
                    this.open_path(p.clone(), w, cx);
                }
            }))
            .relative()
            .size_full()
            .flex()
            .flex_col()
            .bg(theme::bg())
            .text_color(theme::text())
            .child(self.render_top_bar(cx))
            .when_some(sheet.clone(), |d, sheet| {
                d.child(self.render_formula_bar(&sheet, cx))
                    .children(self.render_chips(&sheet, cx))
                    .child(div().flex_1().min_h_0().child(sheet.clone()))
                    .child(self.render_status(&sheet, cx))
            })
            .when(sheet.is_none(), |d| d.child(self.render_welcome(cx)))
            .children(self.render_toast())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs() {
        assert_eq!(parse_ref("B12"), Some(((Some(11), Some(1)), None)));
        assert_eq!(parse_ref("5000"), Some(((Some(4999), None), None)));
        assert_eq!(parse_ref("c"), Some(((None, Some(2)), None)));
        assert_eq!(
            parse_ref("A1:C10"),
            Some(((Some(0), Some(0)), Some((Some(9), Some(2)))))
        );
        assert_eq!(parse_ref("1,000"), Some(((Some(999), None), None)));
        assert_eq!(parse_ref("?"), None);
        assert_eq!(parse_ref("1A"), None);
    }
}

#[cfg(feature = "snapshot")]
impl Workspace {
    pub fn debug_apply_query(
        &mut self,
        search: Option<String>,
        sort: Option<(usize, bool)>,
        filter: Option<(usize, crate::data::query::Cond)>,
        zoom: Option<f32>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(t) = search {
            self.search
                .update(cx, |i, cx| i.set_text(t.clone(), None, cx));
            if let Some(s) = self.sheet() {
                s.update(cx, |s, cx| s.set_search(&t, cx));
            }
        }
        let Some(sheet) = self.sheet().cloned() else {
            return;
        };
        sheet.update(cx, |s, cx| {
            if let Some((c, d)) = sort {
                s.sort_by(c, d, cx);
            }
            if let Some((c, cond)) = filter {
                s.set_filter(c, vec![cond], cx);
            }
            if let Some(z) = zoom {
                s.zoom = z;
                s.update_col_x();
            }
        });
        let _ = window;
    }
}
