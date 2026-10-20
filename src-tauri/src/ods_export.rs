//! Writing the loaded workbook back out as an OpenDocument spreadsheet.
//!
//! Values keep their type: a number is written as `office:value-type="float"`,
//! a date as `office:value-type="date"` and so on, so the result opens cleanly
//! in LibreOffice, Excel and Google Sheets as a real spreadsheet rather than a
//! wall of text. A cell that was calculated keeps its formula beside its cached
//! value, so reopening the report recalculates it rather than leaving a frozen
//! number behind. Rows and columns a sheet hides stay hidden, and a sheet's
//! frozen panes are written back as view settings, so the file opens the way it
//! was laid out rather than as a flat wall of every row.

use std::path::Path;

use serde::Serialize;
use spreadsheet_ods::sheet::Visibility;
use spreadsheet_ods::style::RowStyle;
use spreadsheet_ods::{write_ods_buf, CellStyleRef, Length, Sheet, Value, WorkBook};

use crate::formula;
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
                let style = formats.style_for(
                    &mut book,
                    sheet.formats.get(row_idx, col_idx),
                    matches!(cell, Cell::Text(_)),
                    look,
                    styled,
                );

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
            let Some(style) = formats.style_for(&mut book, None, false, look, false) else {
                continue;
            };
            ods_sheet.set_cellstyle(row, col, &style);
        }

        write_merges(&mut ods_sheet, &sheet.layout);
        write_row_heights(&mut book, &mut ods_sheet, sheet);
        ensure_trailing_columns(&mut ods_sheet, sheet);
        write_hidden(&mut ods_sheet, sheet);
        write_formulas(&mut ods_sheet, sheet);

        book.push_sheet(ods_sheet);
    }

    // The package is built in memory so the view settings can be added to it:
    // a frozen pane is not part of a sheet's content, and the writer has no way
    // to say anything about it.
    let mut package = write_ods_buf(&mut book, Vec::new())
        .map_err(|e| format!("Could not write \"{}\": {e}", out.display()))?;
    if workbook
        .sheets
        .iter()
        .any(|s| s.layout.frozen_rows > 0 || s.layout.frozen_cols > 0)
    {
        package = with_view_settings(package, workbook)?;
    }
    std::fs::write(out, &package)
        .map_err(|e| format!("Could not write \"{}\": {e}", out.display()))?;

    let bytes = package.len() as u64;

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

/// Make sure the table has a column for every width or hidden state it was
/// given.
///
/// The writer sizes its `table:table-column` elements from the used grid, so a
/// wide but blank column at the edge of a report — or a hidden one — would
/// otherwise vanish.
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
        .max(
            data.layout
                .hidden_cols
                .iter()
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

/// Collapse the rows and columns the sheet hides, so they stay hidden.
///
/// ODF records a hidden row or column as a `table:visibility="collapse"` on
/// the row or column itself. The writer only visits rows that hold a cell, so
/// an empty hidden row is given one; otherwise it would be written as part of a
/// run of ordinary rows and come back visible.
fn write_hidden(sheet: &mut Sheet, data: &SheetData) {
    for &col in &data.layout.hidden_cols {
        if col < MAX_COLS {
            sheet.set_col_visible(col as u32, Visibility::Collapsed);
        }
    }
    for &row in &data.layout.hidden_rows {
        if row >= MAX_ROWS {
            continue;
        }
        sheet.set_row_visible(row as u32, Visibility::Collapsed);
        if matches!(sheet.value(row as u32, 0), Value::Empty) {
            sheet.set_value(row as u32, 0, Value::Empty);
        }
    }
}

/// The view settings that make a sheet open with its panes frozen.
///
/// A frozen pane is not part of a sheet's content in ODF: LibreOffice keeps it
/// in `settings.xml`, per sheet, as the split mode and position of the last
/// view. `2` is the mode for a freeze, and the positions are the counts of
/// frozen columns and rows — the same shape this app reads back.
fn view_settings(workbook: &LoadedWorkbook) -> String {
    let mut tables = String::new();
    for sheet in &workbook.sheets {
        let rows = sheet.layout.frozen_rows;
        let cols = sheet.layout.frozen_cols;
        if rows == 0 && cols == 0 {
            continue;
        }
        let sheet_name = sanitize_sheet_name(&sheet.name);
        let name = quick_xml::escape::escape(&sheet_name);
        tables.push_str(&format!(
            "      <config:config-item-map-entry config:name=\"{name}\">\n"
        ));
        if cols > 0 {
            tables.push_str(SPLIT_HORIZONTAL);
            tables.push_str(&format!(
                "        <config:config-item config:name=\"HorizontalSplitPosition\" config:type=\"int\">{cols}</config:config-item>\n\
                 <config:config-item config:name=\"PositionLeft\" config:type=\"short\">0</config:config-item>\n\
                 <config:config-item config:name=\"PositionRight\" config:type=\"short\">{cols}</config:config-item>\n"
            ));
        }
        if rows > 0 {
            tables.push_str(SPLIT_VERTICAL);
            tables.push_str(&format!(
                "        <config:config-item config:name=\"VerticalSplitPosition\" config:type=\"int\">{rows}</config:config-item>\n\
                 <config:config-item config:name=\"PositionTop\" config:type=\"short\">0</config:config-item>\n\
                 <config:config-item config:name=\"PositionBottom\" config:type=\"short\">{rows}</config:config-item>\n"
            ));
        }
        // 3 selects the pane below and to the right of the split: the one that
        // scrolls, which is what a frozen header leaves the reader looking at.
        tables.push_str(
            "        <config:config-item config:name=\"ActiveSplitRange\" config:type=\"short\">3</config:config-item>\n",
        );
        tables.push_str("      </config:config-item-map-entry>\n");
    }

    let active = workbook
        .sheets
        .first()
        .map(|sheet| sanitize_sheet_name(&sheet.name))
        .unwrap_or_default();
    let active = quick_xml::escape::escape(&active);

    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <office:document-settings xmlns:office=\"urn:oasis:names:tc:opendocument:xmlns:office:1.0\" \
         xmlns:ooo=\"http://openoffice.org/2004/office\" \
         xmlns:config=\"urn:oasis:names:tc:opendocument:xmlns:config:1.0\" office:version=\"1.3\">\n\
         <office:settings>\n\
         <config:config-item-set config:name=\"ooo:view-settings\">\n\
         <config:config-item-map-indexed config:name=\"Views\">\n\
         <config:config-item-map-entry>\n\
         <config:config-item config:name=\"ViewId\" config:type=\"string\">view1</config:config-item>\n\
         <config:config-item-map-named config:name=\"Tables\">\n\
         {tables}\
         </config:config-item-map-named>\n\
         <config:config-item config:name=\"ActiveTable\" config:type=\"string\">{active}</config:config-item>\n\
         </config:config-item-map-entry>\n\
         </config:config-item-map-indexed>\n\
         </config:config-item-set>\n\
         </office:settings>\n\
         </office:document-settings>\n"
    )
}

/// Mode 2 is a freeze rather than a movable split, for the vertical bar that
/// pins columns at the left.
const SPLIT_HORIZONTAL: &str = "        <config:config-item config:name=\"HorizontalSplitMode\" config:type=\"short\">2</config:config-item>\n";

/// The same, for the horizontal bar that pins rows at the top.
const SPLIT_VERTICAL: &str = "        <config:config-item config:name=\"VerticalSplitMode\" config:type=\"short\">2</config:config-item>\n";

/// Rebuild the written package with its empty `settings.xml` replaced.
fn with_view_settings(package: Vec<u8>, workbook: &LoadedWorkbook) -> Result<Vec<u8>, String> {
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;

    let settings = view_settings(workbook);
    let fail = |e: zip::result::ZipError| format!("Could not finish the export: {e}");

    let mut archive = zip::ZipArchive::new(Cursor::new(package)).map_err(fail)?;
    let mut out = zip::ZipWriter::new(Cursor::new(Vec::new()));
    for index in 0..archive.len() {
        let mut entry = archive.by_index(index).map_err(fail)?;
        let name = entry.name().to_string();
        if name == "settings.xml" {
            continue;
        }
        // The compression matters for `mimetype`, which ODF requires to be the
        // first entry and stored rather than deflated.
        let options = SimpleFileOptions::default()
            .compression_method(entry.compression())
            .unix_permissions(entry.unix_mode().unwrap_or(0o644));
        if entry.is_dir() {
            out.add_directory(name, options).map_err(fail)?;
            continue;
        }
        out.start_file(name, options).map_err(fail)?;
        std::io::copy(&mut entry, &mut out)
            .map_err(|e| format!("Could not finish the export: {e}"))?;
    }
    out.start_file("settings.xml", SimpleFileOptions::default())
        .map_err(fail)?;
    out.write_all(settings.as_bytes())
        .map_err(|e| format!("Could not finish the export: {e}"))?;
    Ok(out.finish().map_err(fail)?.into_inner())
}

/// Write each calculation beside its result.
///
/// A formula that cannot be expressed faithfully is left out rather than
/// written wrong: the cell keeps the value it already has, which is still
/// correct as of the file it came from.
fn write_formulas(sheet: &mut Sheet, data: &SheetData) {
    if data.formulas.is_empty() {
        return;
    }
    for (row, col, raw) in data.formulas.iter() {
        if row >= MAX_ROWS || col >= MAX_COLS {
            continue;
        }
        let Some(formula) = formula::open_formula(raw) else {
            continue;
        };
        sheet.set_formula(row as u32, col as u32, formula);
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
                formulas: Default::default(),
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
                formulas: Default::default(),
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
                formulas: Default::default(),
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
                formulas: Default::default(),
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
                formulas: Default::default(),
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

    /// A small report whose calculated cells are the point: each one carries
    /// a formula taken from a different source, plus values that must stay put.
    fn workbook_with_formulas() -> LoadedWorkbook {
        use crate::model::SheetFormulas;

        let rows = vec![
            vec![
                C::Text("Region".into()),
                C::Text("Qty".into()),
                C::Text("Total".into()),
            ],
            vec![C::Text("North".into()), C::Int(2), C::Int(20)],
            vec![C::Text("South".into()), C::Int(3), C::Int(30)],
            vec![C::Text("Sum".into()), C::Int(5), C::Int(50)],
            // The last row holds a formula read from an ODS (already
            // OpenFormula) and one this app cannot express faithfully.
            vec![C::Text("Delta".into()), C::Int(0), C::Int(1)],
        ];

        let mut formulas = SheetFormulas::default();
        // Excel's spelling: references bare, no leading `=`.
        formulas.insert(1, 2, "B2*10");
        formulas.insert(2, 2, "B3*10");
        formulas.insert(3, 1, "SUM(B2:B3)");
        formulas.insert(3, 2, "SUM(C2:C3)");
        // An ODS read in keeps the text it came with.
        formulas.insert(4, 1, "of:=SUM([.B2:.B3])");
        // A structured reference has no OpenFormula form here.
        formulas.insert(4, 2, "SUM(Table1[Qty])");
        // A calculated cell with nothing cached yet: the formula is all the
        // file has, and it must still be written.
        formulas.insert(5, 1, "SUM(B2:B3)");

        LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                name: "Report".into(),
                rows,
                cols: 3,
                formats: Default::default(),
                looks: Default::default(),
                layout: Default::default(),
                formulas,
            }],
            truncated: false,
            total_cells: 15,
            used_cells: 15,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        }
    }

    #[test]
    fn formulas_are_written_so_a_reopened_report_recalculates() {
        let wb = workbook_with_formulas();
        let out = temp_path("formulas.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).expect("export should succeed");

        // The attribute a spreadsheet recalculates from.
        let xml = content_xml(&out);
        assert!(
            xml.contains("table:formula=\"of:=[.B2]*10\""),
            "a translated formula never reached the file"
        );
        assert!(
            xml.contains("table:formula=\"of:=SUM([.B2:.B3])\""),
            "a range formula never reached the file"
        );
        assert!(
            !xml.contains("Table1[Qty]"),
            "a formula that cannot be translated must be left out"
        );

        // Reopen it the way the app does: the values are still cached, and
        // every formula is there for the next edit to recalculate.
        let back = crate::workbook::load_with(&out, crate::text_table::Dialect::default())
            .expect("the exported workbook should open");
        let sheet = &back.sheets[0];

        let mut found: Vec<(usize, usize, String)> = sheet
            .formulas
            .iter()
            .map(|(row, col, text)| (row, col, text.to_string()))
            .collect();
        found.sort();
        assert_eq!(
            found,
            vec![
                (1, 2, "of:=[.B2]*10".to_string()),
                (2, 2, "of:=[.B3]*10".to_string()),
                (3, 1, "of:=SUM([.B2:.B3])".to_string()),
                (3, 2, "of:=SUM([.C2:.C3])".to_string()),
                (4, 1, "of:=SUM([.B2:.B3])".to_string()),
                (5, 1, "of:=SUM([.B2:.B3])".to_string()),
            ],
            "the formulas did not survive the round trip"
        );

        // Values stay as the file showed them, so a reader that does not
        // recalculate still sees the report it exported. ODF stores every
        // number as a float, so the figure is what matters, not its tag.
        let number = |cell: &C| match cell {
            C::Int(v) => *v as f64,
            C::Float(v) => *v,
            other => panic!("expected a number, got {other:?}"),
        };
        assert_eq!(number(sheet.cell(1, 2)), 20.0);
        assert_eq!(number(sheet.cell(3, 1)), 5.0);
        // The untranslatable cell keeps its value and gains no formula.
        assert_eq!(number(sheet.cell(4, 2)), 1.0);
        assert!(
            !found.iter().any(|(row, col, _)| (*row, *col) == (4, 2)),
            "an untranslatable formula must not be written"
        );

        let _ = std::fs::remove_file(&out);
    }

    /// A sheet with a column and two rows hidden, one of them past the data,
    /// and one row and one column frozen.
    fn workbook_with_hidden_and_frozen() -> LoadedWorkbook {
        use std::collections::BTreeSet;

        let rows = vec![
            vec![
                C::Text("Region".into()),
                C::Text("Qty".into()),
                C::Text("Note".into()),
            ],
            vec![C::Text("North".into()), C::Int(4), C::Text("ok".into())],
            vec![C::Text("South".into()), C::Int(6), C::Text("late".into())],
            vec![C::Text("East".into()), C::Int(2), C::Text("ok".into())],
        ];

        let mut layout = SheetLayout {
            hidden_cols: BTreeSet::from([1]),
            hidden_rows: BTreeSet::from([2, 9]),
            frozen_rows: 1,
            frozen_cols: 1,
            ..SheetLayout::default()
        };
        // A hidden column still has a width, which must not be lost either.
        layout.col_widths.insert(1, 60.0);

        LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                name: "Report".into(),
                rows,
                cols: 3,
                formats: Default::default(),
                looks: Default::default(),
                layout,
                formulas: Default::default(),
            }],
            truncated: false,
            total_cells: 12,
            used_cells: 12,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        }
    }

    /// One part of an exported package, as text.
    fn part_text(path: &std::path::Path, name: &str) -> String {
        use std::io::Read;
        let file = std::fs::File::open(path).unwrap();
        let mut zip = zip::ZipArchive::new(file).unwrap();
        let mut entry = zip.by_name(name).unwrap();
        let mut text = String::new();
        entry.read_to_string(&mut text).unwrap();
        text
    }

    #[test]
    fn hidden_rows_columns_and_frozen_panes_survive_the_export() {
        let wb = workbook_with_hidden_and_frozen();
        let out = temp_path("layout.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).expect("export should succeed");

        // The rows and columns are collapsed in the sheet itself.
        let xml = content_xml(&out);
        assert!(
            xml.matches("table:visibility=\"collapse\"").count() >= 2,
            "hidden rows or columns were not written as hidden"
        );
        assert!(
            xml.contains("<table:table-column") && xml.contains("table:visibility=\"collapse\""),
            "a hidden column must stay a column, marked collapsed"
        );

        // The freeze is a view setting, beside the content rather than in it.
        let settings = part_text(&out, "settings.xml");
        assert!(
            settings.contains("ooo:view-settings") && settings.contains("config:name=\"Tables\""),
            "the package has no view settings"
        );
        assert!(
            settings.contains("HorizontalSplitMode") && settings.contains("VerticalSplitMode"),
            "the freeze was not written: {settings}"
        );
        assert!(
            settings.contains("config:name=\"Report\""),
            "the freeze is not tied to its sheet: {settings}"
        );

        // Reopening the way the app does must show the same geometry: the
        // hidden column, both hidden rows (one of them past the data), and
        // the frozen pane.
        let back = crate::workbook::load_with(&out, crate::text_table::Dialect::default())
            .expect("the exported workbook should open");
        let layout = &back.sheets[0].layout;
        assert_eq!(layout.hidden_cols.iter().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(
            layout.hidden_rows.iter().copied().collect::<Vec<_>>(),
            [2, 9]
        );
        assert_eq!(layout.frozen_rows, 1);
        assert_eq!(layout.frozen_cols, 1);
        assert_eq!(
            layout.col_widths.get(&1).copied(),
            Some(60.0),
            "the hidden column lost its width"
        );

        // The values are untouched by all of this.
        assert_eq!(back.sheets[0].cell(1, 0), &C::Text("North".into()));

        let _ = std::fs::remove_file(&out);
    }

    /// A sheet whose formats use the two sections ODF has no direct spelling
    /// for: a zero section with nothing in it, and a fourth section for text.
    fn workbook_with_empty_and_text_sections() -> LoadedWorkbook {
        use crate::model::SheetFormats;
        use crate::number_format::NumberFormat;
        use ssfmt::DateSystem;

        let rows = vec![
            vec![C::Text("Amount".into()), C::Text("Note".into())],
            vec![C::Float(1234.0), C::Float(1234.5)],
            vec![C::Float(0.0), C::Text("anything".into())],
        ];

        let hidden_zero =
            NumberFormat::parse("#,##0;-#,##0;", DateSystem::Date1900).expect("a hide-zero format");
        let with_text = NumberFormat::parse(r#"0.00;[Red]-0.00;0.00;"T: "@"#, DateSystem::Date1900)
            .expect("a four-section format");
        let mut formats = SheetFormats::default();
        for row in 0..3 {
            formats.insert(row, 0, hidden_zero.clone());
            formats.insert(row, 1, with_text.clone());
        }

        LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                name: "Sections".into(),
                rows,
                cols: 2,
                formats,
                looks: Default::default(),
                layout: Default::default(),
                formulas: Default::default(),
            }],
            truncated: false,
            total_cells: 6,
            used_cells: 6,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        }
    }

    #[test]
    fn hidden_zeros_and_the_text_section_survive_the_export() {
        let wb = workbook_with_empty_and_text_sections();
        let out = temp_path("sections-text.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).expect("export should succeed");

        let xml = content_xml(&out);
        assert!(
            xml.contains("style:condition=\"value()=0\""),
            "the zero section was not mapped"
        );
        assert!(
            xml.contains("<number:text-style"),
            "the text section was not written as a text style"
        );
        assert!(
            xml.contains("number:text-content"),
            "the text style has nowhere to put the cell's text"
        );

        // Reopen the way the app does: a zero shows nothing, the numbers keep
        // their format, and a text cell gets the label the fourth section asks
        // for while a number with the same code does not.
        let back = crate::workbook::load_with(&out, crate::text_table::Dialect::default())
            .expect("the exported workbook should open");
        let sheet = &back.sheets[0];
        let shown = |row: usize, col: usize, cell: &C| {
            sheet
                .formats
                .get(row, col)
                .and_then(|format| format.render(cell))
        };

        assert_eq!(
            shown(2, 0, &C::Float(0.0)).as_deref(),
            Some(""),
            "a hidden zero came back as a number"
        );
        assert_eq!(shown(1, 0, &C::Float(1234.0)).as_deref(), Some("1,234"));
        assert_eq!(
            shown(2, 1, &C::Text("anything".into())).as_deref(),
            Some("T: anything"),
            "the text section was not applied to text"
        );
        // The same code on a number must keep the numeric style, not the text
        // style the text cell beside it uses.
        assert_eq!(
            shown(1, 1, &C::Float(1234.5)).as_deref(),
            Some("1234.50"),
            "the text style leaked onto numbers"
        );

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn a_literal_text_section_survives_the_export() {
        use crate::model::SheetFormats;
        use crate::number_format::NumberFormat;
        use ssfmt::DateSystem;

        // A fourth section with no `@` shows the literal for any text.
        let format = NumberFormat::parse(r#"0;0;0;"n/a""#, DateSystem::Date1900).unwrap();
        let mut formats = SheetFormats::default();
        formats.insert(0, 0, format.clone());

        let wb = LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                name: "Literal".into(),
                rows: vec![vec![C::Text("whatever".into())]],
                cols: 1,
                formats,
                looks: Default::default(),
                layout: Default::default(),
                formulas: Default::default(),
            }],
            truncated: false,
            total_cells: 1,
            used_cells: 1,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        };

        let out = temp_path("literal-text.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).expect("export should succeed");

        let back = crate::workbook::load_with(&out, crate::text_table::Dialect::default())
            .expect("the exported workbook should open");
        let sheet = &back.sheets[0];
        let shown = sheet
            .formats
            .get(0, 0)
            .and_then(|format| format.render(&C::Text("whatever".into())));
        assert_eq!(
            shown.as_deref(),
            Some("n/a"),
            "a literal text section came back as {:?}",
            shown
        );

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn an_empty_text_section_hides_text() {
        use crate::model::SheetFormats;
        use crate::number_format::NumberFormat;
        use ssfmt::DateSystem;

        // `0;;;` hides text along with the numbers, so its fourth section is
        // empty and the export has to write a text style that shows nothing.
        let format = NumberFormat::parse("0;0;0;", DateSystem::Date1900).unwrap();
        let mut formats = SheetFormats::default();
        formats.insert(0, 0, format.clone());
        formats.insert(0, 1, format.clone());

        let wb = LoadedWorkbook {
            path: "memory.xlsx".into(),
            file_name: "memory.xlsx".into(),
            format: "Excel Workbook (xlsx)".into(),
            file_size: 0,
            sheets: vec![SheetData {
                name: "Hidden".into(),
                rows: vec![vec![C::Text("anything".into()), C::Float(7.0)]],
                cols: 2,
                formats,
                looks: Default::default(),
                layout: Default::default(),
                formulas: Default::default(),
            }],
            truncated: false,
            total_cells: 2,
            used_cells: 2,
            trust: TrustReport::new(Trust::unknown()),
            text: None,
        };

        let out = temp_path("hidden-text.ods");
        let _ = std::fs::remove_file(&out);
        export(&wb, &out).expect("export should succeed");

        let back = crate::workbook::load_with(&out, crate::text_table::Dialect::default())
            .expect("the exported workbook should open");
        let sheet = &back.sheets[0];
        let shown = |row: usize, col: usize, cell: &C| {
            sheet
                .formats
                .get(row, col)
                .and_then(|format| format.render(cell))
        };

        assert_eq!(
            shown(0, 0, &C::Text("anything".into())).as_deref(),
            Some(""),
            "an empty fourth section must hide text"
        );
        assert_eq!(
            shown(0, 1, &C::Float(7.0)).as_deref(),
            Some("7"),
            "the hidden text section must not reach the numbers"
        );
        // The style must not have swallowed the values themselves.
        assert_eq!(sheet.cell(0, 0), &C::Text("anything".into()));
        assert_eq!(sheet.cell(0, 1), &C::Float(7.0));

        let _ = std::fs::remove_file(&out);
    }

    #[test]
    fn view_settings_are_well_formed_and_escape_their_sheet_names() {
        let mut wb = workbook_with_hidden_and_frozen();
        wb.sheets[0].name = "R&D \"Q3\"".into();
        let xml = view_settings(&wb);

        // Every attribute has to hold together, or the package opens as junk.
        let mut reader = quick_xml::Reader::from_str(&xml);
        let mut buffer = Vec::new();
        loop {
            match reader.read_event_into(&mut buffer) {
                Ok(quick_xml::events::Event::Eof) => break,
                Ok(_) => {}
                Err(e) => panic!("written settings.xml is not well-formed: {e}"),
            }
            buffer.clear();
        }
        assert!(
            xml.contains("R&amp;D &quot;Q3&quot;"),
            "a sheet name must be escaped, not spliced in: {xml}"
        );
        assert!(xml.contains("HorizontalSplitPosition\" config:type=\"int\">1"));
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
