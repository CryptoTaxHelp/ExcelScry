//! Reading a worksheet's own geometry: sizes, merges and frozen panes.
//!
//! calamine reports cell values and nothing about how a sheet is laid out, so
//! `xl/worksheets/sheetN.xml`, ODF `content.xml` and ODF `settings.xml` are read
//! here for the things the grid needs in order to look like the file: how wide
//! each column is, how tall each row is, which cells are merged into one, and
//! which rows and columns are frozen on screen.
//!
//! Sizes are converted to CSS pixels at 96 dpi. Excel stores column widths in
//! character units and row heights in points; ODF stores both as lengths. A
//! package this cannot read yields no layout, and the grid falls back to its
//! own uniform metrics.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use zip::ZipArchive;

use crate::model::{MergeInfo, SheetLayout};

/// Excel's own limits, used to clamp ranges a file could otherwise inflate.
const MAX_ROWS: usize = 1_048_576;
const MAX_COLS: usize = 16_384;
/// A package could name a merge for every cell; the grid only draws what fits
/// on screen, so bounding the list is enough to keep a hostile file cheap.
const MAX_MERGES: usize = 20_000;
/// Sizes outside this range are treated as nonsense rather than laid out: a
/// zero-width column cannot be clicked, and a billion-pixel row breaks scroll.
const MIN_COL: f64 = 12.0;
const MAX_COL: f64 = 2_000.0;
const MIN_ROW: f64 = 8.0;
const MAX_ROW: f64 = 1_000.0;

/// The geometry of every sheet a package describes, keyed by sheet name.
#[derive(Debug, Default)]
pub struct WorkbookLayout {
    pub sheets: HashMap<String, SheetLayout>,
}

/// Read a workbook's geometry, or nothing if the package cannot be read.
pub fn read(path: &Path) -> WorkbookLayout {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let Ok(file) = File::open(path) else {
        return WorkbookLayout::default();
    };
    let Ok(mut zip) = ZipArchive::new(BufReader::new(file)) else {
        return WorkbookLayout::default();
    };

    match extension.as_str() {
        "xlsx" | "xlsm" | "xltx" | "xltm" | "xlam" => xlsx(&mut zip),
        "ods" => ods(&mut zip),
        // `.xls` and `.xlsb` keep this in binary records calamine does not
        // expose; the grid falls back to its own metrics there.
        _ => WorkbookLayout::default(),
    }
}

fn part<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Option<Vec<u8>> {
    let mut entry = zip.by_name(name).ok()?;
    let mut out = Vec::new();
    entry.read_to_end(&mut out).ok()?;
    Some(out)
}

fn attr(element: &BytesStart, name: &[u8]) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| a.normalized_value(XmlVersion::Implicit1_0).ok())
        .map(|value| value.into_owned())
}

fn number_attr(element: &BytesStart, name: &[u8]) -> Option<f64> {
    attr(element, name)?.trim().parse().ok()
}

fn count_attr(element: &BytesStart, name: &[u8], default: usize) -> usize {
    attr(element, name)
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(default)
}

/// Whether an OOXML flag attribute says yes. Excel writes `1`, but `true` is a
/// valid spelling of the same thing.
fn is_true(element: &BytesStart, name: &[u8]) -> bool {
    matches!(
        attr(element, name).as_deref().map(str::trim),
        Some("1") | Some("true") | Some("TRUE")
    )
}

/// Whether an ODF element is hidden. `collapse` is what LibreOffice writes for
/// a hidden column or row; `filter` hides it only while filtering, so it stays
/// visible here, and `visible` is the default.
fn is_collapsed(element: &BytesStart) -> bool {
    matches!(
        attr(element, b"visibility").as_deref().map(str::trim),
        Some("collapse")
    )
}

// ---------------------------------------------------------------------------
// OOXML
// ---------------------------------------------------------------------------

fn xlsx<R: Read + Seek>(zip: &mut ZipArchive<R>) -> WorkbookLayout {
    let mut out = WorkbookLayout::default();
    let Some(workbook) = part(zip, "xl/workbook.xml") else {
        return out;
    };
    let sheets = workbook_sheets(&workbook);
    if sheets.is_empty() {
        return out;
    }
    let rels = part(zip, "xl/_rels/workbook.xml.rels").unwrap_or_default();
    let targets = relationships(&rels);

    for (name, relationship) in sheets {
        let Some(target) = targets.get(&relationship) else {
            continue;
        };
        let Some(path) = worksheet_path(target) else {
            continue;
        };
        let Ok(entry) = zip.by_name(&path) else {
            continue;
        };
        let mut layout = SheetLayout::default();
        read_xlsx_sheet(BufReader::new(entry), &mut layout);
        if !is_empty(&layout) {
            out.sheets.insert(name, layout);
        }
    }
    out
}

/// Sheet names in document order, with the relationship id of each part.
fn workbook_sheets(xml: &[u8]) -> Vec<(String, String)> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut sheets = Vec::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                if element.local_name().as_ref() == b"sheet" {
                    if let (Some(name), Some(id)) = (attr(&element, b"name"), attr(&element, b"id"))
                    {
                        sheets.push((name, id));
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
    sheets
}

/// Relationship id to part name.
fn relationships(xml: &[u8]) -> HashMap<String, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut out = HashMap::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                if element.local_name().as_ref() == b"Relationship" {
                    if let (Some(id), Some(target)) =
                        (attr(&element, b"Id"), attr(&element, b"Target"))
                    {
                        out.insert(id, target);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
    out
}

/// A relationship target as a zip entry name.
fn worksheet_path(target: &str) -> Option<String> {
    let target = target.replace('\\', "/");
    if let Some(rest) = target.strip_prefix('/') {
        return Some(rest.to_string());
    }
    if target.starts_with("xl/") {
        return Some(target);
    }
    Some(format!("xl/{target}"))
}

/// The frozen-pane row and column counts an OOXML `<pane>` describes.
///
/// `xSplit` and `ySplit` are the counts for a frozen pane; `topLeftCell` names
/// the first scrolling cell, so it can supply a count when a writer leaves one
/// split out. A split window (`state="split"`) is not a freeze and is ignored.
fn frozen_pane(element: &BytesStart) -> Option<(usize, usize)> {
    let state = attr(element, b"state")?.trim().to_ascii_lowercase();
    if state != "frozen" && state != "frozensplit" {
        return None;
    }
    let top_left = attr(element, b"topLeftCell").and_then(|cell| parse_ref(&cell));
    let cols = number_attr(element, b"xSplit")
        .map_or_else(|| top_left.map_or(0, |(_, col)| col), split_count)
        .min(MAX_COLS);
    let rows = number_attr(element, b"ySplit")
        .map_or_else(|| top_left.map_or(0, |(row, _)| row), split_count)
        .min(MAX_ROWS);
    Some((rows, cols))
}

/// A split position as a whole count of rows or columns.
fn split_count(value: f64) -> usize {
    if !value.is_finite() || value <= 0.0 {
        0
    } else {
        value.floor() as usize
    }
}

/// Read one worksheet part for its default sizes, columns, rows and merges.
fn read_xlsx_sheet<R: std::io::BufRead>(reader: R, layout: &mut SheetLayout) {
    let mut reader = Reader::from_reader(reader);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut row = 0usize;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                match element.local_name().as_ref() {
                    b"sheetFormatPr" => {
                        // Column widths are in character units, row heights in
                        // points. `baseColWidth` is the fallback when a file
                        // leaves the default width out.
                        let width = number_attr(&element, b"defaultColWidth")
                            .or_else(|| number_attr(&element, b"baseColWidth"));
                        if let Some(width) = width.and_then(column_px) {
                            layout.default_col_width = Some(width);
                        }
                        if let Some(height) =
                            number_attr(&element, b"defaultRowHeight").and_then(row_px)
                        {
                            layout.default_row_height = Some(height);
                        }
                    }
                    b"col" => {
                        let min = count_attr(&element, b"min", 0);
                        let last = count_attr(&element, b"max", min).min(MAX_COLS);
                        let hidden = is_true(&element, b"hidden");
                        let width = number_attr(&element, b"width").and_then(column_px);
                        for col in min.max(1)..=last {
                            if hidden {
                                layout.hidden_cols.insert(col - 1);
                            }
                            if let Some(width) = width {
                                layout.col_widths.insert(col - 1, width);
                            }
                        }
                    }
                    b"row" => {
                        if let Some(number) = count_attr(&element, b"r", 0).checked_sub(1) {
                            if number < MAX_ROWS {
                                row = number;
                            }
                        }
                        if is_true(&element, b"hidden") {
                            layout.hidden_rows.insert(row);
                        }
                        if let Some(height) = number_attr(&element, b"ht").and_then(row_px) {
                            layout.row_heights.insert(row, height);
                        }
                    }
                    b"pane" => {
                        if let Some((rows, cols)) = frozen_pane(&element) {
                            layout.frozen_rows = rows;
                            layout.frozen_cols = cols;
                        }
                    }
                    b"mergeCell" => {
                        if let Some(reference) = attr(&element, b"ref") {
                            if let Some(merge) = parse_range(&reference) {
                                if layout.merges.len() < MAX_MERGES {
                                    layout.merges.push(merge);
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
}

// ---------------------------------------------------------------------------
// ODF
// ---------------------------------------------------------------------------

fn ods<R: Read + Seek>(zip: &mut ZipArchive<R>) -> WorkbookLayout {
    let Some(content) = part(zip, "content.xml") else {
        return WorkbookLayout::default();
    };
    let mut layout = read_ods_content(&content);

    // LibreOffice keeps frozen panes in the view settings, by sheet name, not
    // in the sheet content. A package without settings simply has no freeze.
    if let Some(settings) = part(zip, "settings.xml") {
        for (name, (rows, cols)) in read_ods_freeze(&settings) {
            let sheet = layout.sheets.entry(name).or_default();
            sheet.frozen_rows = rows;
            sheet.frozen_cols = cols;
        }
    }
    layout
}

/// One sheet's raw split settings, as LibreOffice writes them.
#[derive(Default)]
struct SplitSettings {
    horizontal_mode: Option<i64>,
    vertical_mode: Option<i64>,
    horizontal_position: Option<i64>,
    vertical_position: Option<i64>,
    right: Option<i64>,
    bottom: Option<i64>,
}

/// Frozen row and column counts from an ODF `settings.xml`.
///
/// LibreOffice stores them under `ooo:view-settings` → `Views` → `Tables`, one
/// entry per sheet. `HorizontalSplitMode`/`VerticalSplitMode` are 2 for a
/// freeze; their positions are then the number of frozen columns and rows.
fn read_ods_freeze(xml: &[u8]) -> HashMap<String, (usize, usize)> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut state = FreezeReader::default();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) => {
                let local = element.local_name().as_ref().to_vec();
                state.start(&element, &local);
            }
            Ok(Event::Empty(element)) => {
                // A self-closing setting has no text and describes no freeze;
                // it still must not disturb the surrounding depth tracking.
                let local = element.local_name().as_ref().to_vec();
                state.self_closing(&element, &local);
            }
            Ok(Event::Text(event)) => {
                if state.item.is_some() {
                    if let Ok(value) = event.xml10_content() {
                        if let Ok(value) = quick_xml::escape::unescape(&value) {
                            state.text.push_str(&value);
                        }
                    }
                }
            }
            Ok(Event::End(element)) => {
                let local = element.local_name().as_ref().to_vec();
                state.end(&local);
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }

    state.out
}

#[derive(Default)]
struct FreezeReader {
    depth: usize,
    views_depth: Option<usize>,
    tables_depth: Option<usize>,
    sheet_depth: Option<usize>,
    sheet: Option<String>,
    item: Option<String>,
    text: String,
    splits: HashMap<String, SplitSettings>,
    out: HashMap<String, (usize, usize)>,
}

impl FreezeReader {
    fn start(&mut self, element: &BytesStart, local: &[u8]) {
        self.depth += 1;
        self.enter(element, local, self.depth);
    }

    fn self_closing(&mut self, element: &BytesStart, local: &[u8]) {
        // An empty config item has no value; reading it would clear a value
        // that a sibling element already supplied.
        if local == b"config-item" {
            return;
        }
        // A self-closing element is both start and end at the current depth.
        let depth = self.depth + 1;
        self.enter(element, local, depth);
        self.leave(local, depth);
    }

    fn enter(&mut self, element: &BytesStart, local: &[u8], depth: usize) {
        match local {
            b"config-item-set" => {
                if attr(element, b"name").as_deref() == Some("ooo:view-settings") {
                    self.views_depth = Some(depth);
                }
            }
            b"config-item-map-named" => {
                if self.views_depth.is_some() && attr(element, b"name").as_deref() == Some("Tables")
                {
                    self.tables_depth = Some(depth);
                }
            }
            b"config-item-map-entry" => {
                if self.tables_depth.is_some_and(|tables| depth == tables + 1) {
                    self.sheet_depth = Some(depth);
                    self.sheet = attr(element, b"name");
                }
            }
            b"config-item" if self.sheet.is_some() => {
                self.item = attr(element, b"name");
                self.text.clear();
            }
            _ => {}
        }
    }

    fn end(&mut self, local: &[u8]) {
        self.leave(local, self.depth);
        self.depth = self.depth.saturating_sub(1);
    }

    fn leave(&mut self, local: &[u8], depth: usize) {
        match local {
            b"config-item" => {
                if let (Some(sheet), Some(name)) = (self.sheet.clone(), self.item.take()) {
                    let value = self.text.trim().parse::<i64>().ok();
                    let split = self.splits.entry(sheet).or_default();
                    match name.as_str() {
                        "HorizontalSplitMode" => split.horizontal_mode = value,
                        "VerticalSplitMode" => split.vertical_mode = value,
                        "HorizontalSplitPosition" => split.horizontal_position = value,
                        "VerticalSplitPosition" => split.vertical_position = value,
                        "PositionRight" => split.right = value,
                        "PositionBottom" => split.bottom = value,
                        _ => {}
                    }
                    self.text.clear();
                }
            }
            b"config-item-map-entry" if self.sheet_depth == Some(depth) => {
                if let Some(sheet) = self.sheet.take() {
                    if let Some(split) = self.splits.remove(&sheet) {
                        let counts = split.freeze();
                        if counts != (0, 0) {
                            self.out.insert(sheet, counts);
                        }
                    }
                }
                self.sheet_depth = None;
            }
            b"config-item-map-named" if self.tables_depth == Some(depth) => {
                self.tables_depth = None;
            }
            b"config-item-set" if self.views_depth == Some(depth) => {
                self.views_depth = None;
            }
            _ => {}
        }
    }
}

impl SplitSettings {
    /// `(rows, columns)` frozen, or `(0, 0)` for a split window rather than a
    /// frozen pane.
    fn freeze(self) -> (usize, usize) {
        let rows = if self.vertical_mode == Some(2) {
            count(self.vertical_position.or(self.bottom))
        } else {
            0
        };
        let cols = if self.horizontal_mode == Some(2) {
            count(self.horizontal_position.or(self.right))
        } else {
            0
        };
        (rows.min(MAX_ROWS), cols.min(MAX_COLS))
    }
}

fn count(value: Option<i64>) -> usize {
    value
        .and_then(|value| usize::try_from(value).ok())
        .unwrap_or(0)
}

/// Everything one pass over `content.xml` contributes.
#[derive(Default)]
struct Content {
    columns: HashMap<String, f64>,
    rows: HashMap<String, f64>,
    default_col: Option<f64>,
    default_row: Option<f64>,
    sheets: HashMap<String, SheetLayout>,
    /// The `style:style` or `style:default-style` being defined: family, name.
    defining: Option<(String, String)>,
    /// The table being scanned and where in it the reader is.
    table: Option<(String, SheetLayout)>,
    row: usize,
    col: usize,
    row_repeat: usize,
}

fn read_ods_content(xml: &[u8]) -> WorkbookLayout {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut state = Content {
        row_repeat: 1,
        ..Content::default()
    };

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) => {
                let local = element.local_name().as_ref().to_vec();
                state.start(&element, &local);
            }
            Ok(Event::Empty(element)) => {
                let local = element.local_name().as_ref().to_vec();
                state.start(&element, &local);
            }
            Ok(Event::End(element)) => {
                let local = element.local_name().as_ref().to_vec();
                state.end(&local);
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }

    WorkbookLayout {
        sheets: state.sheets,
    }
}

impl Content {
    fn start(&mut self, element: &BytesStart, local: &[u8]) {
        match local {
            b"style" | b"default-style" => {
                let family = attr(element, b"family").unwrap_or_default();
                let name = attr(element, b"name").unwrap_or_default();
                self.defining = Some((family, name));
            }
            b"table-column-properties" => {
                let Some((family, name)) = self.defining.clone() else {
                    return;
                };
                if family != "table-column" {
                    return;
                }
                let Some(width) = attr(element, b"column-width").and_then(|v| length_px(&v)) else {
                    return;
                };
                let width = width.clamp(MIN_COL, MAX_COL).round();
                if name.is_empty() {
                    self.default_col = Some(width);
                } else {
                    self.columns.insert(name, width);
                }
            }
            b"table-row-properties" => {
                let Some((family, name)) = self.defining.clone() else {
                    return;
                };
                if family != "table-row" {
                    return;
                }
                let Some(height) = attr(element, b"row-height").and_then(|v| length_px(&v)) else {
                    return;
                };
                let height = height.clamp(MIN_ROW, MAX_ROW).round();
                if name.is_empty() {
                    self.default_row = Some(height);
                } else {
                    self.rows.insert(name, height);
                }
            }
            b"table" => {
                let Some(name) = attr(element, b"name") else {
                    return;
                };
                let layout = SheetLayout {
                    default_col_width: self.default_col,
                    default_row_height: self.default_row,
                    ..SheetLayout::default()
                };
                self.table = Some((name, layout));
                self.row = 0;
                self.col = 0;
                self.row_repeat = 1;
            }
            b"table-column" if self.table.is_some() => {
                let repeat = count_attr(element, b"number-columns-repeated", 1).clamp(1, MAX_COLS);
                let width = attr(element, b"style-name")
                    .and_then(|style| self.columns.get(&style).copied())
                    .or(self.default_col);
                let hidden = is_collapsed(element);
                if width.is_some() || hidden {
                    if let Some((_, layout)) = self.table.as_mut() {
                        for col in self.col..(self.col + repeat).min(MAX_COLS) {
                            if let Some(width) = width {
                                layout.col_widths.insert(col, width);
                            }
                            if hidden {
                                layout.hidden_cols.insert(col);
                            }
                        }
                    }
                }
                self.col = (self.col + repeat).min(MAX_COLS);
            }
            b"table-row" if self.table.is_some() => {
                self.col = 0;
                let repeat = count_attr(element, b"number-rows-repeated", 1).clamp(1, MAX_ROWS);
                let height =
                    attr(element, b"style-name").and_then(|style| self.rows.get(&style).copied());
                let hidden = is_collapsed(element);
                if height.is_some() || hidden {
                    if let Some((_, layout)) = self.table.as_mut() {
                        for row in self.row..(self.row + repeat).min(MAX_ROWS) {
                            if let Some(height) = height {
                                layout.row_heights.insert(row, height);
                            }
                            if hidden {
                                layout.hidden_rows.insert(row);
                            }
                        }
                    }
                }
                self.row_repeat = repeat;
            }
            b"table-cell" | b"covered-table-cell" if self.table.is_some() => {
                let repeat = count_attr(element, b"number-columns-repeated", 1).clamp(1, MAX_COLS);
                if local == b"table-cell" {
                    let colspan = count_attr(element, b"number-columns-spanned", 1).max(1);
                    let rowspan = count_attr(element, b"number-rows-spanned", 1).max(1);
                    if (colspan > 1 || rowspan > 1) && self.row < MAX_ROWS {
                        let merge = MergeInfo {
                            row: self.row,
                            col: self.col,
                            rows: rowspan.min(MAX_ROWS - self.row),
                            cols: colspan.min(MAX_COLS - self.col),
                        };
                        if let Some((_, layout)) = self.table.as_mut() {
                            if layout.merges.len() < MAX_MERGES {
                                layout.merges.push(merge);
                            }
                        }
                    }
                }
                self.col = (self.col + repeat).min(MAX_COLS);
            }
            _ => {}
        }
    }

    fn end(&mut self, local: &[u8]) {
        match local {
            b"style" | b"default-style" => self.defining = None,
            b"table-row" if self.table.is_some() => {
                self.row = (self.row + self.row_repeat).min(MAX_ROWS);
                self.row_repeat = 1;
                self.col = 0;
            }
            b"table" => {
                if let Some((name, layout)) = self.table.take() {
                    if !is_empty(&layout) {
                        self.sheets.insert(name, layout);
                    }
                }
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Units, references and limits
// ---------------------------------------------------------------------------

/// Excel's column width (in characters of the default font) as pixels.
///
/// The character unit is 7 px at Calibri 11, plus the 5 px cell padding.
fn column_px(chars: f64) -> Option<f64> {
    if !chars.is_finite() || chars <= 0.0 {
        return None;
    }
    Some((chars * 7.0 + 5.0).round().clamp(MIN_COL, MAX_COL))
}

/// Excel's row height, in points, as pixels.
fn row_px(points: f64) -> Option<f64> {
    if !points.is_finite() || points <= 0.0 {
        return None;
    }
    Some((points * 96.0 / 72.0).round().clamp(MIN_ROW, MAX_ROW))
}

/// An ODF length (`2.258cm`, `15pt`, `0.6in`...) as pixels.
fn length_px(value: &str) -> Option<f64> {
    let text = value.trim();
    let split = text.find(|c: char| c.is_ascii_alphabetic())?;
    let (number, unit) = text.split_at(split);
    let number: f64 = number.trim().parse().ok()?;
    if !number.is_finite() || number <= 0.0 {
        return None;
    }
    Some(match unit.to_ascii_lowercase().as_str() {
        "cm" => number * 96.0 / 2.54,
        "mm" => number * 9.6 / 2.54,
        "in" => number * 96.0,
        "pt" => number * 96.0 / 72.0,
        "pc" => number * 16.0,
        "px" => number,
        _ => return None,
    })
}

/// `A1` as zero-based `(row, column)`.
fn parse_ref(text: &str) -> Option<(usize, usize)> {
    let mut col = 0usize;
    let mut digits = String::new();
    let mut seen_digit = false;
    for ch in text.trim().chars() {
        match ch {
            '$' => {}
            ch if ch.is_ascii_alphabetic() => {
                if seen_digit {
                    return None;
                }
                col = col * 26 + (ch.to_ascii_uppercase() as usize - 'A' as usize + 1);
                if col > MAX_COLS {
                    return None;
                }
            }
            ch if ch.is_ascii_digit() => {
                seen_digit = true;
                digits.push(ch);
            }
            _ => return None,
        }
    }
    if col == 0 || digits.is_empty() {
        return None;
    }
    let row: usize = digits.parse().ok()?;
    if row == 0 || row > MAX_ROWS {
        return None;
    }
    Some((row - 1, col - 1))
}

/// `B2:D3` as a merged region.
fn parse_range(text: &str) -> Option<MergeInfo> {
    let (start, end) = text.split_once(':').unwrap_or((text, text));
    let (row1, col1) = parse_ref(start)?;
    let (row2, col2) = parse_ref(end)?;
    let row = row1.min(row2);
    let col = col1.min(col2);
    Some(MergeInfo {
        row,
        col,
        rows: row1.max(row2) - row + 1,
        cols: col1.max(col2) - col + 1,
    })
}

/// Whether a package said anything about the layout at all.
fn is_empty(layout: &SheetLayout) -> bool {
    layout.col_widths.is_empty()
        && layout.row_heights.is_empty()
        && layout.default_col_width.is_none()
        && layout.default_row_height.is_none()
        && layout.merges.is_empty()
        && layout.hidden_rows.is_empty()
        && layout.hidden_cols.is_empty()
        && layout.frozen_rows == 0
        && layout.frozen_cols == 0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("excelscry-sheet-layout");
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Write a zip with the given parts, as a package would contain them.
    fn package(name: &str, parts: &[(&str, &str)]) -> std::path::PathBuf {
        let path = temp_dir().join(name);
        let _ = std::fs::remove_file(&path);
        let file = std::fs::File::create(&path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        for (part, body) in parts {
            zip.start_file(*part, zip::write::SimpleFileOptions::default())
                .unwrap();
            zip.write_all(body.as_bytes()).unwrap();
        }
        zip.finish().unwrap();
        path
    }

    fn xlsx_package(name: &str, sheet: &str) -> std::path::PathBuf {
        let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <sheets><sheet name="Sized" sheetId="1" r:id="rId1"/></sheets>
</workbook>"#;
        let rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
        package(
            name,
            &[
                ("xl/workbook.xml", workbook),
                ("xl/_rels/workbook.xml.rels", rels),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        )
    }

    #[test]
    fn cell_references_and_ranges_are_parsed() {
        assert_eq!(parse_ref("A1"), Some((0, 0)));
        assert_eq!(parse_ref("C7"), Some((6, 2)));
        assert_eq!(parse_ref("$AA$10"), Some((9, 26)));
        assert_eq!(parse_ref("7"), None);
        assert_eq!(parse_ref("A"), None);
        assert_eq!(
            parse_range("B2:D3"),
            Some(MergeInfo {
                row: 1,
                col: 1,
                rows: 2,
                cols: 3
            })
        );
        assert_eq!(
            parse_range("D3:B2"),
            Some(MergeInfo {
                row: 1,
                col: 1,
                rows: 2,
                cols: 3
            })
        );
        // A single-cell "merge" is still one cell.
        assert_eq!(
            parse_range("A1"),
            Some(MergeInfo {
                row: 0,
                col: 0,
                rows: 1,
                cols: 1
            })
        );
    }

    #[test]
    fn lengths_convert_from_each_unit() {
        assert_eq!(length_px("2.54cm").unwrap().round(), 96.0);
        assert_eq!(length_px("25.4mm").unwrap().round(), 96.0);
        assert_eq!(length_px("1in").unwrap().round(), 96.0);
        assert_eq!(length_px("72pt").unwrap().round(), 96.0);
        assert_eq!(length_px("6pc").unwrap().round(), 96.0);
        assert_eq!(length_px("40px").unwrap(), 40.0);
        assert_eq!(length_px("0.889cm").unwrap().round(), 34.0);
        assert_eq!(length_px("wide"), None);
        assert_eq!(length_px("0cm"), None);
    }

    #[test]
    fn xlsx_columns_rows_defaults_and_merges_are_read() {
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetViews><sheetView workbookViewId="0">
    <pane xSplit="1" ySplit="2" topLeftCell="B3" activePane="bottomRight" state="frozen"/>
  </sheetView></sheetViews>
  <sheetFormatPr defaultColWidth="10.5" defaultRowHeight="18"/>
  <cols>
    <col min="1" max="1" width="24" customWidth="1"/>
    <col min="2" max="2" hidden="1"/>
    <col min="3" max="4" width="12.5" customWidth="1"/>
  </cols>
  <sheetData>
    <row r="2" ht="30" customHeight="1"><c r="A2"><v>1</v></c></row>
    <row r="3" hidden="1"><c r="A3"><v>2</v></c></row>
  </sheetData>
  <mergeCells count="1"><mergeCell ref="B2:D3"/></mergeCells>
</worksheet>"#;
        let path = xlsx_package("sizes.xlsx", sheet);
        let layout = read(&path);
        let sheet = layout.sheets.get("Sized").expect("layout for Sized");

        // 10.5 characters -> 10.5 * 7 + 5 pixels; 18 points -> 24 pixels.
        assert_eq!(sheet.default_col_width, Some(79.0));
        assert_eq!(sheet.default_row_height, Some(24.0));
        assert_eq!(sheet.col_widths.get(&0), Some(&173.0));
        assert_eq!(sheet.col_widths.get(&2), Some(&93.0));
        assert_eq!(sheet.col_widths.get(&3), Some(&93.0));
        assert_eq!(sheet.col_widths.get(&1), None);
        // A row's `r` is 1-based; the height belongs to the second row.
        assert_eq!(sheet.row_heights.get(&1), Some(&40.0));
        assert_eq!(sheet.row_heights.get(&0), None);
        // A hidden column has no width of its own; it is still hidden.
        assert_eq!(sheet.hidden_cols.iter().copied().collect::<Vec<_>>(), [1]);
        assert_eq!(sheet.hidden_rows.iter().copied().collect::<Vec<_>>(), [2]);
        assert_eq!(sheet.frozen_rows, 2);
        assert_eq!(sheet.frozen_cols, 1);
        assert_eq!(
            sheet.merges,
            vec![MergeInfo {
                row: 1,
                col: 1,
                rows: 2,
                cols: 3
            }]
        );
    }

    #[test]
    fn a_split_window_is_not_a_frozen_pane() {
        // `split` keeps panes that scroll with their own scrollbars; only a
        // frozen pane pins content, and only that should reach the grid.
        let sheet = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
            <sheetViews><sheetView><pane xSplit="3" ySplit="4" state="split"/></sheetView></sheetViews>
            <sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData>
        </worksheet>"#;
        let path = xlsx_package("split.xlsx", sheet);
        assert!(read(&path).sheets.is_empty());
    }

    #[test]
    fn ods_column_widths_row_heights_and_spans_are_read() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0">
  <office:automatic-styles>
    <style:style style:name="co1" style:family="table-column">
      <style:table-column-properties style:column-width="3cm"/>
    </style:style>
    <style:style style:name="ro1" style:family="table-row">
      <style:table-row-properties style:row-height="0.8cm"/>
    </style:style>
  </office:automatic-styles>
  <office:body><office:spreadsheet>
    <table:table table:name="Spans">
      <table:table-column table:style-name="co1"/>
      <table:table-column table:number-columns-repeated="2" table:visibility="collapse"/>
      <table:table-row>
        <table:table-cell table:number-columns-spanned="2" table:number-rows-spanned="2" office:value-type="string"><text:p>x</text:p></table:table-cell>
        <table:covered-table-cell/>
      </table:table-row>
      <table:table-row table:style-name="ro1">
        <table:covered-table-cell/>
      </table:table-row>
      <table:table-row table:visibility="collapse"/>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#;
        let settings = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-settings
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:config="urn:oasis:names:tc:opendocument:xmlns:config:1.0">
  <office:settings>
    <config:config-item-set config:name="ooo:view-settings">
      <config:config-item-map-indexed config:name="Views">
        <config:config-item-map-entry>
          <config:config-item-map-named config:name="Tables">
            <config:config-item-map-entry config:name="Spans">
              <config:config-item config:name="HorizontalSplitMode" config:type="short">2</config:config-item>
              <config:config-item config:name="HorizontalSplitPosition" config:type="int">1</config:config-item>
              <config:config-item config:name="VerticalSplitMode" config:type="short">2</config:config-item>
              <config:config-item config:name="VerticalSplitPosition" config:type="int">2</config:config-item>
            </config:config-item-map-entry>
          </config:config-item-map-named>
        </config:config-item-map-entry>
      </config:config-item-map-indexed>
    </config:config-item-set>
  </office:settings>
</office:document-settings>"#;
        let path = package(
            "spans.ods",
            &[("content.xml", content), ("settings.xml", settings)],
        );
        let layout = read(&path);
        let sheet = layout.sheets.get("Spans").expect("layout for Spans");

        // 3 cm is 96 / 2.54 * 3 pixels; the two repeated columns keep the
        // package default (none here), so only column A is sized.
        assert_eq!(sheet.col_widths.get(&0), Some(&113.0));
        assert_eq!(sheet.col_widths.len(), 1);
        // 0.8 cm, on the second row only.
        assert_eq!(sheet.row_heights.get(&1), Some(&30.0));
        assert_eq!(sheet.row_heights.get(&0), None);
        // The repeated hidden column hides every column it covers.
        assert_eq!(
            sheet.hidden_cols.iter().copied().collect::<Vec<_>>(),
            [1, 2]
        );
        assert_eq!(sheet.hidden_rows.iter().copied().collect::<Vec<_>>(), [2]);
        assert_eq!(sheet.frozen_rows, 2);
        assert_eq!(sheet.frozen_cols, 1);
        assert_eq!(
            sheet.merges,
            vec![MergeInfo {
                row: 0,
                col: 0,
                rows: 2,
                cols: 2
            }]
        );
    }

    #[test]
    fn an_ods_freeze_alone_still_counts_as_layout() {
        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body><office:spreadsheet>
    <table:table table:name="Frozen">
      <table:table-row><table:table-cell office:value-type="string"><text:p>x</text:p></table:table-cell></table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#;
        let settings = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-settings xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:config="urn:oasis:names:tc:opendocument:xmlns:config:1.0">
  <office:settings>
    <config:config-item-set config:name="ooo:view-settings">
      <config:config-item-map-indexed config:name="Views">
        <config:config-item-map-entry>
          <config:config-item-map-named config:name="Tables">
            <config:config-item-map-entry config:name="Frozen">
              <config:config-item config:name="VerticalSplitMode" config:type="short">2</config:config-item>
              <config:config-item config:name="VerticalSplitPosition" config:type="int">3</config:config-item>
            </config:config-item-map-entry>
          </config:config-item-map-named>
        </config:config-item-map-entry>
      </config:config-item-map-indexed>
    </config:config-item-set>
  </office:settings>
</office:document-settings>"#;
        let path = package(
            "freeze-only.ods",
            &[("content.xml", content), ("settings.xml", settings)],
        );
        let layout = read(&path);
        let sheet = layout.sheets.get("Frozen").expect("a freeze is layout");
        assert_eq!(sheet.frozen_rows, 3);
        assert_eq!(sheet.frozen_cols, 0);
    }

    #[test]
    fn a_hidden_row_or_column_alone_counts_as_layout() {
        // Hiding is geometry even when no size is given for the row or column:
        // an empty hidden row has no height of its own.
        let sheet = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
            <sheetData>
              <row r="1"><c r="A1"><v>1</v></c></row>
              <row r="2" hidden="1"/>
            </sheetData>
        </worksheet>"#;
        let path = xlsx_package("hidden-row.xlsx", sheet);
        let sheet = read(&path)
            .sheets
            .remove("Sized")
            .expect("a hidden row is layout");
        assert_eq!(sheet.hidden_rows.iter().copied().collect::<Vec<_>>(), [1]);

        let content = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0">
  <office:body><office:spreadsheet>
    <table:table table:name="Hidden">
      <table:table-column table:visibility="collapse"/>
      <table:table-row><table:table-cell office:value-type="string"><text:p>x</text:p></table:table-cell></table:table-row>
    </table:table>
  </office:spreadsheet></office:body>
</office:document-content>"#;
        let path = package("hidden-column.ods", &[("content.xml", content)]);
        let sheet = read(&path)
            .sheets
            .remove("Hidden")
            .expect("a hidden column is layout");
        assert_eq!(sheet.hidden_cols.iter().copied().collect::<Vec<_>>(), [0]);
        assert!(sheet.col_widths.is_empty(), "no width was given");
    }

    #[test]
    fn packages_without_geometry_yield_nothing() {
        let sheet = r#"<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
            <sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData>
        </worksheet>"#;
        let path = xlsx_package("plain.xlsx", sheet);
        assert!(
            read(&path).sheets.is_empty(),
            "a sheet with no sizes needs no layout"
        );

        let path = temp_dir().join("not-a-package.xlsx");
        std::fs::write(&path, b"not a zip").unwrap();
        assert!(read(&path).sheets.is_empty());
    }
}
