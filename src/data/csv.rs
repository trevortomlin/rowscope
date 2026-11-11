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
    let mut i = 0;
    let mut col = 0;
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

struct Piece {
    even: Vec<u64>,
    odd: Vec<u64>,
    flips: bool,
}

/// Newlines of `b[from..to]` at even and odd quote parity.
fn parity_scan(b: &[u8], from: usize, to: usize) -> Piece {
    let mut p = Piece {
        even: Vec::with_capacity((to - from) / 64),
        odd: Vec::new(),
        flips: false,
    };
    let mut odd = false;
    for k in memchr2_iter(b'"', b'\n', &b[from..to]) {
        let pos = from + k;
        if b[pos] == b'"' {
            odd = !odd;
        } else if odd {
            p.odd.push(pos as u64 + 1);
        } else {
            p.even.push(pos as u64 + 1);
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
    let mut even = par.even;
    even.retain(|&s| (s as usize) < b.len());
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
        let mut pos = start;
        let mut in_quote = false;
        let mut first = true;
        while pos < len {
            if cancel.load(Relaxed) {
                return Ok(());
            }
            // A small first batch puts the first rows on screen right away.
            let batch = if first { 1 << 20 } else { PIECE * threads * 2 };
            first = false;
            let end = (pos + batch).min(len);
            let piece = if end - pos <= PIECE { end - pos } else { PIECE };
            let ranges: Vec<(usize, usize)> = (pos..end)
                .step_by(piece)
                .map(|s| (s, (s + piece).min(end)))
                .collect();
            let pieces: Vec<Piece> = ranges
                .par_iter()
                .map(|&(s, e)| parity_scan(b, s, e))
                .collect();
            for p in pieces {
                let list = if in_quote { &p.odd } else { &p.even };
                for &s in list {
                    if (s as usize) < len {
                        builder.push_start(s);
                    }
                }
                in_quote ^= p.flips;
            }
            pos = end;
            if pos < len {
                send(&builder, pos);
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
