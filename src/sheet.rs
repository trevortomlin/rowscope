//! One open table: its data as it loads, the query (search, filters, sort)
//! and the view it produces, the selection, scrolling, edits and the column
//! menus. The grid itself is painted by [`crate::grid::Grid`].

use std::ops::Range;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Duration;

use futures::StreamExt;
use gpui::{
    App, Bounds, ClipboardItem, Context, Entity, EventEmitter, FocusHandle, Focusable,
    KeyDownEvent, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Pixels, Point, Render,
    ScrollEvent, SharedString, Subscription, Task, Window, actions, anchored, deferred, div, point,
    prelude::*, px,
};

use crate::column_menu::ColumnMenu;
use crate::data::query::{self, ColFilter, Cond, Edits, Query, SortSpec, Stats};
use crate::data::value::{self, ColType, col_name, parse_number};
use crate::data::{self, Data, LoadEvent, RowFormat};
use crate::grid::Grid;
use crate::text_input::{TextInput, TextInputEvent};
use crate::theme;
use crate::ui::icon_el;

actions!(
    sheet,
    [
        MoveUp,
        MoveDown,
        MoveLeft,
        MoveRight,
        SelectUp,
        SelectDown,
        SelectLeft,
        SelectRight,
        JumpUp,
        JumpDown,
        JumpLeft,
        JumpRight,
        JumpSelectUp,
        JumpSelectDown,
        JumpSelectLeft,
        JumpSelectRight,
        PageUp,
        PageDown,
        SelectPageUp,
        SelectPageDown,
        RowStart,
        RowEnd,
        SheetStart,
        SheetEnd,
        SelectAll,
        SelectColumn,
        SelectRow,
        TabNext,
        TabPrev,
        Enter,
        EditCell,
        ClearCells,
        Copy,
        Cut,
        Paste,
        Undo,
        Redo,
        CommitEdit,
        CommitEditRight,
        CancelEdit,
        ClearFilters,
        Dismiss,
    ]
);

pub const BASE_ROW_H: f32 = 24.;
pub const BASE_FONT: f32 = 13.;
const LETTER_H: f32 = 20.;
const NAME_H: f32 = 30.;
pub const SCROLLBAR: f32 = 10.;
const MIN_COL_W: f32 = 36.;
const MAX_CELLS_COPY: u64 = 2_000_000;
const MAX_CELLS_EDIT: u64 = 200_000;

/// Which rows are shown, in order.
#[derive(Clone)]
pub enum View {
    /// Every data row in source order.
    All,
    /// Data rows (filtered and/or sorted).
    Rows(Arc<Vec<u32>>),
}

#[derive(Clone, Copy, Debug, Default)]
pub struct Geom {
    pub bounds: Bounds<Pixels>,
    pub gutter: f32,
    pub head: f32,
    pub row_h: f32,
}

impl Geom {
    pub fn viewport(&self) -> Bounds<Pixels> {
        let b = self.bounds;
        Bounds::new(
            point(b.left() + px(self.gutter), b.top() + px(self.head)),
            gpui::size(
                (b.size.width - px(self.gutter)).max(px(0.)),
                (b.size.height - px(self.head)).max(px(0.)),
            ),
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Corner,
    ColEdge(usize),
    ColHeader { col: usize, button: bool },
    RowHeader(u64),
    Cell(u64, usize),
    VThumb(f32),
    VTrack,
    HThumb(f32),
    HTrack,
    Outside,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Drag {
    None,
    Cells,
    Cols,
    Rows,
    Resize {
        col: usize,
        start_x: f32,
        start_w: f32,
    },
    VScroll {
        grab: f32,
    },
    HScroll {
        grab: f32,
    },
}

pub enum SheetEvent {
    /// Something shown outside the grid changed (status, selection, title).
    Changed,
    Toast(String),
}

struct Editing {
    input: Entity<TextInput>,
    phys: u64,
    col: usize,
    _sub: Subscription,
}

type EditBatch = Vec<(u64, usize, Option<Arc<str>>, Option<Arc<str>>)>;

pub struct ContextMenu {
    pub position: Point<Pixels>,
    pub row: u64,
    pub col: usize,
}

pub struct Sheet {
    pub path: PathBuf,
    pub focus: FocusHandle,

    // Loading.
    pub data: Data,
    pub loading: Option<f32>,
    pub load_elapsed: Option<Duration>,
    pub error: Option<String>,
    load_cancel: Arc<AtomicBool>,
    _load_task: Option<Task<()>>,
    pub sheets: Vec<String>,
    pub sheet_name: Option<String>,
    columns_ready: bool,

    // Shape.
    pub has_header: bool,
    header_auto: bool,
    pub header: Vec<SharedString>,
    pub ncols: usize,
    pub types: Vec<ColType>,
    pub widths: Vec<f32>,
    pub col_x: Vec<f32>,
    /// Widest row seen while painting, to grow `ncols` for ragged files.
    pub seen_cols: std::cell::Cell<usize>,

    // Query.
    pub query: Query,
    pub view: View,
    query_gen: u64,
    query_cancel: Arc<AtomicBool>,
    _query_task: Option<Task<()>>,
    pub query_running: bool,
    pub query_error: Option<String>,
    pub query_elapsed: Option<Duration>,
    query_partial: bool,

    // Selection and scrolling, in view rows.
    pub anchor: (u64, usize),
    pub cursor: (u64, usize),
    pub scroll_y: f64,
    pub scroll_x: f32,
    pub zoom: f32,
    pub geom: std::cell::Cell<Geom>,
    drag: Drag,
    pub hover: Hit,

    // Edits.
    pub edits: Arc<Edits>,
    undo: Vec<EditBatch>,
    redo: Vec<EditBatch>,
    editing: Option<Editing>,

    // Status.
    pub stats: Option<Stats>,
    stats_gen: u64,
    _stats_task: Option<Task<()>>,

    pub menu: Option<(Entity<ColumnMenu>, Point<Pixels>)>,
    pub context_menu: Option<ContextMenu>,
}

impl EventEmitter<SheetEvent> for Sheet {}

impl Focusable for Sheet {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus.clone()
    }
}

impl Drop for Sheet {
    fn drop(&mut self) {
        self.load_cancel.store(true, Relaxed);
        self.query_cancel.store(true, Relaxed);
    }
}

impl Sheet {
    pub fn new(path: PathBuf, sheet: Option<String>, cx: &mut Context<Self>) -> Self {
        let format = match data::kind_of(&path) {
            data::Kind::Delimited => RowFormat::Delimited { delim: b',' },
            data::Kind::Workbook => RowFormat::Packed,
        };
        let mut s = Sheet {
            path,
            focus: cx.focus_handle(),
            data: Data::empty(format),
            loading: Some(0.0),
            load_elapsed: None,
            error: None,
            load_cancel: Arc::new(AtomicBool::new(false)),
            _load_task: None,
            sheets: Vec::new(),
            sheet_name: sheet.clone(),
            columns_ready: false,
            has_header: true,
            header_auto: true,
            header: Vec::new(),
            ncols: 0,
            types: Vec::new(),
            widths: Vec::new(),
            col_x: vec![0.],
            seen_cols: std::cell::Cell::new(0),
            query: Query::default(),
            view: View::All,
            query_gen: 0,
            query_cancel: Arc::new(AtomicBool::new(false)),
            _query_task: None,
            query_running: false,
            query_error: None,
            query_elapsed: None,
            query_partial: false,
            anchor: (0, 0),
            cursor: (0, 0),
            scroll_y: 0.,
            scroll_x: 0.,
            zoom: 1.0,
            geom: Default::default(),
            drag: Drag::None,
            hover: Hit::Outside,
            edits: Arc::new(Edits::default()),
            undo: Vec::new(),
            redo: Vec::new(),
            editing: None,
            stats: None,
            stats_gen: 0,
            _stats_task: None,
            menu: None,
            context_menu: None,
        };
        s.start_load(sheet, cx);
        s
    }

    pub fn title(&self) -> SharedString {
        self.path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.path.display().to_string())
            .into()
    }

    pub fn is_dirty(&self) -> bool {
        !self.edits.is_empty()
    }

    // ── Loading ─────────────────────────────────────────────────────────

    fn start_load(&mut self, sheet: Option<String>, cx: &mut Context<Self>) {
        self.load_cancel.store(true, Relaxed);
        self.load_cancel = Arc::new(AtomicBool::new(false));
        self.loading = Some(0.0);
        self.error = None;
        let mut rx = data::spawn_load(self.path.clone(), sheet, self.load_cancel.clone());
        self._load_task = Some(cx.spawn(async move |this, cx| {
            while let Some(ev) = rx.next().await {
                if this.update(cx, |s, cx| s.on_load_event(ev, cx)).is_err() {
                    break;
                }
            }
        }));
    }

    /// Switches to another sheet of a workbook.
    pub fn open_sheet(&mut self, name: String, cx: &mut Context<Self>) {
        if self.sheet_name.as_deref() == Some(name.as_str()) && self.error.is_none() {
            return;
        }
        let format = self.data.format;
        self.data = Data::empty(format);
        self.sheet_name = Some(name.clone());
        self.columns_ready = false;
        self.header_auto = true;
        self.query = Query::default();
        self.view = View::All;
        self.edits = Arc::new(Edits::default());
        self.undo.clear();
        self.redo.clear();
        self.editing = None;
        self.anchor = (0, 0);
        self.cursor = (0, 0);
        self.scroll_x = 0.;
        self.scroll_y = 0.;
        self.menu = None;
        self.start_load(Some(name), cx);
        cx.emit(SheetEvent::Changed);
        cx.notify();
    }

    fn on_load_event(&mut self, ev: LoadEvent, cx: &mut Context<Self>) {
        match ev {
            LoadEvent::Sheets(names) => {
                if self.sheet_name.is_none() {
                    self.sheet_name = names.first().cloned();
                }
                self.sheets = names;
            }
            LoadEvent::Progress { data, fraction } => {
                self.data = data;
                self.loading = Some(fraction);
                if !self.columns_ready && self.data.rows > 0 {
                    self.setup_columns(false);
                }
            }
            LoadEvent::Done { data, elapsed } => {
                self.data = data;
                self.loading = None;
                self.load_elapsed = Some(elapsed);
                // Re-guess with a sample spread over the whole file.
                self.setup_columns(self.columns_ready);
                if !self.query.is_identity() {
                    self.run_query(cx);
                }
                self.update_stats(cx);
            }
            LoadEvent::Failed(e) => {
                self.loading = None;
                self.error = Some(e);
            }
        }
        cx.emit(SheetEvent::Changed);
        cx.notify();
    }

    /// Physical rows before the first data row.
    pub fn first(&self) -> u64 {
        u64::from(self.has_header && self.data.rows > 0)
    }

    pub fn data_rows(&self) -> u64 {
        self.data.rows.saturating_sub(self.first())
    }

    /// Picks the column count, header, types and widths from a sample.
    /// `keep_layout` keeps widths the user may have changed.
    fn setup_columns(&mut self, keep_layout: bool) {
        let rows = self.data.rows;
        let mut sample: Vec<u64> = (0..rows.min(1000)).collect();
        if rows > 1000 {
            let step = (rows / 1000).max(1);
            sample.extend((1000..rows).step_by(step as usize));
        }
        let mut ncols = self.ncols;
        for &r in &sample {
            let mut n = 0;
            self.data.for_each_field(r, 16_384, |i, _| n = i + 1);
            ncols = ncols.max(n);
        }
        self.ncols = ncols.max(1);
        if self.header_auto {
            self.has_header = self.guess_header();
        }
        self.refresh_header();
        self.types = value::infer_types(&self.data, self.first(), self.ncols, 4000);
        if !keep_layout || self.widths.len() != self.ncols {
            let old = std::mem::take(&mut self.widths);
            self.widths = (0..self.ncols)
                .map(|c| {
                    old.get(c)
                        .copied()
                        .filter(|_| keep_layout)
                        .unwrap_or_else(|| self.guess_width(c, &sample))
                })
                .collect();
        }
        self.update_col_x();
        self.columns_ready = true;
    }

    /// The first row is a header when it has text where the rest of a
    /// column holds numbers, or when it is all distinct non-empty text.
    fn guess_header(&self) -> bool {
        if self.data.rows < 2 {
            return true;
        }
        let row0 = self.data.row_fields(0, self.ncols);
        if row0.is_empty() {
            return false;
        }
        let types = value::infer_types(&self.data, 1, self.ncols, 2000);
        let mut numeric_cols = 0;
        for (c, t) in types.iter().enumerate() {
            if *t == ColType::Number || *t == ColType::Date {
                numeric_cols += 1;
                let v = row0.get(c).map(|v| &v[..]).unwrap_or(&[]);
                if !v.trim_ascii().is_empty()
                    && parse_number(v).is_none()
                    && !value::looks_like_date(v)
                {
                    return true;
                }
            }
        }
        if numeric_cols > 0 {
            return false;
        }
        let mut seen = std::collections::HashSet::new();
        row0.iter()
            .all(|v| !v.trim_ascii().is_empty() && seen.insert(v.clone()))
    }

    fn refresh_header(&mut self) {
        self.header = if self.has_header && self.data.rows > 0 {
            let f = self.data.row_fields(0, self.ncols);
            (0..self.ncols)
                .map(|c| {
                    let name = f
                        .get(c)
                        .map(|v| data::lossy(v).trim().to_string())
                        .unwrap_or_default();
                    if name.is_empty() {
                        col_name(c).into()
                    } else {
                        name.into()
                    }
                })
                .collect()
        } else {
            (0..self.ncols).map(|c| col_name(c).into()).collect()
        };
    }

    fn guess_width(&self, c: usize, sample: &[u64]) -> f32 {
        // Bold name plus the type badge and the filter button.
        let head = self.header.get(c).map_or(0., |h| {
            h.chars().count() as f32 * 8.2 + if self.has_header { 66. } else { 0. }
        });
        let mut chars = 3;
        for &r in sample.iter().take(400) {
            let v = self.data.cell(r, c);
            let n = std::str::from_utf8(&v).map_or(v.len(), |s| s.chars().count());
            chars = chars.max(n.min(60));
        }
        (chars as f32 * 7.4 + 24.).max(head).clamp(60., 360.)
    }

    pub fn update_col_x(&mut self) {
        let mut x = 0.;
        self.col_x.clear();
        self.col_x.push(0.);
        for w in &self.widths {
            x += w * self.zoom;
            self.col_x.push(x);
        }
    }

    /// Grows the column count when a painted row had more fields.
    fn grow_columns(&mut self) {
        let seen = self.seen_cols.get();
        if seen > self.ncols {
            let sample: Vec<u64> = (0..self.data.rows.min(200)).collect();
            for c in self.ncols..seen {
                self.header.push(col_name(c).into());
                self.types.push(ColType::Text);
                let w = self.guess_width(c, &sample);
                self.widths.push(w);
            }
            self.ncols = seen;
            self.update_col_x();
        }
    }

    pub fn set_has_header(&mut self, on: bool, cx: &mut Context<Self>) {
        if self.has_header == on {
            return;
        }
        self.has_header = on;
        self.header_auto = false;
        self.refresh_header();
        self.types = value::infer_types(&self.data, self.first(), self.ncols, 4000);
        self.query.filters.clear();
        self.query.sort = None;
        self.run_query(cx);
        cx.emit(SheetEvent::Changed);
        cx.notify();
    }

    // ── Views ───────────────────────────────────────────────────────────

    pub fn view_len(&self) -> u64 {
        match &self.view {
            View::All => self.data_rows(),
            View::Rows(v) => v.len() as u64,
        }
    }

    /// The data row shown at view row `vr`.
    #[inline]
    pub fn data_row(&self, vr: u64) -> u64 {
        match &self.view {
            View::All => vr,
            View::Rows(v) => v.get(vr as usize).copied().unwrap_or(0) as u64,
        }
    }

    /// Where data row `dr` is in the view, if it is shown.
    pub fn view_row_of(&self, dr: u64) -> Option<u64> {
        match &self.view {
            View::All => (dr < self.data_rows()).then_some(dr),
            View::Rows(v) => {
                let dr = u32::try_from(dr).ok()?;
                if self.query.sort.is_none() {
                    v.binary_search(&dr).ok().map(|i| i as u64)
                } else {
                    v.iter().position(|&r| r == dr).map(|i| i as u64)
                }
            }
        }
    }

    pub fn phys_row(&self, vr: u64) -> u64 {
        self.data_row(vr) + self.first()
    }

    fn view_fn(&self) -> Arc<dyn Fn(u64) -> u64 + Send + Sync> {
        match &self.view {
            View::All => Arc::new(|r| r),
            View::Rows(v) => {
                let v = v.clone();
                Arc::new(move |r| v.get(r as usize).copied().unwrap_or(0) as u64)
            }
        }
    }

    pub fn cell_text(&self, vr: u64, col: usize) -> String {
        let phys = self.phys_row(vr);
        data::lossy(&query::cell_text(&self.data, &self.edits, phys, col)).into_owned()
    }

    pub fn is_filtered(&self) -> bool {
        self.query.filters_rows()
    }

    // ── Query ───────────────────────────────────────────────────────────

    pub fn set_search(&mut self, text: &str, cx: &mut Context<Self>) {
        if self.query.search == text {
            return;
        }
        self.query.search = text.to_string();
        self.run_query(cx);
    }

    pub fn set_filter(&mut self, col: usize, conds: Vec<Cond>, cx: &mut Context<Self>) {
        self.query.filters.retain(|f| f.col != col);
        for cond in conds {
            self.query.filters.push(ColFilter { col, cond });
        }
        self.run_query(cx);
    }

    pub fn clear_filter(&mut self, col: usize, cx: &mut Context<Self>) {
        self.set_filter(col, Vec::new(), cx);
    }

    pub fn clear_all_filters(&mut self, cx: &mut Context<Self>) {
        if self.query.filters.is_empty() && self.query.search.is_empty() {
            return;
        }
        self.query.filters.clear();
        self.query.search.clear();
        self.run_query(cx);
    }

    pub fn remove_filter_at(&mut self, i: usize, cx: &mut Context<Self>) {
        if i < self.query.filters.len() {
            self.query.filters.remove(i);
            self.run_query(cx);
        }
    }

    pub fn set_sort(&mut self, sort: Option<SortSpec>, cx: &mut Context<Self>) {
        self.query.sort = sort;
        self.run_query(cx);
    }

    pub fn sort_by(&mut self, col: usize, desc: bool, cx: &mut Context<Self>) {
        let numeric = self.types.get(col).is_some_and(|t| t.numeric());
        self.set_sort(Some(SortSpec { col, desc, numeric }), cx);
    }

    pub fn has_filter(&self, col: usize) -> bool {
        self.query.filters.iter().any(|f| f.col == col)
    }

    pub fn run_query(&mut self, cx: &mut Context<Self>) {
        self.query_cancel.store(true, Relaxed);
        self.query_cancel = Arc::new(AtomicBool::new(false));
        self.query_gen += 1;
        self.query_error = None;
        let cursor_col = self.cursor.1;
        if self.query.is_identity() {
            self.view = View::All;
            self.query_running = false;
            self.query_elapsed = None;
            self._query_task = None;
            self.after_view_change(cursor_col, cx);
            return;
        }
        let generation = self.query_gen;
        let cancel = self.query_cancel.clone();
        let data = self.data.clone();
        let first = self.first();
        let q = self.query.clone();
        let edits = self.edits.clone();
        let ncols = self.ncols;
        let partial = self.loading.is_some();
        // Typing in the search box on a big table: wait for a pause.
        let delay = if data.rows > 500_000 { 90 } else { 0 };
        self.query_running = true;
        let executor = cx.background_executor().clone();
        self._query_task = Some(cx.spawn(async move |this, cx| {
            if delay > 0 {
                executor.timer(Duration::from_millis(delay)).await;
                if cancel.load(Relaxed) {
                    return;
                }
            }
            let job = executor.spawn({
                let cancel = cancel.clone();
                async move { query::run(&data, first, &q, &edits, ncols, &cancel) }
            });
            let result = job.await;
            this.update(cx, |s, cx| {
                if s.query_gen != generation {
                    return;
                }
                s.query_running = false;
                match result {
                    Ok(Some(r)) => {
                        s.query_elapsed = Some(r.elapsed);
                        s.query_partial = partial;
                        s.view = View::Rows(Arc::new(r.rows));
                        s.after_view_change(cursor_col, cx);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        s.query_error = Some(e);
                        cx.emit(SheetEvent::Changed);
                        cx.notify();
                    }
                }
            })
            .ok();
        }));
        cx.emit(SheetEvent::Changed);
        cx.notify();
    }

    fn after_view_change(&mut self, col: usize, cx: &mut Context<Self>) {
        self.scroll_y = 0.;
        self.anchor = (0, col);
        self.cursor = (0, col);
        self.update_stats(cx);
        cx.emit(SheetEvent::Changed);
        cx.notify();
    }

    /// Runs `q` but for the filters on `col`, for the column menu's value
    /// list (it shows what the other filters leave).
    pub fn rows_without_filter(&self, col: usize) -> (Data, u64, Query, Arc<Edits>, usize) {
        let mut q = self.query.clone();
        q.filters.retain(|f| f.col != col);
        q.sort = None;
        (
            self.data.clone(),
            self.first(),
            q,
            self.edits.clone(),
            self.ncols,
        )
    }

    // ── Geometry ────────────────────────────────────────────────────────

    pub fn row_h(&self) -> f32 {
        (BASE_ROW_H * self.zoom).round()
    }

    pub fn font_size(&self) -> f32 {
        BASE_FONT * self.zoom
    }

    pub fn head_h(&self) -> f32 {
        ((if self.has_header {
            LETTER_H + NAME_H
        } else {
            LETTER_H + 4.
        }) * self.zoom)
            .round()
    }

    pub fn gutter_w(&self) -> f32 {
        let digits = (self.data_rows().max(1) as f64).log10().floor() as usize + 1;
        ((digits.max(3) as f32 * 7.6 + 22.) * self.zoom).round()
    }

    pub fn letter_h(&self) -> f32 {
        (LETTER_H * self.zoom).round()
    }

    pub fn content_w(&self) -> f32 {
        *self.col_x.last().unwrap_or(&0.)
    }

    pub fn content_h(&self) -> f64 {
        self.view_len() as f64 * self.row_h() as f64
    }

    pub fn max_scroll_y(&self, g: &Geom) -> f64 {
        let vh = f32::from(g.viewport().size.height) as f64;
        (self.content_h() + self.row_h() as f64 * 2. - vh).max(0.)
    }

    pub fn max_scroll_x(&self, g: &Geom) -> f32 {
        let vw = f32::from(g.viewport().size.width);
        (self.content_w() + 40. - vw).max(0.)
    }

    fn clamp_scroll(&mut self) {
        let g = self.geom.get();
        self.scroll_y = self.scroll_y.clamp(0., self.max_scroll_y(&g));
        self.scroll_x = self.scroll_x.clamp(0., self.max_scroll_x(&g));
    }

    pub fn col_at(&self, content_x: f32) -> Option<usize> {
        if content_x < 0. || content_x >= self.content_w() {
            return None;
        }
        Some(
            self.col_x
                .partition_point(|&x| x <= content_x)
                .saturating_sub(1),
        )
    }

    /// Scrollbar thumb (offset, length) along the track, or `None` when
    /// everything fits.
    pub fn v_thumb(&self, g: &Geom) -> Option<(f32, f32)> {
        let track = f32::from(g.viewport().size.height) - SCROLLBAR;
        let max = self.max_scroll_y(g);
        if max <= 0. || track <= 0. {
            return None;
        }
        let total = max + track as f64;
        let len = ((track as f64 * track as f64 / total) as f32)
            .max(32.)
            .min(track);
        let off = ((self.scroll_y / max) as f32) * (track - len);
        Some((off, len))
    }

    pub fn h_thumb(&self, g: &Geom) -> Option<(f32, f32)> {
        let track = f32::from(g.viewport().size.width) - SCROLLBAR;
        let max = self.max_scroll_x(g);
        if max <= 0. || track <= 0. {
            return None;
        }
        let total = max + track;
        let len = (track * track / total).max(32.).min(track);
        let off = (self.scroll_x / max) * (track - len);
        Some((off, len))
    }

    pub fn hit(&self, g: &Geom, p: Point<Pixels>) -> Hit {
        let b = g.bounds;
        if !b.contains(&p) {
            return Hit::Outside;
        }
        let vp = g.viewport();
        let (x, y) = (f32::from(p.x - b.left()), f32::from(p.y - b.top()));
        // Scrollbars sit over the cells along the right and bottom edges.
        if p.x >= vp.right() - px(SCROLLBAR)
            && p.y >= vp.top()
            && let Some((off, len)) = self.v_thumb(g)
        {
            let ty = f32::from(p.y - vp.top());
            return if ty >= off && ty <= off + len {
                Hit::VThumb(ty - off)
            } else {
                Hit::VTrack
            };
        }
        if p.y >= vp.bottom() - px(SCROLLBAR)
            && p.x >= vp.left()
            && let Some((off, len)) = self.h_thumb(g)
        {
            let tx = f32::from(p.x - vp.left());
            return if tx >= off && tx <= off + len {
                Hit::HThumb(tx - off)
            } else {
                Hit::HTrack
            };
        }
        if y < g.head {
            if x < g.gutter {
                return Hit::Corner;
            }
            let cx = x - g.gutter + self.scroll_x;
            // Column edges: a few pixels either side of each boundary.
            if let Some(c) = self
                .col_x
                .iter()
                .enumerate()
                .skip(1)
                .find_map(|(i, &e)| ((cx - e).abs() <= 4.).then_some(i - 1))
            {
                return Hit::ColEdge(c);
            }
            return match self.col_at(cx) {
                Some(col) => {
                    let right = self.col_x[col + 1];
                    let button = y >= self.letter_h() && cx >= right - 26. * self.zoom;
                    Hit::ColHeader { col, button }
                }
                None => Hit::Outside,
            };
        }
        let ry = (y - g.head) as f64 + self.scroll_y;
        let row = (ry / g.row_h as f64).floor() as u64;
        if row >= self.view_len() {
            return Hit::Outside;
        }
        if x < g.gutter {
            return Hit::RowHeader(row);
        }
        match self.col_at(x - g.gutter + self.scroll_x) {
            Some(col) => Hit::Cell(row, col),
            None => Hit::Outside,
        }
    }

    /// Cell bounds in window coordinates.
    pub fn cell_bounds(&self, vr: u64, col: usize) -> Option<Bounds<Pixels>> {
        let g = self.geom.get();
        if col >= self.ncols {
            return None;
        }
        let vp = g.viewport();
        let y = (vr as f64 * g.row_h as f64 - self.scroll_y) as f32;
        let x = self.col_x[col] - self.scroll_x;
        Some(Bounds::new(
            point(vp.left() + px(x), vp.top() + px(y)),
            gpui::size(px(self.col_x[col + 1] - self.col_x[col]), px(g.row_h)),
        ))
    }

    // ── Selection ───────────────────────────────────────────────────────

    pub fn sel_rows(&self) -> Range<u64> {
        let (a, b) = (
            self.anchor.0.min(self.cursor.0),
            self.anchor.0.max(self.cursor.0),
        );
        a..b + 1
    }

    pub fn sel_cols(&self) -> Range<usize> {
        let (a, b) = (
            self.anchor.1.min(self.cursor.1),
            self.anchor.1.max(self.cursor.1),
        );
        a..b + 1
    }

    pub fn sel_cells(&self) -> u64 {
        let r = self.sel_rows();
        (r.end - r.start) * self.sel_cols().len() as u64
    }

    pub fn is_whole_cols(&self) -> bool {
        let r = self.sel_rows();
        r.start == 0 && r.end >= self.view_len() && self.view_len() > 1
    }

    pub fn is_whole_rows(&self) -> bool {
        let c = self.sel_cols();
        c.start == 0 && c.end >= self.ncols && self.ncols > 1
    }

    fn set_cursor(&mut self, row: u64, col: usize, extend: bool, cx: &mut Context<Self>) {
        let rows = self.view_len();
        let row = row.min(rows.saturating_sub(1));
        let col = col.min(self.ncols.saturating_sub(1));
        self.cursor = (row, col);
        if !extend {
            self.anchor = self.cursor;
        }
        self.context_menu = None;
        self.reveal(row, col);
        self.update_stats(cx);
        cx.emit(SheetEvent::Changed);
        cx.notify();
    }

    pub fn go_to(&mut self, row: u64, col: usize, cx: &mut Context<Self>) {
        self.commit_edit(cx);
        self.set_cursor(row, col, false, cx);
    }

    pub fn select_range(&mut self, a: (u64, usize), b: (u64, usize), cx: &mut Context<Self>) {
        self.anchor = a;
        self.set_cursor(b.0, b.1, true, cx);
    }

    /// Scrolls so the cell is visible.
    fn reveal(&mut self, row: u64, col: usize) {
        let g = self.geom.get();
        let vp = g.viewport();
        let (vw, vh) = (
            f32::from(vp.size.width) - SCROLLBAR,
            f32::from(vp.size.height) - SCROLLBAR,
        );
        if vh <= 0. {
            return;
        }
        let rh = self.row_h() as f64;
        let top = row as f64 * rh;
        if top < self.scroll_y {
            self.scroll_y = top;
        } else if top + rh > self.scroll_y + vh as f64 {
            self.scroll_y = top + rh - vh as f64;
        }
        if col + 1 < self.col_x.len() {
            let (l, r) = (self.col_x[col], self.col_x[col + 1]);
            if l < self.scroll_x {
                self.scroll_x = l;
            } else if r > self.scroll_x + vw {
                self.scroll_x = (r - vw).min(l);
            }
        }
        self.clamp_scroll();
    }

    fn page_rows(&self) -> u64 {
        let g = self.geom.get();
        ((f32::from(g.viewport().size.height) / self.row_h()).floor() as u64)
            .saturating_sub(1)
            .max(1)
    }

    fn step(&mut self, dr: i64, dc: i64, extend: bool, cx: &mut Context<Self>) {
        self.commit_edit(cx);
        let (r, c) = self.cursor;
        let r = (r as i64 + dr).clamp(0, self.view_len().saturating_sub(1) as i64) as u64;
        let c = (c as i64 + dc).clamp(0, self.ncols.saturating_sub(1) as i64) as usize;
        self.set_cursor(r, c, extend, cx);
    }

    fn jump(&mut self, dr: i64, dc: i64, extend: bool, cx: &mut Context<Self>) {
        self.commit_edit(cx);
        let (mut r, mut c) = self.cursor;
        if dr < 0 {
            r = 0;
        } else if dr > 0 {
            r = self.view_len().saturating_sub(1);
        }
        if dc < 0 {
            c = 0;
        } else if dc > 0 {
            c = self.ncols.saturating_sub(1);
        }
        self.set_cursor(r, c, extend, cx);
    }

    // ── Statistics ──────────────────────────────────────────────────────

    pub fn update_stats(&mut self, cx: &mut Context<Self>) {
        self.stats_gen += 1;
        let generation = self.stats_gen;
        if self.view_len() == 0 || self.ncols == 0 || self.sel_cells() <= 1 {
            self.stats = None;
            self._stats_task = None;
            return;
        }
        let data = self.data.clone();
        let first = self.first();
        let rows = self.sel_rows();
        let cols = self.sel_cols();
        let view = self.view_fn();
        let edits = self.edits.clone();
        if self.sel_cells() <= 20_000 {
            self.stats = query::stats(
                &data,
                first,
                rows,
                cols,
                &*view,
                &edits,
                &AtomicBool::new(false),
            );
            self._stats_task = None;
            return;
        }
        self.stats = None;
        let cancel = Arc::new(AtomicBool::new(false));
        let job = cx.background_spawn({
            let cancel = cancel.clone();
            async move { query::stats(&data, first, rows, cols, &*view, &edits, &cancel) }
        });
        self._stats_task = Some(cx.spawn(async move |this, cx| {
            let _cancel_on_drop = CancelOnDrop(cancel);
            let r = job.await;
            this.update(cx, |s, cx| {
                if s.stats_gen == generation {
                    s.stats = r;
                    cx.emit(SheetEvent::Changed);
                    cx.notify();
                }
            })
            .ok();
        }));
    }

    // ── Editing ─────────────────────────────────────────────────────────

    fn start_edit(&mut self, initial: Option<String>, window: &mut Window, cx: &mut Context<Self>) {
        if self.view_len() == 0 || self.ncols == 0 {
            return;
        }
        let (r, c) = self.cursor;
        self.reveal(r, c);
        let phys = self.phys_row(r);
        let current = self.cell_text(r, c);
        let input = cx.new(|cx| {
            let mut t = TextInput::new("", cx);
            match &initial {
                Some(s) => t.set_text(s.clone(), None, cx),
                None => t.set_text(current.replace(['\n', '\r'], " "), None, cx),
            }
            t
        });
        let sub = cx.subscribe(&input, |_, _, _: &TextInputEvent, cx| cx.notify());
        input.read(cx).focus_handle(cx).focus(window, cx);
        self.editing = Some(Editing {
            input,
            phys,
            col: c,
            _sub: sub,
        });
        cx.notify();
    }

    /// Saves the cell being edited, if any.
    pub fn commit_edit(&mut self, cx: &mut Context<Self>) {
        let Some(e) = self.editing.take() else {
            return;
        };
        let text = e.input.read(cx).text().to_string();
        self.apply_edits(vec![(e.phys, e.col, text)], cx);
    }

    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.take().is_some() {
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    /// Sets cells, recording one undo step.
    fn apply_edits(&mut self, cells: Vec<(u64, usize, String)>, cx: &mut Context<Self>) {
        let mut batch: EditBatch = Vec::new();
        let edits = Arc::make_mut(&mut self.edits);
        for (phys, col, text) in cells {
            let old = edits.cells.get(&(phys, col as u32)).cloned();
            let original = if phys < self.data.rows {
                self.data.cell(phys, col).into_owned()
            } else {
                Vec::new()
            };
            let new: Option<Arc<str>> = if text.as_bytes() == original.as_slice() {
                None
            } else {
                Some(text.into())
            };
            if old == new {
                continue;
            }
            set_edit(edits, phys, col, new.clone());
            batch.push((phys, col, old, new));
        }
        if !batch.is_empty() {
            self.undo.push(batch);
            self.redo.clear();
        }
        self.update_stats(cx);
        cx.emit(SheetEvent::Changed);
        cx.notify();
    }

    fn undo_redo(&mut self, undo: bool, cx: &mut Context<Self>) {
        self.commit_edit(cx);
        let Some(batch) = (if undo {
            self.undo.pop()
        } else {
            self.redo.pop()
        }) else {
            return;
        };
        let edits = Arc::make_mut(&mut self.edits);
        for (phys, col, old, new) in &batch {
            set_edit(
                edits,
                *phys,
                *col,
                if undo { old.clone() } else { new.clone() },
            );
        }
        if undo {
            self.redo.push(batch);
        } else {
            self.undo.push(batch);
        }
        self.update_stats(cx);
        cx.emit(SheetEvent::Changed);
        cx.notify();
    }

    fn clear_cells(&mut self, cx: &mut Context<Self>) {
        if self.sel_cells() > MAX_CELLS_EDIT {
            cx.emit(SheetEvent::Toast(format!(
                "Select at most {} cells to clear",
                value::count(MAX_CELLS_EDIT)
            )));
            return;
        }
        let mut cells = Vec::new();
        for vr in self.sel_rows() {
            let phys = self.phys_row(vr);
            for c in self.sel_cols() {
                cells.push((phys, c, String::new()));
            }
        }
        self.apply_edits(cells, cx);
    }

    fn copy(&mut self, cx: &mut Context<Self>) -> bool {
        let n = self.sel_cells();
        if n > MAX_CELLS_COPY {
            cx.emit(SheetEvent::Toast(format!(
                "Selection too large to copy ({} cells; the limit is {})",
                value::count(n),
                value::count(MAX_CELLS_COPY)
            )));
            return false;
        }
        let view = self.view_fn();
        let header = (self.is_whole_cols() && self.has_header).then(|| {
            self.header
                .iter()
                .map(|h| h.to_string())
                .collect::<Vec<_>>()
        });
        let text = query::copy_text(
            &self.data,
            self.first(),
            self.sel_rows(),
            self.sel_cols(),
            &*view,
            &self.edits,
            header.as_deref(),
        );
        cx.write_to_clipboard(ClipboardItem::new_string(text));
        if n > 1 {
            cx.emit(SheetEvent::Toast(format!(
                "Copied {} cells",
                value::count(n)
            )));
        }
        true
    }

    fn paste(&mut self, cx: &mut Context<Self>) {
        let Some(text) = cx.read_from_clipboard().and_then(|c| c.text()) else {
            return;
        };
        let text = text.strip_suffix('\n').unwrap_or(&text);
        let text = text.strip_suffix('\r').unwrap_or(text);
        let (r0, c0) = (self.sel_rows().start, self.sel_cols().start);
        let mut cells = Vec::new();
        for (i, line) in text.split('\n').enumerate() {
            let vr = r0 + i as u64;
            if vr >= self.view_len() {
                break;
            }
            let phys = self.phys_row(vr);
            for (j, v) in line.trim_end_matches('\r').split('\t').enumerate() {
                let c = c0 + j;
                if c >= self.ncols {
                    break;
                }
                cells.push((phys, c, v.to_string()));
            }
            if cells.len() as u64 > MAX_CELLS_EDIT {
                cx.emit(SheetEvent::Toast("Pasted the first 200,000 cells".into()));
                break;
            }
        }
        let rows = text.split('\n').count() as u64;
        let cols = text
            .split('\n')
            .map(|l| l.split('\t').count())
            .max()
            .unwrap_or(1);
        self.apply_edits(cells, cx);
        let end = (
            (r0 + rows.saturating_sub(1)).min(self.view_len().saturating_sub(1)),
            (c0 + cols.saturating_sub(1)).min(self.ncols.saturating_sub(1)),
        );
        self.select_range((r0, c0), end, cx);
    }

    // ── Column operations ───────────────────────────────────────────────

    pub fn autofit(&mut self, col: usize, window: &mut Window) {
        let font = window.text_style().font();
        let size = px(self.font_size());
        let g = self.geom.get();
        let first_row = (self.scroll_y / g.row_h.max(1.) as f64) as u64;
        let mut w: f32 = 0.;
        let measure = |window: &mut Window, s: &str, bold: bool| -> f32 {
            let s: String = s
                .chars()
                .take(200)
                .collect::<String>()
                .replace(['\n', '\r', '\t'], " ");
            if s.is_empty() {
                return 0.;
            }
            let mut f = font.clone();
            if bold {
                f.weight = gpui::FontWeight::SEMIBOLD;
            }
            let run = gpui::TextRun {
                len: s.len(),
                font: f,
                color: theme::text(),
                background_color: None,
                underline: None,
                strikethrough: None,
            };
            let line = window
                .text_system()
                .shape_line(s.into(), size, &[run], None);
            f32::from(line.width())
        };
        if self.has_header {
            let h = self
                .header
                .get(col)
                .map(|h| h.to_string())
                .unwrap_or_default();
            w = measure(window, &h, true) + 30. * self.zoom;
        }
        let end = (first_row + 300).min(self.view_len());
        let texts: Vec<String> = (first_row..end).map(|r| self.cell_text(r, col)).collect();
        for t in &texts {
            w = w.max(measure(window, t, false));
        }
        self.widths[col] = ((w / self.zoom) + 20.).clamp(MIN_COL_W, 900.);
        self.update_col_x();
    }

    pub fn open_menu(&mut self, col: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_edit(cx);
        self.context_menu = None;
        if self
            .menu
            .as_ref()
            .is_some_and(|(m, _)| m.read(cx).col == col)
        {
            self.menu = None;
            cx.notify();
            return;
        }
        let g = self.geom.get();
        let x = g.bounds.left() + px(g.gutter + self.col_x[col + 1] - self.scroll_x - 300.);
        let x = x.max(g.bounds.left() + px(g.gutter)).max(g.bounds.left());
        let pos = point(x, g.bounds.top() + px(g.head) + px(2.));
        let sheet = cx.entity().downgrade();
        let (data, first, query, edits, ncols) = self.rows_without_filter(col);
        let init = crate::column_menu::MenuInit {
            name: self.header.get(col).cloned().unwrap_or_default(),
            kind: self.types.get(col).copied().unwrap_or_default(),
            filters: self
                .query
                .filters
                .iter()
                .filter(|f| f.col == col)
                .map(|f| f.cond.clone())
                .collect(),
            data,
            first,
            query,
            edits,
            ncols,
        };
        let menu = cx.new(|cx| ColumnMenu::new(sheet, col, init, window, cx));
        self.menu = Some((menu, pos));
        cx.notify();
    }

    pub fn close_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.menu.take().is_some() {
            self.focus.focus(window, cx);
            cx.notify();
        }
    }

    /// Filter to (or exclude) the value of a cell.
    fn filter_by_cell(&mut self, row: u64, col: usize, exclude: bool, cx: &mut Context<Self>) {
        let v = self.cell_text(row, col);
        let set: rustc_hash::FxHashSet<Box<[u8]>> =
            [v.trim().as_bytes().into()].into_iter().collect();
        self.query.filters.push(ColFilter {
            col,
            cond: Cond::Values {
                set: Arc::new(set),
                exclude,
            },
        });
        self.run_query(cx);
    }

    // ── Mouse ───────────────────────────────────────────────────────────

    pub fn mouse_down(&mut self, e: &MouseDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        let g = self.geom.get();
        let hit = self.hit(&g, e.position);
        if hit == Hit::Outside {
            return;
        }
        if self.editing.is_some() {
            self.commit_edit(cx);
        }
        self.context_menu = None;
        self.focus.focus(window, cx);
        let shift = e.modifiers.shift;
        if e.button == MouseButton::Right {
            match hit {
                Hit::ColHeader { col, .. } => self.open_menu(col, window, cx),
                Hit::Cell(r, c) => {
                    if !(self.sel_rows().contains(&r) && self.sel_cols().contains(&c)) {
                        self.set_cursor(r, c, false, cx);
                    }
                    self.context_menu = Some(ContextMenu {
                        position: e.position,
                        row: r,
                        col: c,
                    });
                    cx.notify();
                }
                _ => {}
            }
            return;
        }
        if e.button != MouseButton::Left {
            return;
        }
        match hit {
            Hit::Corner => self.select_all(cx),
            Hit::ColEdge(col) => {
                if e.click_count >= 2 {
                    self.autofit(col, window);
                    self.drag = Drag::None;
                } else {
                    self.drag = Drag::Resize {
                        col,
                        start_x: f32::from(e.position.x),
                        start_w: self.widths[col],
                    };
                }
                cx.notify();
            }
            Hit::ColHeader { col, button } => {
                if button {
                    self.open_menu(col, window, cx);
                } else {
                    let last = self.view_len().saturating_sub(1);
                    if shift {
                        self.anchor = (0, self.anchor.1);
                    } else {
                        self.anchor = (0, col);
                    }
                    self.cursor = (last, col);
                    self.drag = Drag::Cols;
                    self.update_stats(cx);
                    cx.emit(SheetEvent::Changed);
                    cx.notify();
                }
            }
            Hit::RowHeader(row) => {
                let last = self.ncols.saturating_sub(1);
                if shift {
                    self.anchor = (self.anchor.0, 0);
                } else {
                    self.anchor = (row, 0);
                }
                self.cursor = (row, last);
                self.drag = Drag::Rows;
                self.update_stats(cx);
                cx.emit(SheetEvent::Changed);
                cx.notify();
            }
            Hit::Cell(r, c) => {
                if e.click_count >= 2 && !shift {
                    self.set_cursor(r, c, false, cx);
                    self.start_edit(None, window, cx);
                    return;
                }
                self.set_cursor(r, c, shift, cx);
                self.drag = Drag::Cells;
            }
            Hit::VThumb(grab) => self.drag = Drag::VScroll { grab },
            Hit::HThumb(grab) => self.drag = Drag::HScroll { grab },
            Hit::VTrack => {
                let page = f32::from(g.viewport().size.height) as f64 * 0.9;
                let (off, _) = self.v_thumb(&g).unwrap_or((0., 0.));
                let below = f32::from(e.position.y - g.viewport().top()) > off;
                self.scroll_y += if below { page } else { -page };
                self.clamp_scroll();
                cx.notify();
            }
            Hit::HTrack => {
                let page = f32::from(g.viewport().size.width) * 0.9;
                let (off, _) = self.h_thumb(&g).unwrap_or((0., 0.));
                let right = f32::from(e.position.x - g.viewport().left()) > off;
                self.scroll_x += if right { page } else { -page };
                self.clamp_scroll();
                cx.notify();
            }
            Hit::Outside => {}
        }
    }

    pub fn mouse_move(&mut self, e: &MouseMoveEvent, cx: &mut Context<Self>) {
        let g = self.geom.get();
        match self.drag {
            Drag::None => {
                let hit = self.hit(&g, e.position);
                // Only hover states that change the drawing matter.
                let hover = match hit {
                    Hit::ColEdge(_) | Hit::ColHeader { .. } | Hit::VThumb(_) | Hit::HThumb(_) => {
                        hit
                    }
                    _ => Hit::Outside,
                };
                let same = match (hover, self.hover) {
                    (Hit::VThumb(_), Hit::VThumb(_)) | (Hit::HThumb(_), Hit::HThumb(_)) => true,
                    (a, b) => a == b,
                };
                if !same {
                    self.hover = hover;
                    cx.notify();
                }
            }
            Drag::Resize {
                col,
                start_x,
                start_w,
            } => {
                let w = start_w + (f32::from(e.position.x) - start_x) / self.zoom;
                self.widths[col] = w.max(MIN_COL_W);
                self.update_col_x();
                cx.notify();
            }
            Drag::VScroll { grab } => {
                let vp = g.viewport();
                if let Some((_, len)) = self.v_thumb(&g) {
                    let track = f32::from(vp.size.height) - SCROLLBAR;
                    let top = f32::from(e.position.y - vp.top()) - grab;
                    let p = (top / (track - len).max(1.)).clamp(0., 1.) as f64;
                    self.scroll_y = p * self.max_scroll_y(&g);
                    cx.notify();
                }
            }
            Drag::HScroll { grab } => {
                let vp = g.viewport();
                if let Some((_, len)) = self.h_thumb(&g) {
                    let track = f32::from(vp.size.width) - SCROLLBAR;
                    let left = f32::from(e.position.x - vp.left()) - grab;
                    let p = (left / (track - len).max(1.)).clamp(0., 1.);
                    self.scroll_x = p * self.max_scroll_x(&g);
                    cx.notify();
                }
            }
            Drag::Cells | Drag::Cols | Drag::Rows => {
                if !e.dragging() {
                    self.drag = Drag::None;
                    return;
                }
                let vp = g.viewport();
                // Past an edge: scroll that way.
                let rh = g.row_h as f64;
                if e.position.y > vp.bottom() {
                    self.scroll_y += rh * (1. + f32::from(e.position.y - vp.bottom()) as f64 / 8.);
                } else if e.position.y < vp.top() && self.drag != Drag::Cols {
                    self.scroll_y -= rh * (1. + f32::from(vp.top() - e.position.y) as f64 / 8.);
                }
                if e.position.x > vp.right() {
                    self.scroll_x += 12. + f32::from(e.position.x - vp.right());
                } else if e.position.x < vp.left() && self.drag != Drag::Rows {
                    self.scroll_x -= 12. + f32::from(vp.left() - e.position.x);
                }
                self.clamp_scroll();
                let y = f32::from(e.position.y.clamp(vp.top(), vp.bottom() - px(1.)) - vp.top());
                let x = f32::from(e.position.x.clamp(vp.left(), vp.right() - px(1.)) - vp.left());
                let row = ((y as f64 + self.scroll_y) / rh).floor() as u64;
                let col = self
                    .col_at(x + self.scroll_x)
                    .unwrap_or(self.ncols.saturating_sub(1));
                let (row, col) = match self.drag {
                    Drag::Cols => (self.view_len().saturating_sub(1), col),
                    Drag::Rows => (row, self.ncols.saturating_sub(1)),
                    _ => (row, col),
                };
                let row = row.min(self.view_len().saturating_sub(1));
                if (row, col) != self.cursor {
                    self.cursor = (row, col);
                    cx.notify();
                }
                cx.notify();
            }
        }
    }

    pub fn mouse_up(&mut self, _: &MouseUpEvent, cx: &mut Context<Self>) {
        if self.drag != Drag::None {
            let was_select = matches!(self.drag, Drag::Cells | Drag::Cols | Drag::Rows);
            self.drag = Drag::None;
            if was_select {
                self.update_stats(cx);
                cx.emit(SheetEvent::Changed);
            }
            cx.notify();
        }
    }

    pub fn scroll(&mut self, e: &ScrollEvent, cx: &mut Context<Self>) {
        let d = e.delta.pixel_delta(px(self.row_h() * 1.5));
        let (mut dx, mut dy) = (f32::from(d.x), f32::from(d.y));
        if e.modifiers.shift && dx == 0. {
            dx = dy;
            dy = 0.;
        }
        let before = (self.scroll_x, self.scroll_y);
        self.scroll_y -= dy as f64;
        self.scroll_x -= dx;
        self.clamp_scroll();
        if before != (self.scroll_x, self.scroll_y) {
            self.context_menu = None;
            cx.notify();
        }
    }

    pub fn resizing(&self) -> bool {
        matches!(self.drag, Drag::Resize { .. })
    }

    // ── Actions ─────────────────────────────────────────────────────────

    pub fn select_all(&mut self, cx: &mut Context<Self>) {
        self.commit_edit(cx);
        self.anchor = (0, 0);
        self.cursor = (
            self.view_len().saturating_sub(1),
            self.ncols.saturating_sub(1),
        );
        self.update_stats(cx);
        cx.emit(SheetEvent::Changed);
        cx.notify();
    }

    fn on_key_down(&mut self, e: &KeyDownEvent, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.is_some() {
            return;
        }
        let m = e.keystroke.modifiers;
        if m.control || m.alt || m.platform {
            return;
        }
        let Some(ch) = e.keystroke.key_char.as_deref() else {
            return;
        };
        if ch.is_empty() || ch.chars().any(|c| c.is_control()) {
            return;
        }
        // Typing replaces the cell, as in a spreadsheet.
        self.start_edit(Some(ch.to_string()), window, cx);
        cx.stop_propagation();
    }

    fn render_editor(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let e = self.editing.as_ref()?;
        let vr = self.cursor.0;
        let b = self.cell_bounds(vr, e.col)?;
        let g = self.geom.get();
        let origin = b.origin - g.bounds.origin;
        let font = px(self.font_size());
        Some(
            div()
                .key_context("CellEditor")
                .absolute()
                .left(origin.x - px(1.))
                .top(origin.y - px(1.))
                .min_w(b.size.width + px(2.))
                .w(b.size.width.max(px(220.)))
                .h(b.size.height + px(2.))
                .px(px(7.))
                .flex()
                .items_center()
                .bg(theme::raised())
                .border_2()
                .border_color(theme::accent())
                .shadow_lg()
                .text_size(font)
                .text_color(theme::text())
                .on_action(cx.listener(|s, _: &CommitEdit, w, cx| {
                    s.commit_edit(cx);
                    s.focus.focus(w, cx);
                    s.step(1, 0, false, cx);
                }))
                .on_action(cx.listener(|s, _: &CommitEditRight, w, cx| {
                    s.commit_edit(cx);
                    s.focus.focus(w, cx);
                    s.step(0, 1, false, cx);
                }))
                .on_action(cx.listener(|s, _: &CancelEdit, w, cx| s.cancel_edit(w, cx)))
                .child(e.input.clone()),
        )
    }

    fn render_context_menu(&self, cx: &mut Context<Self>) -> Option<impl IntoElement + use<>> {
        let m = self.context_menu.as_ref()?;
        let (row, col) = (m.row, m.col);
        let value = self.cell_text(row, col);
        let short: String = {
            let v = value.trim().replace(['\n', '\r'], " ");
            if v.chars().count() > 24 {
                format!("{}…", v.chars().take(24).collect::<String>())
            } else if v.is_empty() {
                "(blank)".into()
            } else {
                v
            }
        };
        let numeric = self.types.get(col).is_some_and(|t| t.numeric());
        let name = self.header.get(col).cloned().unwrap_or_default();
        type Run = Box<dyn Fn(&mut Sheet, &mut Window, &mut Context<Sheet>)>;
        let mut items: Vec<(&'static str, String, &'static str, Run)> = vec![
            (
                "copy",
                "Copy".into(),
                "Ctrl+C",
                Box::new(|s, _, cx| {
                    s.copy(cx);
                }),
            ),
            (
                "clipboard",
                "Paste".into(),
                "Ctrl+V",
                Box::new(|s, _, cx| s.paste(cx)),
            ),
            (
                "x",
                "Clear cells".into(),
                "Del",
                Box::new(|s, _, cx| s.clear_cells(cx)),
            ),
        ];
        items.push(("", String::new(), "", Box::new(|_, _, _| {})));
        items.push((
            "filter",
            format!("Filter by “{short}”"),
            "",
            Box::new(move |s, _, cx| s.filter_by_cell(row, col, false, cx)),
        ));
        items.push((
            "filter-x",
            format!("Exclude “{short}”"),
            "",
            Box::new(move |s, _, cx| s.filter_by_cell(row, col, true, cx)),
        ));
        items.push((
            "sort-asc",
            format!(
                "Sort {} {}",
                name,
                if numeric { "smallest first" } else { "A → Z" }
            ),
            "",
            Box::new(move |s, _, cx| s.sort_by(col, false, cx)),
        ));
        items.push((
            "sort-desc",
            format!(
                "Sort {} {}",
                name,
                if numeric { "largest first" } else { "Z → A" }
            ),
            "",
            Box::new(move |s, _, cx| s.sort_by(col, true, cx)),
        ));
        if self.has_filter(col) {
            items.push((
                "filter-x",
                format!("Clear filter on {name}"),
                "",
                Box::new(move |s, _, cx| s.clear_filter(col, cx)),
            ));
        }
        items.push(("", String::new(), "", Box::new(|_, _, _| {})));
        items.push((
            "columns",
            "Fit column width".into(),
            "",
            Box::new(move |s, w, cx| {
                s.autofit(col, w);
                cx.notify();
            }),
        ));
        items.push((
            "sliders",
            "Column filter…".into(),
            "",
            Box::new(move |s, w, cx| s.open_menu(col, w, cx)),
        ));
        let items: Vec<_> = items
            .into_iter()
            .enumerate()
            .map(|(i, (icon, label, key, run))| {
                if icon.is_empty() {
                    return div()
                        .h(px(1.))
                        .my_1()
                        .bg(theme::border())
                        .into_any_element();
                }
                div()
                    .id(("ctx", i))
                    .h(px(30.))
                    .px_2()
                    .flex()
                    .items_center()
                    .gap_2()
                    .rounded_md()
                    .cursor_pointer()
                    .text_sm()
                    .text_color(theme::text())
                    .hover(|s| s.bg(theme::hover()))
                    .on_click(cx.listener(move |s, _, w, cx| {
                        s.context_menu = None;
                        run(s, w, cx);
                        cx.notify();
                    }))
                    .child(icon_el(icon, theme::muted(), 14.))
                    .child(div().flex_1().truncate().child(label))
                    .when(!key.is_empty(), |d| {
                        d.child(div().text_xs().text_color(theme::faint()).child(key))
                    })
                    .into_any_element()
            })
            .collect();
        Some(
            deferred(
                anchored()
                    .position(m.position)
                    .snap_to_window_with_margin(px(8.))
                    .child(
                        div()
                            .id("context-menu")
                            .occlude()
                            .w(px(280.))
                            .p_1()
                            .rounded_lg()
                            .bg(theme::raised())
                            .border_1()
                            .border_color(theme::border_strong())
                            .shadow_lg()
                            .flex()
                            .flex_col()
                            .on_mouse_down_out(cx.listener(|s, _, _, cx| {
                                s.context_menu = None;
                                cx.notify();
                            }))
                            .children(items),
                    ),
            )
            .with_priority(2),
        )
    }

    fn render_overlay(&self) -> Option<impl IntoElement + use<>> {
        let (icon, title, detail, color) = if let Some(e) = &self.error {
            (
                "alert",
                "Couldn't open this file".to_string(),
                e.clone(),
                theme::danger(),
            )
        } else if self.data.rows == 0 && self.loading.is_some() {
            (
                "spinner",
                "Opening…".to_string(),
                self.path.display().to_string(),
                theme::accent(),
            )
        } else if self.data.rows == 0 {
            (
                "table",
                "This file is empty".to_string(),
                String::new(),
                theme::muted(),
            )
        } else if self.view_len() == 0 && !self.query_running && self.is_filtered() {
            (
                "filter",
                "No rows match".to_string(),
                "Change or clear the filters to see rows again.".to_string(),
                theme::muted(),
            )
        } else {
            return None;
        };
        let g = self.geom.get();
        Some(
            div()
                .absolute()
                .left(px(g.gutter))
                .right_0()
                .top(px(g.head + 40.))
                .flex()
                .justify_center()
                .child(
                    div()
                        .max_w(px(520.))
                        .px_5()
                        .py_4()
                        .rounded_xl()
                        .bg(theme::card())
                        .border_1()
                        .border_color(theme::border())
                        .shadow_lg()
                        .flex()
                        .gap_3()
                        .child(icon_el(icon, color, 22.))
                        .child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_1()
                                .child(
                                    div()
                                        .text_sm()
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme::text())
                                        .child(title),
                                )
                                .when(!detail.is_empty(), |d| {
                                    d.child(
                                        div().text_xs().text_color(theme::muted()).child(detail),
                                    )
                                }),
                        ),
                ),
        )
    }
}

fn set_edit(edits: &mut Edits, phys: u64, col: usize, v: Option<Arc<str>>) {
    match v {
        Some(v) => edits.set(phys, col, v),
        None => {
            edits.cells.remove(&(phys, col as u32));
            if !edits.cells.keys().any(|(p, _)| *p == phys) {
                edits.rows.remove(&phys);
            }
        }
    }
}

struct CancelOnDrop(Arc<AtomicBool>);

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Relaxed);
    }
}

impl Render for Sheet {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.grow_columns();
        let menu = self.menu.as_ref().map(|(m, pos)| {
            deferred(
                anchored()
                    .position(*pos)
                    .snap_to_window_with_margin(px(8.))
                    .child(m.clone()),
            )
            .with_priority(2)
        });
        div()
            .id("sheet")
            .key_context("Sheet")
            .track_focus(&self.focus)
            .on_action(cx.listener(|s, _: &MoveUp, _, cx| s.step(-1, 0, false, cx)))
            .on_action(cx.listener(|s, _: &MoveDown, _, cx| s.step(1, 0, false, cx)))
            .on_action(cx.listener(|s, _: &MoveLeft, _, cx| s.step(0, -1, false, cx)))
            .on_action(cx.listener(|s, _: &MoveRight, _, cx| s.step(0, 1, false, cx)))
            .on_action(cx.listener(|s, _: &SelectUp, _, cx| s.step(-1, 0, true, cx)))
            .on_action(cx.listener(|s, _: &SelectDown, _, cx| s.step(1, 0, true, cx)))
            .on_action(cx.listener(|s, _: &SelectLeft, _, cx| s.step(0, -1, true, cx)))
            .on_action(cx.listener(|s, _: &SelectRight, _, cx| s.step(0, 1, true, cx)))
            .on_action(cx.listener(|s, _: &JumpUp, _, cx| s.jump(-1, 0, false, cx)))
            .on_action(cx.listener(|s, _: &JumpDown, _, cx| s.jump(1, 0, false, cx)))
            .on_action(cx.listener(|s, _: &JumpLeft, _, cx| s.jump(0, -1, false, cx)))
            .on_action(cx.listener(|s, _: &JumpRight, _, cx| s.jump(0, 1, false, cx)))
            .on_action(cx.listener(|s, _: &JumpSelectUp, _, cx| s.jump(-1, 0, true, cx)))
            .on_action(cx.listener(|s, _: &JumpSelectDown, _, cx| s.jump(1, 0, true, cx)))
            .on_action(cx.listener(|s, _: &JumpSelectLeft, _, cx| s.jump(0, -1, true, cx)))
            .on_action(cx.listener(|s, _: &JumpSelectRight, _, cx| s.jump(0, 1, true, cx)))
            .on_action(cx.listener(|s, _: &PageUp, _, cx| {
                let n = s.page_rows() as i64;
                s.scroll_y -= n as f64 * s.row_h() as f64;
                s.step(-n, 0, false, cx)
            }))
            .on_action(cx.listener(|s, _: &PageDown, _, cx| {
                let n = s.page_rows() as i64;
                s.scroll_y += n as f64 * s.row_h() as f64;
                s.clamp_scroll();
                s.step(n, 0, false, cx)
            }))
            .on_action(cx.listener(|s, _: &SelectPageUp, _, cx| {
                let n = s.page_rows() as i64;
                s.step(-n, 0, true, cx)
            }))
            .on_action(cx.listener(|s, _: &SelectPageDown, _, cx| {
                let n = s.page_rows() as i64;
                s.step(n, 0, true, cx)
            }))
            .on_action(cx.listener(|s, _: &RowStart, _, cx| s.jump(0, -1, false, cx)))
            .on_action(cx.listener(|s, _: &RowEnd, _, cx| s.jump(0, 1, false, cx)))
            .on_action(cx.listener(|s, _: &SheetStart, _, cx| s.go_to(0, 0, cx)))
            .on_action(cx.listener(|s, _: &SheetEnd, _, cx| {
                let (r, c) = (s.view_len().saturating_sub(1), s.ncols.saturating_sub(1));
                s.go_to(r, c, cx)
            }))
            .on_action(cx.listener(|s, _: &SelectAll, _, cx| s.select_all(cx)))
            .on_action(cx.listener(|s, _: &SelectColumn, _, cx| {
                let c = s.cursor.1;
                let a = s.anchor.1;
                s.anchor = (0, a);
                s.cursor = (s.view_len().saturating_sub(1), c);
                s.update_stats(cx);
                cx.emit(SheetEvent::Changed);
                cx.notify();
            }))
            .on_action(cx.listener(|s, _: &SelectRow, _, cx| {
                let (r, a) = (s.cursor.0, s.anchor.0);
                s.anchor = (a, 0);
                s.cursor = (r, s.ncols.saturating_sub(1));
                s.update_stats(cx);
                cx.emit(SheetEvent::Changed);
                cx.notify();
            }))
            .on_action(cx.listener(|s, _: &TabNext, _, cx| s.step(0, 1, false, cx)))
            .on_action(cx.listener(|s, _: &TabPrev, _, cx| s.step(0, -1, false, cx)))
            .on_action(cx.listener(|s, _: &Enter, _, cx| s.step(1, 0, false, cx)))
            .on_action(cx.listener(|s, _: &EditCell, w, cx| s.start_edit(None, w, cx)))
            .on_action(cx.listener(|s, _: &ClearCells, _, cx| s.clear_cells(cx)))
            .on_action(cx.listener(|s, _: &Copy, _, cx| {
                s.copy(cx);
            }))
            .on_action(cx.listener(|s, _: &Cut, _, cx| {
                if s.copy(cx) {
                    s.clear_cells(cx);
                }
            }))
            .on_action(cx.listener(|s, _: &Paste, _, cx| s.paste(cx)))
            .on_action(cx.listener(|s, _: &Undo, _, cx| s.undo_redo(true, cx)))
            .on_action(cx.listener(|s, _: &Redo, _, cx| s.undo_redo(false, cx)))
            .on_action(cx.listener(|s, _: &ClearFilters, _, cx| s.clear_all_filters(cx)))
            .on_action(cx.listener(|s, _: &Dismiss, w, cx| {
                if s.context_menu.take().is_some() || s.menu.is_some() {
                    s.close_menu(w, cx);
                    cx.notify();
                } else if s.sel_cells() > 1 {
                    let c = s.cursor;
                    s.set_cursor(c.0, c.1, false, cx);
                } else {
                    cx.propagate();
                }
            }))
            .on_key_down(cx.listener(Self::on_key_down))
            .relative()
            .size_full()
            .overflow_hidden()
            .bg(theme::bg())
            .child(Grid::new(cx.entity()))
            .children(self.render_overlay())
            .children(self.render_editor(cx))
            .children(menu)
            .children(self.render_context_menu(cx))
    }
}

#[cfg(feature = "snapshot")]
impl Sheet {
    pub fn debug_context_menu(&mut self, row: u64, col: usize, cx: &mut Context<Self>) {
        let b = self.cell_bounds(row, col).unwrap_or_default();
        self.context_menu = Some(ContextMenu {
            position: b.center(),
            row,
            col,
        });
        cx.notify();
    }

    pub fn debug_edit(&mut self, text: String, window: &mut Window, cx: &mut Context<Self>) {
        self.start_edit(Some(text), window, cx);
    }
}
