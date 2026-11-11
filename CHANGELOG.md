# Changelog

All notable changes to Rowscope are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

## [Unreleased]

### Changed

Faster and leaner on large files (10 M-row CSV, medians of five runs, before → after):

- Indexing a file: 114 → 72 ms. Batches of rows are stitched in while the next batch is
  scanned, and the per-piece row lists are half the size.
- Number filters and selection statistics: about 1.7× faster, from a direct parser for plain
  decimal numbers that gives the same results as the standard library.
- Text filters: 1.3× faster, from splitting rows on 16-byte SIMD masks and testing each
  filter as its field goes by.
- Sorting text: 615 → 233 ms and 474 → 196 MB peak. Columns with few distinct values rank
  each value once and sort 8-byte integers.
- Sorting numbers: 505 → 320 ms and 363 → 231 MB peak, by building the keys in place and
  writing the result over the view.
- Exporting all rows: 412 → 241 ms. Unedited rows go straight from the mapped file to disk.
- Memory freed by finished jobs is returned to the system, so the app falls back to its
  resting size after each sort or filter instead of keeping 20–80 MB.
- `--bench` reports each step's peak memory, and adds a sort of a column whose values are
  all distinct.

## [0.1.0] - 2026-11-10

The first release.

### Added

- Opens CSV, TSV and other delimited text (delimiter sniffed), and XLSX, XLSM, XLSB, XLS and
  ODS workbooks with sheet tabs.
- Memory-mapped CSV with a parallel, quote-aware row index (4 bytes per row). The first rows
  show while the rest of the file indexes. XLSX and XLSB sheets are streamed.
- A custom-painted grid that reads only visible cells, with column letters, a header row,
  type badges, row numbers, zebra striping, column resizing and autofit, and zoom.
- Parallel search across all cells.
- Column menus with sorting, conditions (text, number and regex), and a value checklist with
  counts. Filter chips. Filter or exclude by a cell's value from the context menu.
- Selection with Count, Sum, Average, Min and Max in the status bar.
- Name box and formula bar; go to a cell or range.
- In-cell editing, paste, clear, and undo/redo, kept on top of the source file.
- Export of the rows shown (filtered, sorted, edited) to CSV.
- Tabs for several files, drag and drop, recent files, and a desktop entry.

[0.1.0]: https://github.com/trevortomlin/rowscope/releases/tag/v0.1.0
