//! Filtering, sorting, distinct values, selection statistics and export,
//! all parallel over chunks.
//!
//! Rows are addressed two ways: *physical* rows count every line of the
//! source; *data* rows skip the header row (`first` physical rows). Views and
//! filter results hold data rows as `u32`.

use std::borrow::Cow;
use std::cmp::Ordering;
use std::io::Write as _;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::Relaxed};

use aho_corasick::{AhoCorasick, AhoCorasickBuilder, Input};
use rayon::prelude::*;
use regex::bytes::{Regex, RegexBuilder};
use rustc_hash::{FxHashMap, FxHashSet};

use super::value::parse_number;
use super::{CHUNK_ROWS, CHUNK_SHIFT, Data, RowFormat, csv};

// ── Edits ───────────────────────────────────────────────────────────────

/// Cell values changed by the user, layered over the source.
#[derive(Default, Clone)]
pub struct Edits {
    pub cells: FxHashMap<(u64, u32), Arc<str>>,
    pub rows: FxHashSet<u64>,
}

impl Edits {
    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
    pub fn get(&self, phys: u64, col: usize) -> Option<&Arc<str>> {
        if self.rows.contains(&phys) {
            self.cells.get(&(phys, col as u32))
        } else {
            None
        }
    }
    pub fn set(&mut self, phys: u64, col: usize, v: Arc<str>) {
        self.cells.insert((phys, col as u32), v);
        self.rows.insert(phys);
    }
}

/// A cell's current text, edits included.
pub fn cell_text<'a>(data: &'a Data, edits: &'a Edits, phys: u64, col: usize) -> Cow<'a, [u8]> {
    if let Some(v) = edits.get(phys, col) {
        return Cow::Borrowed(v.as_bytes());
    }
    if phys >= data.rows {
        return Cow::Borrowed(&[]);
    }
    data.cell(phys, col)
}

/// A row's fields (up to `ncols`) with edits applied.
fn row_with_edits<'a>(
    data: &'a Data,
    edits: &'a Edits,
    phys: u64,
    ncols: usize,
) -> Vec<Cow<'a, [u8]>> {
    let mut v = data.row_fields(phys, ncols);
    if edits.rows.contains(&phys) {
        for c in 0..ncols {
            if let Some(e) = edits.cells.get(&(phys, c as u32)) {
                if v.len() <= c {
                    v.resize(c + 1, Cow::Borrowed(&[]));
                }
                v[c] = Cow::Borrowed(e.as_bytes());
            }
        }
    }
    v
}

/// Trimmed cell values, for value-list filters.
pub type ValueSet = Arc<FxHashSet<Box<[u8]>>>;
/// Sort keys of one part of the rows: numbers with positions, and the rest.
type NumPart = (Vec<(u64, u32)>, Vec<u32>);
/// Lowercase text keys of one part of the rows: an arena and spans into it.
type TextPart = (Vec<u8>, Vec<(u32, u32)>);

// ── Conditions ──────────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TextOp {
    Contains,
    NotContains,
    Equals,
    NotEquals,
    StartsWith,
    EndsWith,
    Regex,
}

impl TextOp {
    pub const ALL: [TextOp; 7] = [
        TextOp::Contains,
        TextOp::NotContains,
        TextOp::Equals,
        TextOp::NotEquals,
        TextOp::StartsWith,
        TextOp::EndsWith,
        TextOp::Regex,
    ];
    pub fn label(self) -> &'static str {
        match self {
            TextOp::Contains => "contains",
            TextOp::NotContains => "does not contain",
            TextOp::Equals => "equals",
            TextOp::NotEquals => "does not equal",
            TextOp::StartsWith => "begins with",
            TextOp::EndsWith => "ends with",
            TextOp::Regex => "matches regex",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum NumOp {
    Eq,
    Ne,
    Gt,
    Ge,
    Lt,
    Le,
    Between,
}

impl NumOp {
    pub const ALL: [NumOp; 7] = [
        NumOp::Eq,
        NumOp::Ne,
        NumOp::Gt,
        NumOp::Ge,
        NumOp::Lt,
        NumOp::Le,
        NumOp::Between,
    ];
    pub fn label(self) -> &'static str {
        match self {
            NumOp::Eq => "=",
            NumOp::Ne => "≠",
            NumOp::Gt => ">",
            NumOp::Ge => "≥",
            NumOp::Lt => "<",
            NumOp::Le => "≤",
            NumOp::Between => "between",
        }
    }
}

#[derive(Clone, Debug)]
pub enum Cond {
    Text {
        op: TextOp,
        needle: String,
    },
    Num {
        op: NumOp,
        a: f64,
        b: f64,
    },
    /// The cell (trimmed) is one of these values, or with `exclude`, none
    /// of them.
    Values {
        set: Arc<FxHashSet<Box<[u8]>>>,
        exclude: bool,
    },
}

impl Cond {
    /// A short description for the filter chips.
    pub fn describe(&self) -> String {
        match self {
            Cond::Text { op, needle } => format!("{} “{}”", op.label(), needle),
            Cond::Num {
                op: NumOp::Between,
                a,
                b,
            } => format!(
                "between {} and {}",
                super::value::format_number(*a),
                super::value::format_number(*b)
            ),
            Cond::Num { op, a, .. } => {
                format!("{} {}", op.label(), super::value::format_number(*a))
            }
            Cond::Values { set, exclude } => {
                let (one, many) = if *exclude {
                    ("≠", "is none of")
                } else {
                    ("=", "is one of")
                };
                if set.len() == 1 {
                    let v = set.iter().next().unwrap();
                    if v.is_empty() {
                        if *exclude {
                            "is not blank".into()
                        } else {
                            "is blank".into()
                        }
                    } else {
                        format!("{one} “{}”", String::from_utf8_lossy(v))
                    }
                } else {
                    format!("{many} {} values", set.len())
                }
            }
        }
    }
}

#[derive(Clone, Debug)]
pub struct ColFilter {
    pub col: usize,
    pub cond: Cond,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SortSpec {
    pub col: usize,
    pub desc: bool,
    pub numeric: bool,
}

#[derive(Clone, Debug, Default)]
pub struct Query {
    pub search: String,
    pub filters: Vec<ColFilter>,
    pub sort: Option<SortSpec>,
}

impl Query {
    pub fn filters_rows(&self) -> bool {
        !self.search.trim().is_empty() || !self.filters.is_empty()
    }
    pub fn is_identity(&self) -> bool {
        !self.filters_rows() && self.sort.is_none()
    }
}

/// Case-insensitive text matching: ASCII-fast, with a lowercase fallback for
/// non-ASCII needles.
struct TextMatcher {
    op: TextOp,
    lower: Vec<u8>,
    ascii: bool,
    ac: Option<AhoCorasick>,
    re: Option<Regex>,
}

impl TextMatcher {
    fn new(op: TextOp, needle: &str) -> Result<Self, String> {
        let lower = needle.to_lowercase().into_bytes();
        let ascii = needle.is_ascii();
        let ac = if matches!(op, TextOp::Contains | TextOp::NotContains) && ascii {
            Some(
                AhoCorasickBuilder::new()
                    .ascii_case_insensitive(true)
                    .build([needle.as_bytes()])
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        let re = if op == TextOp::Regex {
            Some(
                RegexBuilder::new(needle)
                    .case_insensitive(true)
                    .size_limit(1 << 22)
                    .build()
                    .map_err(|e| e.to_string())?,
            )
        } else {
            None
        };
        Ok(TextMatcher {
            op,
            lower,
            ascii,
            ac,
            re,
        })
    }

    fn test(&self, cell: &[u8]) -> bool {
        if let Some(re) = &self.re {
            return re.is_match(cell);
        }
        let cell = cell.trim_ascii();
        if !self.ascii {
            let low = String::from_utf8_lossy(cell).to_lowercase();
            let c = low.as_bytes();
            let n = &self.lower[..];
            return match self.op {
                TextOp::Contains => memchr::memmem::find(c, n).is_some(),
                TextOp::NotContains => memchr::memmem::find(c, n).is_none(),
                TextOp::Equals => c == n,
                TextOp::NotEquals => c != n,
                TextOp::StartsWith => c.starts_with(n),
                TextOp::EndsWith => c.ends_with(n),
                TextOp::Regex => unreachable!(),
            };
        }
        let n = &self.lower[..];
        match self.op {
            TextOp::Contains => self.ac.as_ref().unwrap().is_match(cell),
            TextOp::NotContains => !self.ac.as_ref().unwrap().is_match(cell),
            TextOp::Equals => cell.eq_ignore_ascii_case(n),
            TextOp::NotEquals => !cell.eq_ignore_ascii_case(n),
            TextOp::StartsWith => cell.len() >= n.len() && cell[..n.len()].eq_ignore_ascii_case(n),
            TextOp::EndsWith => {
                cell.len() >= n.len() && cell[cell.len() - n.len()..].eq_ignore_ascii_case(n)
            }
            TextOp::Regex => unreachable!(),
        }
    }
}

enum Test {
    Text(TextMatcher),
    Num(NumOp, f64, f64),
    Values(Arc<FxHashSet<Box<[u8]>>>, bool),
}

impl Test {
    fn test(&self, cell: &[u8]) -> bool {
        match self {
            Test::Text(m) => m.test(cell),
            Test::Num(op, a, b) => {
                let Some(v) = parse_number(cell) else {
                    return *op == NumOp::Ne;
                };
                match op {
                    NumOp::Eq => v == *a,
                    NumOp::Ne => v != *a,
                    NumOp::Gt => v > *a,
                    NumOp::Ge => v >= *a,
                    NumOp::Lt => v < *a,
                    NumOp::Le => v <= *a,
                    NumOp::Between => v >= a.min(*b) && v <= a.max(*b),
                }
            }
            Test::Values(set, exclude) => set.contains(cell.trim_ascii()) != *exclude,
        }
    }
}

/// A query ready to run.
pub struct Compiled {
    search: Option<AhoCorasick>,
    search_lower: Option<String>,
    tests: Vec<(usize, Test)>,
    max_col: usize,
}

impl Compiled {
    pub fn new(q: &Query) -> Result<Compiled, String> {
        let s = q.search.trim();
        let (search, search_lower) = if s.is_empty() {
            (None, None)
        } else {
            let ac = AhoCorasickBuilder::new()
                .ascii_case_insensitive(true)
                .build([s.as_bytes()])
                .map_err(|e| e.to_string())?;
            (Some(ac), (!s.is_ascii()).then(|| s.to_lowercase()))
        };
        let mut tests = Vec::new();
        for f in &q.filters {
            let t = match &f.cond {
                Cond::Text { op, needle } => Test::Text(TextMatcher::new(*op, needle)?),
                Cond::Num { op, a, b } => Test::Num(*op, *a, *b),
                Cond::Values { set, exclude } => Test::Values(set.clone(), *exclude),
            };
            tests.push((f.col, t));
        }
        let max_col = tests.iter().map(|(c, _)| c + 1).max().unwrap_or(0);
        Ok(Compiled {
            search,
            search_lower,
            tests,
            max_col,
        })
    }

    fn row_passes_tests(&self, data: &Data, phys: u64) -> bool {
        if self.tests.is_empty() {
            return true;
        }
        let mut cells: [Cow<[u8]>; 16] = Default::default();
        let mut big: Vec<Cow<[u8]>>;
        let cells: &mut [Cow<[u8]>] = if self.max_col <= 16 {
            &mut cells[..self.max_col]
        } else {
            big = vec![Cow::Borrowed(&[][..]); self.max_col];
            &mut big[..]
        };
        data.for_each_field(phys, self.max_col, |i, v| cells[i] = v);
        self.tests.iter().all(|(c, t)| t.test(&cells[*c]))
    }

    /// The slow path for edited rows: fields with edits applied.
    fn row_passes_slow(&self, data: &Data, edits: &Edits, phys: u64, ncols: usize) -> bool {
        let fields = row_with_edits(data, edits, phys, ncols.max(self.max_col));
        let get = |c: usize| fields.get(c).map(|v| &v[..]).unwrap_or(&[]);
        if let Some(ac) = &self.search {
            let hit = fields.iter().any(|f| match &self.search_lower {
                Some(low) => String::from_utf8_lossy(f)
                    .to_lowercase()
                    .contains(low.as_str()),
                None => ac.is_match(&f[..]),
            });
            if !hit {
                return false;
            }
        }
        self.tests.iter().all(|(c, t)| t.test(get(*c)))
    }

    /// Data rows of chunk `ci` that pass.
    fn run_chunk(&self, data: &Data, first: u64, ci: usize, out: &mut Vec<u32>) {
        let (bytes, chunk) = data.chunk_bytes(ci);
        let base = (ci as u64) << CHUNK_SHIFT;
        let n = chunk.rows();
        let lo = first.saturating_sub(base).min(n as u64) as usize;
        let push = |out: &mut Vec<u32>, i: usize| {
            let phys = base + i as u64;
            if self.row_passes_tests(data, phys) {
                out.push((phys - first) as u32);
            }
        };
        match &self.search {
            None => {
                for i in lo..n {
                    push(out, i);
                }
            }
            Some(ac) => {
                let off = &chunk.offsets;
                let end = off.get(n) as usize;
                let mut pos = off.get(lo) as usize;
                if let Some(low) = &self.search_lower {
                    // Non-ASCII search: lowercase each row.
                    for i in lo..n {
                        let row = &bytes[off.get(i) as usize..off.get(i + 1) as usize];
                        if String::from_utf8_lossy(row)
                            .to_lowercase()
                            .contains(low.as_str())
                        {
                            push(out, i);
                        }
                    }
                    return;
                }
                while pos < end {
                    let Some(m) = ac.find(Input::new(&bytes[..end]).span(pos..end)) else {
                        break;
                    };
                    let i = off.row_at(m.start() as u64);
                    // A match spanning two rows only counts if it starts and
                    // ends inside one.
                    let row_end = off.get(i + 1) as usize;
                    if m.end() <= row_end && i >= lo {
                        push(out, i);
                    }
                    pos = row_end.max(m.start() + 1);
                }
            }
        }
    }
}

/// The result of running a query: data rows in view order.
pub struct QueryResult {
    pub rows: Vec<u32>,
    pub elapsed: std::time::Duration,
}

/// Runs `q` over all data rows. `None` when cancelled.
pub fn run(
    data: &Data,
    first: u64,
    q: &Query,
    edits: &Edits,
    ncols: usize,
    cancel: &AtomicBool,
) -> Result<Option<QueryResult>, String> {
    let t0 = std::time::Instant::now();
    let compiled = Compiled::new(q)?;
    let data_rows = data.rows.saturating_sub(first);
    let mut rows: Vec<u32> = if q.filters_rows() {
        let parts: Vec<Vec<u32>> = (0..data.chunks.len())
            .into_par_iter()
            .map(|ci| {
                let mut v = Vec::new();
                if !cancel.load(Relaxed) {
                    compiled.run_chunk(data, first, ci, &mut v);
                }
                v
            })
            .collect();
        if cancel.load(Relaxed) {
            return Ok(None);
        }
        let mut rows = Vec::with_capacity(parts.iter().map(Vec::len).sum());
        for p in parts {
            rows.extend_from_slice(&p);
        }
        // Edited rows: re-check with their edits.
        if !edits.is_empty() {
            let mut fix: Vec<u32> = edits
                .rows
                .iter()
                .filter(|&&p| p >= first && p < data.rows)
                .map(|&p| (p - first) as u32)
                .collect();
            fix.sort_unstable();
            let edited: FxHashSet<u32> = fix.iter().copied().collect();
            rows.retain(|r| !edited.contains(r));
            for r in fix {
                if compiled.row_passes_slow(data, edits, r as u64 + first, ncols) {
                    rows.push(r);
                }
            }
            rows.par_sort_unstable();
        }
        rows
    } else {
        (0..data_rows as u32).collect()
    };
    if let Some(s) = q.sort
        && !sort_rows(data, first, &mut rows, s, edits, cancel)
    {
        return Ok(None);
    }
    Ok(Some(QueryResult {
        rows,
        elapsed: t0.elapsed(),
    }))
}

// ── Sorting ─────────────────────────────────────────────────────────────

fn lower_key(v: &[u8], out: &mut Vec<u8>) {
    let v = v.trim_ascii();
    if v.is_ascii() {
        out.extend(v.iter().map(u8::to_ascii_lowercase));
    } else {
        out.extend_from_slice(String::from_utf8_lossy(v).to_lowercase().as_bytes());
    }
}

/// An f64 as a u64 with the same order.
#[inline]
fn f64_key(v: f64) -> u64 {
    let b = v.to_bits();
    if b >> 63 == 1 { !b } else { b | 1 << 63 }
}

/// Eight bytes of `k` starting at `at`, zero padded, big-endian.
#[inline]
fn chunk_at(k: &[u8], at: usize) -> u64 {
    let mut p = [0u8; 8];
    if at < k.len() {
        let n = (k.len() - at).min(8);
        p[..n].copy_from_slice(&k[at..at + n]);
    }
    u64::from_be_bytes(p)
}

/// Sorts `(key chunk, index)` pairs, then each run of equal chunks by the
/// next eight bytes, and so on: an MSD radix sort over 8-byte digits where
/// each digit is an integer sort. Ties keep index order.
fn sort_text_level<'k>(
    items: &mut [(u64, u32)],
    depth: usize,
    key: &(dyn Fn(u32) -> &'k [u8] + Sync),
    desc: bool,
) {
    if depth > 0 {
        for it in items.iter_mut() {
            let c = chunk_at(key(it.1), depth * 8);
            it.0 = if desc { !c } else { c };
        }
    }
    if items.len() > 50_000 {
        items.par_sort_unstable();
    } else {
        items.sort_unstable();
    }
    let next = (depth + 1) * 8;
    let deeper = |run: &mut [(u64, u32)]| {
        if run.len() > 1 && run.iter().any(|it| key(it.1).len() > next) {
            sort_text_level(run, depth + 1, key, desc);
        }
    };
    if items.len() > 50_000 {
        items.par_chunk_by_mut(|a, b| a.0 == b.0).for_each(deeper);
    } else {
        items.chunk_by_mut(|a, b| a.0 == b.0).for_each(deeper);
    }
}

/// Sorts data rows by one column. Empty cells (and, for numeric sorts, cells
/// that aren't numbers) go last either way; ties keep view order.
pub fn sort_rows(
    data: &Data,
    first: u64,
    rows: &mut Vec<u32>,
    s: SortSpec,
    edits: &Edits,
    cancel: &AtomicBool,
) -> bool {
    const PART: usize = 1 << 16;
    if s.numeric {
        // (key, position) for numbers; positions of the rest, in order.
        let parts: Vec<NumPart> = rows
            .par_chunks(PART)
            .enumerate()
            .map(|(pi, part)| {
                let mut keyed = Vec::with_capacity(part.len());
                let mut rest = Vec::new();
                for (j, &r) in part.iter().enumerate() {
                    let pos = (pi * PART + j) as u32;
                    let v = cell_text(data, edits, r as u64 + first, s.col);
                    match parse_number(&v) {
                        Some(x) if !x.is_nan() => {
                            let k = f64_key(x);
                            keyed.push((if s.desc { !k } else { k }, pos));
                        }
                        _ => rest.push(pos),
                    }
                }
                (keyed, rest)
            })
            .collect();
        if cancel.load(Relaxed) {
            return false;
        }
        let mut keyed: Vec<(u64, u32)> = Vec::with_capacity(rows.len());
        let mut rest: Vec<u32> = Vec::new();
        for (k, r) in parts {
            keyed.extend_from_slice(&k);
            rest.extend_from_slice(&r);
        }
        keyed.par_sort_unstable();
        let sorted: Vec<u32> = keyed
            .iter()
            .map(|&(_, pos)| rows[pos as usize])
            .chain(rest.iter().map(|&pos| rows[pos as usize]))
            .collect();
        *rows = sorted;
        return true;
    }
    // Text: lowercase keys in per-part arenas.
    let parts: Vec<TextPart> = rows
        .par_chunks(PART)
        .map(|part| {
            let mut arena = Vec::new();
            let mut spans = Vec::with_capacity(part.len());
            for &r in part {
                let v = cell_text(data, edits, r as u64 + first, s.col);
                let at = arena.len();
                lower_key(&v, &mut arena);
                spans.push((at as u32, (arena.len() - at) as u32));
            }
            (arena, spans)
        })
        .collect();
    if cancel.load(Relaxed) {
        return false;
    }
    let key = |i: u32| -> &[u8] {
        let i = i as usize;
        let (arena, spans) = &parts[i / PART];
        let (at, len) = spans[i % PART];
        &arena[at as usize..(at + len) as usize]
    };
    let mut items: Vec<(u64, u32)> = Vec::with_capacity(rows.len());
    let mut empty: Vec<u32> = Vec::new();
    for i in 0..rows.len() as u32 {
        let k = key(i);
        if k.is_empty() {
            empty.push(i);
        } else {
            let c = chunk_at(k, 0);
            items.push((if s.desc { !c } else { c }, i));
        }
    }
    sort_text_level(&mut items, 0, &key, s.desc);
    if cancel.load(Relaxed) {
        return false;
    }
    let sorted: Vec<u32> = items
        .iter()
        .map(|&(_, i)| rows[i as usize])
        .chain(empty.iter().map(|&i| rows[i as usize]))
        .collect();
    *rows = sorted;
    true
}

// ── Distinct values ─────────────────────────────────────────────────────

pub struct Distinct {
    /// (value, count), in display order.
    pub values: Vec<(Box<[u8]>, u64)>,
    /// More distinct values exist than were counted.
    pub truncated: bool,
}

const DISTINCT_CAP: usize = 100_000;
const DISTINCT_SHOW: usize = 10_000;

/// Distinct (trimmed) values of `col` over `rows` (all data rows if `None`).
pub fn distinct(
    data: &Data,
    first: u64,
    rows: Option<&[u32]>,
    col: usize,
    numeric: bool,
    edits: &Edits,
    cancel: &AtomicBool,
) -> Option<Distinct> {
    type Map = FxHashMap<Box<[u8]>, u64>;
    let add = |m: &mut Map, full: &mut bool, phys: u64| {
        let v = cell_text(data, edits, phys, col);
        let v = v.trim_ascii();
        if let Some(c) = m.get_mut(v) {
            *c += 1;
        } else if m.len() < DISTINCT_CAP {
            m.insert(v.into(), 1);
        } else {
            *full = true;
        }
    };
    let maps: Vec<(Map, bool)> = match rows {
        Some(rows) => rows
            .par_chunks(CHUNK_ROWS)
            .map(|part| {
                let mut m = Map::default();
                let mut full = false;
                if !cancel.load(Relaxed) {
                    for &r in part {
                        add(&mut m, &mut full, r as u64 + first);
                    }
                }
                (m, full)
            })
            .collect(),
        None => (0..data.chunks.len())
            .into_par_iter()
            .map(|ci| {
                let mut m = Map::default();
                let mut full = false;
                if !cancel.load(Relaxed) {
                    let base = (ci as u64) << CHUNK_SHIFT;
                    let n = data.chunks[ci].rows() as u64;
                    for phys in base.max(first)..base + n {
                        add(&mut m, &mut full, phys);
                    }
                }
                (m, full)
            })
            .collect(),
    };
    if cancel.load(Relaxed) {
        return None;
    }
    let mut truncated = false;
    let mut total = Map::default();
    for (m, full) in maps {
        truncated |= full;
        for (k, c) in m {
            if let Some(t) = total.get_mut(&k) {
                *t += c;
            } else if total.len() < DISTINCT_CAP {
                total.insert(k, c);
            } else {
                truncated = true;
            }
        }
    }
    let mut values: Vec<(Box<[u8]>, u64)> = total.into_iter().collect();
    if values.len() > DISTINCT_SHOW {
        values.par_sort_unstable_by(|a, b| b.1.cmp(&a.1));
        values.truncate(DISTINCT_SHOW);
        truncated = true;
    }
    sort_values(&mut values, numeric);
    Some(Distinct { values, truncated })
}

/// Blanks first, then numbers or case-insensitive text.
pub fn sort_values(values: &mut [(Box<[u8]>, u64)], numeric: bool) {
    values.sort_by(|a, b| {
        match (a.0.is_empty(), b.0.is_empty()) {
            (true, false) => return Ordering::Less,
            (false, true) => return Ordering::Greater,
            _ => {}
        }
        if numeric {
            match (parse_number(&a.0), parse_number(&b.0)) {
                (Some(x), Some(y)) => return x.partial_cmp(&y).unwrap_or(Ordering::Equal),
                (Some(_), None) => return Ordering::Less,
                (None, Some(_)) => return Ordering::Greater,
                _ => {}
            }
        }
        let mut ka = Vec::new();
        let mut kb = Vec::new();
        lower_key(&a.0, &mut ka);
        lower_key(&b.0, &mut kb);
        ka.cmp(&kb).then_with(|| a.0.cmp(&b.0))
    });
}

// ── Selection statistics ────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    pub cells: u64,
    pub count: u64,
    pub nums: u64,
    pub sum: f64,
    pub min: f64,
    pub max: f64,
}

impl Stats {
    fn merge(mut self, o: Stats) -> Stats {
        if o.nums > 0 {
            if self.nums == 0 {
                self.min = o.min;
                self.max = o.max;
            } else {
                self.min = self.min.min(o.min);
                self.max = self.max.max(o.max);
            }
        }
        self.cells += o.cells;
        self.count += o.count;
        self.nums += o.nums;
        self.sum += o.sum;
        self
    }
    pub fn avg(&self) -> f64 {
        self.sum / self.nums.max(1) as f64
    }
}

/// Count, sum, min and max over view rows `rows` and columns `cols`.
/// `view(i)` maps a view row to a data row.
pub fn stats(
    data: &Data,
    first: u64,
    rows: std::ops::Range<u64>,
    cols: std::ops::Range<usize>,
    view: &(dyn Fn(u64) -> u64 + Sync),
    edits: &Edits,
    cancel: &AtomicBool,
) -> Option<Stats> {
    let n = rows.end - rows.start;
    let parts = n.div_ceil(16_384);
    let s = (0..parts)
        .into_par_iter()
        .map(|p| {
            let mut s = Stats::default();
            if cancel.load(Relaxed) {
                return s;
            }
            let lo = rows.start + p * 16_384;
            let hi = (lo + 16_384).min(rows.end);
            for vr in lo..hi {
                let phys = view(vr) + first;
                if phys >= data.rows {
                    continue;
                }
                let edited = edits.rows.contains(&phys);
                let mut one = |c: usize, v: &[u8]| {
                    if !cols.contains(&c) {
                        return;
                    }
                    let v = if edited {
                        edits
                            .cells
                            .get(&(phys, c as u32))
                            .map_or(v, |e| e.as_bytes())
                    } else {
                        v
                    };
                    if v.trim_ascii().is_empty() {
                        return;
                    }
                    s.count += 1;
                    if let Some(x) = parse_number(v) {
                        if s.nums == 0 {
                            s.min = x;
                            s.max = x;
                        } else {
                            s.min = s.min.min(x);
                            s.max = s.max.max(x);
                        }
                        s.nums += 1;
                        s.sum += x;
                    }
                };
                let mut seen = 0usize;
                data.for_each_field(phys, cols.end, |c, v| {
                    seen = c + 1;
                    one(c, &v);
                });
                if edited {
                    for c in seen.max(cols.start)..cols.end {
                        one(c, &[]);
                    }
                }
                s.cells += (cols.end - cols.start) as u64;
            }
            s
        })
        .reduce(Stats::default, Stats::merge);
    (!cancel.load(Relaxed)).then_some(s)
}

// ── Export ──────────────────────────────────────────────────────────────

/// Writes the header and `count` view rows as CSV. `progress` counts rows
/// written. Rows of a comma-separated source without edits are copied as is.
#[allow(clippy::too_many_arguments)]
pub fn export_csv(
    path: &Path,
    data: &Data,
    first: u64,
    header: Option<&[String]>,
    count: u64,
    view: &(dyn Fn(u64) -> u64 + Sync),
    ncols: usize,
    edits: &Edits,
    progress: &AtomicU64,
    cancel: &AtomicBool,
) -> anyhow::Result<u64> {
    let tmp = path.with_extension("rowscope-tmp");
    let file = std::fs::File::create(&tmp)?;
    let mut w = std::io::BufWriter::with_capacity(4 << 20, file);
    let mut line = Vec::with_capacity(4096);
    if let Some(h) = header {
        for (i, name) in h.iter().enumerate().take(ncols) {
            if i > 0 {
                line.push(b',');
            }
            csv::write_field(&mut line, name.as_bytes(), b',');
        }
        line.push(b'\n');
        w.write_all(&line)?;
    }
    let raw_ok = data.format == RowFormat::Delimited { delim: b',' };
    // Encode blocks of rows in parallel, write them in order.
    const BLOCK: u64 = 32_768;
    let mut at = 0u64;
    while at < count {
        if cancel.load(Relaxed) {
            drop(w);
            let _ = std::fs::remove_file(&tmp);
            return Err(anyhow::anyhow!("cancelled"));
        }
        let hi = (at + BLOCK * rayon::current_num_threads() as u64).min(count);
        let blocks: Vec<Vec<u8>> = (at..hi)
            .step_by(BLOCK as usize)
            .collect::<Vec<_>>()
            .into_par_iter()
            .map(|lo| {
                let mut out = Vec::with_capacity(1 << 20);
                for vr in lo..(lo + BLOCK).min(hi) {
                    let phys = view(vr) + first;
                    if raw_ok && !edits.rows.contains(&phys) {
                        out.extend_from_slice(data.row_bytes(phys));
                    } else {
                        let fields = row_with_edits(data, edits, phys, ncols);
                        for (i, f) in fields.iter().enumerate() {
                            if i > 0 {
                                out.push(b',');
                            }
                            csv::write_field(&mut out, f, b',');
                        }
                    }
                    out.push(b'\n');
                }
                out
            })
            .collect();
        for b in blocks {
            w.write_all(&b)?;
        }
        progress.store(hi, Relaxed);
        at = hi;
    }
    w.flush()?;
    drop(w);
    std::fs::rename(&tmp, path)?;
    Ok(count)
}

/// Tab-separated text of a block of cells, for the clipboard.
pub fn copy_text(
    data: &Data,
    first: u64,
    rows: std::ops::Range<u64>,
    cols: std::ops::Range<usize>,
    view: &dyn Fn(u64) -> u64,
    edits: &Edits,
    header: Option<&[String]>,
) -> String {
    let mut out = String::new();
    let clean = |s: &str| s.replace(['\t', '\n', '\r'], " ");
    if let Some(h) = header {
        for c in cols.clone() {
            if c > cols.start {
                out.push('\t');
            }
            out.push_str(&clean(h.get(c).map(String::as_str).unwrap_or("")));
        }
        out.push('\n');
    }
    for vr in rows {
        let phys = view(vr) + first;
        let fields = row_with_edits(data, edits, phys, cols.end);
        for c in cols.clone() {
            if c > cols.start {
                out.push('\t');
            }
            if let Some(f) = fields.get(c) {
                out.push_str(&clean(&String::from_utf8_lossy(f)));
            }
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::{Data, RowFormat};
    use super::*;

    /// A packed table from rows of cells.
    fn table(rows: &[&[&str]]) -> Data {
        let mut buf = Vec::new();
        let mut starts = vec![0u64];
        for r in rows {
            buf.extend_from_slice(r.join("\u{1f}").as_bytes());
            starts.push(buf.len() as u64);
        }
        Data {
            map: None,
            format: RowFormat::Packed,
            chunks: Arc::new(vec![Arc::new(super::super::Chunk {
                offsets: super::super::Offsets::build(&starts),
                owned: Some(buf),
            })]),
            rows: rows.len() as u64,
        }
    }

    fn sample() -> Data {
        table(&[
            &["name", "city", "amount"],
            &["Ann", "New York", "10"],
            &["bob", "Boston", "5"],
            &["Cid", "new york", "7.5"],
            &["Dee", "", "abc"],
            &["Eve", "Austin", "-2"],
        ])
    }

    fn q(search: &str, filters: Vec<ColFilter>, sort: Option<SortSpec>) -> Vec<u32> {
        let d = sample();
        run(
            &d,
            1,
            &Query {
                search: search.into(),
                filters,
                sort,
            },
            &Edits::default(),
            3,
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap()
        .rows
    }

    #[test]
    fn search_is_case_insensitive() {
        assert_eq!(q("NEW YORK", vec![], None), vec![0, 2]);
        assert_eq!(q("bo", vec![], None), vec![1]);
        assert_eq!(q("zzz", vec![], None), Vec::<u32>::new());
    }

    #[test]
    fn column_filters() {
        let f = |col, cond| vec![ColFilter { col, cond }];
        assert_eq!(
            q(
                "",
                f(
                    2,
                    Cond::Num {
                        op: NumOp::Gt,
                        a: 6.0,
                        b: 0.0
                    }
                ),
                None
            ),
            vec![0, 2]
        );
        assert_eq!(
            q(
                "",
                f(
                    1,
                    Cond::Text {
                        op: TextOp::StartsWith,
                        needle: "new".into()
                    }
                ),
                None
            ),
            vec![0, 2]
        );
        let blank: FxHashSet<Box<[u8]>> = [b"".as_slice().into()].into_iter().collect();
        assert_eq!(
            q(
                "",
                f(
                    1,
                    Cond::Values {
                        set: Arc::new(blank),
                        exclude: false
                    }
                ),
                None
            ),
            vec![3]
        );
        let set: FxHashSet<Box<[u8]>> = [b"Boston".as_slice().into(), b"Austin".as_slice().into()]
            .into_iter()
            .collect();
        let set = Arc::new(set);
        assert_eq!(
            q(
                "",
                f(
                    1,
                    Cond::Values {
                        set: set.clone(),
                        exclude: false
                    }
                ),
                None
            ),
            vec![1, 4]
        );
        assert_eq!(
            q("", f(1, Cond::Values { set, exclude: true }), None),
            vec![0, 2, 3]
        );
        assert_eq!(
            q(
                "",
                f(
                    0,
                    Cond::Text {
                        op: TextOp::Regex,
                        needle: "^[a-c]".into()
                    }
                ),
                None
            ),
            vec![0, 1, 2]
        );
    }

    #[test]
    fn number_keys_keep_order() {
        let v = [-1e9, -2.5, -0.0, 0.0, 1e-9, 3.0, 1e300];
        for w in v.windows(2) {
            assert!(f64_key(w[0]) <= f64_key(w[1]), "{w:?}");
        }
    }

    #[test]
    fn long_text_keys_sort_fully() {
        let d = table(&[
            &["k"],
            &["abcdefghZZ"],
            &["abcdefgh"],
            &["abcdefghAA"],
            &["abcdefghijklmnopB"],
            &["abcdefghijklmnopA"],
            &[""],
            &["ABCDEFGHAA"],
        ]);
        let mut rows: Vec<u32> = (0..7).collect();
        let spec = SortSpec {
            col: 0,
            desc: false,
            numeric: false,
        };
        sort_rows(
            &d,
            1,
            &mut rows,
            spec,
            &Edits::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(rows, vec![1, 2, 6, 4, 3, 0, 5]);
        let mut rows: Vec<u32> = (0..7).collect();
        let spec = SortSpec {
            col: 0,
            desc: true,
            numeric: false,
        };
        sort_rows(
            &d,
            1,
            &mut rows,
            spec,
            &Edits::default(),
            &AtomicBool::new(false),
        );
        assert_eq!(rows, vec![0, 3, 4, 2, 6, 1, 5]);
    }

    #[test]
    fn sorting() {
        let s = |col, desc, numeric| Some(SortSpec { col, desc, numeric });
        assert_eq!(q("", vec![], s(2, false, true)), vec![4, 1, 2, 0, 3]);
        assert_eq!(q("", vec![], s(2, true, true)), vec![0, 2, 1, 4, 3]);
        assert_eq!(q("", vec![], s(0, false, false)), vec![0, 1, 2, 3, 4]);
        assert_eq!(q("", vec![], s(0, true, false)), vec![4, 3, 2, 1, 0]);
        // Empty city last both ways.
        assert_eq!(q("", vec![], s(1, false, false)), vec![4, 1, 0, 2, 3]);
        assert_eq!(q("", vec![], s(1, true, false))[4], 3);
    }

    #[test]
    fn edits_are_filtered() {
        let d = sample();
        let mut e = Edits::default();
        e.set(2, 1, "Paris".into());
        let r = run(
            &d,
            1,
            &Query {
                search: "paris".into(),
                ..Default::default()
            },
            &e,
            3,
            &AtomicBool::new(false),
        )
        .unwrap()
        .unwrap();
        assert_eq!(r.rows, vec![1]);
    }

    #[test]
    fn distinct_values() {
        let d = sample();
        let v = distinct(
            &d,
            1,
            None,
            1,
            false,
            &Edits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        let names: Vec<String> = v
            .values
            .iter()
            .map(|(k, _)| String::from_utf8_lossy(k).into())
            .collect();
        assert_eq!(names, ["", "Austin", "Boston", "New York", "new york"]);
    }

    #[test]
    fn selection_stats() {
        let d = sample();
        let s = stats(
            &d,
            1,
            0..5,
            2..3,
            &|r| r,
            &Edits::default(),
            &AtomicBool::new(false),
        )
        .unwrap();
        assert_eq!(s.count, 5);
        assert_eq!(s.nums, 4);
        assert_eq!(s.sum, 20.5);
        assert_eq!(s.min, -2.0);
        assert_eq!(s.max, 10.0);
    }
}
