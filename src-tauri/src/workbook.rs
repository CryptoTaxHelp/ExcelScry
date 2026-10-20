//! Reading spreadsheets with `calamine`.
//!
//! Everything calamine can open is accepted: `xlsx`, `xlsm`, `xlsb`, `xls`,
//! `xla` and `ods`. Cells are lifted into [`Cell`]s addressed by *absolute*
//! row/column so the grid matches what Excel would show. Delimited text files
//! (`.csv`, `.tsv`) are not a package at all and go to [`crate::text_table`],
//! which builds the same shape out of records.

use std::fs;
use std::path::Path;

use calamine::{open_workbook_auto, Data, Error as CalamineError, Reader};
use chrono::{Duration, NaiveTime};

use crate::cell_styles;
use crate::model::{Cell, LoadedWorkbook, SheetData, SheetFormulas, SheetLayout, SheetLooks};
use crate::sheet_layout;
use crate::text_table;
use crate::trust::{self, TrustReport};

/// Upper bound on materialised cells (including sparse padding), so a
/// pathologically large sheet degrades into a truncated view instead of
/// exhausting memory.
pub const MAX_CELLS: usize = 2_000_000;

/// Excel's own limits, used to clamp absurd dimensions from corrupt files.
const MAX_ROWS: usize = 1_048_576;
const MAX_COLS: usize = 16_384;

/// Load a workbook from disk, with a delimited file read the way the caller
/// chose.
///
/// `dialect` only matters for `.csv`/`.tsv`; every other format carries its own
/// types and needs nothing decided. A caller with nothing to say passes
/// `Dialect::default()`, which is what an open from the command line does.
pub fn load_with(path: &Path, dialect: text_table::Dialect) -> Result<LoadedWorkbook, String> {
    let meta =
        fs::metadata(path).map_err(|e| format!("Cannot open \"{}\": {e}", path.display()))?;
    if meta.is_dir() {
        return Err(format!(
            "\"{}\" is a folder. Choose a spreadsheet file.",
            path.display()
        ));
    }

    // A delimited file is not a zip, an OLE2 file or anything else calamine
    // reads; it is text, and it becomes the same cell model a sheet would.
    if let Some(kind) = text_table::kind_of(path) {
        return text_table::load_with(path, &meta, kind, dialect);
    }

    let mut book = open_workbook_auto(path).map_err(|e| describe(path, &e))?;
    // Inspect the package before lifting any cells: what a workbook *carries*
    // is worth knowing even before it is read, and it never blocks the open.
    let trust = TrustReport::new(trust::inspect(path, &mut book));
    // The style tables live outside what the reader exposes, so they are read
    // separately; a package without them simply renders as it always has.
    let cell_styles::WorkbookFormats {
        sheets: mut formats,
        mut looks,
        ..
    } = cell_styles::read(path);
    // Column widths, row heights and merges live in the worksheet parts too,
    // and in a shape the reader does not expose.
    let mut layouts = sheet_layout::read(path).sheets;
    let names = book.sheet_names();

    let mut sheets = Vec::with_capacity(names.len());
    let mut truncated = false;
    let mut budget = MAX_CELLS;

    for name in &names {
        let range = match book.worksheet_range(name) {
            Ok(r) => r,
            // A single unreadable sheet should not sink the whole workbook.
            Err(e) => {
                return Err(format!(
                    "Cannot read sheet \"{name}\": {}",
                    describe(path, &e)
                ));
            }
        };

        let (start_row, start_col) = range.start().unwrap_or((0, 0));
        let (height, width) = range.get_size();

        let total_rows = (start_row as usize + height).min(MAX_ROWS);
        let total_cols = (start_col as usize + width).min(MAX_COLS);

        let mut rows: Vec<Vec<Cell>> = vec![Vec::new(); total_rows];
        let mut widest = 0usize;
        let mut sheet_cells = 0usize;
        let mut sheet_truncated = false;

        for (rel_row, rel_col, data) in range.used_cells() {
            if budget == 0 {
                truncated = true;
                sheet_truncated = true;
                break;
            }
            // `used_cells` yields positions relative to the range origin.
            let row = start_row as usize + rel_row;
            let col = start_col as usize + rel_col;
            if row >= total_rows || col >= total_cols {
                continue;
            }
            let cell = convert(data);
            if cell.is_empty() {
                continue;
            }

            let target = &mut rows[row];
            if target.len() <= col {
                let grow = col + 1 - target.len();
                if grow > budget {
                    truncated = true;
                    sheet_truncated = true;
                    break;
                }
                target.resize(col + 1, Cell::Empty);
                budget -= grow;
                sheet_cells += grow;
            }
            target[col] = cell;
            if col + 1 > widest {
                widest = col + 1;
            }
        }

        // Trim trailing empty columns so `cols` reflects real content.
        let mut cols = rows
            .iter()
            .map(|r| {
                r.iter()
                    .rposition(|c| !c.is_empty())
                    .map(|i| i + 1)
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0)
            .max(widest.min(total_cols));

        let layout = layouts.remove(name).unwrap_or_default();
        cols = cover_merges(&mut rows, cols, &layout);
        let sheet_looks = looks.remove(name).unwrap_or_default();
        cols = cover_looks(&mut rows, cols, &sheet_looks);

        // Formulas are what make an exported report recalculable, so they are
        // kept for the ODS writer. Reading them costs a second pass over the
        // sheet, which is not worth paying for a sheet whose values did not
        // fit, and each formula is charged to the same budget as a cell: a
        // sheet of a million formulas would otherwise cost more memory than
        // the values they computed. A reader without formulas simply says so,
        // and the sheet is exported as the values it already has.
        let mut formulas = SheetFormulas::default();
        if !sheet_truncated && budget > 0 {
            if let Ok(chain) = book.worksheet_formula(name) {
                let (formula_row, formula_col) = chain.start().unwrap_or((0, 0));
                for (rel_row, rel_col, text) in chain.used_cells() {
                    if text.trim().is_empty() {
                        continue;
                    }
                    if budget == 0 {
                        truncated = true;
                        break;
                    }
                    let row = formula_row as usize + rel_row;
                    let col = formula_col as usize + rel_col;
                    if row >= MAX_ROWS || col >= MAX_COLS {
                        continue;
                    }
                    formulas.insert(row, col, text.clone());
                    budget -= 1;
                }
            }
        }

        sheets.push(SheetData {
            name: name.clone(),
            rows,
            cols,
            formats: formats.remove(name).unwrap_or_default(),
            looks: sheet_looks,
            layout,
            formulas,
        });

        if sheet_truncated {
            // Keep the sheets already read, but stop paying for more.
            for remaining in names.iter().skip(sheets.len()) {
                sheets.push(SheetData {
                    name: remaining.clone(),
                    rows: Vec::new(),
                    cols: 0,
                    formats: Default::default(),
                    looks: Default::default(),
                    layout: Default::default(),
                    formulas: Default::default(),
                });
            }
            break;
        }
        let _ = sheet_cells;
    }

    let used_cells = sheets
        .iter()
        .map(|s| {
            s.rows
                .iter()
                .map(|r| r.iter().filter(|c| !c.is_empty()).count())
                .sum::<usize>()
        })
        .sum();

    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    Ok(LoadedWorkbook {
        path: path.display().to_string(),
        file_name,
        format: format_label(path),
        file_size: meta.len(),
        sheets,
        truncated,
        total_cells: MAX_CELLS - budget,
        used_cells,
        trust,
        text: None,
    })
}

/// Map a calamine value onto our typed [`Cell`].
fn convert(data: &Data) -> Cell {
    match data {
        Data::Empty => Cell::Empty,
        Data::Int(v) => Cell::Int(*v),
        Data::Float(v) => Cell::Float(*v),
        Data::String(s) => Cell::Text(s.clone()),
        Data::Bool(b) => Cell::Bool(*b),
        Data::DateTime(dt) => {
            if dt.is_datetime() {
                // Excel has no separate "time of day" type: a bare time is a
                // fraction of a day, so anything below 1 is a clock time.
                if dt.as_f64() >= 0.0 && dt.as_f64() < 1.0 {
                    if let Some(ndt) = dt.as_datetime() {
                        return Cell::Time(ndt.time());
                    }
                }
                match dt.as_datetime() {
                    Some(ndt) => Cell::DateTime(ndt),
                    None => Cell::Float(dt.as_f64()),
                }
            } else {
                match dt.as_duration() {
                    Some(d) => Cell::Duration(d),
                    None => Cell::Float(dt.as_f64()),
                }
            }
        }
        Data::DateTimeIso(s) => parse_iso(s),
        // ODF stores a `time` value as an ISO duration, and calamine hands it
        // back verbatim. Left as text, a 09:30 cell read `PT09H30M00S` in the
        // grid; parsed, it is the clock time the sheet actually shows.
        Data::DurationIso(s) => parse_duration(s).unwrap_or_else(|| Cell::Text(s.clone())),
        Data::Error(e) => Cell::Error(error_label(e)),
    }
}

/// xlsx can carry dates as ISO strings; keep them as dates when possible.
///
/// A writer may also emit a time of day as an ISO datetime on Excel's day zero
/// (`1899-12-30T09:30:00`), which would otherwise render as a bogus 1899 date.
/// ODF's own `time` values arrive as durations instead; see
/// [`parse_duration`].
fn parse_iso(s: &str) -> Cell {
    if let Ok(dt) = s.parse::<chrono::NaiveDateTime>() {
        if is_excel_day_zero(dt.date()) {
            return Cell::Time(dt.time());
        }
        return Cell::DateTime(dt);
    }
    if let Ok(d) = s.parse::<chrono::NaiveDate>() {
        if is_excel_day_zero(d) {
            return Cell::Time(NaiveTime::MIN);
        }
        return Cell::DateTime(d.and_hms_opt(0, 0, 0).expect("midnight is valid"));
    }
    if let Ok(t) = s.parse::<chrono::NaiveTime>() {
        return Cell::Time(t);
    }
    Cell::Text(s.to_string())
}

/// Parse an ISO 8601 duration, the form ODF uses for a `time` cell.
///
/// ODF has no time-of-day type: a bare time is written as a duration shorter
/// than 24 hours (`PT09H30M00S`). Those are shown as clock times, because that
/// is what the spreadsheet displays. Anything a day or longer — and anything
/// negative — is a span, so it keeps the `[h]:mm:ss` rendering that does not
/// wrap at 24 hours (`PT36H` → `36:00:00`). Returns `None` for anything that
/// is not a duration, so the caller can keep the original text.
fn parse_duration(s: &str) -> Option<Cell> {
    let trimmed = s.trim();
    let (negative, body) = match trimmed.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, trimmed),
    };
    let body = body.strip_prefix('P')?;

    let (day_part, time_part) = match body.split_once('T') {
        Some((days, time)) => (days, Some(time)),
        None => (body, None),
    };
    // A duration needs at least one component; `P` and `PT` alone are not
    // durations, and treating them as zero would turn junk into 00:00:00.
    let mut saw_component = false;
    let days: f64 = if day_part.is_empty() {
        0.0
    } else {
        saw_component = true;
        day_part.strip_suffix('D')?.parse().ok()?
    };

    let mut seconds = 0.0f64;
    if let Some(time_part) = time_part {
        let mut number = String::new();
        for ch in time_part.chars() {
            match ch {
                '0'..='9' | '.' => number.push(ch),
                'H' | 'M' | 'S' => {
                    if number.is_empty() {
                        return None;
                    }
                    let value: f64 = number.parse().ok()?;
                    seconds += match ch {
                        'H' => value * 3600.0,
                        'M' => value * 60.0,
                        _ => value,
                    };
                    saw_component = true;
                    number.clear();
                }
                _ => return None,
            }
        }
        if !number.is_empty() {
            return None;
        }
    }
    if !saw_component {
        return None;
    }

    let total = (days * 86_400.0 + seconds) * if negative { -1.0 } else { 1.0 };
    if (0.0..86_400.0).contains(&total) {
        let whole = total.trunc() as u64;
        let nanos = ((total - total.trunc()) * 1_000_000_000.0).round() as u32;
        let time = NaiveTime::from_num_seconds_from_midnight_opt(whole as u32, nanos)?;
        return Some(Cell::Time(time));
    }
    Some(Cell::Duration(Duration::nanoseconds(
        (total * 1_000_000_000.0).round() as i64,
    )))
}

/// Excel's day zero. A "date" of 1899-12-30 can only mean a bare time of day:
/// no real spreadsheet has an entry on that date.
fn is_excel_day_zero(date: chrono::NaiveDate) -> bool {
    use chrono::Datelike;
    date.year() == 1899 && date.month() == 12 && date.day() == 30
}

fn error_label(e: &calamine::CellErrorType) -> String {
    use calamine::CellErrorType::*;
    match e {
        Div0 => "#DIV/0!",
        NA => "#N/A",
        Name => "#NAME?",
        Null => "#NULL!",
        Num => "#NUM!",
        Ref => "#REF!",
        Value => "#VALUE!",
        GettingData => "#GETTING_DATA",
    }
    .to_string()
}

const SUPPORTED: &str = "Supported formats: .xlsx, .xlsm, .xlsb, .xls, .ods, .csv and .tsv.";

/// Turn calamine's errors into something a user can act on.
fn describe(path: &Path, e: &CalamineError) -> String {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());
    match e {
        CalamineError::Msg(m) if m.contains("Cannot detect file format") => {
            format!("\"{name}\" is not a spreadsheet ExcelScry can read. {SUPPORTED}")
        }
        CalamineError::Io(io) => format!("Cannot read \"{name}\": {io}"),
        // A damaged file, a renamed file, and a workbook protected with a
        // password all surface as a format-level parse error. Say that plainly
        // instead of leaking a zip/xml diagnostic at the user.
        CalamineError::Xlsx(_)
        | CalamineError::Xls(_)
        | CalamineError::Xlsb(_)
        | CalamineError::Ods(_) => format!(
            "\"{name}\" could not be opened. It may be damaged, password-protected, \
             or not really a spreadsheet. {SUPPORTED}"
        ),
        other => format!("Cannot read \"{name}\": {other}"),
    }
}

pub(crate) fn format_label(path: &Path) -> String {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .as_deref()
    {
        Some("xlsx") | Some("xlam") => "Excel Workbook (xlsx)",
        Some("xlsm") => "Excel Macro-Enabled Workbook (xlsm)",
        Some("xls") | Some("xla") => "Excel 97-2003 (xls)",
        Some("xlsb") => "Excel Binary (xlsb)",
        Some("ods") => "OpenDocument (ods)",
        Some("csv") => "Comma-Separated Values (csv)",
        Some("tsv") | Some("tab") => "Tab-Separated Values (tsv)",
        Some(other) => other,
        None => "spreadsheet",
    }
    .to_string()
}

/// Grow a sheet's extent to include every merged region's far corner.
///
/// A merge is part of the sheet even when only its anchor holds a value, and
/// the grid cannot draw a region that falls outside the rows it was told about.
/// Returns the column count the sheet needs.
fn cover_merges(rows: &mut Vec<Vec<Cell>>, cols: usize, layout: &SheetLayout) -> usize {
    let mut cols = cols;
    for merge in &layout.merges {
        cols = cols.max((merge.col + merge.cols).min(MAX_COLS));
    }
    let merged_rows = layout
        .merges
        .iter()
        .map(|m| m.row + m.rows)
        .max()
        .unwrap_or(0)
        .min(MAX_ROWS);
    if merged_rows > rows.len() {
        rows.resize(merged_rows, Vec::new());
    }
    cols
}

/// Extend the sheet to its furthest styled cell.
///
/// A fill on a blank cell draws part of the table even when no value reaches
/// that row or column, so the grid has to be at least that large. The rows are
/// empty vectors, not materialised cells; the cell budget is untouched.
fn cover_looks(rows: &mut Vec<Vec<Cell>>, cols: usize, looks: &SheetLooks) -> usize {
    let Some((last_row, last_col)) = looks.extent() else {
        return cols;
    };
    let styled_rows = (last_row + 1).min(MAX_ROWS);
    if styled_rows > rows.len() {
        rows.resize(styled_rows, Vec::new());
    }
    cols.max((last_col + 1).min(MAX_COLS))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The default read, which is what an open from the command line does.
    fn load(path: &Path) -> Result<LoadedWorkbook, String> {
        load_with(path, text_table::Dialect::default())
    }

    #[test]
    fn missing_file_is_reported_clearly() {
        let err = load(Path::new("definitely-not-here-12345.xlsx")).unwrap_err();
        assert!(err.contains("Cannot open"), "unexpected: {err}");
    }

    #[test]
    fn folder_is_rejected() {
        let err = load(Path::new(".")).unwrap_err();
        assert!(err.contains("is a folder"), "unexpected: {err}");
    }

    #[test]
    fn non_spreadsheet_is_rejected_with_a_hint() {
        let dir = std::env::temp_dir().join("excelscry-test-junk");
        fs::create_dir_all(&dir).unwrap();
        let p = dir.join("notes.xlsx");
        fs::write(&p, b"this is definitely not a workbook").unwrap();
        let err = load(&p).unwrap_err();
        assert!(err.contains("Supported formats"), "unexpected: {err}");
        // A raw parser diagnostic such as "Zip error: Could not find EOCD"
        // must not reach the user.
        assert!(!err.contains("EOCD"), "leaked a parser error: {err}");
        assert!(!err.contains("Zip error"), "leaked a parser error: {err}");
        let _ = fs::remove_file(&p);
    }

    #[test]
    fn iso_times_of_day_are_not_read_as_1899_dates() {
        // This is exactly what calamine returns for an ODS `time` cell.
        assert_eq!(
            parse_iso("1899-12-30T09:30:00"),
            Cell::Time(NaiveTime::from_hms_opt(9, 30, 0).unwrap())
        );
        assert_eq!(
            parse_iso("1899-12-30T23:59:59"),
            Cell::Time(NaiveTime::from_hms_opt(23, 59, 59).unwrap())
        );
    }

    #[test]
    fn ods_time_values_are_clock_times_not_durations() {
        // A bare time is an ISO duration under 24 hours in ODF.
        assert_eq!(
            parse_duration("PT09H30M00S"),
            Some(Cell::Time(NaiveTime::from_hms_opt(9, 30, 0).unwrap()))
        );
        assert_eq!(
            parse_duration("PT00H00M00S"),
            Some(Cell::Time(NaiveTime::MIN))
        );
        // Fractional seconds survive rather than being rounded away.
        assert_eq!(
            parse_duration("PT09H30M00.5S"),
            Some(Cell::Time(
                NaiveTime::from_hms_milli_opt(9, 30, 0, 500).unwrap()
            ))
        );
    }

    #[test]
    fn ods_spans_longer_than_a_day_stay_durations() {
        // 36 hours must render as 36:00:00, not as a clock time or as text.
        let cell = parse_duration("PT36H00M00S").expect("a duration");
        assert_eq!(cell, Cell::Duration(Duration::hours(36)));
        assert_eq!(cell.display(), "36:00:00");

        // Days are part of the form an ODF writer may use.
        assert_eq!(
            parse_duration("P1DT12H"),
            Some(Cell::Duration(Duration::hours(36)))
        );
        assert_eq!(
            parse_duration("P1D"),
            Some(Cell::Duration(Duration::hours(24)))
        );
        // A negative span keeps its sign instead of becoming a bogus time.
        assert_eq!(
            parse_duration("-PT00H01M30S"),
            Some(Cell::Duration(Duration::seconds(-90)))
        );
    }

    #[test]
    fn anything_that_is_not_a_duration_is_left_as_text() {
        for value in ["", "9:30", "PT", "PT9", "PT9H30", "tomorrow", "P1Y"] {
            assert_eq!(parse_duration(value), None, "{value} is not a duration");
        }
    }

    #[test]
    fn iso_dates_are_kept_as_dates() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        assert_eq!(
            parse_iso("2026-10-15T00:00:00"),
            Cell::DateTime(d.and_hms_opt(0, 0, 0).unwrap())
        );
        assert_eq!(
            parse_iso("2026-10-15"),
            Cell::DateTime(d.and_hms_opt(0, 0, 0).unwrap())
        );
        // Anything unparseable is preserved verbatim rather than dropped.
        assert_eq!(parse_iso("not a date"), Cell::Text("not a date".into()));
    }

    #[test]
    fn an_opendocument_spreadsheet_opens_as_a_workbook() {
        // The fixture is written by scripts/make_ods_sample.py as plain ODF XML,
        // not by this crate's exporter, so this reads the package a real
        // producer (LibreOffice, Google Sheets) would hand the app.
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("samples")
            .join("ExcelScry-sample.ods");
        let wb = load(&path).expect("an ods should open");

        assert_eq!(wb.format, "OpenDocument (ods)");
        assert_eq!(wb.file_name, "ExcelScry-sample.ods");
        assert!(wb.file_size > 0);
        assert!(!wb.truncated);
        assert_eq!(
            wb.sheets
                .iter()
                .map(|s| s.name.as_str())
                .collect::<Vec<_>>(),
            vec!["Sales report", "Wide sheet"]
        );

        let sheet = &wb.sheets[0];
        // The table starts at C6, so the grid has to keep the offset.
        assert_eq!(sheet.cell(5, 2), &Cell::Text("Region".into()));
        assert_eq!(sheet.cell(5, 9), &Cell::Text("Start time".into()));
        assert_eq!(sheet.cell(6, 3), &Cell::Text("Widget".into()));
        assert_eq!(sheet.cell(6, 4), &Cell::Float(1200.0));
        assert_eq!(
            sheet.cell(6, 7),
            &Cell::DateTime(
                chrono::NaiveDate::from_ymd_opt(2026, 1, 15)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap()
            )
        );
        // ODF stores a time of day as a duration; it must not read as 1899.
        assert_eq!(
            sheet.cell(6, 9),
            &Cell::Time(NaiveTime::from_hms_opt(9, 30, 0).unwrap())
        );
        assert_eq!(
            sheet.cell(6, 9).display(),
            "09:30:00",
            "a time of day rendered as a date"
        );
        // A blank row inside the range stays a row, and the record after it
        // keeps its place rather than sliding up.
        assert!(sheet.rows[10].iter().all(|c| c.is_empty()));
        assert_eq!(
            sheet.cell(11, 8),
            &Cell::Text("<b>not bold</b> & a bare ampersand".into()),
            "text must not be interpreted as markup"
        );
        assert_eq!(sheet.cell(8, 8), &Cell::Bool(true));

        // A sheet wider than the viewport is loaded in full, not clipped.
        assert_eq!(wb.sheets[1].cols, 60);
        assert_eq!(wb.sheets[1].cell(0, 59), &Cell::Text("col 60".into()));
    }

    /// The text the grid would show at one cell.
    fn shown(wb: &LoadedWorkbook, sheet: usize, row: usize, col: usize) -> String {
        let page = wb.page(sheet, row, 1, col, 1).expect("the sheet exists");
        page.rows[0][0]
            .as_ref()
            .map(|(_, text, _, _, _)| text.clone())
            .unwrap_or_default()
    }

    fn sample(name: &str) -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("samples")
            .join(name)
    }

    #[test]
    fn an_opendocument_spreadsheet_shows_its_number_formats() {
        let wb = load(&sample("ExcelScry-sample.ods")).expect("an ods should open");
        // The table starts at C6; row 6 is its first data row.
        assert_eq!(shown(&wb, 0, 6, 4), "1,200", "units are grouped");
        assert_eq!(shown(&wb, 0, 6, 5), "$19.99", "unit price is money");
        assert_eq!(shown(&wb, 0, 6, 6), "$23,988.00", "revenue is money");
        assert_eq!(shown(&wb, 0, 6, 7), "15/01/2026", "the date is day-first");
        assert_eq!(shown(&wb, 0, 6, 9), "09:30", "the time is hh:mm");
        assert_eq!(shown(&wb, 0, 6, 10), "35.0%", "margin is a percentage");
        // The values themselves are untouched, so an export still writes numbers.
        assert_eq!(wb.sheets[0].cell(6, 6), &Cell::Float(23988.0));
    }

    #[test]
    fn a_workbook_reports_the_sheet_geometry_it_has() {
        let wb = load(&sample("ExcelScry-sample.xlsx")).expect("an xlsx should open");
        let info = wb.info();
        let sheet = &info.sheets[0];

        // The generator sizes the table's columns and leaves A and B alone.
        assert_eq!(sheet.default_row_height, Some(20.0));
        assert!(sheet.default_col_width.is_some());
        assert!(
            sheet.col_widths.contains(&(2, 75.0)),
            "column C should carry its own width: {:?}",
            sheet.col_widths
        );
        assert_eq!(wb.sheets[0].layout.col_widths.get(&2), Some(&75.0));
        assert!(
            sheet.row_heights.is_empty(),
            "the sample sets no row heights"
        );
        assert!(sheet.merges.is_empty());

        // The sample's total row is bold on a pale blue fill; both live in
        // `styles.xml` and are read beside the widths.
        let look = wb.sheets[0]
            .looks
            .get(5, 2)
            .expect("C6 should carry the total row's look");
        assert!(look.bold, "C6 is bold in the sample");
        assert_eq!(look.background.as_deref(), Some("#DCE6F1"));

        // And the look reaches the page the grid asks for, not only the model.
        let page = wb.page(0, 5, 1, 2, 1).expect("the sheet exists");
        let view = page.rows[0][0].as_ref().expect("C6 is not empty");
        let look = view.2.as_ref().expect("C6 carries its look");
        assert!(look.bold);
        assert_eq!(look.background.as_deref(), Some("#DCE6F1"));
    }

    #[test]
    fn merged_regions_extend_the_sheet() {
        use crate::model::MergeInfo;

        let mut rows = vec![Vec::new(); 1];
        let mut layout = SheetLayout::default();
        layout.merges.push(MergeInfo {
            row: 2,
            col: 4,
            rows: 3,
            cols: 2,
        });

        let cols = cover_merges(&mut rows, 1, &layout);
        assert_eq!(cols, 6, "the grid has to reach the merge's far column");
        assert_eq!(rows.len(), 5, "and its last row");
        assert!(rows.iter().all(Vec::is_empty));
    }

    #[test]
    fn a_styled_blank_extends_the_sheet() {
        use crate::model::{CellLook, SheetLooks};
        use std::sync::Arc;

        let mut rows = vec![Vec::new()];
        let mut looks = SheetLooks::default();
        looks.insert(
            3,
            4,
            Arc::new(CellLook {
                background: Some("#DDEEFF".into()),
                ..CellLook::default()
            }),
        );

        let cols = cover_looks(&mut rows, 1, &looks);
        assert_eq!(cols, 5, "the grid has to reach the fill's far column");
        assert_eq!(rows.len(), 4, "and its last row");
        assert!(rows.iter().all(Vec::is_empty));
    }

    #[test]
    fn an_excel_workbook_shows_its_number_formats() {
        let wb = load(&sample("ExcelScry-sample.xlsx")).expect("an xlsx should open");
        assert_eq!(shown(&wb, 0, 6, 4), "1,200");
        assert_eq!(shown(&wb, 0, 6, 5), "$19.99");
        assert_eq!(shown(&wb, 0, 6, 6), "$23,988.00");
        assert_eq!(shown(&wb, 0, 6, 7), "15/01/2026");
        assert_eq!(shown(&wb, 0, 6, 10), "09:30");
        assert_eq!(shown(&wb, 0, 6, 11), "35.0%");
        // A cell without a format keeps the plain rendering.
        assert_eq!(shown(&wb, 0, 6, 2), "North");
    }

    #[test]
    fn a_comma_separated_file_opens_in_the_grid() {
        let wb = load(&sample("ExcelScry-sample.csv")).expect("a csv should open");
        assert_eq!(wb.format, "Comma-Separated Values (csv)");
        assert_eq!(wb.file_name, "ExcelScry-sample.csv");
        assert_eq!(wb.sheets.len(), 1);
        assert_eq!(wb.sheets[0].name, "ExcelScry-sample");
        assert!(!wb.truncated);
        assert_eq!(wb.trust.badge, "No macros or links");

        let sheet = &wb.sheets[0];
        assert_eq!(sheet.cell(0, 0), &Cell::Text("Region".into()));
        assert_eq!(sheet.cell(1, 6), &Cell::Text("repeat customer".into()));
        // A quoted field keeps the delimiter it contains.
        assert_eq!(
            sheet.cell(3, 6),
            &Cell::Text("backordered, with a comma".into())
        );
        // A doubled quote is one quote.
        assert_eq!(sheet.cell(4, 6), &Cell::Text("said \"ship it\"".into()));
        // A value that only looks numeric is left exactly as written.
        assert_eq!(sheet.cell(5, 2), &Cell::Text("000123".into()));
        // And a quoted field may span lines.
        assert_eq!(shown(&wb, 0, 5, 6), "two\nlines");
        // The blank record between real ones is still a row, so the record
        // after it keeps its place.
        assert!(sheet.rows[6].iter().all(Cell::is_empty));
        assert_eq!(
            sheet.cell(7, 6),
            &Cell::Text("café ✅ <b>not bold</b>".into()),
            "text must not be interpreted as markup"
        );
    }

    #[test]
    fn a_tab_separated_file_opens_in_the_grid() {
        let wb = load(&sample("ExcelScry-sample.tsv")).expect("a tsv should open");
        assert_eq!(wb.format, "Tab-Separated Values (tsv)");
        assert_eq!(wb.sheets[0].name, "ExcelScry-sample");
        let sheet = &wb.sheets[0];
        assert_eq!(sheet.cell(0, 0), &Cell::Text("Region".into()));
        // The comma belongs to the note; on a tab file it is not a delimiter.
        assert_eq!(
            sheet.cell(3, 6),
            &Cell::Text("backordered, with a comma".into())
        );
        assert_eq!(shown(&wb, 0, 1, 3), "19.99");
    }

    /// A minimal xlsx package: one sheet whose B column is calculated, with
    /// the filled-down range Excel writes as a shared formula.
    fn workbook_with_formulas(name: &str) -> std::path::PathBuf {
        use std::io::Write;

        const CONTENT_TYPES: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types">
  <Default Extension="rels" ContentType="application/vnd.openxmlformats-package.relationships+xml"/>
  <Default Extension="xml" ContentType="application/xml"/>
  <Override PartName="/xl/workbook.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml"/>
  <Override PartName="/xl/worksheets/sheet1.xml" ContentType="application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml"/>
</Types>"#;

        const ROOT_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument" Target="xl/workbook.xml"/>
</Relationships>"#;

        const WORKBOOK: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Report" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;

        const WORKBOOK_RELS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;

        const SHEET: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"><sheetData>
  <row r="1"><c r="A1" t="inlineStr"><is><t>Region</t></is></c><c r="B1" t="inlineStr"><is><t>Qty</t></is></c></row>
  <row r="2"><c r="A2" t="inlineStr"><is><t>North</t></is></c><c r="B2"><f t="shared" ref="B2:B4" si="0">A2*10</f><v>20</v></c></row>
  <row r="3"><c r="A3" t="inlineStr"><is><t>South</t></is></c><c r="B3"><f t="shared" si="0"/><v>30</v></c></row>
  <row r="4"><c r="A4" t="inlineStr"><is><t>Sum</t></is></c><c r="B4"><f>SUM(B2:B3)</f><v>50</v></c></row>
</sheetData></worksheet>"#;

        let dir = std::env::temp_dir().join("excelscry-formula-tests");
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let _ = fs::remove_file(&path);
        let file = fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (part, body) in [
            ("[Content_Types].xml", CONTENT_TYPES),
            ("_rels/.rels", ROOT_RELS),
            ("xl/workbook.xml", WORKBOOK),
            ("xl/_rels/workbook.xml.rels", WORKBOOK_RELS),
            ("xl/worksheets/sheet1.xml", SHEET),
        ] {
            zip.start_file(part, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    #[test]
    fn formulas_are_read_beside_the_values_they_computed() {
        let path = workbook_with_formulas("formulas.xlsx");
        let wb = load(&path).expect("an xlsx should open");
        let _ = fs::remove_file(&path);

        let sheet = &wb.sheets[0];
        // The values are what the grid shows, exactly as before.
        assert_eq!(sheet.cell(1, 1), &Cell::Float(20.0));
        assert_eq!(sheet.cell(2, 1), &Cell::Float(30.0));

        // The formulas are kept in Excel's spelling at their true positions,
        // including the one Excel only stores once as a shared formula.
        let mut found: Vec<(usize, usize, String)> = sheet
            .formulas
            .iter()
            .map(|(row, col, text)| (row, col, text.to_string()))
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                (1, 1, "A2*10".to_string()),
                (2, 1, "A3*10".to_string()),
                (3, 1, "SUM(B2:B3)".to_string()),
            ],
            "the sheet's formulas did not come through"
        );
    }

    #[test]
    fn format_labels_are_human_readable() {
        assert_eq!(format_label(Path::new("a.XLSX")), "Excel Workbook (xlsx)");
        assert_eq!(
            format_label(Path::new("a.xlsm")),
            "Excel Macro-Enabled Workbook (xlsm)"
        );
        assert_eq!(format_label(Path::new("a.xls")), "Excel 97-2003 (xls)");
        assert_eq!(format_label(Path::new("a.xlsb")), "Excel Binary (xlsb)");
        assert_eq!(format_label(Path::new("a.ods")), "OpenDocument (ods)");
        assert_eq!(
            format_label(Path::new("a.csv")),
            "Comma-Separated Values (csv)"
        );
        assert_eq!(
            format_label(Path::new("a.tsv")),
            "Tab-Separated Values (tsv)"
        );
    }
}
