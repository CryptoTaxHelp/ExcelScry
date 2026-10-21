//! Cell and workbook data model.
//!
//! The model keeps *typed* values (not pre-rendered strings) so that an
//! export to ODS writes real numbers, dates and booleans rather than text.
//! Rendering for the grid happens separately via [`Cell::display`].

use chrono::{Duration, NaiveDateTime, NaiveTime};
use serde::Serialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;

use crate::number_format::NumberFormat;
use crate::text_prefs::SortChoice;
use crate::trust::TrustReport;
use crate::view_prefs::ViewState;

/// A single cell, preserving enough type information to export faithfully.
#[derive(Debug, Clone, PartialEq)]
pub enum Cell {
    Empty,
    /// Integer kept as `i64` so large IDs (account numbers, tweet ids...)
    /// survive without going through `f64`.
    Int(i64),
    Float(f64),
    Text(String),
    Bool(bool),
    DateTime(NaiveDateTime),
    /// A time of day. Excel stores these as a fractional day, which would
    /// otherwise surface as a bogus 1899-12-31 date.
    Time(NaiveTime),
    /// An Excel time/duration (a span, e.g. 36 hours).
    Duration(Duration),
    /// A spreadsheet error such as `#DIV/0!`.
    Error(String),
}

impl Cell {
    /// Single-character kind code sent to the UI, used for alignment/colour.
    pub fn kind(&self) -> &'static str {
        match self {
            Cell::Empty => "z",
            Cell::Int(_) => "i",
            Cell::Float(_) => "f",
            Cell::Text(_) => "s",
            Cell::Bool(_) => "b",
            Cell::DateTime(_) => "d",
            Cell::Time(_) => "t",
            Cell::Duration(_) => "t",
            Cell::Error(_) => "e",
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Cell::Empty)
    }

    /// Human-readable rendering shown in the grid.
    pub fn display(&self) -> String {
        match self {
            Cell::Empty => String::new(),
            Cell::Int(v) => v.to_string(),
            Cell::Float(v) => format_float(*v),
            Cell::Text(s) => s.clone(),
            Cell::Bool(b) => if *b { "TRUE" } else { "FALSE" }.to_string(),
            Cell::DateTime(dt) => format_datetime(dt),
            Cell::Time(t) => format_time(t),
            Cell::Duration(d) => format_duration(d),
            Cell::Error(e) => e.clone(),
        }
    }
}

/// Render an `f64` without a spurious `.0` on integral values, while keeping
/// the shortest round-tripping representation for everything else.
fn format_float(v: f64) -> String {
    if v.is_nan() {
        return "NaN".to_string();
    }
    if v.is_infinite() {
        return if v > 0.0 { "∞" } else { "-∞" }.to_string();
    }
    // Excel/displays treat up to 15 significant digits as exact.
    if v.fract() == 0.0 && v.abs() < 9_007_199_254_740_992.0 {
        return format!("{}", v as i64);
    }
    format!("{v}")
}

fn format_datetime(dt: &NaiveDateTime) -> String {
    use chrono::Timelike;
    if dt.hour() == 0 && dt.minute() == 0 && dt.second() == 0 && dt.nanosecond() == 0 {
        dt.format("%Y-%m-%d").to_string()
    } else if dt.nanosecond() == 0 {
        dt.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        dt.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
    }
}

fn format_time(t: &NaiveTime) -> String {
    use chrono::Timelike;
    if t.nanosecond() == 0 {
        t.format("%H:%M:%S").to_string()
    } else {
        t.format("%H:%M:%S%.3f").to_string()
    }
}

/// Render a duration the way spreadsheets do: `[h]:mm:ss`, where the hour
/// field is not wrapped at 24 (so 1.5 days is `36:00:00`).
fn format_duration(d: &Duration) -> String {
    let negative = d.num_milliseconds() < 0;
    let total_ms = d.num_milliseconds().unsigned_abs();
    let total_secs = total_ms / 1000;
    let millis = total_ms % 1000;
    let hours = total_secs / 3600;
    let minutes = (total_secs % 3600) / 60;
    let seconds = total_secs % 60;
    let sign = if negative { "-" } else { "" };
    if millis > 0 {
        format!("{sign}{hours}:{minutes:02}:{seconds:02}.{millis:03}")
    } else {
        format!("{sign}{hours}:{minutes:02}:{seconds:02}")
    }
}

/// The formats of the cells that carry one, keyed by absolute position.
///
/// Sparse on purpose: most cells in a sheet use `General`, and this should
/// cost memory in proportion to the cells that actually have a format rather
/// than eight bytes for every cell in the file.
#[derive(Debug, Clone, Default)]
pub struct SheetFormats {
    cells: HashMap<u64, Arc<NumberFormat>>,
}

/// Absolute row and column packed into one key (both fit in 32 bits).
fn cell_key(row: usize, col: usize) -> u64 {
    ((row as u64) << 32) | col as u64
}

impl SheetFormats {
    pub fn insert(&mut self, row: usize, col: usize, format: Arc<NumberFormat>) {
        self.cells.insert(cell_key(row, col), format);
    }

    pub fn get(&self, row: usize, col: usize) -> Option<&NumberFormat> {
        self.cells.get(&cell_key(row, col)).map(|f| f.as_ref())
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }
}

/// How a cell looks beyond its value: its font weight and slant, and the two
/// colours that make a coloured report recognisable.
///
/// Colours are `#RRGGBB`, upper-case, with any alpha dropped: ODF stores opaque
/// colours and the grid has nothing to composite against. A cell with no entry
/// in [`SheetLooks`] looks like plain text, so an unstyled sheet costs nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CellLook {
    // Only what the file actually sets is sent: a whole sheet of fills would
    // otherwise carry two false flags per cell in every page of the grid.
    #[serde(skip_serializing_if = "is_false")]
    pub bold: bool,
    #[serde(skip_serializing_if = "is_false")]
    pub italic: bool,
    /// The text colour, when the file sets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub color: Option<String>,
    /// The cell's fill, when the file sets one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub background: Option<String>,
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl CellLook {
    /// True when the cell would look the same as one with no style at all.
    pub fn is_plain(&self) -> bool {
        !self.bold && !self.italic && self.color.is_none() && self.background.is_none()
    }
}

/// The appearance of the cells that carry one, keyed by absolute position.
///
/// Sparse for the same reason [`SheetFormats`] is: most sheets style a header
/// row, some totals and a few coloured cells, not every cell they contain.
#[derive(Debug, Clone, Default)]
pub struct SheetLooks {
    cells: HashMap<u64, Arc<CellLook>>,
}

impl SheetLooks {
    pub fn insert(&mut self, row: usize, col: usize, look: Arc<CellLook>) {
        self.cells.insert(cell_key(row, col), look);
    }

    pub fn get(&self, row: usize, col: usize) -> Option<&CellLook> {
        self.cells.get(&cell_key(row, col)).map(|l| l.as_ref())
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    /// The far corner of the styled region, as `(row, col)`. A fill can extend
    /// a table past its last value, and the grid has to reach it to draw it.
    pub fn extent(&self) -> Option<(usize, usize)> {
        self.cells.keys().fold(None, |extent, key| {
            let row = (key >> 32) as usize;
            let col = (key & u32::MAX as u64) as usize;
            Some(match extent {
                Some((last_row, last_col)) => (last_row.max(row), last_col.max(col)),
                None => (row, col),
            })
        })
    }

    /// Every styled position, including ones with no value: a filled blank
    /// cell is part of how a table looks.
    pub fn iter(&self) -> impl Iterator<Item = (usize, usize, &CellLook)> {
        self.cells.iter().map(|(key, look)| {
            (
                (key >> 32) as usize,
                (key & u32::MAX as u64) as usize,
                look.as_ref(),
            )
        })
    }
}

/// The formulas the file gives its cells, keyed by absolute position.
///
/// Sparse like [`SheetFormats`]: a report is mostly values, and the formulas
/// matter only when the workbook is written back out. The text is kept exactly
/// as the reader returned it — A1-style for an Excel package, already
/// OpenFormula for an ODS — because translating it belongs to the export, which
/// can leave a cell value-only when a formula cannot be expressed faithfully.
#[derive(Debug, Clone, Default)]
pub struct SheetFormulas {
    cells: HashMap<u64, String>,
}

impl SheetFormulas {
    pub fn insert(&mut self, row: usize, col: usize, formula: impl Into<String>) {
        self.cells.insert(cell_key(row, col), formula.into());
    }

    /// The formula the file gives one cell, as the reader returned it.
    pub fn get(&self, row: usize, col: usize) -> Option<&str> {
        self.cells.get(&cell_key(row, col)).map(String::as_str)
    }

    pub fn is_empty(&self) -> bool {
        self.cells.is_empty()
    }

    pub fn iter(&self) -> impl Iterator<Item = (usize, usize, &str)> {
        self.cells.iter().map(|(key, formula)| {
            (
                (key >> 32) as usize,
                (key & u32::MAX as u64) as usize,
                formula.as_str(),
            )
        })
    }
}

/// A merged region, anchored at its top-left cell.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MergeInfo {
    pub row: usize,
    pub col: usize,
    pub rows: usize,
    pub cols: usize,
}

/// The worksheet's own geometry: how wide its columns and tall its rows are,
/// which of them it hides, which cells it merges, and which rows and columns it
/// freezes on screen.
///
/// Sizes are in CSS pixels, converted from whatever unit the package used, and
/// sparse: a sheet that leaves every column at the default stores nothing, and
/// a sheet with three wider columns stores three entries. Merged regions are a
/// plain list because the grid needs both the anchor and what it covers.
#[derive(Debug, Clone, Default)]
pub struct SheetLayout {
    pub col_widths: BTreeMap<usize, f64>,
    pub row_heights: BTreeMap<usize, f64>,
    /// The workbook's default column width, when it differs from this app's.
    pub default_col_width: Option<f64>,
    /// The workbook's default row height, when it differs from this app's.
    pub default_row_height: Option<f64>,
    /// Columns the sheet hides, counted from column A.
    ///
    /// Positions rather than widths: a hidden column still has a width, and it
    /// is the collapsed state — not a narrow measurement — that a file records
    /// and that the export has to write back.
    pub hidden_cols: BTreeSet<usize>,
    /// Rows the sheet hides, counted from row 1.
    pub hidden_rows: BTreeSet<usize>,
    pub merges: Vec<MergeInfo>,
    /// Columns pinned at the left, counted from column A. Zero means none.
    pub frozen_cols: usize,
    /// Rows pinned at the top, counted from row 1. Zero means none.
    pub frozen_rows: usize,
}

/// One worksheet, stored as a dense row table addressed by *absolute* row
/// index (row 0 is spreadsheet row 1) with each row trimmed to its last
/// non-empty column.
#[derive(Debug, Clone)]
pub struct SheetData {
    pub name: String,
    pub rows: Vec<Vec<Cell>>,
    /// Widest row, i.e. the absolute column count.
    pub cols: usize,
    /// How each formatted cell should be displayed; `General` cells are absent.
    pub formats: SheetFormats,
    /// Bold, italic and the colours of the cells that carry them.
    pub looks: SheetLooks,
    /// Column widths, row heights, merged regions and frozen panes, as the
    /// file has them.
    pub layout: SheetLayout,
    /// What each calculated cell was computed from, when the file says.
    pub formulas: SheetFormulas,
    /// Where each row sits in the file, once the records have been ordered.
    ///
    /// `None` while the rows are in the file's own order — always true of a
    /// workbook, and of a delimited file until someone sorts it. A sorted table
    /// keeps one position per row, so the file's order can be put back without
    /// reading the file again; it is a number per row, not a copy of the rows.
    pub original_order: Option<Vec<usize>>,
}

impl SheetData {
    pub fn row_count(&self) -> usize {
        self.rows.len()
    }

    pub fn cell(&self, row: usize, col: usize) -> &Cell {
        self.rows
            .get(row)
            .and_then(|r| r.get(col))
            .unwrap_or(&Cell::Empty)
    }

    /// The text the grid shows for a cell: its value through the sheet's own
    /// number format, or the plain value when no format renders it.
    pub fn text_at(&self, row: usize, col: usize) -> String {
        let cell = self.cell(row, col);
        if cell.is_empty() {
            return String::new();
        }
        self.formats
            .get(row, col)
            .and_then(|format| format.render(cell))
            .unwrap_or_else(|| cell.display())
    }

    /// First row containing at least one non-empty cell.
    pub fn first_used_row(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| r.iter().any(|c| !c.is_empty()))
    }

    /// First column containing at least one non-empty cell.
    ///
    /// With [`first_used_row`](Self::first_used_row) this is the corner of the
    /// data, which is not A1 on a report that leaves its first rows and columns
    /// empty for a title: it is the cell a jump to the data should land on.
    pub fn first_used_col(&self) -> Option<usize> {
        self.rows
            .iter()
            .filter_map(|row| row.iter().position(|cell| !cell.is_empty()))
            .min()
    }

    /// Sort the records of a delimited table by one column, from `first` on.
    ///
    /// A record is a row, so its other fields travel with it: sorting by
    /// `Units` keeps each product beside its own figure. Everything before
    /// `first` stays put — a table's title, its note, its header — and the
    /// comparison is on the text, since a delimited file has no types.
    ///
    /// Records are moved, not copied, and each field is rendered once to sort
    /// on, so a large file pays one string per record rather than one per
    /// comparison.
    pub fn sort_records(&mut self, first: usize, column: usize, descending: bool) {
        if first >= self.rows.len() || column >= self.cols {
            return;
        }
        // Each row's place in the file, so the file's own order can be put back
        // without reading it again. A second sort composes with the first: what
        // is remembered is where the rows started, not the last order asked for.
        let mut order = self
            .original_order
            .take()
            .unwrap_or_else(|| (0..self.rows.len()).collect());
        let sources = order.split_off(first);
        let mut keyed: Vec<(String, bool, Vec<Cell>, usize)> = self
            .rows
            .split_off(first)
            .into_iter()
            .zip(sources)
            .map(|(row, source)| {
                let key = row.get(column).map(Cell::display).unwrap_or_default();
                let blank = key.trim().is_empty();
                (key, blank, row, source)
            })
            .collect();
        keyed.sort_by(|a, b| order_fields(&a.0, a.1, &b.0, b.1, descending));
        let mut sorted = Vec::with_capacity(keyed.len());
        self.rows
            .extend(keyed.into_iter().map(|(_, _, row, source)| {
                sorted.push(source);
                row
            }));
        order.extend(sorted);
        // A sort that leaves every row where it was is the file's own order
        // still: there is nothing for the reader to go back to.
        self.original_order = (!is_identity(&order)).then_some(order);
    }

    /// Put the rows back in the order the file has them.
    ///
    /// Returns whether anything moved: a table nobody sorted is already in the
    /// file's order, and asking for it is not a change. Nothing is read again —
    /// the rows are moved in memory, back to the places they came from.
    pub fn restore_file_order(&mut self) -> bool {
        let Some(order) = self.original_order.take() else {
            return false;
        };
        let mut placed: Vec<(usize, Vec<Cell>)> =
            order.into_iter().zip(self.rows.drain(..)).collect();
        placed.sort_by_key(|(source, _)| *source);
        self.rows = placed.into_iter().map(|(_, row)| row).collect();
        true
    }
}

/// Whether a permutation leaves every row where it was.
fn is_identity(order: &[usize]) -> bool {
    order
        .iter()
        .enumerate()
        .all(|(index, source)| index == *source)
}

/// Order two fields of a delimited table, with blanks last either way.
///
/// A spreadsheet sorts blanks to the end whichever direction it is asked for,
/// and a viewer should not bury a record's missing value among its numbers by
/// reversing the order.
fn order_fields(a: &str, a_blank: bool, b: &str, b_blank: bool, descending: bool) -> Ordering {
    match (a_blank, b_blank) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => {
            let order = compare_fields(a, b);
            if descending {
                order.reverse()
            } else {
                order
            }
        }
    }
}

/// Compare two fields the way a person reading the table would.
///
/// Two numbers compare as numbers, so `9` comes before `10` rather than after
/// it; everything else compares case-insensitively, with the exact text as the
/// tie-break, so `Ada` and `ada` keep a stable order instead of swapping on
/// every sort. A field that is a number and one that is not falls back to text.
fn compare_fields(a: &str, b: &str) -> Ordering {
    if let (Some(left), Some(right)) = (number_of(a), number_of(b)) {
        return left.partial_cmp(&right).unwrap_or(Ordering::Equal);
    }
    let folded = a.to_lowercase().cmp(&b.to_lowercase());
    if folded == Ordering::Equal {
        a.cmp(b)
    } else {
        folded
    }
}

/// A field as a number, when the whole of it is one.
///
/// A grouped figure (`1,234`) is left as text: the comma is the separator this
/// file was read with, and guessing which comma is a thousands mark would sort
/// some files by a number and others by a string.
fn number_of(text: &str) -> Option<f64> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    trimmed
        .parse::<f64>()
        .ok()
        .filter(|value| value.is_finite())
}

/// How a delimited text file was read.
///
/// A `.csv` has nothing but bytes and the convention of a delimiter, so what it
/// was read with is worth reporting: the file may not be what its extension
/// says, and a reader that guessed wrong has to be able to say so.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TextInfo {
    /// The delimiter the file was read with: `comma`, `semicolon`, `tab` or
    /// `pipe`.
    pub delimiter: String,
    /// The encoding it was decoded with, as a name: `UTF-8`, `Windows-1252`...
    pub encoding: String,
    /// Whether the delimiter was chosen rather than detected from the file.
    pub delimiter_chosen: bool,
    pub encoding_chosen: bool,
    /// True when bytes did not decode and replacement characters stand in for
    /// them, which is what a wrong encoding looks like in the grid.
    pub undecodable: bool,
    /// The row pinned as the header when the file was last open, counted from
    /// zero as the grid counts.
    ///
    /// Absent when nobody has chosen one and the first record pins as usual;
    /// `null` when the file was deliberately left unpinned — a headerless file
    /// should not come back with a header; a number for a row.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header_row: Option<Option<usize>>,
    /// The order the records were left in, when one was chosen: the column, the
    /// direction and the column's name. The rows are already in it by the time
    /// the grid is told, so this is what the strip and the status bar read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<SortChoice>,
}

/// A parsed workbook held in application state.
#[derive(Debug, Clone)]
pub struct LoadedWorkbook {
    pub path: String,
    pub file_name: String,
    /// `xlsx`, `xls`, `xlsb`, `ods`, `csv`...
    pub format: String,
    pub file_size: u64,
    pub sheets: Vec<SheetData>,
    /// True when the cell budget was exhausted and the tail was dropped.
    pub truncated: bool,
    pub total_cells: usize,
    /// Count of non-empty cells across all sheets.
    pub used_cells: usize,
    /// Macros, links and other active content found in the package.
    pub trust: TrustReport,
    /// Set for `.csv`/`.tsv`, absent for every other format.
    pub text: Option<TextInfo>,
}

/// One cell as sent over IPC: `[kind, display, look?, hidden?, formula?]`.
/// `None` means empty.
///
/// A cell with no style still carries a third, null element so the shape is
/// uniform; a styled blank is `["z", "", look]` and is drawn, because a fill
/// on an empty cell is part of how the table looks.
///
/// The fourth element is the value the cell holds but its format does not show
/// — a zero under `#,##0;-#,##0;`, anything under `;;;` — and is null for every
/// cell the format leaves visible. The grid draws those cells with a mark and
/// the readout names what is behind the blank, so a hidden number is not
/// mistaken for an empty cell.
///
/// The fifth is the formula the file stores beside that value, in whatever
/// syntax it was written in, or null for a cell that holds only a value. The
/// grid marks those cells and shows the formula in the readout, so a calculated
/// total can be told from a typed-in number.
pub type CellView = (
    String,
    String,
    Option<CellLook>,
    Option<String>,
    Option<String>,
);

/// A window of rows returned to the UI.
#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RowsPage {
    pub sheet: usize,
    pub row_start: usize,
    pub row_count: usize,
    pub col_start: usize,
    pub col_count: usize,
    pub total_rows: usize,
    pub total_cols: usize,
    /// Outer index = row; `None` = empty cell.
    pub rows: Vec<Vec<Option<CellView>>>,
}

/// Per-sheet summary included in [`WorkbookInfo`].
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetInfo {
    pub name: String,
    pub rows: usize,
    pub cols: usize,
    pub used_cells: usize,
    /// Absolute row index of the first non-empty row, so the UI can offer
    /// "jump to data" on sheets that start far from A1.
    pub first_used_row: Option<usize>,
    /// Absolute column index of the first non-empty column, for the same jump:
    /// the two together are the corner of the data.
    pub first_used_col: Option<usize>,
    /// `[column, width in pixels]` for the columns the sheet sizes itself.
    pub col_widths: Vec<(usize, f64)>,
    /// `[row, height in pixels]` for the rows the sheet sizes itself.
    pub row_heights: Vec<(usize, f64)>,
    pub default_col_width: Option<f64>,
    pub default_row_height: Option<f64>,
    pub merges: Vec<MergeInfo>,
    /// Columns the sheet hides as inclusive `[first, last]` runs, from column A.
    ///
    /// The grid collapses them as the sheet does, so a report whose detail
    /// columns are hidden reads the way its author meant it; the UI offers to
    /// show them again, which is why the positions travel rather than a flag.
    /// Runs rather than positions keep a sheet that hides the rest of its
    /// million rows to a single pair.
    pub hidden_col_runs: Vec<(usize, usize)>,
    /// Rows the sheet hides as inclusive `[first, last]` runs, from row 1.
    pub hidden_row_runs: Vec<(usize, usize)>,
    /// Columns the sheet freezes at the left, zero when it freezes none.
    pub frozen_cols: usize,
    /// Rows the sheet freezes at the top, zero when it freezes none.
    pub frozen_rows: usize,
}

/// A sorted set of positions as inclusive `(first, last)` runs.
///
/// A hidden axis reaches the UI this way: the grid collapses a run, draws one
/// seam where it was, and can hand the whole run back at once, so the pair is
/// both smaller on the wire and closer to what is drawn.
fn runs_of(positions: &BTreeSet<usize>) -> Vec<(usize, usize)> {
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for &position in positions {
        match runs.last_mut() {
            Some((_, last)) if *last + 1 == position => *last = position,
            _ => runs.push((position, position)),
        }
    }
    runs
}

/// The workbook description returned when a file is opened.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkbookInfo {
    pub path: String,
    pub file_name: String,
    pub format: String,
    pub file_size: u64,
    pub sheets: Vec<SheetInfo>,
    pub truncated: bool,
    pub total_cells: usize,
    pub used_cells: usize,
    pub trust: TrustReport,
    /// Set for `.csv`/`.tsv`, absent for every other format. The UI offers a
    /// delimiter and encoding picker when it is there.
    pub text: Option<TextInfo>,
    /// Where the grid was left in this file, when it has been open before.
    ///
    /// Filled in by the command that opens a file, not here: the store lives
    /// in the per-user data directory, which a workbook has no business
    /// knowing about.
    pub view: Option<ViewState>,
}

impl LoadedWorkbook {
    pub fn info(&self) -> WorkbookInfo {
        WorkbookInfo {
            path: self.path.clone(),
            file_name: self.file_name.clone(),
            format: self.format.clone(),
            file_size: self.file_size,
            sheets: self
                .sheets
                .iter()
                .map(|s| SheetInfo {
                    name: s.name.clone(),
                    rows: s.row_count(),
                    cols: s.cols,
                    used_cells: s
                        .rows
                        .iter()
                        .map(|r| r.iter().filter(|c| !c.is_empty()).count())
                        .sum(),
                    first_used_row: s.first_used_row(),
                    first_used_col: s.first_used_col(),
                    col_widths: s
                        .layout
                        .col_widths
                        .iter()
                        .map(|(&col, &width)| (col, width))
                        .collect(),
                    row_heights: s
                        .layout
                        .row_heights
                        .iter()
                        .map(|(&row, &height)| (row, height))
                        .collect(),
                    default_col_width: s.layout.default_col_width,
                    default_row_height: s.layout.default_row_height,
                    merges: s.layout.merges.clone(),
                    hidden_col_runs: runs_of(&s.layout.hidden_cols),
                    hidden_row_runs: runs_of(&s.layout.hidden_rows),
                    frozen_cols: s.layout.frozen_cols,
                    frozen_rows: s.layout.frozen_rows,
                })
                .collect(),
            truncated: self.truncated,
            total_cells: self.total_cells,
            used_cells: self.used_cells,
            trust: self.trust.clone(),
            text: self.text.clone(),
            view: None,
        }
    }

    /// Sort a delimited table's records by one column, below its header row.
    ///
    /// Only a delimited file can be sorted: a workbook's rows are its layout,
    /// not a list of records, and reordering them would be an edit. `first` is
    /// the row after the header — or the top of the file when nothing is
    /// pinned — and everything above it is left where it is, so a title and a
    /// note keep their places.
    pub fn sort_text(
        &mut self,
        sheet: usize,
        header_row: Option<usize>,
        column: usize,
        descending: bool,
    ) -> Result<(), String> {
        if self.text.is_none() {
            return Err("Only a delimited text file can be sorted.".to_string());
        }
        let first = header_row.map_or(0, |row| row + 1);
        let rows = self.sheets.get(sheet).map_or(0, |data| data.rows.len());
        if first > rows {
            return Err("That header row is past the end of the file.".to_string());
        }
        let data = self
            .sheets
            .get_mut(sheet)
            .ok_or_else(|| "That worksheet no longer exists.".to_string())?;
        data.sort_records(first, column, descending);
        Ok(())
    }

    /// Extract a rectangular window of cells for the UI.
    pub fn page(
        &self,
        sheet: usize,
        row_start: usize,
        row_count: usize,
        col_start: usize,
        col_count: usize,
    ) -> Option<RowsPage> {
        let sd = self.sheets.get(sheet)?;
        let total_rows = sd.row_count();
        let total_cols = sd.cols;

        let row_end = row_count.min(total_rows.saturating_sub(row_start)) + row_start;
        let col_end = col_count.min(total_cols.saturating_sub(col_start)) + col_start;

        let mut rows = Vec::with_capacity(row_end.saturating_sub(row_start));
        for r in row_start..row_end {
            let mut out = Vec::with_capacity(col_end.saturating_sub(col_start));
            for c in col_start..col_end {
                let cell = sd.cell(r, c);
                let look = sd.looks.get(r, c);
                let formula = sd.formulas.get(r, c);
                // A cell with nothing cached but a formula beside it is still a
                // cell: the grid marks it, so dropping it would lose the fact
                // that the file calculates it.
                if cell.is_empty() && look.is_none() && formula.is_none() {
                    out.push(None);
                } else {
                    // The sheet's own format decides the text; a format that
                    // refuses the value (a 17-digit integer, a boolean) falls
                    // back to the plain rendering rather than to nothing.
                    let text = if cell.is_empty() {
                        String::new()
                    } else {
                        sd.formats
                            .get(r, c)
                            .and_then(|f| f.render(cell))
                            .unwrap_or_else(|| cell.display())
                    };
                    // A format that renders nothing hides whatever the cell
                    // holds; the plain value rides along so the UI can say so.
                    // An empty string is not hidden: there is nothing to hide.
                    let hidden = (text.is_empty() && !cell.is_empty())
                        .then(|| cell.display())
                        .filter(|value| !value.is_empty());
                    out.push(Some((
                        cell.kind().to_string(),
                        text,
                        look.cloned(),
                        hidden,
                        formula.map(str::to_string),
                    )));
                }
            }
            rows.push(out);
        }

        Some(RowsPage {
            sheet,
            row_start,
            row_count: rows.len(),
            col_start,
            col_count: col_end.saturating_sub(col_start),
            total_rows,
            total_cols,
            rows,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::NaiveDate;

    fn text_info(header_row: Option<Option<usize>>) -> TextInfo {
        TextInfo {
            delimiter: "comma".to_string(),
            encoding: "UTF-8".to_string(),
            delimiter_chosen: false,
            encoding_chosen: false,
            undecodable: false,
            header_row,
            sort: None,
        }
    }

    #[test]
    fn a_text_table_reports_its_header_row_as_absent_null_or_a_row() {
        // The UI tells these three apart: absent pins the first record as it
        // always did, null leaves the file unpinned, a number pins that row.
        let json = serde_json::to_value(text_info(None)).unwrap();
        assert!(
            !json.as_object().unwrap().contains_key("headerRow"),
            "nobody chose a header: the field is left out"
        );

        let json = serde_json::to_value(text_info(Some(None))).unwrap();
        assert_eq!(json["headerRow"], serde_json::Value::Null);

        let json = serde_json::to_value(text_info(Some(Some(2)))).unwrap();
        assert_eq!(json["headerRow"], 2);
    }

    fn sheet_of(rows: Vec<Vec<Cell>>) -> SheetData {
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        SheetData {
            name: "Report".to_string(),
            rows,
            cols,
            formats: Default::default(),
            looks: Default::default(),
            layout: Default::default(),
            formulas: Default::default(),
            original_order: None,
        }
    }

    #[test]
    fn the_corner_of_the_data_is_the_first_used_row_and_column() {
        // A report with a blank first row and two blank columns: the data
        // starts at C2, which is where a jump to data should land.
        let sheet = sheet_of(vec![
            vec![],
            vec![Cell::Empty, Cell::Empty, Cell::Text("Region".to_string())],
            vec![Cell::Empty, Cell::Empty, Cell::Text("North".to_string())],
        ]);
        assert_eq!(sheet.first_used_row(), Some(1));
        assert_eq!(sheet.first_used_col(), Some(2));
    }

    #[test]
    fn a_sheet_with_nothing_in_it_has_no_corner_to_jump_to() {
        let sheet = sheet_of(vec![vec![], vec![Cell::Empty]]);
        assert_eq!(sheet.first_used_row(), None);
        assert_eq!(sheet.first_used_col(), None);
    }

    #[test]
    fn integral_floats_render_without_decimal() {
        assert_eq!(Cell::Float(42.0).display(), "42");
        assert_eq!(Cell::Float(-7.0).display(), "-7");
        assert_eq!(Cell::Float(0.0).display(), "0");
    }

    #[test]
    fn fractional_floats_keep_precision() {
        assert_eq!(Cell::Float(0.1).display(), "0.1");
        assert_eq!(Cell::Float(12.3456).display(), "12.3456");
        assert_eq!(Cell::Float(-2.5).display(), "-2.5");
    }

    #[test]
    fn large_integers_do_not_lose_precision() {
        // 2^63-1 would be destroyed by an f64 round trip.
        let big = i64::MAX;
        assert_eq!(Cell::Int(big).display(), "9223372036854775807");
    }

    #[test]
    fn dates_drop_the_time_when_midnight() {
        let d = NaiveDate::from_ymd_opt(2026, 10, 15)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap();
        assert_eq!(Cell::DateTime(d).display(), "2026-10-15");

        let dt = NaiveDate::from_ymd_opt(2026, 10, 15)
            .unwrap()
            .and_hms_opt(13, 45, 9)
            .unwrap();
        assert_eq!(Cell::DateTime(dt).display(), "2026-10-15 13:45:09");
    }

    #[test]
    fn times_of_day_render_as_clock_times() {
        let t = NaiveTime::from_hms_opt(9, 5, 0).unwrap();
        assert_eq!(Cell::Time(t).display(), "09:05:00");
        let t = NaiveTime::from_hms_opt(23, 59, 59).unwrap();
        assert_eq!(Cell::Time(t).display(), "23:59:59");
    }

    #[test]
    fn durations_do_not_wrap_at_24_hours() {
        assert_eq!(Cell::Duration(Duration::hours(36)).display(), "36:00:00");
        assert_eq!(Cell::Duration(Duration::seconds(3723)).display(), "1:02:03");
        assert_eq!(Cell::Duration(Duration::seconds(-90)).display(), "-0:01:30");
    }

    #[test]
    fn booleans_use_spreadsheet_case() {
        assert_eq!(Cell::Bool(true).display(), "TRUE");
        assert_eq!(Cell::Bool(false).display(), "FALSE");
    }

    #[test]
    fn sheet_info_carries_geometry_in_the_front_end_shape() {
        use crate::trust::{Trust, TrustReport};

        let mut layout = SheetLayout::default();
        layout.col_widths.insert(2, 75.0);
        layout.row_heights.insert(3, 40.0);
        layout.default_col_width = Some(64.0);
        layout.default_row_height = Some(20.0);
        layout.merges.push(MergeInfo {
            row: 0,
            col: 0,
            rows: 2,
            cols: 3,
        });
        layout.frozen_cols = 1;
        layout.frozen_rows = 2;
        layout.hidden_cols = BTreeSet::from([1]);
        layout.hidden_rows = BTreeSet::from([2, 3, 9]);

        let wb = LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                original_order: None,
                name: "Sized".into(),
                rows: vec![vec![Cell::Text("a".into())]],
                cols: 1,
                formats: SheetFormats::default(),
                looks: SheetLooks::default(),
                layout,
                formulas: Default::default(),
            }],
            truncated: false,
            total_cells: 1,
            used_cells: 1,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        };

        // The front end reads these by name: `[[index, pixels], ...]` for the
        // sparse sizes and camelCase merge fields.
        let info = serde_json::to_value(wb.info()).unwrap();
        let sheet = &info["sheets"][0];
        assert_eq!(sheet["colWidths"], serde_json::json!([[2, 75.0]]));
        assert_eq!(sheet["rowHeights"], serde_json::json!([[3, 40.0]]));
        assert_eq!(sheet["defaultColWidth"], serde_json::json!(64.0));
        assert_eq!(sheet["defaultRowHeight"], serde_json::json!(20.0));
        assert_eq!(sheet["merges"][0]["row"], serde_json::json!(0));
        assert_eq!(sheet["merges"][0]["rows"], serde_json::json!(2));
        assert_eq!(sheet["merges"][0]["cols"], serde_json::json!(3));
        assert_eq!(sheet["frozenCols"], serde_json::json!(1));
        assert_eq!(sheet["frozenRows"], serde_json::json!(2));
        // Hidden axes travel as runs: a collapsed run takes no space, and one
        // pair can stand for a million hidden rows.
        assert_eq!(sheet["hiddenColRuns"], serde_json::json!([[1, 1]]));
        assert_eq!(sheet["hiddenRowRuns"], serde_json::json!([[2, 3], [9, 9]]));
    }

    #[test]
    fn a_page_carries_cell_looks_and_styled_blanks() {
        use crate::trust::{Trust, TrustReport};
        use std::sync::Arc;

        let mut looks = SheetLooks::default();
        looks.insert(
            0,
            0,
            Arc::new(CellLook {
                bold: true,
                color: Some("#1F4E79".into()),
                ..CellLook::default()
            }),
        );
        // A fill on a cell with no value still has to reach the grid.
        looks.insert(
            1,
            1,
            Arc::new(CellLook {
                background: Some("#DDEEFF".into()),
                ..CellLook::default()
            }),
        );

        let wb = LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                original_order: None,
                name: "Styled".into(),
                rows: vec![
                    vec![Cell::Text("Region".into()), Cell::Text("Qty".into())],
                    vec![Cell::Empty, Cell::Empty],
                ],
                cols: 2,
                formats: SheetFormats::default(),
                looks,
                layout: SheetLayout::default(),
                formulas: Default::default(),
            }],
            truncated: false,
            total_cells: 4,
            used_cells: 2,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        };

        let page = serde_json::to_value(wb.page(0, 0, 2, 0, 2).unwrap()).unwrap();
        // The front end reads `[kind, text, look, hidden]`, and an unset part of
        // a look is absent rather than false or null.
        assert_eq!(
            page["rows"][0][0],
            serde_json::json!(["s", "Region", {"bold": true, "color": "#1F4E79"}, null, null])
        );
        assert_eq!(
            page["rows"][0][1],
            serde_json::json!(["s", "Qty", null, null, null])
        );
        // A styled blank is a cell; an unstyled one is still absence.
        assert_eq!(
            page["rows"][1][1],
            serde_json::json!(["z", "", {"background": "#DDEEFF"}, null, null])
        );
        assert_eq!(page["rows"][1][0], serde_json::Value::Null);
    }

    #[test]
    fn hidden_axes_are_sent_as_runs() {
        // Adjacent positions join a run and a gap starts a new one, so the
        // grid gets the fewest pairs that describe what is hidden.
        assert_eq!(
            runs_of(&BTreeSet::from([4, 5, 6, 9, 11, 12])),
            vec![(4, 6), (9, 9), (11, 12)]
        );
        assert_eq!(runs_of(&BTreeSet::new()), Vec::new());
    }

    #[test]
    fn a_page_names_the_value_a_format_hides() {
        use crate::trust::{Trust, TrustReport};
        use ssfmt::DateSystem;

        // `#,##0;-#,##0;` shows nothing for a zero, which the grid has to draw
        // as a marked cell rather than as an empty one.
        let hide_zero = NumberFormat::parse("#,##0;-#,##0;", DateSystem::Date1900).unwrap();
        let mut formats = SheetFormats::default();
        for col in 0..4 {
            formats.insert(0, col, hide_zero.clone());
        }

        let wb = LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                original_order: None,
                name: "Hidden".into(),
                rows: vec![vec![
                    Cell::Float(0.0),
                    Cell::Text(String::new()),
                    Cell::Empty,
                    Cell::Float(1234.0),
                ]],
                cols: 4,
                formats,
                looks: SheetLooks::default(),
                layout: SheetLayout::default(),
                formulas: Default::default(),
            }],
            truncated: false,
            total_cells: 4,
            used_cells: 3,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        };

        let page = serde_json::to_value(wb.page(0, 0, 1, 0, 4).unwrap()).unwrap();
        // The hidden value rides along in the fourth slot, so the readout can
        // name what the blank is hiding.
        assert_eq!(
            page["rows"][0][0],
            serde_json::json!(["f", "", null, "0", null])
        );
        // An empty string holds nothing, so nothing is hidden; an empty cell is
        // still absence; and a visible value is not marked.
        assert_eq!(
            page["rows"][0][1],
            serde_json::json!(["s", "", null, null, null])
        );
        assert_eq!(page["rows"][0][2], serde_json::Value::Null);
        assert_eq!(
            page["rows"][0][3],
            serde_json::json!(["f", "1,234", null, null, null])
        );
    }

    /// A delimited table with the rows given, the way the reader builds one.
    fn text_workbook(rows: &[&[&str]]) -> LoadedWorkbook {
        use crate::trust::{Trust, TrustReport};

        let rows: Vec<Vec<Cell>> = rows
            .iter()
            .map(|row| {
                row.iter()
                    .map(|field| Cell::Text((*field).into()))
                    .collect()
            })
            .collect();
        let cols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let used: usize = rows.iter().flatten().filter(|c| !c.is_empty()).count();
        LoadedWorkbook {
            path: "memory.csv".into(),
            file_name: "memory.csv".into(),
            format: "Delimited text (csv)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                original_order: None,
                name: "table".into(),
                rows,
                cols,
                formats: SheetFormats::default(),
                looks: SheetLooks::default(),
                layout: SheetLayout::default(),
                formulas: SheetFormulas::default(),
            }],
            truncated: false,
            total_cells: used,
            used_cells: used,
            trust: TrustReport::new(Trust::unknown()),
            text: Some(TextInfo {
                delimiter: "comma".into(),
                encoding: "UTF-8".into(),
                delimiter_chosen: false,
                encoding_chosen: false,
                undecodable: false,
                header_row: Some(Some(0)),
                sort: None,
            }),
        }
    }

    fn field(book: &LoadedWorkbook, row: usize, col: usize) -> String {
        book.sheets[0].cell(row, col).display()
    }

    #[test]
    fn sorting_moves_whole_records_and_orders_numbers_as_numbers() {
        let mut book = text_workbook(&[
            &["Name", "Units"],
            &["Beta", "10"],
            &["alpha", "9"],
            &["Gamma", "2"],
        ]);

        book.sort_text(0, Some(0), 1, false).expect("sort");
        // A record is a row: the name travels with its own figure, and `9`
        // comes before `10` rather than after it.
        assert_eq!(field(&book, 0, 0), "Name", "the header stays put");
        assert_eq!(field(&book, 1, 0), "Gamma");
        assert_eq!(field(&book, 2, 0), "alpha");
        assert_eq!(field(&book, 3, 0), "Beta");

        // A second press reverses it, and the header is still the header.
        book.sort_text(0, Some(0), 1, true).expect("sort");
        assert_eq!(field(&book, 0, 0), "Name");
        assert_eq!(field(&book, 1, 0), "Beta");
        assert_eq!(field(&book, 3, 0), "Gamma");
    }

    #[test]
    fn sorting_ignores_case_and_keeps_blanks_last_both_ways() {
        let mut book = text_workbook(&[
            &["Owner", "Note"],
            &["beta", ""],
            &["Alpha", "x"],
            &["", "y"],
            &["alpha", "z"],
        ]);

        // Case-insensitive, with the exact text breaking a tie so the order is
        // stable: `Alpha` before `alpha`, then `beta`, then the blank owner.
        book.sort_text(0, Some(0), 0, false).expect("ascending");
        let names: Vec<String> = (1..4).map(|row| field(&book, row, 0)).collect();
        assert_eq!(names, ["Alpha", "alpha", "beta"]);
        assert_eq!(field(&book, 4, 0), "", "a blank sorts last");

        book.sort_text(0, Some(0), 0, true).expect("descending");
        assert_eq!(field(&book, 1, 0), "beta");
        assert_eq!(field(&book, 2, 0), "alpha");
        assert_eq!(field(&book, 3, 0), "Alpha");
        assert_eq!(field(&book, 4, 0), "", "a blank still sorts last");
    }

    #[test]
    fn sorting_starts_below_the_header_and_leaves_a_preamble_alone() {
        let mut book = text_workbook(&[
            &["Quarterly export, all regions"],
            &["Generated by the warehouse system"],
            &["Region", "Units"],
            &["South", "3"],
            &["North", "7"],
        ]);

        book.sort_text(0, Some(2), 1, false).expect("sort");
        assert_eq!(field(&book, 0, 0), "Quarterly export, all regions");
        assert_eq!(field(&book, 1, 0), "Generated by the warehouse system");
        assert_eq!(field(&book, 2, 0), "Region");
        assert_eq!(field(&book, 3, 0), "South", "3 before 7");
        assert_eq!(field(&book, 4, 0), "North");
    }

    #[test]
    fn a_workbook_is_never_sorted() {
        let mut book = text_workbook(&[&["Name", "Units"], &["Beta", "10"]]);
        book.text = None;
        let refused = book.sort_text(0, Some(0), 1, false);
        assert!(refused.is_err(), "a workbook's rows are its layout");
        assert_eq!(field(&book, 1, 0), "Beta", "the refusal changed nothing");
    }

    #[test]
    fn a_sorted_table_is_what_the_grid_is_handed() {
        let mut book = text_workbook(&[&["Name", "Units"], &["Beta", "10"], &["alpha", "9"]]);
        book.sort_text(0, Some(0), 1, false).expect("sort");

        // The grid draws whatever window `page` hands it, so the order the sort
        // produced has to be the order this returns: `9` is the first record,
        // `10` the second, and the header is still the row above both.
        let page = book.page(0, 1, 2, 0, 2).expect("page");
        let first = page.rows[0][0].clone().expect("a record");
        assert_eq!(first.1, "alpha");
        let second = page.rows[1][0].clone().expect("a record");
        assert_eq!(second.1, "Beta");
        let head = book.page(0, 0, 1, 0, 1).expect("header");
        assert_eq!(head.rows[0][0].clone().expect("a header").1, "Name");
    }

    #[test]
    fn a_sorted_table_can_go_back_to_the_order_its_file_has() {
        let mut book = text_workbook(&[
            &["Quarterly export"],
            &["Region", "Units"],
            &["South", "3"],
            &["North", "7"],
            &["East", "5"],
        ]);

        // Nothing was sorted, so there is no order to go back to and nothing
        // moves.
        assert!(!book.sheets[0].restore_file_order());

        book.sort_text(0, Some(1), 1, false).expect("sort");
        let sorted: Vec<String> = (2..5).map(|row| field(&book, row, 0)).collect();
        assert_eq!(sorted, ["South", "East", "North"], "3, 5, 7");

        assert!(book.sheets[0].restore_file_order());
        let back: Vec<String> = (2..5).map(|row| field(&book, row, 0)).collect();
        assert_eq!(back, ["South", "North", "East"], "the file's own order");
        assert_eq!(field(&book, 0, 0), "Quarterly export");
        assert_eq!(field(&book, 1, 0), "Region", "the header stays the header");
        assert!(
            book.sheets[0].original_order.is_none(),
            "nothing is left to go back to"
        );
    }

    #[test]
    fn a_second_sort_still_goes_back_to_the_file_order() {
        let mut book = text_workbook(&[
            &["Name", "Units"],
            &["Beta", "10"],
            &["alpha", "9"],
            &["Gamma", "2"],
        ]);
        let file_order: Vec<String> = (1..4).map(|row| field(&book, row, 0)).collect();

        book.sort_text(0, Some(0), 1, false).expect("ascending");
        book.sort_text(0, Some(0), 0, true).expect("by name");
        let sorted: Vec<String> = (1..4).map(|row| field(&book, row, 0)).collect();
        assert_ne!(sorted, file_order, "the second sort moved the records");

        // What is remembered is where the rows started, not the last order
        // asked for, so one press is enough however many sorts came before it.
        book.sheets[0].restore_file_order();
        let back: Vec<String> = (1..4).map(|row| field(&book, row, 0)).collect();
        assert_eq!(back, file_order);
    }

    #[test]
    fn a_sort_that_leaves_the_rows_where_they_were_keeps_no_order() {
        let mut book = text_workbook(&[&["Name"], &["Beta"], &["alpha"]]);

        // `Beta` then `alpha` is the file's order and the descending order, so
        // there is nothing for the reader to go back to.
        book.sort_text(0, Some(0), 0, true).expect("descending");
        assert!(book.sheets[0].original_order.is_none());
        assert!(!book.sheets[0].restore_file_order());
    }

    #[test]
    fn a_page_carries_the_formula_beside_its_value() {
        use crate::trust::{Trust, TrustReport};

        // A calculated cell is identifiable: the formula the file stores rides
        // along with the value it produced, and a typed-in number has none.
        let mut formulas = SheetFormulas::default();
        formulas.insert(0, 1, "B1*2");
        // A formula whose value the file did not cache is still a cell the
        // grid has to mark, so it is not dropped as empty.
        formulas.insert(0, 2, "A1+1");

        let wb = LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                original_order: None,
                name: "Calc".into(),
                rows: vec![vec![Cell::Int(4), Cell::Int(8), Cell::Empty]],
                cols: 3,
                formats: SheetFormats::default(),
                looks: SheetLooks::default(),
                layout: SheetLayout::default(),
                formulas,
            }],
            truncated: false,
            total_cells: 3,
            used_cells: 2,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        };

        let page = serde_json::to_value(wb.page(0, 0, 1, 0, 3).unwrap()).unwrap();
        assert_eq!(
            page["rows"][0][0],
            serde_json::json!(["i", "4", null, null, null])
        );
        assert_eq!(
            page["rows"][0][1],
            serde_json::json!(["i", "8", null, null, "B1*2"])
        );
        assert_eq!(
            page["rows"][0][2],
            serde_json::json!(["z", "", null, null, "A1+1"])
        );
    }

    #[test]
    fn kinds_are_stable() {
        assert_eq!(Cell::Empty.kind(), "z");
        assert_eq!(Cell::Int(1).kind(), "i");
        assert_eq!(Cell::Float(1.0).kind(), "f");
        assert_eq!(Cell::Text("a".into()).kind(), "s");
        assert_eq!(Cell::Bool(true).kind(), "b");
        assert_eq!(Cell::DateTime(Default::default()).kind(), "d");
        assert_eq!(Cell::Time(NaiveTime::MIN).kind(), "t");
        assert_eq!(Cell::Duration(Duration::zero()).kind(), "t");
        assert_eq!(Cell::Error("#N/A".into()).kind(), "e");
    }
}
