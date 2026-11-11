//! Workbooks (xlsx, xlsm, xlsb, xls, ods) through calamine, converted into
//! packed rows. xlsx and xlsb sheets are streamed cell by cell, so a sheet
//! with a million rows never exists as a full calamine `Range`.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
use std::time::Instant;

use anyhow::{Context as _, anyhow};
use calamine::{Data as CData, DataRef, Reader, Sheets, open_workbook_auto};

use super::value::{excel_date, format_number};
use super::{
    CHUNK_ROWS, Chunk, Data as Table, LoadEvent, LoadSender, Offsets, PACKED_SEP, RowFormat,
};

/// Builds packed chunks from cells arriving in row-major order.
struct Packer {
    buf: Vec<u8>,
    starts: Vec<u64>,
    done: Vec<Arc<Chunk>>,
    rows: u64,
    /// Sheet row of the row being written, once the first cell arrived.
    row: Option<u32>,
    col: u32,
    first_col: u32,
}

impl Packer {
    fn new(first_col: u32) -> Self {
        Packer {
            buf: Vec::with_capacity(1 << 20),
            starts: vec![0],
            done: Vec::new(),
            rows: 0,
            row: None,
            col: 0,
            first_col,
        }
    }

    fn end_row(&mut self) {
        self.starts.push(self.buf.len() as u64);
        self.rows += 1;
        if self.starts.len() == CHUNK_ROWS + 1 {
            let buf = std::mem::replace(&mut self.buf, Vec::with_capacity(1 << 20));
            self.done.push(Arc::new(Chunk {
                offsets: Offsets::build(&self.starts),
                owned: Some(buf),
            }));
            self.starts.clear();
            self.starts.push(0);
        }
    }

    /// Adds a cell. `col` counts fields already written in the current row.
    fn cell(&mut self, row: u32, col: u32, text: &str) {
        if text.is_empty() {
            return;
        }
        match self.row {
            None => self.row = Some(row),
            Some(r) if row > r => {
                self.end_row();
                for _ in r + 1..row {
                    self.end_row();
                }
                self.row = Some(row);
                self.col = 0;
            }
            Some(_) => {}
        }
        let col = col.saturating_sub(self.first_col);
        if col < self.col {
            return; // out of order within the row
        }
        let seps = if self.col == 0 {
            col
        } else {
            col - self.col + 1
        };
        self.buf
            .extend(std::iter::repeat_n(PACKED_SEP, seps as usize));
        self.buf
            .extend(text.bytes().map(|b| if b == PACKED_SEP { b' ' } else { b }));
        self.col = col + 1;
    }

    fn snapshot(&self, final_: bool) -> Table {
        let mut chunks = self.done.clone();
        let mut rows = self.rows;
        let pending_row = final_ && self.col > 0;
        let mut starts = self.starts.clone();
        if pending_row {
            starts.push(self.buf.len() as u64);
            rows += 1;
        }
        if starts.len() > 1 {
            chunks.push(Arc::new(Chunk {
                offsets: Offsets::build(&starts),
                owned: Some(self.buf[..*starts.last().unwrap() as usize].to_vec()),
            }));
        }
        Table {
            map: None,
            format: RowFormat::Packed,
            chunks: Arc::new(chunks),
            rows,
        }
    }
}

fn data_ref_text<'a>(v: &'a DataRef<'_>, scratch: &'a mut String) -> &'a str {
    scratch.clear();
    use std::fmt::Write;
    match v {
        DataRef::Int(i) => {
            let _ = write!(scratch, "{i}");
        }
        DataRef::Float(f) => scratch.push_str(&format_number(*f)),
        DataRef::String(s) => return s.as_str(),
        DataRef::SharedString(s) => return s,
        DataRef::Bool(b) => scratch.push_str(if *b { "TRUE" } else { "FALSE" }),
        DataRef::DateTime(d) => scratch.push_str(&date_text(d.as_f64(), d.is_duration())),
        DataRef::DateTimeIso(s) | DataRef::DurationIso(s) => return s.as_str(),
        DataRef::Error(e) => {
            let _ = write!(scratch, "{e}");
        }
        DataRef::Empty => {}
    }
    scratch.as_str()
}

fn data_text(v: &CData) -> String {
    match v {
        CData::Int(i) => i.to_string(),
        CData::Float(f) => format_number(*f),
        CData::String(s) => s.clone(),
        CData::Bool(b) => if *b { "TRUE" } else { "FALSE" }.into(),
        CData::DateTime(d) => date_text(d.as_f64(), d.is_duration()),
        CData::DateTimeIso(s) | CData::DurationIso(s) => s.clone(),
        CData::Error(e) => e.to_string(),
        CData::Empty => String::new(),
    }
}

fn date_text(serial: f64, duration: bool) -> String {
    if duration || (0.0..1.0).contains(&serial) {
        let total = (serial * 86_400.0).round() as i64;
        let (h, m, s) = (total / 3600, total / 60 % 60, total % 60);
        return if s == 0 && !duration {
            format!("{h:02}:{m:02}")
        } else {
            format!("{h:02}:{m:02}:{s:02}")
        };
    }
    excel_date(serial, false)
}

pub fn load(
    path: &Path,
    sheet: Option<&str>,
    tx: &LoadSender,
    cancel: &AtomicBool,
) -> anyhow::Result<()> {
    let t0 = Instant::now();
    let mut wb = open_workbook_auto(path).with_context(|| format!("opening {}", path.display()))?;
    let names = wb.sheet_names().to_vec();
    if names.is_empty() {
        return Err(anyhow!("the workbook has no sheets"));
    }
    let _ = tx.unbounded_send(LoadEvent::Sheets(names.clone()));
    let name = sheet
        .filter(|s| names.iter().any(|n| n == s))
        .unwrap_or(&names[0])
        .to_string();

    let mut last_sent = Instant::now();
    let mut stream = |packer: &mut Packer, total_rows: u32, row: u32| {
        if last_sent.elapsed().as_millis() > 250 {
            last_sent = Instant::now();
            let _ = tx.unbounded_send(LoadEvent::Progress {
                data: packer.snapshot(false),
                fraction: if total_rows > 0 {
                    row as f32 / total_rows as f32
                } else {
                    0.0
                },
            });
        }
    };

    let packer = match &mut wb {
        Sheets::Xlsx(x) => {
            let mut r = x.worksheet_cells_reader(&name)?;
            let dims = r.dimensions();
            let mut p = Packer::new(dims.start.1);
            let mut scratch = String::new();
            let mut n = 0u32;
            while let Some(cell) = r.next_cell()? {
                let (row, col) = cell.get_position();
                let text = data_ref_text(cell.get_value(), &mut scratch).to_owned();
                p.cell(row, col, &text);
                n = n.wrapping_add(1);
                if n.is_multiple_of(16_384) {
                    if cancel.load(Relaxed) {
                        return Ok(());
                    }
                    stream(&mut p, dims.end.0, row);
                }
            }
            p
        }
        Sheets::Xlsb(x) => {
            let mut r = x.worksheet_cells_reader(&name)?;
            let dims = r.dimensions();
            let mut p = Packer::new(dims.start.1);
            let mut scratch = String::new();
            let mut n = 0u32;
            while let Some(cell) = r.next_cell()? {
                let (row, col) = cell.get_position();
                let text = data_ref_text(cell.get_value(), &mut scratch).to_owned();
                p.cell(row, col, &text);
                n = n.wrapping_add(1);
                if n.is_multiple_of(16_384) {
                    if cancel.load(Relaxed) {
                        return Ok(());
                    }
                    stream(&mut p, dims.end.0, row);
                }
            }
            p
        }
        other => {
            let range = other.worksheet_range(&name)?;
            let (r0, c0) = range.start().unwrap_or((0, 0));
            let mut p = Packer::new(c0);
            for (ri, row) in range.rows().enumerate() {
                for (ci, v) in row.iter().enumerate() {
                    let t = data_text(v);
                    p.cell(r0 + ri as u32, c0 + ci as u32, &t);
                }
            }
            p
        }
    };
    let _ = tx.unbounded_send(LoadEvent::Done {
        data: packer.snapshot(true),
        elapsed: t0.elapsed(),
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rows(t: &Table) -> Vec<Vec<String>> {
        (0..t.rows)
            .map(|r| {
                t.row_fields(r, 100)
                    .iter()
                    .map(|f| String::from_utf8_lossy(f).into_owned())
                    .collect()
            })
            .collect()
    }

    #[test]
    fn packs_cells() {
        let mut p = Packer::new(0);
        p.cell(0, 0, "a");
        p.cell(0, 1, "b");
        p.cell(1, 1, "x");
        p.cell(3, 0, "y");
        p.cell(3, 2, "z");
        let t = p.snapshot(true);
        assert_eq!(
            rows(&t),
            vec![vec!["a", "b"], vec!["", "x"], vec![], vec!["y", "", "z"],]
        );
    }

    #[test]
    fn times() {
        assert_eq!(date_text(0.5, false), "12:00");
        assert_eq!(date_text(1.5, true), "36:00:00");
    }
}
