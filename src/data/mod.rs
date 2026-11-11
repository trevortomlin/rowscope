//! The table storage: rows held as byte ranges in fixed-size chunks.
//!
//! A CSV file is memory-mapped and only its row offsets are stored (4 bytes a
//! row); cells are parsed when they are drawn, filtered or sorted. Workbooks
//! (xlsx, xls, ods) are converted once into the same layout, with the fields
//! of a row packed into a buffer and separated by 0x1F.
//!
//! Chunks are immutable and shared (`Arc`), so a [`Data`] snapshot is cheap to
//! clone and hand to background jobs while loading keeps adding rows.

pub mod csv;
pub mod query;
pub mod value;
pub mod workbook;

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use memmap2::Mmap;

pub const CHUNK_SHIFT: u32 = 16;
pub const CHUNK_ROWS: usize = 1 << CHUNK_SHIFT;
const CHUNK_MASK: u64 = CHUNK_ROWS as u64 - 1;

/// Separates the fields of a packed (workbook) row.
pub const PACKED_SEP: u8 = 0x1F;

/// Row start offsets of one chunk, plus the end of its last row.
pub enum Offsets {
    /// Offsets relative to `base` (the common case: 4 bytes a row).
    Narrow { base: u64, deltas: Vec<u32> },
    /// For chunks spanning more than 4 GiB.
    Wide(Vec<u64>),
}

impl Offsets {
    pub fn build(starts: &[u64]) -> Offsets {
        let base = starts.first().copied().unwrap_or(0);
        let last = starts.last().copied().unwrap_or(0);
        if last - base <= u32::MAX as u64 {
            Offsets::Narrow {
                base,
                deltas: starts.iter().map(|&s| (s - base) as u32).collect(),
            }
        } else {
            Offsets::Wide(starts.to_vec())
        }
    }

    #[inline]
    pub fn get(&self, i: usize) -> u64 {
        match self {
            Offsets::Narrow { base, deltas } => base + deltas[i] as u64,
            Offsets::Wide(v) => v[i],
        }
    }

    /// Entries, i.e. rows + 1.
    pub fn len(&self) -> usize {
        match self {
            Offsets::Narrow { deltas, .. } => deltas.len(),
            Offsets::Wide(v) => v.len(),
        }
    }

    /// The row containing byte `pos` (the last start `<= pos`).
    pub fn row_at(&self, pos: u64) -> usize {
        let n = self.len() - 1;
        match self {
            Offsets::Narrow { base, deltas } => {
                if pos < *base {
                    return 0;
                }
                let d = (pos - base).min(u32::MAX as u64) as u32;
                deltas[..n].partition_point(|&x| x <= d).saturating_sub(1)
            }
            Offsets::Wide(v) => v[..n].partition_point(|&x| x <= pos).saturating_sub(1),
        }
    }

    pub fn heap_bytes(&self) -> usize {
        match self {
            Offsets::Narrow { deltas, .. } => deltas.capacity() * 4,
            Offsets::Wide(v) => v.capacity() * 8,
        }
    }
}

pub struct Chunk {
    pub offsets: Offsets,
    /// Packed row bytes for workbook data; `None` means the bytes are in the
    /// mapped file.
    pub owned: Option<Vec<u8>>,
}

impl Chunk {
    pub fn rows(&self) -> usize {
        self.offsets.len() - 1
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum RowFormat {
    /// Delimited text. `strict_quotes`: a quote only opens a quoted field at
    /// the start of a field (always true when parsing cells).
    Delimited {
        delim: u8,
    },
    Packed,
}

/// An immutable snapshot of the rows loaded so far.
#[derive(Clone)]
pub struct Data {
    pub map: Option<Arc<Mmap>>,
    pub format: RowFormat,
    pub chunks: Arc<Vec<Arc<Chunk>>>,
    pub rows: u64,
}

impl Data {
    pub fn empty(format: RowFormat) -> Data {
        Data {
            map: None,
            format,
            chunks: Arc::new(Vec::new()),
            rows: 0,
        }
    }

    /// The raw bytes of physical row `row`, without its line ending.
    #[inline]
    pub fn row_bytes(&self, row: u64) -> &[u8] {
        let chunk = &self.chunks[(row >> CHUNK_SHIFT) as usize];
        let i = (row & CHUNK_MASK) as usize;
        let (s, e) = (chunk.offsets.get(i), chunk.offsets.get(i + 1));
        match &chunk.owned {
            Some(buf) => &buf[s as usize..e as usize],
            None => {
                let map = self.map.as_deref().map(|m| &m[..]).unwrap_or(&[]);
                trim_eol(&map[s as usize..e as usize])
            }
        }
    }

    /// Where physical row `row` of a mapped file lies in the map, without
    /// its line ending.
    #[inline]
    pub fn row_span(&self, row: u64) -> (usize, usize) {
        let chunk = &self.chunks[(row >> CHUNK_SHIFT) as usize];
        let i = (row & CHUNK_MASK) as usize;
        let s = chunk.offsets.get(i) as usize;
        let e = chunk.offsets.get(i + 1) as usize;
        let map = self.map.as_deref().map(|m| &m[..]).unwrap_or(&[]);
        (s, s + trim_eol(&map[s..e]).len())
    }

    /// The bytes of a whole chunk and its offsets, for scanning many rows at
    /// once (search).
    pub fn chunk_bytes(&self, ci: usize) -> (&[u8], &Chunk) {
        let chunk = &self.chunks[ci];
        match &chunk.owned {
            Some(buf) => (&buf[..], chunk),
            None => (self.map.as_deref().map(|m| &m[..]).unwrap_or(&[]), chunk),
        }
    }

    /// Calls `f` with each field of a row, stopping after `max_cols` fields.
    #[inline]
    pub fn for_each_field<'a>(
        &'a self,
        row: u64,
        max_cols: usize,
        mut f: impl FnMut(usize, Cow<'a, [u8]>),
    ) {
        let bytes = self.row_bytes(row);
        match self.format {
            RowFormat::Delimited { delim } => csv::fields(bytes, delim, max_cols, &mut f),
            RowFormat::Packed => {
                if bytes.is_empty() {
                    return;
                }
                for (i, field) in bytes.split(|&b| b == PACKED_SEP).enumerate() {
                    if i >= max_cols {
                        break;
                    }
                    f(i, Cow::Borrowed(field));
                }
            }
        }
    }

    /// One cell's bytes (empty when the row is short).
    pub fn cell(&self, row: u64, col: usize) -> Cow<'_, [u8]> {
        let mut out = Cow::Borrowed(&[][..]);
        self.for_each_field(row, col + 1, |i, v| {
            if i == col {
                out = v;
            }
        });
        out
    }

    /// The first `max_cols` fields of a row.
    pub fn row_fields(&self, row: u64, max_cols: usize) -> Vec<Cow<'_, [u8]>> {
        let mut v = Vec::new();
        self.for_each_field(row, max_cols, |_, f| v.push(f));
        v
    }

    pub fn heap_bytes(&self) -> usize {
        self.chunks
            .iter()
            .map(|c| c.offsets.heap_bytes() + c.owned.as_ref().map_or(0, |b| b.capacity()))
            .sum()
    }
}

#[inline]
fn trim_eol(b: &[u8]) -> &[u8] {
    let mut e = b.len();
    if e > 0 && b[e - 1] == b'\n' {
        e -= 1;
        if e > 0 && b[e - 1] == b'\r' {
            e -= 1;
        }
    } else if e > 0 && b[e - 1] == b'\r' {
        e -= 1;
    }
    &b[..e]
}

/// Hands memory freed by a finished job back to the system. The sorts and
/// filters allocate and free hundreds of MB across threads, and glibc keeps
/// much of it in its arenas otherwise. Takes a few ms; call it off the UI
/// thread.
pub fn release_free_memory() {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    // SAFETY: malloc_trim has no preconditions.
    unsafe {
        libc::malloc_trim(0);
    }
}

pub fn lossy(b: &[u8]) -> Cow<'_, str> {
    String::from_utf8_lossy(b)
}

/// Accumulates row offsets into full chunks as rows are found.
pub struct ChunkBuilder {
    starts: Vec<u64>,
    pub done: Vec<Arc<Chunk>>,
    pub rows: u64,
}

impl Default for ChunkBuilder {
    fn default() -> Self {
        Self::new()
    }
}

impl ChunkBuilder {
    pub fn new() -> Self {
        ChunkBuilder {
            starts: Vec::with_capacity(CHUNK_ROWS + 1),
            done: Vec::new(),
            rows: 0,
        }
    }

    /// Adds a row starting at `start`. Rows end where the next one starts.
    #[inline]
    pub fn push_start(&mut self, start: u64) {
        if self.starts.len() == CHUNK_ROWS {
            // The new start also ends the full chunk.
            self.starts.push(start);
            self.done.push(Arc::new(Chunk {
                offsets: Offsets::build(&self.starts),
                owned: None,
            }));
            self.starts.clear();
        }
        self.starts.push(start);
        self.rows += 1;
    }

    /// Adds rows starting at `base` plus each of `list`, in order.
    pub fn push_starts(&mut self, base: u64, mut list: &[u32]) {
        while let Some(&d) = list.first() {
            if self.starts.len() == CHUNK_ROWS {
                self.push_start(base + d as u64);
                list = &list[1..];
                continue;
            }
            let n = (CHUNK_ROWS - self.starts.len()).min(list.len());
            self.starts
                .extend(list[..n].iter().map(|&d| base + d as u64));
            self.rows += n as u64;
            list = &list[n..];
        }
    }

    /// The finished chunks plus the partial one ending at `end`.
    pub fn snapshot(&self, end: u64) -> Vec<Arc<Chunk>> {
        let mut v = self.done.clone();
        if !self.starts.is_empty() {
            let mut s = self.starts.clone();
            s.push(end);
            v.push(Arc::new(Chunk {
                offsets: Offsets::build(&s),
                owned: None,
            }));
        }
        v
    }
}

// ── Opening files ───────────────────────────────────────────────────────

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Kind {
    Delimited,
    Workbook,
}

pub fn kind_of(path: &Path) -> Kind {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "xlsx" | "xlsm" | "xlsb" | "xls" | "xla" | "xlam" | "ods" => Kind::Workbook,
        _ => Kind::Delimited,
    }
}

/// Progress from a loader thread.
pub enum LoadEvent {
    /// Workbook sheet names, sent first for workbooks.
    Sheets(Vec<String>),
    Progress {
        data: Data,
        fraction: f32,
    },
    Done {
        data: Data,
        elapsed: std::time::Duration,
    },
    Failed(String),
}

pub type LoadSender = futures::channel::mpsc::UnboundedSender<LoadEvent>;

/// Loads `path` (sheet `sheet` of a workbook) on a new thread.
pub fn spawn_load(
    path: PathBuf,
    sheet: Option<String>,
    cancel: Arc<std::sync::atomic::AtomicBool>,
) -> futures::channel::mpsc::UnboundedReceiver<LoadEvent> {
    let (tx, rx) = futures::channel::mpsc::unbounded();
    std::thread::Builder::new()
        .name("loader".into())
        .spawn(move || {
            let r = match kind_of(&path) {
                Kind::Delimited => csv::load(&path, &tx, &cancel),
                Kind::Workbook => workbook::load(&path, sheet.as_deref(), &tx, &cancel),
            };
            if let Err(e) = r {
                let _ = tx.unbounded_send(LoadEvent::Failed(format!("{e:#}")));
            }
        })
        .expect("spawn loader");
    rx
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builder_makes_full_chunks() {
        let mut b = ChunkBuilder::new();
        for i in 0..(CHUNK_ROWS as u64 * 2 + 5) {
            b.push_start(i * 10);
        }
        let end = (CHUNK_ROWS as u64 * 2 + 5) * 10;
        let chunks = b.snapshot(end);
        // Pushing in batches of any size builds the same chunks.
        for step in [1, 7, CHUNK_ROWS - 1, CHUNK_ROWS, CHUNK_ROWS + 3] {
            let all: Vec<u32> = (0..(CHUNK_ROWS as u32 * 2 + 5)).map(|i| i * 10).collect();
            let mut c = ChunkBuilder::new();
            for part in all.chunks(step) {
                c.push_starts(0, part);
            }
            assert_eq!(c.rows, b.rows);
            let got = c.snapshot(end);
            assert_eq!(got.len(), chunks.len());
            for (g, w) in got.iter().zip(&chunks) {
                assert_eq!(g.rows(), w.rows());
                for i in 0..=g.rows() {
                    assert_eq!(g.offsets.get(i), w.offsets.get(i));
                }
            }
        }
        assert_eq!(chunks.len(), 3);
        assert_eq!(chunks[0].rows(), CHUNK_ROWS);
        assert_eq!(chunks[2].rows(), 5);
        assert_eq!(chunks[1].offsets.get(0), CHUNK_ROWS as u64 * 10);
        assert_eq!(chunks[1].offsets.get(CHUNK_ROWS), CHUNK_ROWS as u64 * 20);
        assert_eq!(chunks[2].offsets.get(5), end);
    }

    #[test]
    fn row_at_finds_rows() {
        let o = Offsets::build(&[100, 110, 125, 130]);
        assert_eq!(o.row_at(100), 0);
        assert_eq!(o.row_at(109), 0);
        assert_eq!(o.row_at(110), 1);
        assert_eq!(o.row_at(124), 1);
        assert_eq!(o.row_at(125), 2);
        assert_eq!(o.row_at(130), 2);
    }
}
