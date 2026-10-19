//! Cell and workbook data model.
//!
//! The model keeps *typed* values (not pre-rendered strings) so that an
//! export to ODS writes real numbers, dates and booleans rather than text.
//! Rendering for the grid happens separately via [`Cell::display`].

use chrono::{Duration, NaiveDateTime, NaiveTime};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::Arc;

use crate::number_format::NumberFormat;
use crate::trust::TrustReport;

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

    /// First row containing at least one non-empty cell.
    pub fn first_used_row(&self) -> Option<usize> {
        self.rows
            .iter()
            .position(|r| r.iter().any(|c| !c.is_empty()))
    }
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
}

/// One cell as sent over IPC: `[kind, display]`. `None` means empty.
pub type CellView = (String, String);

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
                })
                .collect(),
            truncated: self.truncated,
            total_cells: self.total_cells,
            used_cells: self.used_cells,
            trust: self.trust.clone(),
        }
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
                if cell.is_empty() {
                    out.push(None);
                } else {
                    // The sheet's own format decides the text; a format that
                    // refuses the value (a 17-digit integer, a boolean) falls
                    // back to the plain rendering rather than to nothing.
                    let text = sd
                        .formats
                        .get(r, c)
                        .and_then(|f| f.render(cell))
                        .unwrap_or_else(|| cell.display());
                    out.push(Some((cell.kind().to_string(), text)));
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
