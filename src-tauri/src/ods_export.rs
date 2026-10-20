//! Writing the loaded workbook back out as an OpenDocument spreadsheet.
//!
//! Values keep their type: a number is written as `office:value-type="float"`,
//! a date as `office:value-type="date"` and so on, so the result opens cleanly
//! in LibreOffice, Excel and Google Sheets as a real spreadsheet rather than a
//! wall of text.

use std::path::Path;

use serde::Serialize;
use spreadsheet_ods::style::RowStyle;
use spreadsheet_ods::{write_ods, CellStyleRef, Length, Sheet, Value, WorkBook};

use crate::model::{Cell, CellLook, LoadedWorkbook, SheetData, SheetLayout};
use crate::ods_formats::FormatStyles;
#[cfg(test)]
use crate::trust::{Trust, TrustReport};

/// Excel's own limits, which ODF shares: a merge or a style beyond them would
/// describe rows and columns no application can address.
const MAX_ROWS: usize = 1_048_576;
const MAX_COLS: usize = 16_384;

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

    // Formats and appearances repeat massively (a whole column of money is one code), so each
    // distinct code is translated into one ODF style and reused.
    let mut formats = FormatStyles::new();
    let mut cells = 0usize;

    for sheet in &workbook.sheets {
        let mut ods_sheet = Sheet::new(sanitize_sheet_name(&sheet.name));
        let header_row = sheet.first_used_row();

        // A width or height is stored in pixels for the grid; ODF wants a
        // physical length, and 96 CSS pixels are one inch.
        let layout_cols = sheet
            .layout
            .col_widths
            .keys()
            .next_back()
            .map_or(0, |col| col + 1);
        for col in 0..sheet.cols.max(layout_cols).min(MAX_COLS) {
            let pixels = sheet
                .layout
                .col_widths
                .get(&col)
                .copied()
                .or(sheet.layout.default_col_width);
            if let Some(length) = pixels.and_then(length_of) {
                ods_sheet.set_col_width(col as u32, length);
            }
        }

        let plain = CellLook::default();

        for (row_idx, row) in sheet.rows.iter().enumerate() {
            for (col_idx, cell) in row.iter().enumerate() {
                if cell.is_empty() {
                    continue;
                }
                let row_u32 = row_idx as u32;
                let col_u32 = col_idx as u32;
                let styled = header_row == Some(row_idx);

                // A cell's own format and appearance are independent: a total
                // can be money and bold, a header can be plain text and filled.
                let look = sheet.looks.get(row_idx, col_idx).unwrap_or(&plain);
                let style =
                    formats.style_for(&mut book, sheet.formats.get(row_idx, col_idx), look, styled);

                match cell {
                    Cell::Empty => continue,
                    Cell::Int(v) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *v as f64, style.as_ref())
                    }
                    Cell::Float(v) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *v, style.as_ref())
                    }
                    Cell::Text(s) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, s.as_str(), style.as_ref())
                    }
                    Cell::Bool(b) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *b, style.as_ref())
                    }
                    Cell::DateTime(dt) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *dt, style.as_ref())
                    }
                    Cell::Time(t) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *t, style.as_ref())
                    }
                    Cell::Duration(d) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, *d, style.as_ref())
                    }
                    // Errors have no ODS value type of their own; keep the text.
                    Cell::Error(e) => {
                        write_cell(&mut ods_sheet, row_u32, col_u32, e.as_str(), style.as_ref())
                    }
                }
                cells += 1;
            }
        }

        // A fill on an empty cell still draws part of the table. Such cells
        // carry no value, so they are written after the value pass, with the
        // cell style alone.
        for (row, col, look) in sheet.looks.iter() {
            if !sheet.cell(row, col).is_empty() {
                continue;
            }
            let (Ok(row), Ok(col)) = (u32::try_from(row), u32::try_from(col)) else {
                continue;
            };
            let Some(style) = formats.style_for(&mut book, None, look, false) else {
                continue;
            };
            ods_sheet.set_cellstyle(row, col, &style);
        }

        write_merges(&mut ods_sheet, &sheet.layout);
        write_row_heights(&mut book, &mut ods_sheet, sheet);
        ensure_trailing_columns(&mut ods_sheet, sheet);

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

/// Pixels at 96 dpi as the centimetres ODF stores, to a hundredth.
fn length_of(pixels: f64) -> Option<Length> {
    if !pixels.is_finite() || pixels <= 0.0 {
        return None;
    }
    let cm = (pixels * 2.54 / 96.0 * 100.0).round() / 100.0;
    (cm > 0.0).then_some(Length::Cm(cm))
}

/// Write merged regions as spanning cells, with their covered cells marked.
fn write_merges(sheet: &mut Sheet, layout: &SheetLayout) {
    for merge in &layout.merges {
        let (Ok(row), Ok(col)) = (u32::try_from(merge.row), u32::try_from(merge.col)) else {
            continue;
        };
        if merge.row >= MAX_ROWS || merge.col >= MAX_COLS {
            continue;
        }
        let rows = merge.rows.min(MAX_ROWS - merge.row);
        let cols = merge.cols.min(MAX_COLS - merge.col);
        if rows == 0 || cols == 0 || (rows == 1 && cols == 1) {
            continue;
        }
        let (rows, cols) = (rows as u32, cols as u32);
        sheet.set_row_span(row, col, rows);
        sheet.set_col_span(row, col, cols);

        // The writer only emits `table:covered-table-cell` for cells it
        // visits. An empty row inside a vertical span would otherwise be
        // written as a run of plain empty cells, so give each covered row one
        // cell to visit; the writer expands it across the covered columns.
        for covered_row in row + 1..row + rows {
            if matches!(sheet.value(covered_row, col), Value::Empty) {
                sheet.set_value(covered_row, col, Value::Empty);
            }
        }
    }
}

/// Write row heights, keeping the workbook's default where it has one.
fn write_row_heights(book: &mut WorkBook, sheet: &mut Sheet, data: &SheetData) {
    let explicit = &data.layout.row_heights;
    let last_used = data
        .rows
        .iter()
        .rposition(|row| row.iter().any(|cell| !cell.is_empty()));
    let last_explicit = explicit.keys().next_back().copied().unwrap_or(0);
    let rows = last_used
        .map_or(0, |row| row + 1)
        .max(last_explicit.saturating_add(1))
        .min(MAX_ROWS);

    // ODF has no row-height default this writer can set, so a default height is
    // applied through one shared row style. Rows with a height of their own get
    // their own style below, and are skipped here so that style wins.
    if let Some(length) = data.layout.default_row_height.and_then(length_of) {
        let mut style = RowStyle::new_empty();
        style.set_row_height(length);
        let style = book.add_rowstyle(style);
        for row in 0..rows {
            if !explicit.contains_key(&row) {
                // Both are needed: `set_row_height` records the height for the
                // writer, and the style reference keeps every default row on
                // one shared style instead of one style per row.
                sheet.set_row_height(row as u32, length);
                sheet.set_rowstyle(row as u32, &style);
            }
        }
    }

    for (&row, &pixels) in explicit {
        if row >= MAX_ROWS {
            continue;
        }
        let Some(length) = length_of(pixels) else {
            continue;
        };
        sheet.set_row_height(row as u32, length);
        // A row that holds only a height still has to be visited: the writer
        // emits a run of empty rows once, with the first row's style.
        if matches!(sheet.value(row as u32, 0), Value::Empty) {
            sheet.set_value(row as u32, 0, Value::Empty);
        }
    }
}

/// Make sure the table has a column for every width it was given.
///
/// The writer sizes its `table:table-column` elements from the used grid, so a
/// wide but blank column at the edge of a report would otherwise vanish.
fn ensure_trailing_columns(sheet: &mut Sheet, data: &SheetData) {
    let needed = data
        .cols
        .max(
            data.layout
                .col_widths
                .keys()
                .next_back()
                .map_or(0, |col| col + 1),
        )
        .min(MAX_COLS);
    let (_, used) = sheet.used_grid_size();
    if needed > used as usize {
        let col = (needed - 1) as u32;
        if matches!(sheet.value(0, col), Value::Empty) {
            sheet.set_value(0, col, Value::Empty);
        }
    }
}

/// Write one value, with a style when the cell has one to show.
fn write_cell<V: Into<spreadsheet_ods::Value>>(
    sheet: &mut Sheet,
    row: u32,
    col: u32,
    value: V,
    style: Option<&CellStyleRef>,
) {
    match style {
        Some(style) => sheet.set_styled_value(row, col, value, style),
        None => sheet.set_value(row, col, value),
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
                formats: Default::default(),
                looks: Default::default(),
                layout: Default::default(),
            }],
            truncated: false,
            total_cells: 20,
            used_cells: 10,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
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

    /// A sheet whose cells each carry a format code, with a value of the
    /// matching kind under it.
    fn workbook_with_formats() -> LoadedWorkbook {
        use crate::model::SheetFormats;
        use crate::number_format::NumberFormat;
        use ssfmt::DateSystem;

        fn date(hour: u32, minute: u32) -> C {
            C::DateTime(
                NaiveDate::from_ymd_opt(2026, 1, 9)
                    .unwrap()
                    .and_hms_opt(hour, minute, 0)
                    .unwrap(),
            )
        }

        let rows = vec![
            vec![
                C::Text("Region".into()),
                C::Text("Amount".into()),
                C::Text("Loss".into()),
            ],
            vec![
                C::Text("North".into()),
                C::Float(1234.5),
                C::Float(0.1234),
                date(0, 0),
                C::Duration(Duration::hours(36)),
                C::Float(-1234.5),
            ],
            vec![
                C::Text("South".into()),
                C::Int(19),
                C::Float(0.075),
                date(18, 5),
                C::Time(NaiveTime::from_hms_opt(9, 30, 0).unwrap()),
                C::Float(-0.075),
            ],
            vec![C::Text("12".into()), C::Float(0.5), date(6, 7)],
        ];

        let mut formats = SheetFormats::default();
        let codes = [
            (0, 1, "#,##0.00"),
            (1, 1, "[$€-407] #,##0.00"),
            (1, 2, "0.00%"),
            (1, 3, "dd/mm/yyyy"),
            (1, 4, "[h]:mm:ss"),
            (2, 1, "#,##0.00"),
            (2, 2, "0%"),
            (2, 3, "mm/dd/yyyy h:mm AM/PM"),
            (2, 4, "hh:mm"),
            (1, 5, "$#,##0.00;($#,##0.00)"),
            (2, 5, "0.0%;[Red]-0.0%"),
            (3, 0, "@\" units\""),
            (3, 1, "0.0"),
            (3, 2, "yyyy-mm-dd"),
        ];
        for (row, col, code) in codes {
            let format = NumberFormat::parse(code, DateSystem::Date1900)
                .unwrap_or_else(|| panic!("{code} should parse"));
            formats.insert(row, col, format);
        }

        LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                name: "Formatting".into(),
                rows,
                cols: 6,
                formats,
                looks: Default::default(),
                layout: Default::default(),
            }],
            truncated: false,
            total_cells: 20,
            used_cells: 20,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        }
    }

    /// The `content.xml` of an exported package, as text.
    fn content_xml(path: &std::path::Path) -> String {
        use std::io::Read;
        let file = std::fs::File::open(path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        let mut entry = zip.by_name("content.xml").unwrap();
        let mut xml = String::new();
        entry.read_to_string(&mut xml).unwrap();
        xml
    }

    #[test]
    fn exported_cells_show_the_same_money_dates_and_percentages() {
        let wb = workbook_with_formats();
        let out = temp_path("formats.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).expect("export should succeed");

        // Read the export back with the app's own ODF reader: a style that
        // survives that trip renders the same as the original code did.
        let formats = crate::cell_styles::read(&out);
        let sheet = formats
            .sheets
            .get("Formatting")
            .expect("exported sheet should carry formats");

        let original = &wb.sheets[0];
        let mut checked = 0;
        for row in 0..original.rows.len() {
            for col in 0..original.rows[row].len() {
                let Some(before) = original
                    .formats
                    .get(row, col)
                    .and_then(|f| f.render(original.cell(row, col)))
                else {
                    continue;
                };
                let code = original.formats.get(row, col).unwrap().code().to_string();
                let after = sheet
                    .get(row, col)
                    .unwrap_or_else(|| panic!("no format survived at {row},{col} ({code})"))
                    .render(original.cell(row, col))
                    .unwrap_or_else(|| panic!("format at {row},{col} cannot render its cell"));
                assert_eq!(
                    after,
                    before,
                    "{code} at row {} column {} changed on export",
                    row + 1,
                    col + 1
                );
                checked += 1;
            }
        }
        assert_eq!(checked, 14, "every formatted cell should have been checked");

        // And the way a user actually meets the file: open it in the app. The
        // grid must show the same text the original sheet showed.
        let reopened = crate::workbook::load_with(&out, crate::text_table::Dialect::default())
            .expect("the exported workbook should open");
        let grid = &reopened.sheets[0];
        for row in 0..original.rows.len() {
            for col in 0..original.rows[row].len() {
                let Some(before) = original
                    .formats
                    .get(row, col)
                    .and_then(|f| f.render(original.cell(row, col)))
                else {
                    continue;
                };
                let after = grid
                    .formats
                    .get(row, col)
                    .and_then(|f| f.render(grid.cell(row, col)));
                assert_eq!(
                    after.as_deref(),
                    Some(before.as_str()),
                    "row {} column {} shows differently after reopening",
                    row + 1,
                    col + 1
                );
            }
        }

        // The styles must be real ODF styles, not numbers dressed as text.
        let xml = content_xml(&out);
        for element in [
            "number:percentage-style",
            "number:currency-style",
            "number:currency-symbol",
            "number:date-style",
            "number:time-style",
            "number:truncate-on-overflow=\"false\"",
            "number:am-pm",
            "number:text-content",
            // Negative money is a mapped second style, not a lost section.
            "style:map",
        ] {
            assert!(
                xml.contains(element),
                "exported content.xml lacks {element}"
            );
        }
        // The plain header style and the formatted header cell are both bold.
        assert!(
            xml.matches("fo:font-weight=\"bold\"").count() >= 2,
            "a formatted header cell lost its bold"
        );

        let _ = std::fs::remove_file(&out);
    }

    fn workbook_with_looks() -> LoadedWorkbook {
        use crate::model::{CellLook, SheetLayout, SheetLooks};
        use std::sync::Arc;

        let rows = vec![
            vec![
                C::Text("Region".into()),
                C::Text("Qty".into()),
                C::Text("Note".into()),
            ],
            vec![C::Text("North".into()), C::Int(42), C::Text("ok".into())],
            vec![C::Text("South".into()), C::Int(7), C::Text("late".into())],
        ];

        let mut looks = SheetLooks::default();
        looks.insert(
            0,
            0,
            Arc::new(CellLook {
                bold: true,
                italic: false,
                color: Some("#FF0000".into()),
                background: Some("#FFFF00".into()),
            }),
        );
        looks.insert(
            1,
            0,
            Arc::new(CellLook {
                bold: false,
                italic: true,
                color: Some("#336699".into()),
                background: None,
            }),
        );
        looks.insert(
            1,
            1,
            Arc::new(CellLook {
                background: Some("#E0E0E0".into()),
                ..CellLook::default()
            }),
        );
        // A filled cell with no value: tables band blank cells too.
        looks.insert(
            3,
            0,
            Arc::new(CellLook {
                background: Some("#DDEEFF".into()),
                ..CellLook::default()
            }),
        );

        let layout = SheetLayout {
            col_widths: [(0usize, 160.0), (1, 75.0)].into_iter().collect(),
            default_col_width: Some(64.0),
            ..Default::default()
        };

        LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                name: "Styled".into(),
                rows,
                cols: 3,
                formats: Default::default(),
                looks,
                layout,
            }],
            truncated: false,
            total_cells: 9,
            used_cells: 9,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        }
    }

    #[test]
    fn exported_cells_keep_their_weight_slant_and_colours() {
        let wb = workbook_with_looks();
        let out = temp_path("looks.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).expect("export should succeed");

        let xml = content_xml(&out);
        for attribute in [
            "fo:font-weight=\"bold\"",
            "fo:font-style=\"italic\"",
            "fo:color=\"#ff0000\"",
            "fo:color=\"#336699\"",
            "fo:background-color=\"#ffff00\"",
            "fo:background-color=\"#e0e0e0\"",
            // 160 px, 75 px and the 64 px default, as ODF lengths.
            "style:column-width=\"4.23cm\"",
            "style:column-width=\"1.98cm\"",
            "style:column-width=\"1.69cm\"",
        ] {
            assert!(
                xml.contains(attribute),
                "exported content.xml lacks {attribute}"
            );
        }

        // The app's own readers must see what the exporter wrote: that is how
        // the reopened file looks, not just how its XML reads.
        let looks = crate::cell_styles::read(&out);
        let sheet = looks.looks.get("Styled").expect("sheet should carry looks");
        let first = sheet.get(0, 0).expect("A1 should be styled");
        assert!(first.bold);
        assert_eq!(first.color.as_deref(), Some("#FF0000"));
        assert_eq!(first.background.as_deref(), Some("#FFFF00"));
        let second = sheet.get(1, 0).expect("A2 should be styled");
        assert!(second.italic);
        assert_eq!(second.color.as_deref(), Some("#336699"));

        let reopened = crate::workbook::load_with(&out, crate::text_table::Dialect::default())
            .expect("the exported workbook should open");
        let grid = &reopened.sheets[0];
        assert_eq!(
            grid.looks
                .get(0, 0)
                .map(|look| (look.bold, look.color.clone())),
            Some((true, Some("#FF0000".to_string()))),
            "the reopened grid lost the header's look"
        );
        assert_eq!(
            grid.looks
                .get(1, 1)
                .and_then(|look| look.background.as_deref()),
            Some("#E0E0E0")
        );
        assert_eq!(grid.layout.col_widths.get(&0).copied(), Some(160.0));
        assert_eq!(grid.layout.col_widths.get(&1).copied(), Some(75.0));
        assert_eq!(grid.layout.col_widths.get(&2).copied(), Some(64.0));
        assert_eq!(
            grid.looks
                .get(3, 0)
                .and_then(|look| look.background.as_deref()),
            Some("#DDEEFF"),
            "a fill on a blank cell should survive"
        );

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn a_real_styled_workbook_keeps_its_header_look_on_reopen() {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("samples")
            .join("ExcelScry-sample.xlsx");
        let wb = crate::workbook::load_with(&source, crate::text_table::Dialect::default())
            .expect("the sample should open");
        let before = wb.sheets[0]
            .looks
            .get(5, 2)
            .expect("C6 is styled in the sample");
        assert!(before.bold);
        assert_eq!(before.background.as_deref(), Some("#DCE6F1"));

        let out = temp_path("sample-looks.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).expect("export should succeed");

        let reopened = crate::workbook::load_with(&out, crate::text_table::Dialect::default())
            .expect("the exported workbook should open");
        let after = &reopened.sheets[0];
        let look = after
            .looks
            .get(5, 2)
            .expect("C6 should still be styled after a round trip");
        assert!(look.bold, "the bold total row came back plain");
        assert_eq!(look.background.as_deref(), Some("#DCE6F1"));
        assert_eq!(
            after.layout.col_widths.get(&2).copied(),
            Some(75.0),
            "column C lost its width"
        );
        assert_eq!(after.cell(5, 2), wb.sheets[0].cell(5, 2));

        let _ = std::fs::remove_file(&out);
    }

    /// A report whose geometry is not uniform: a merged title, a merged total,
    /// a tall header, a tall total row, and a wide but empty last column.
    fn workbook_with_geometry() -> LoadedWorkbook {
        use crate::model::{MergeInfo, SheetLayout};

        let text = |value: &str| C::Text(value.to_string());
        let rows = vec![
            vec![text("Report")],
            vec![text("Region"), text("Qty"), text("Note")],
            vec![text("North"), C::Int(42), text("ok")],
            vec![text("South"), C::Int(7), text("late")],
            Vec::new(),
            vec![C::Empty, C::Empty, text("Total")],
            Vec::new(),
        ];

        let mut layout = SheetLayout::default();
        layout.col_widths.insert(0, 160.0);
        layout.col_widths.insert(1, 75.0);
        // Column D holds no values but is still part of the report's shape.
        layout.col_widths.insert(3, 120.0);
        layout.default_col_width = Some(64.0);
        layout.default_row_height = Some(20.0);
        layout.row_heights.insert(0, 40.0);
        layout.row_heights.insert(5, 60.0);
        // A height on a row after the last value: the report still ends there.
        layout.row_heights.insert(8, 30.0);
        layout.merges.push(MergeInfo {
            row: 0,
            col: 0,
            rows: 1,
            cols: 3,
        });
        layout.merges.push(MergeInfo {
            row: 5,
            col: 2,
            rows: 2,
            cols: 2,
        });

        LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                name: "Geometry".into(),
                rows,
                cols: 4,
                formats: Default::default(),
                looks: Default::default(),
                layout,
            }],
            truncated: false,
            total_cells: 20,
            used_cells: 12,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        }
    }

    #[test]
    fn widths_heights_and_merges_survive_the_export() {
        let wb = workbook_with_geometry();
        let out = temp_path("geometry.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).expect("export should succeed");

        let xml = content_xml(&out);
        for element in [
            // 160, 75, 120 and the 64 px default.
            "style:column-width=\"4.23cm\"",
            "style:column-width=\"1.98cm\"",
            "style:column-width=\"3.18cm\"",
            "style:column-width=\"1.69cm\"",
            // 40, 60 and 30 px rows, and the 20 px default.
            "style:row-height=\"1.06cm\"",
            "style:row-height=\"1.59cm\"",
            "style:row-height=\"0.79cm\"",
            "style:row-height=\"0.53cm\"",
            "table:number-columns-spanned=\"3\"",
            "table:number-rows-spanned=\"2\"",
            "table:covered-table-cell",
        ] {
            assert!(
                xml.contains(element),
                "exported content.xml lacks {element}"
            );
        }

        // Open the export the way the user will: the grid must lay it out the
        // same way it laid out the original, not merely have the XML right.
        let reopened = crate::workbook::load_with(&out, crate::text_table::Dialect::default())
            .expect("the exported workbook should open");
        let grid = &reopened.sheets[0];

        assert_eq!(*grid.cell(0, 0), C::Text("Report".into()));
        assert_eq!(*grid.cell(5, 2), C::Text("Total".into()));
        // The cells under and beside the merges are covered, not duplicated.
        assert_eq!(*grid.cell(0, 1), C::Empty);
        assert_eq!(*grid.cell(0, 2), C::Empty);
        assert_eq!(*grid.cell(6, 2), C::Empty);
        assert_eq!(*grid.cell(6, 3), C::Empty);

        assert_eq!(grid.layout.col_widths.get(&0), Some(&160.0));
        assert_eq!(grid.layout.col_widths.get(&1), Some(&75.0));
        assert_eq!(grid.layout.col_widths.get(&2), Some(&64.0));
        assert_eq!(grid.layout.col_widths.get(&3), Some(&120.0));

        assert_eq!(grid.layout.row_heights.get(&0), Some(&40.0));
        assert_eq!(grid.layout.row_heights.get(&1), Some(&20.0));
        assert_eq!(grid.layout.row_heights.get(&5), Some(&60.0));
        assert_eq!(
            grid.layout.row_heights.get(&8),
            Some(&30.0),
            "a height past the last value should still describe the report"
        );

        use crate::model::MergeInfo;
        assert_eq!(grid.layout.merges.len(), 2);
        assert!(grid.layout.merges.contains(&MergeInfo {
            row: 0,
            col: 0,
            rows: 1,
            cols: 3,
        }));
        assert!(grid.layout.merges.contains(&MergeInfo {
            row: 5,
            col: 2,
            rows: 2,
            cols: 2,
        }));

        let _ = std::fs::remove_file(&out);
    }

    fn workbook_with_zero_and_negative_sections() -> LoadedWorkbook {
        use crate::model::SheetFormats;
        use crate::number_format::NumberFormat;
        use ssfmt::DateSystem;

        let code = r##"[Blue]#,##0.00;[Red]-#,##0.00;"-""##;
        let format = NumberFormat::parse(code, DateSystem::Date1900)
            .unwrap_or_else(|| panic!("{code} should parse"));
        let percent_code = r##"0.0%;[Red]-0.0%;"-""##;
        let percent = NumberFormat::parse(percent_code, DateSystem::Date1900)
            .unwrap_or_else(|| panic!("{percent_code} should parse"));
        let mut formats = SheetFormats::default();
        for row in 1..=3 {
            formats.insert(row, 0, format.clone());
            formats.insert(row, 1, percent.clone());
        }

        LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                name: "Sections".into(),
                rows: vec![
                    vec![C::Text("Amount".into()), C::Text("Share".into())],
                    vec![C::Float(1234.5), C::Float(0.5)],
                    vec![C::Float(-1234.5), C::Float(-0.5)],
                    vec![C::Float(0.0), C::Float(0.0)],
                ],
                cols: 2,
                formats,
                looks: Default::default(),
                layout: Default::default(),
            }],
            truncated: false,
            total_cells: 8,
            used_cells: 8,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        }
    }

    #[test]
    fn negative_colour_and_zero_section_survive_the_export() {
        let wb = workbook_with_zero_and_negative_sections();
        let out = temp_path("sections.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).expect("export should succeed");

        let xml = content_xml(&out);
        for element in [
            "style:condition=\"value()&lt;0\"",
            "style:condition=\"value()=0\"",
            "fo:color=\"#0000ff\"",
            "fo:color=\"#ff0000\"",
            "<number:text>-</number:text>",
        ] {
            assert!(
                xml.contains(element),
                "exported content.xml lacks {element}"
            );
        }

        // Read the export back the way the app does: three sections, the
        // negative red and the zero turned into a dash.
        let formats = crate::cell_styles::read(&out);
        let sheet = formats
            .sheets
            .get("Sections")
            .expect("exported sheet should carry formats");
        let format = sheet
            .get(1, 0)
            .expect("the amount column should keep its format");
        assert!(
            format.code().contains("[Blue]") && format.code().contains("[Red]"),
            "a section lost its colour: {}",
            format.code()
        );
        assert_eq!(
            format.render(&C::Float(1234.5)).as_deref(),
            Some("1,234.50")
        );
        assert_eq!(
            format.render(&C::Float(-1234.5)).as_deref(),
            Some("-1,234.50")
        );
        assert_eq!(
            format.render(&C::Float(0.0)).as_deref(),
            Some("-"),
            "zero should read as a dash, not as 0.00"
        );

        let percent = sheet
            .get(1, 1)
            .expect("the share column should keep its format");
        assert_eq!(percent.render(&C::Float(0.5)).as_deref(), Some("50.0%"));
        assert_eq!(
            percent.render(&C::Float(0.0)).as_deref(),
            Some("-"),
            "a percentage zero section should not gain a stray %"
        );

        let reopened = crate::workbook::load_with(&out, crate::text_table::Dialect::default())
            .expect("the exported workbook should open");
        let grid = &reopened.sheets[0];
        let shown = |row: usize| {
            grid.formats
                .get(row, 0)
                .and_then(|format| format.render(grid.cell(row, 0)))
        };
        assert_eq!(shown(2).as_deref(), Some("-1,234.50"));
        assert_eq!(shown(3).as_deref(), Some("-"));
        assert_eq!(
            grid.formats
                .get(3, 1)
                .and_then(|format| format.render(grid.cell(3, 1)))
                .as_deref(),
            Some("-"),
            "the reopened percentage zero should stay a dash"
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
