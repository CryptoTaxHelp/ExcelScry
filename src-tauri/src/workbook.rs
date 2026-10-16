//! Reading spreadsheets with `calamine`.
//!
//! Everything calamine can open is accepted: `xlsx`, `xlsm`, `xlsb`, `xls`,
//! `xla` and `ods`. Cells are lifted into [`Cell`]s addressed by *absolute*
//! row/column so the grid matches what Excel would show.

use std::fs;
use std::path::Path;

use calamine::{open_workbook_auto, Data, Error as CalamineError, Reader};
use chrono::NaiveTime;

use crate::model::{Cell, LoadedWorkbook, SheetData};

/// Upper bound on materialised cells (including sparse padding), so a
/// pathologically large sheet degrades into a truncated view instead of
/// exhausting memory.
pub const MAX_CELLS: usize = 2_000_000;

/// Excel's own limits, used to clamp absurd dimensions from corrupt files.
const MAX_ROWS: usize = 1_048_576;
const MAX_COLS: usize = 16_384;

/// Load a workbook from disk.
pub fn load(path: &Path) -> Result<LoadedWorkbook, String> {
    let meta = fs::metadata(path).map_err(|e| format!("Cannot open \"{}\": {e}", path.display()))?;
    if meta.is_dir() {
        return Err(format!(
            "\"{}\" is a folder. Choose a spreadsheet file.",
            path.display()
        ));
    }

    let mut book = open_workbook_auto(path).map_err(|e| describe(path, &e))?;
    let names = book.sheet_names();

    let mut sheets = Vec::with_capacity(names.len());
    let mut truncated = false;
    let mut budget = MAX_CELLS;

    for name in &names {
        let range = match book.worksheet_range(name) {
            Ok(r) => r,
            // A single unreadable sheet should not sink the whole workbook.
            Err(e) => {
                return Err(format!("Cannot read sheet \"{name}\": {}", describe(path, &e)));
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
        let cols = rows
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

        sheets.push(SheetData {
            name: name.clone(),
            rows,
            cols,
        });

        if sheet_truncated {
            // Keep the sheets already read, but stop paying for more.
            for remaining in names.iter().skip(sheets.len()) {
                sheets.push(SheetData {
                    name: remaining.clone(),
                    rows: Vec::new(),
                    cols: 0,
                });
            }
            break;
        }
        let _ = sheet_cells;
    }

    let used_cells = sheets
        .iter()
        .map(|s| s.rows.iter().map(|r| r.iter().filter(|c| !c.is_empty()).count()).sum::<usize>())
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
        Data::DurationIso(s) => Cell::Text(s.clone()),
        Data::Error(e) => Cell::Error(error_label(e)),
    }
}

/// xlsx can carry dates as ISO strings; keep them as dates when possible.
///
/// This is also where ODS time-of-day cells land: ODF stores a bare time as an
/// ISO duration, and calamine hands it back as a datetime on Excel's day zero,
/// so a 09:30 cell would otherwise render as "1899-12-30 09:30:00".
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

const SUPPORTED: &str = "Supported formats: .xlsx, .xlsm, .xlsb, .xls and .ods.";

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
        CalamineError::Xlsx(_) | CalamineError::Xls(_) | CalamineError::Xlsb(_)
        | CalamineError::Ods(_) => format!(
            "\"{name}\" could not be opened. It may be damaged, password-protected, \
             or not really a spreadsheet. {SUPPORTED}"
        ),
        other => format!("Cannot read \"{name}\": {other}"),
    }
}

fn format_label(path: &Path) -> String {
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
        Some(other) => other,
        None => "spreadsheet",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn iso_dates_are_kept_as_dates() {
        let d = chrono::NaiveDate::from_ymd_opt(2026, 10, 15).unwrap();
        assert_eq!(
            parse_iso("2026-10-15T00:00:00"),
            Cell::DateTime(d.and_hms_opt(0, 0, 0).unwrap())
        );
        assert_eq!(parse_iso("2026-10-15"), Cell::DateTime(d.and_hms_opt(0, 0, 0).unwrap()));
        // Anything unparseable is preserved verbatim rather than dropped.
        assert_eq!(parse_iso("not a date"), Cell::Text("not a date".into()));
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
    }
}
