//! Delimited text: sniffing, the parallel row index and the cell parser.
//!
//! Row boundaries are found with the quote-parity trick so pieces of the file
//! can be scanned on all cores at once: each piece records the newlines it
//! sees at even and at odd quote parity, and once the pieces are stitched in
//! order the parity at each piece's start picks which list is real. That is
//! only correct when quotes appear at the start of fields, so a sample of the
//! file is first checked against a strict scanner, and files with stray
//! quotes (`5" pipe`) are indexed with the strict scanner instead.

use std::borrow::Cow;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Instant;

use anyhow::Context as _;
use memchr::{memchr, memchr2_iter, memchr3_iter};
use memmap2::Mmap;
use rayon::prelude::*;

use super::{ChunkBuilder, Data, LoadEvent, LoadSender, RowFormat};

const PIECE: usize = 8 << 20;
const SAMPLE: usize = 8 << 20;

// ── Cells ───────────────────────────────────────────────────────────────

/// Splits one row into fields: `"…"` quoting with `""` escapes, quotes
/// only opening a quoted field at the start of a field.
///
/// Delimiters are found 16 bytes at a time as a bitmask, so splitting a row
/// costs a few instructions per field. A field that starts with a quote is
/// handed to [`fields_from`], which handles quoting.
#[inline]
pub fn fields<'a>(
    b: &'a [u8],
    delim: u8,
    max_cols: usize,
    f: &mut impl FnMut(usize, Cow<'a, [u8]>),
) {
    if b.is_empty() || max_cols == 0 {
        return;
    }
    let len = b.len();
    // The current field starts at `i`.
    let mut i = 0;
    let mut col = 0;
    let mut base = 0;
    while base < len {
        let mut m = delim_mask(&b[base..], delim);
        if i > base {
            m &= u32::MAX << (i - base);
        }
        loop {
            if i < len && b[i] == b'"' {
                return fields_from(b, delim, max_cols, f, i, col);
            }
            if m == 0 {
                break;
            }
            let p = base + m.trailing_zeros() as usize;
            m &= m - 1;
            f(col, Cow::Borrowed(&b[i..p]));
            col += 1;
            i = p + 1;
            if col >= max_cols {
                return;
            }
            if i == len {
                f(col, Cow::Borrowed(&[]));
                return;
            }
        }
        base += 16;
    }
    f(col, Cow::Borrowed(&b[i..]));
}

/// Bit `k` is set when `b[k] == delim`, for the first 16 bytes of `b`.
#[inline]
fn delim_mask(b: &[u8], delim: u8) -> u32 {
    if let Some(blk) = b.first_chunk::<16>() {
        return mask16(blk, delim);
    }
    let mut tail = [0u8; 16];
    tail[..b.len()].copy_from_slice(b);
    mask16(&tail, delim) & ((1u32 << b.len()) - 1)
}

/// Bit `k` is set when `b[k] == delim`.
#[inline]
#[cfg(target_arch = "x86_64")]
fn mask16(b: &[u8; 16], delim: u8) -> u32 {
    use std::arch::x86_64::*;
    // SAFETY: SSE2 is part of the x86_64 baseline, and the load reads the
    // 16 bytes of `b`.
    unsafe {
        let v = _mm_loadu_si128(b.as_ptr().cast());
        _mm_movemask_epi8(_mm_cmpeq_epi8(v, _mm_set1_epi8(delim as i8))) as u32
    }
}

#[inline]
#[cfg(not(target_arch = "x86_64"))]
fn mask16(b: &[u8; 16], delim: u8) -> u32 {
    let mut m = 0;
    for (k, &c) in b.iter().enumerate() {
        m |= ((c == delim) as u32) << k;
    }
    m
}

/// The general splitter, starting at field `col` at byte `i`.
fn fields_from<'a>(
    b: &'a [u8],
    delim: u8,
    max_cols: usize,
    f: &mut impl FnMut(usize, Cow<'a, [u8]>),
    mut i: usize,
    mut col: usize,
) {
    let len = b.len();
    loop {
        if col >= max_cols {
            return;
        }
        if i < len && b[i] == b'"' {
            i += 1;
            let mut seg = i;
            let mut owned: Option<Vec<u8>> = None;
            let value: Cow<'a, [u8]>;
            loop {
                match memchr(b'"', &b[i..]) {
                    None => {
                        value = match owned {
                            Some(mut o) => {
                                o.extend_from_slice(&b[seg..]);
                                Cow::Owned(o)
                            }
                            None => Cow::Borrowed(&b[seg..]),
                        };
                        i = len;
                        break;
                    }
                    Some(k) => {
                        let q = i + k;
                        if q + 1 < len && b[q + 1] == b'"' {
                            owned
                                .get_or_insert_with(Vec::new)
                                .extend_from_slice(&b[seg..=q]);
                            i = q + 2;
                            seg = i;
                            continue;
                        }
                        // Closing quote; anything up to the delimiter is kept.
                        let rest_end = memchr(delim, &b[q + 1..]).map_or(len, |k| q + 1 + k);
                        let trailing = &b[q + 1..rest_end];
                        value = match (owned, trailing.is_empty()) {
                            (None, true) => Cow::Borrowed(&b[seg..q]),
                            (o, _) => {
                                let mut o = o.unwrap_or_default();
                                o.extend_from_slice(&b[seg..q]);
                                o.extend_from_slice(trailing);
                                Cow::Owned(o)
                            }
                        };
                        i = rest_end;
                        break;
                    }
                }
            }
            f(col, value);
            col += 1;
            if i >= len {
                return;
            }
            // b[i] is the delimiter.
            i += 1;
            if i == len {
                if col < max_cols {
                    f(col, Cow::Borrowed(&[]));
                }
                return;
            }
        } else {
            match memchr(delim, &b[i..]) {
                None => {
                    f(col, Cow::Borrowed(&b[i..]));
                    return;
                }
                Some(k) => {
                    f(col, Cow::Borrowed(&b[i..i + k]));
                    i += k + 1;
                    col += 1;
                    if i == len {
                        if col < max_cols {
                            f(col, Cow::Borrowed(&[]));
                        }
                        return;
                    }
                }
            }
        }
    }
}

/// Appends `v` as one CSV field.
pub fn write_field(out: &mut Vec<u8>, v: &[u8], delim: u8) {
    let needs = v
        .iter()
        .any(|&c| c == delim || c == b'"' || c == b'\n' || c == b'\r')
        || v.first().is_some_and(|c| c.is_ascii_whitespace())
        || v.last().is_some_and(|c| c.is_ascii_whitespace());
    if !needs {
        out.extend_from_slice(v);
        return;
    }
    out.push(b'"');
    for &c in v {
        if c == b'"' {
            out.push(b'"');
        }
        out.push(c);
    }
    out.push(b'"');
}

// ── Sniffing ────────────────────────────────────────────────────────────

pub fn sniff_delimiter(path: &Path, head: &[u8]) -> u8 {
    let ext = path
        .extension()
        .map(|e| e.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if ext == "tsv" || ext == "tab" {
        return b'\t';
    }
    let lines: Vec<&[u8]> = head.split(|&b| b == b'\n').take(30).collect();
    // Drop a possibly cut-off last line.
    let lines = if lines.len() > 2 {
        &lines[..lines.len() - 1]
    } else {
        &lines[..]
    };
    let mut best = (b',', 0usize, 0usize);
    for d in *b",\t;|" {
        let counts: Vec<usize> = lines
            .iter()
            .filter(|l| !l.is_empty())
            .map(|l| {
                let mut n = 0;
                fields(l, d, usize::MAX, &mut |_, _| n += 1);
                n
            })
            .collect();
        let Some(&first) = counts.first() else {
            continue;
        };
        if first < 2 {
            continue;
        }
        let consistent = counts.iter().filter(|&&c| c == first).count();
        if (consistent, first) > (best.1, best.2) {
            best = (d, consistent, first);
        }
    }
    best.0
}

// ── Indexing ────────────────────────────────────────────────────────────

/// Row starts by the strict rules, sequentially. Returns the parse state
/// (`in_quote`) at the end.
fn strict_scan(b: &[u8], from: usize, to: usize, delim: u8, out: &mut impl FnMut(u64)) {
    let mut in_quote = false;
    let mut field_start = from;
    let mut i = from;
    let slice = &b[..to];
    while i < to {
        if in_quote {
            match memchr(b'"', &slice[i..]) {
                None => return,
                Some(k) => {
                    let q = i + k;
                    if q + 1 < to && slice[q + 1] == b'"' {
                        i = q + 2;
                    } else {
                        in_quote = false;
                        i = q + 1;
                    }
                }
            }
            continue;
        }
        let Some(k) = memchr3_iter(delim, b'"', b'\n', &slice[i..]).next() else {
            return;
        };
        let p = i + k;
        match slice[p] {
            b'\n' => {
                if p + 1 < b.len() {
                    out(p as u64 + 1);
                }
                field_start = p + 1;
            }
            b'"' => {
                if p == field_start {
                    in_quote = true;
                }
            }
            _ => field_start = p + 1,
        }
        i = p + 1;
    }
}

/// Row starts found in one piece, relative to its start (pieces are at
/// most [`PIECE`] bytes, so they fit in 4 bytes).
struct Piece {
    from: u64,
    even: Vec<u32>,
    odd: Vec<u32>,
    flips: bool,
}

/// Newlines of `b[from..to]` at even and odd quote parity.
fn parity_scan(b: &[u8], from: usize, to: usize) -> Piece {
    let mut p = Piece {
        from: from as u64,
        even: Vec::with_capacity((to - from) / 64),
        odd: Vec::new(),
        flips: false,
    };
    let mut odd = false;
    for k in memchr2_iter(b'"', b'\n', &b[from..to]) {
        if b[from + k] == b'"' {
            odd = !odd;
        } else if odd {
            p.odd.push(k as u32 + 1);
        } else {
            p.even.push(k as u32 + 1);
        }
    }
    p.flips = odd;
    p
}

/// Whether the parity trick finds the same rows as the strict scanner on
/// the start of the file.
fn parity_is_safe(b: &[u8], start: usize, delim: u8) -> bool {
    let to = (start + SAMPLE).min(b.len());
    // End the sample at a newline outside quotes is unknowable; compare the
    // rows both find before the sample end instead.
    let mut strict = Vec::new();
    strict_scan(b, start, to, delim, &mut |s| strict.push(s));
    let par = parity_scan(b, start, to);
    let even: Vec<u64> = par
        .even
        .iter()
        .map(|&d| par.from + d as u64)
        .filter(|&s| (s as usize) < b.len())
        .collect();
    strict == even
}

pub fn load(path: &Path, tx: &LoadSender, cancel: &AtomicBool) -> anyhow::Result<()> {
    let t0 = Instant::now();
    let file = std::fs::File::open(path).with_context(|| format!("opening {}", path.display()))?;
    let len = file.metadata()?.len() as usize;
    let map = if len == 0 {
        None
    } else {
        let m = unsafe { Mmap::map(&file) }.context("mapping the file")?;
        #[cfg(unix)]
        let _ = m.advise(memmap2::Advice::Sequential);
        Some(Arc::new(m))
    };
    let b: &[u8] = map.as_deref().map(|m| &m[..]).unwrap_or(&[]);
    let start = if b.starts_with(&[0xEF, 0xBB, 0xBF]) {
        3
    } else {
        0
    };
    let delim = sniff_delimiter(path, &b[start..(start + 256 * 1024).min(len)]);
    let format = RowFormat::Delimited { delim };
    let mut builder = ChunkBuilder::new();
    if start < len {
        builder.push_start(start as u64);
    }
    let send = |builder: &ChunkBuilder, done_to: usize| {
        let data = Data {
            map: map.clone(),
            format,
            chunks: Arc::new(builder.snapshot(done_to as u64)),
            rows: builder.rows,
        };
        let _ = tx.unbounded_send(LoadEvent::Progress {
            data,
            fraction: done_to as f32 / len.max(1) as f32,
        });
    };

    if start < len && !parity_is_safe(b, start, delim) {
        // Stray quotes: one sequential strict pass, reporting as it goes.
        let mut last_sent = Instant::now();
        let mut n = 0u32;
        strict_scan(b, start, len, delim, &mut |s| {
            builder.push_start(s);
            n = n.wrapping_add(1);
            if n.is_multiple_of(65_536) && last_sent.elapsed().as_millis() > 120 {
                send(&builder, s as usize);
                last_sent = Instant::now();
            }
        });
    } else {
        let threads = rayon::current_num_threads().max(1);
        // Scans `b[pos..pos + batch]` in pieces on all cores.
        let scan = |pos: usize, batch: usize| -> (usize, Vec<Piece>) {
            let end = (pos + batch).min(len);
            let piece = (end - pos).clamp(1, PIECE);
            let ranges: Vec<(usize, usize)> = (pos..end)
                .step_by(piece)
                .map(|s| (s, (s + piece).min(end)))
                .collect();
            let pieces = ranges
                .par_iter()
                .map(|&(s, e)| parity_scan(b, s, e))
                .collect();
            (end, pieces)
        };
        let mut in_quote = false;
        let mut stitch = |builder: &mut ChunkBuilder, pieces: Vec<Piece>| {
            for p in pieces {
                let mut list = if in_quote { &p.odd[..] } else { &p.even[..] };
                // A newline that ends the file starts no row.
                if list
                    .last()
                    .is_some_and(|&d| (p.from + d as u64) as usize >= len)
                {
                    list = &list[..list.len() - 1];
                }
                builder.push_starts(p.from, list);
                in_quote ^= p.flips;
            }
        };
        // A small first batch puts the first rows on screen right away.
        let (mut pos, pieces) = scan(start, 1 << 20);
        stitch(&mut builder, pieces);
        if pos < len {
            send(&builder, pos);
        }
        // Then each batch is stitched in order while the next is scanned.
        let mut pending: Option<(usize, Vec<Piece>)> = None;
        while pos < len || pending.is_some() {
            if cancel.load(Relaxed) {
                return Ok(());
            }
            let (scanned, ()) = rayon::join(
                || (pos < len).then(|| scan(pos, PIECE * threads * 2)),
                || {
                    if let Some((end, pieces)) = pending.take() {
                        stitch(&mut builder, pieces);
                        if end < len {
                            send(&builder, end);
                        }
                    }
                },
            );
            if let Some((end, pieces)) = scanned {
                pos = end;
                pending = Some((end, pieces));
            }
        }
    }

    let data = Data {
        map: map.clone(),
        format,
        chunks: Arc::new(builder.snapshot(len as u64)),
        rows: builder.rows,
    };
    #[cfg(unix)]
    if let Some(m) = &map {
        let _ = m.advise(memmap2::Advice::Normal);
    }
    let _ = tx.unbounded_send(LoadEvent::Done {
        data,
        elapsed: t0.elapsed(),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn split(s: &str) -> Vec<String> {
        let mut v = Vec::new();
        fields(s.as_bytes(), b',', usize::MAX, &mut |_, f| {
            v.push(String::from_utf8(f.into_owned()).unwrap())
        });
        v
    }

    #[test]
    fn plain_fields() {
        assert_eq!(split("a,b,c"), ["a", "b", "c"]);
        assert_eq!(split("a,,c,"), ["a", "", "c", ""]);
        assert_eq!(split(""), Vec::<String>::new());
        assert_eq!(split(","), ["", ""]);
    }

    #[test]
    fn quoted_fields() {
        assert_eq!(split(r#""a,b",c"#), ["a,b", "c"]);
        assert_eq!(split(r#""say ""hi""",x"#), [r#"say "hi""#, "x"]);
        assert_eq!(split("\"multi\nline\",2"), ["multi\nline", "2"]);
        assert_eq!(split(r#""a"b,c"#), ["ab", "c"]);
        assert_eq!(split(r#"5" pipe,x"#), [r#"5" pipe"#, "x"]);
        assert_eq!(split(r#""unterminated"#), ["unterminated"]);
        assert_eq!(split(r#""a","#), ["a", ""]);
    }

    /// The bitmask splitter against the general one, on rows of every
    /// length around the 64-byte blocks, with quoted fields anywhere.
    #[test]
    fn fast_fields_match_general() {
        let mut x: u64 = 0x1234_5678_9ABC_DEF1;
        for _ in 0..50_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 200) as usize;
            let mut y = x;
            let b: Vec<u8> = (0..len)
                .map(|i| {
                    y = y
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    match (y >> 59) % 12 {
                        0 | 1 => b',',
                        2 if x & 1 == 0 => b'"',
                        _ => b'a' + i as u8 % 26,
                    }
                })
                .collect();
            for max in [1, 3, 7, usize::MAX] {
                let mut fast = Vec::new();
                fields(&b, b',', max, &mut |c, v| fast.push((c, v.into_owned())));
                let mut slow = Vec::new();
                if !b.is_empty() {
                    fields_from(
                        &b,
                        b',',
                        max,
                        &mut |c, v| slow.push((c, v.into_owned())),
                        0,
                        0,
                    );
                }
                assert_eq!(fast, slow, "{:?} max {max}", String::from_utf8_lossy(&b));
            }
        }
    }

    #[test]
    fn parity_scan_matches_naive() {
        let mut x: u64 = 0xDEAD_BEEF_1234_5678;
        for _ in 0..5_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let len = (x % 400) as usize;
            let mut y = x;
            let b: Vec<u8> = (0..len)
                .map(|_| {
                    y = y
                        .wrapping_mul(6364136223846793005)
                        .wrapping_add(1442695040888963407);
                    match (y >> 59) % 10 {
                        0 => b'"',
                        1 => b'\n',
                        _ => b'a',
                    }
                })
                .collect();
            let from = (x >> 20) as usize % (len + 1);
            let p = parity_scan(&b, from, len);
            let (mut even, mut odd, mut q) = (Vec::new(), Vec::new(), false);
            for (i, &c) in b.iter().enumerate().skip(from) {
                match c {
                    b'"' => q = !q,
                    b'\n' if q => odd.push((i - from) as u32 + 1),
                    b'\n' => even.push((i - from) as u32 + 1),
                    _ => {}
                }
            }
            assert_eq!((p.even, p.odd, p.flips), (even, odd, q));
        }
    }

    #[test]
    fn max_cols_stops_early() {
        let mut n = 0;
        fields(b"a,b,c,d", b',', 2, &mut |_, _| n += 1);
        assert_eq!(n, 2);
    }

    fn index(text: &str) -> Vec<String> {
        let dir = std::env::temp_dir().join(format!("rowscope-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(format!("t{}.csv", text.len()));
        std::fs::write(&p, text).unwrap();
        let (tx, mut rx) = futures::channel::mpsc::unbounded();
        load(&p, &tx, &AtomicBool::new(false)).unwrap();
        drop(tx);
        let mut last = None;
        while let Ok(ev) = rx.try_recv() {
            if let LoadEvent::Done { data, .. } = ev {
                last = Some(data);
            }
        }
        let data = last.unwrap();
        (0..data.rows)
            .map(|r| String::from_utf8_lossy(data.row_bytes(r)).into_owned())
            .collect()
    }

    #[test]
    fn index_rows() {
        assert_eq!(index("a,b\n1,2\n3,4\n"), ["a,b", "1,2", "3,4"]);
        assert_eq!(index("a,b\r\n1,2"), ["a,b", "1,2"]);
        assert_eq!(index("a,b\n\"x\ny\",2\n"), ["a,b", "\"x\ny\",2"]);
        assert_eq!(index("\u{feff}h\n1\n"), ["h", "1"]);
        assert_eq!(index(""), Vec::<String>::new());
    }

    #[test]
    fn stray_quotes_use_strict_rows() {
        assert_eq!(
            index("a,b\n5\" pipe,2\n3,4\n6\" pipe,5\n"),
            ["a,b", "5\" pipe,2", "3,4", "6\" pipe,5"]
        );
    }

    #[test]
    fn sniffs_delimiters() {
        let p = Path::new("x.csv");
        assert_eq!(sniff_delimiter(p, b"a;b;c\n1;2;3\n"), b';');
        assert_eq!(sniff_delimiter(p, b"a\tb\n1\t2\n"), b'\t');
        assert_eq!(sniff_delimiter(p, b"a,b\n1,2\n"), b',');
        assert_eq!(sniff_delimiter(p, b"\"x;y\",b\n1,2\n"), b',');
    }

    #[test]
    fn writes_fields() {
        let mut v = Vec::new();
        write_field(&mut v, b"a,b", b',');
        v.push(b',');
        write_field(&mut v, b"say \"hi\"", b',');
        v.push(b',');
        write_field(&mut v, b"plain", b',');
        assert_eq!(v, br#""a,b","say ""hi""",plain"#);
    }
}
