# Changelog

All notable changes to Rowscope are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and versions follow
[Semantic Versioning](https://semver.org/).

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
