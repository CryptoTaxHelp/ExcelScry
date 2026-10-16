//! Writing the loaded workbook back out as an OpenDocument spreadsheet.
//!
//! Values keep their type: a number is written as `office:value-type="float"`,
//! a date as `office:value-type="date"` and so on, so the result opens cleanly
//! in LibreOffice, Excel and Google Sheets as a real spreadsheet rather than a
//! wall of text.

use std::path::Path;

use serde::Serialize;
use spreadsheet_ods::format::ValueFormatText;
use spreadsheet_ods::{write_ods, CellStyle, CellStyleRef, Sheet, WorkBook};

use crate::model::{Cell, LoadedWorkbook};
#[cfg(test)]
use crate::trust::{Trust, TrustReport};

/// What the user gets told after a successful export.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExportSummary {
    pub path: String,
    pub sheets: usize,
    pub cells: usize,
    pub bytes: u64,
    /// Cells that could not be represented exactly (currently never set, but
    /// kept so the UI contract is stable).
    pub warnings: Vec<String>,
}

/// Write `workbook` to `out` as ODS.
pub fn export(workbook: &LoadedWorkbook, out: &Path) -> Result<ExportSummary, String> {
    if workbook.sheets.is_empty() {
        return Err("This workbook has no sheets to export.".to_string());
    }

    let mut book = WorkBook::new_empty();

    // A bold first row style, so an exported sheet reads like a table.
    let header = {
        let fmt = book.add_text_format(ValueFormatText::new_named("scry_header_text"));
        let mut style = CellStyle::new("scry_header", &fmt);
        style.set_font_bold();
        book.add_cellstyle(style)
    };

    let mut cells = 0usize;

    for sheet in &workbook.sheets {
        let mut ods_sheet = Sheet::new(sanitize_sheet_name(&sheet.name));
        let header_row = sheet.first_used_row();

        for (row_idx, row) in sheet.rows.iter().enumerate() {
            for (col_idx, cell) in row.iter().enumerate() {
                if cell.is_empty() {
                    continue;
                }
                let row_u32 = row_idx as u32;
                let col_u32 = col_idx as u32;
                let styled = header_row == Some(row_idx);

                match cell {
                    Cell::Empty => continue,
                    Cell::Int(v) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *v as f64, styled, &header)
                    }
                    Cell::Float(v) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *v, styled, &header)
                    }
                    Cell::Text(s) => write_cell(
                        &mut ods_sheet,
                        row_u32,
                        col_u32,
                        s.as_str(),
                        styled,
                        &header,
                    ),
                    Cell::Bool(b) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *b, styled, &header)
                    }
                    Cell::DateTime(dt) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *dt, styled, &header)
                    }
                    Cell::Time(t) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *t, styled, &header)
                    }
                    Cell::Duration(d) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *d, styled, &header)
                    }
                    // Errors have no ODS value type of their own; keep the text.
                    Cell::Error(e) => write_cell(
                        &mut ods_sheet,
                        row_u32,
                        col_u32,
                        e.as_str(),
                        styled,
                        &header,
                    ),
                }
                cells += 1;
            }
        }

        book.push_sheet(ods_sheet);
    }

    write_ods(&mut book, out).map_err(|e| format!("Could not write \"{}\": {e}", out.display()))?;

    let bytes = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);

    Ok(ExportSummary {
        path: out.display().to_string(),
        sheets: workbook.sheets.len(),
        cells,
        bytes,
        warnings: Vec::new(),
    })
}

/// Write one value, optionally with the bold header style applied.
fn write_cell<V: Into<spreadsheet_ods::Value>>(
    sheet: &mut Sheet,
    row: u32,
    col: u32,
    value: V,
    styled: bool,
    style: &CellStyleRef,
) {
    if styled {
        sheet.set_styled_value(row, col, value, style);
    } else {
        sheet.set_value(row, col, value);
    }
}

/// ODF forbids a few characters in table names, and Excel caps them at 31.
fn sanitize_sheet_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '[' | ']' | '*' | '?' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    let out = if trimmed.is_empty() { "Sheet" } else { trimmed };
    out.chars().take(31).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Cell as C, SheetData};
    use calamine::Data;
    use chrono::{Duration, NaiveDate, NaiveTime};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("excelscry-ods-tests");
        std::fs::create_dir_all(&dir).unwrap();
        dir.join(name)
    }

    /// `DATA_ROW` / `DATA_COL` are absolute 0-based coordinates, i.e.
    /// spreadsheet row 5 / column C.
    const DATA_ROW: usize = 4;
    const DATA_COL: usize = 2;

    /// The calendar day of a date cell, whichever representation the ODS
    /// reader chose (it reports ODS dates as ISO strings).
    fn day_of(data: &Data) -> Option<String> {
        match data {
            Data::DateTime(dt) => dt.as_datetime().map(|d| d.date().to_string()),
            Data::DateTimeIso(s) => s.split(['T', ' ']).next().map(str::to_string),
            _ => None,
        }
    }

    /// `(hours, minutes)` of a duration, again accepting either form.
    fn hours_minutes(data: &Data) -> Option<(i64, i64)> {
        match data {
            Data::DateTime(dt) => dt
                .as_duration()
                .map(|d| (d.num_hours(), d.num_minutes() % 60)),
            Data::DurationIso(s) => parse_iso_hours_minutes(s),
            // ODF writes a time of day as an ISO duration, but calamine hands
            // it back as a datetime on Excel's day zero.
            Data::DateTimeIso(s) if s.starts_with("1899-12-30T") => {
                let (_, clock) = s.split_once('T')?;
                let (hours, rest) = clock.split_once(':')?;
                let (minutes, _seconds) = rest.split_once(':')?;
                Some((hours.parse().ok()?, minutes.parse().ok()?))
            }
            _ => None,
        }
    }

    /// Read the hours and minutes out of an ISO-8601 duration ("PT36H0M0.0S").
    fn parse_iso_hours_minutes(text: &str) -> Option<(i64, i64)> {
        let mut digits = String::new();
        let mut hours = 0i64;
        let mut minutes = 0i64;
        for ch in text.strip_prefix("PT")?.chars() {
            match ch {
                '0'..='9' | '.' => digits.push(ch),
                'H' => {
                    hours = digits.parse::<f64>().ok()? as i64;
                    digits.clear();
                }
                'M' => {
                    minutes = digits.parse::<f64>().ok()? as i64;
                    digits.clear();
                }
                'S' => digits.clear(),
                _ => return None,
            }
        }
        Some((hours, minutes))
    }

    fn workbook_with_block_at_row5_col_c() -> LoadedWorkbook {
        // Deliberately place the block away from A1: the export must keep it
        // at its true position rather than shifting it to the top-left.
        let mut rows: Vec<Vec<C>> = vec![Vec::new(); DATA_ROW];
        rows.push(vec![
            C::Empty,
            C::Empty,
            C::Text("Region".into()),
            C::Text("Qty".into()),
        ]);
        rows.push(vec![
            C::Empty,
            C::Empty,
            C::Text("North".into()),
            C::Int(1_234_567_890_123),
        ]);
        rows.push(vec![
            C::Empty,
            C::Empty,
            C::Text("South".into()),
            C::Float(2.5),
        ]);
        rows.push(vec![
            C::Empty,
            C::Empty,
            C::Bool(true),
            C::DateTime(
                NaiveDate::from_ymd_opt(2026, 10, 15)
                    .unwrap()
                    .and_hms_opt(0, 0, 0)
                    .unwrap(),
            ),
        ]);
        rows.push(vec![
            C::Empty,
            C::Empty,
            C::Duration(Duration::hours(36)),
            C::Time(NaiveTime::from_hms_opt(9, 30, 0).unwrap()),
        ]);

        LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                name: "Report".into(),
                rows,
                cols: 4,
            }],
            truncated: false,
            total_cells: 20,
            used_cells: 10,
            trust: TrustReport::new(Trust::unknown()),
        }
    }

    #[test]
    fn export_produces_a_readable_ods() {
        let wb = workbook_with_block_at_row5_col_c();
        let out = temp_path("roundtrip.ods");
        let _ = std::fs::remove_file(&out);

        let summary = export(&wb, &out).expect("export should succeed");
        assert_eq!(summary.sheets, 1);
        assert!(summary.bytes > 0, "ods should not be empty");
        assert!(
            summary.bytes > 1000,
            "an ods is a zip with several parts, got {} bytes",
            summary.bytes
        );

        // It must be a real zip archive.
        let head = std::fs::read(&out).unwrap();
        assert_eq!(&head[0..2], b"PK", "ods must be a zip container");

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn exported_dates_ranges_values_and_offsets_survive_a_round_trip() {
        use calamine::{open_workbook_auto, Reader};

        let wb = workbook_with_block_at_row5_col_c();
        let out = temp_path("verify.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).unwrap();

        let mut back = open_workbook_auto(&out).expect("reopen exported ods");
        assert_eq!(back.sheet_names(), vec!["Report".to_string()]);

        let range = back.worksheet_range("Report").unwrap();
        // The block began at spreadsheet row 5, column C.
        assert_eq!(
            range.start(),
            Some((DATA_ROW as u32, DATA_COL as u32)),
            "exported data must keep its absolute position"
        );

        let (row, col) = range.start().unwrap();
        let at = |r: u32, c: u32| -> Data {
            range
                .get_value((row + r, col + c))
                .cloned()
                .unwrap_or(Data::Empty)
        };

        assert_eq!(at(0, 0), Data::String("Region".to_string()));
        assert_eq!(at(0, 1), Data::String("Qty".to_string()));
        assert_eq!(at(1, 0), Data::String("North".to_string()));

        // Numbers must stay numeric, not become text.
        match at(1, 1) {
            Data::Float(v) if (v - 1_234_567_890_123.0).abs() < 1.0 => {}
            Data::Int(1_234_567_890_123) => {}
            other => panic!("expected a number, got {other:?}"),
        }
        match at(2, 1) {
            Data::Float(v) if (v - 2.5).abs() < 1e-9 => {}
            other => panic!("expected float 2.5, got {other:?}"),
        }

        // Booleans stay boolean.
        assert_eq!(at(3, 0), Data::Bool(true));

        // The date keeps its calendar day.
        assert_eq!(
            day_of(&at(3, 1)).as_deref(),
            Some("2026-10-15"),
            "date did not survive the round trip"
        );

        // 36 hours is a duration, not a wrapped 12:00.
        assert_eq!(
            hours_minutes(&at(4, 0)),
            Some((36, 0)),
            "a duration must not wrap at 24 hours"
        );

        // A time of day is a span since midnight, and must stay 09:30 rather
        // than becoming a bogus 1899 date.
        assert_eq!(
            hours_minutes(&at(4, 1)),
            Some((9, 30)),
            "09:30 became {:?}",
            at(4, 1)
        );

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn sheet_names_are_sanitized_and_capped() {
        assert_eq!(sanitize_sheet_name("a/b:c"), "a_b_c");
        assert_eq!(sanitize_sheet_name("   "), "Sheet");
        assert_eq!(sanitize_sheet_name(&"x".repeat(50)).chars().count(), 31);
    }

    #[test]
    fn empty_workbooks_are_refused() {
        let mut wb = workbook_with_block_at_row5_col_c();
        wb.sheets.clear();
        let err = export(&wb, &temp_path("empty.ods")).unwrap_err();
        assert!(err.contains("no sheets"), "unexpected: {err}");
    }
}
