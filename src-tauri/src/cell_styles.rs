//! Reading the number formats a workbook assigns to its cells.
//!
//! `calamine` reports values; how a cell is *displayed* is stored separately in
//! the package's style tables. Those are read here:
//!
//! - OOXML (`.xlsx`, `.xlsm`): `xl/styles.xml` holds the format codes and the
//!   `cellXfs` table the cells index into; each worksheet carries a style index
//!   per cell, and the workbook says which calendar the serial dates use.
//! - ODF (`.ods`): `content.xml` holds `number:*` style definitions, cell
//!   styles that point at them, and cells that name a cell style. ODF styles
//!   are translated into the Excel format-code grammar so both families render
//!   through one path (see [`crate::number_format`]).
//!
//! Nothing here is fatal: a package that cannot be read, or that has no formats
//! beyond `General`, simply yields no overrides and the grid renders as before.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek};
use std::path::Path;
use std::sync::Arc;

use quick_xml::events::{BytesStart, Event};
use quick_xml::{Reader, XmlVersion};
use ssfmt::DateSystem;
use zip::ZipArchive;

use crate::model::SheetFormats;
use crate::number_format::NumberFormat;

/// Bound on the formatted cells remembered for one workbook, mirroring the
/// reader's own cell budget so a hostile package cannot exhaust memory here.
const MAX_FORMATTED: usize = 2_000_000;

/// Excel's own column and row limits, used to clamp repeated ODF cells.
const MAX_ROWS: u32 = 1_048_576;
const MAX_COLS: u32 = 16_384;

/// What a package says about cell formatting.
#[derive(Debug, Default)]
pub struct WorkbookFormats {
    /// The calendar serial dates are counted on.
    pub date_system: DateSystem,
    /// Formats per sheet *name*: that is how sheets are identified when the
    /// rows are read, and it survives a package whose sheet order differs.
    pub sheets: HashMap<String, SheetFormats>,
}

/// Read a workbook's formats, or nothing if the package cannot be read.
pub fn read(path: &Path) -> WorkbookFormats {
    let extension = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();

    let out = WorkbookFormats::default();
    let Ok(file) = File::open(path) else {
        return out;
    };
    let Ok(mut zip) = ZipArchive::new(BufReader::new(file)) else {
        return out;
    };

    match extension.as_str() {
        "xlsx" | "xlsm" | "xltx" | "xltm" | "xlam" => xlsx(&mut zip),
        "ods" => ods(&mut zip),
        // `.xls`/`.xlsb` keep their formats in binary tables that calamine
        // does not expose; the grid falls back to the plain rendering there.
        _ => out,
    }
}

// ---------------------------------------------------------------------------
// OOXML
// ---------------------------------------------------------------------------

fn xlsx<R: Read + Seek>(zip: &mut ZipArchive<R>) -> WorkbookFormats {
    let mut out = WorkbookFormats::default();
    let Some(workbook) = part(zip, "xl/workbook.xml") else {
        return out;
    };
    let sheets = parse_workbook(&workbook, &mut out.date_system);
    if sheets.is_empty() {
        return out;
    }

    let rels = part(zip, "xl/_rels/workbook.xml.rels").unwrap_or_default();
    let targets = parse_rels(&rels);

    let Some(styles) = part(zip, "xl/styles.xml") else {
        return out;
    };
    let formats = parse_styles(&styles, out.date_system);
    if formats.iter().all(Option::is_none) {
        // Every style is `General`: no worksheet needs scanning at all.
        return out;
    }

    let mut budget = MAX_FORMATTED;
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
        let mut found = SheetFormats::default();
        read_sheet(BufReader::new(entry), &formats, &mut found, &mut budget);
        if !found.is_empty() {
            out.sheets.insert(name, found);
        }
    }
    out
}

fn part<R: Read + Seek>(zip: &mut ZipArchive<R>, name: &str) -> Option<Vec<u8>> {
    let mut entry = zip.by_name(name).ok()?;
    let mut buffer = Vec::with_capacity(entry.size() as usize);
    entry.read_to_end(&mut buffer).ok()?;
    Some(buffer)
}

/// An attribute by local name, with XML entities decoded.
fn attr(element: &BytesStart, name: &[u8]) -> Option<String> {
    element
        .attributes()
        .flatten()
        .find(|a| a.key.local_name().as_ref() == name)
        .and_then(|a| a.normalized_value(XmlVersion::Implicit1_0).ok())
        .map(|value| value.into_owned())
}

/// Sheet names in document order, with the relationship id of each part.
fn parse_workbook(xml: &[u8], system: &mut DateSystem) -> Vec<(String, String)> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut sheets = Vec::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                match element.local_name().as_ref() {
                    b"sheet" => {
                        // `sheetId` is not `id`, and `r:id` is the part we want.
                        if let (Some(name), Some(id)) =
                            (attr(&element, b"name"), attr(&element, b"id"))
                        {
                            sheets.push((name, id));
                        }
                    }
                    b"workbookPr" => {
                        if matches!(
                            attr(&element, b"date1904").as_deref(),
                            Some("1") | Some("true")
                        ) {
                            *system = DateSystem::Date1904;
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
    sheets
}

/// Relationship id to part name.
fn parse_rels(xml: &[u8]) -> HashMap<String, String> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut targets = HashMap::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                if element.local_name().as_ref() == b"Relationship" {
                    if let (Some(id), Some(target)) =
                        (attr(&element, b"Id"), attr(&element, b"Target"))
                    {
                        targets.insert(id, target);
                    }
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
    targets
}

/// A relationship target as a zip entry name.
fn worksheet_path(target: &str) -> Option<String> {
    if target.is_empty() || target.contains("..") || target.contains(':') {
        return None;
    }
    let trimmed = target.trim_start_matches('/');
    Some(if trimmed.starts_with("xl/") {
        trimmed.to_string()
    } else {
        format!("xl/{trimmed}")
    })
}

/// Format of each `cellXfs` entry, indexed by the style index cells carry.
fn parse_styles(xml: &[u8], system: DateSystem) -> Vec<Option<Arc<NumberFormat>>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut custom: HashMap<u32, String> = HashMap::new();
    let mut ids: Vec<u32> = Vec::new();
    let mut in_cell_xfs = false;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) => match element.local_name().as_ref() {
                b"cellXfs" => in_cell_xfs = true,
                b"xf" if in_cell_xfs => ids.push(format_id(&element)),
                // Some producers write the pair instead of a self-closing tag.
                b"numFmt" => {
                    let id = attr(&element, b"numFmtId").and_then(|v| v.parse::<u32>().ok());
                    if let (Some(id), Some(code)) = (id, attr(&element, b"formatCode")) {
                        custom.insert(id, code);
                    }
                }
                _ => {}
            },
            Ok(Event::Empty(element)) => match element.local_name().as_ref() {
                b"numFmt" => {
                    let id = attr(&element, b"numFmtId").and_then(|v| v.parse::<u32>().ok());
                    if let (Some(id), Some(code)) = (id, attr(&element, b"formatCode")) {
                        custom.insert(id, code);
                    }
                }
                b"xf" if in_cell_xfs => ids.push(format_id(&element)),
                _ => {}
            },
            Ok(Event::End(element)) if element.local_name().as_ref() == b"cellXfs" => {
                in_cell_xfs = false;
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }

    ids.into_iter()
        .map(|id| {
            let code = custom
                .get(&id)
                .cloned()
                .or_else(|| ssfmt::format_code_from_id(id).map(str::to_string));
            code.and_then(|code| NumberFormat::parse(&code, system))
        })
        .collect()
}

fn format_id(element: &BytesStart) -> u32 {
    attr(element, b"numFmtId")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0)
}

/// Collect the style index of every cell in a worksheet.
fn read_sheet<R: BufRead>(
    reader: R,
    formats: &[Option<Arc<NumberFormat>>],
    out: &mut SheetFormats,
    budget: &mut usize,
) {
    let mut reader = Reader::from_reader(reader);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut row = 0u32;
    let mut col = 0u32;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                match element.local_name().as_ref() {
                    b"row" => col = 0,
                    b"c" => {
                        // Attributes are parsed in place: a worksheet can hold
                        // a million cells, and allocating a string per
                        // attribute is most of the cost of this pass.
                        let mut reference = None;
                        let mut style = None;
                        for attribute in element.attributes().flatten() {
                            let value = std::str::from_utf8(attribute.value.as_ref()).ok();
                            match attribute.key.local_name().as_ref() {
                                b"r" => reference = value.and_then(parse_cell_ref),
                                b"s" => style = value.and_then(|text| text.parse::<usize>().ok()),
                                _ => {}
                            }
                        }
                        let position = reference.unwrap_or((row, col));
                        (row, col) = (position.0, position.1 + 1);
                        if let Some(format) = style
                            .and_then(|index| formats.get(index))
                            .and_then(Option::as_ref)
                        {
                            if *budget == 0 {
                                return;
                            }
                            *budget -= 1;
                            out.insert(position.0 as usize, position.1 as usize, format.clone());
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

/// `C7` (with optional `$`) as a zero-based `(row, column)`.
fn parse_cell_ref(text: &str) -> Option<(u32, u32)> {
    let text = text.rsplit('!').next().unwrap_or(text);
    let mut col = 0u32;
    let mut row = 0u32;
    let mut in_digits = false;
    for ch in text.chars() {
        match ch {
            // `$` marks an absolute reference; it does not change the position.
            '$' => {}
            c if c.is_ascii_digit() => {
                in_digits = true;
                row = row
                    .checked_mul(10)?
                    .checked_add(c as u32 - u32::from(b'0'))?;
            }
            c => {
                if in_digits {
                    return None;
                }
                let letter = c.to_ascii_uppercase() as u32;
                if !(u32::from(b'A')..=u32::from(b'Z')).contains(&letter) {
                    return None;
                }
                col = col
                    .checked_mul(26)?
                    .checked_add(letter - u32::from(b'A') + 1)?;
            }
        }
    }
    if !in_digits || col == 0 {
        return None;
    }
    Some((row.checked_sub(1)?, col - 1))
}

// ---------------------------------------------------------------------------
// ODF
// ---------------------------------------------------------------------------

fn ods<R: Read + Seek>(zip: &mut ZipArchive<R>) -> WorkbookFormats {
    let Ok(entry) = zip.by_name("content.xml") else {
        return WorkbookFormats::default();
    };
    parse_content(BufReader::new(entry))
}

/// An ODF style being translated into a format code.
struct Build {
    name: String,
    kind: Kind,
    code: String,
    /// Stack depth of the style's parent; used to know when it closes.
    depth: usize,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    Number,
    Currency,
    Percentage,
    Date,
    Time,
    Text,
    Scientific,
    Fraction,
    Boolean,
}

fn kind_of(local: &[u8]) -> Option<Kind> {
    Some(match local {
        b"number-style" => Kind::Number,
        b"currency-style" => Kind::Currency,
        b"percentage-style" => Kind::Percentage,
        b"date-style" => Kind::Date,
        b"time-style" => Kind::Time,
        b"text-style" => Kind::Text,
        b"scientific-number-style" => Kind::Scientific,
        b"fraction-style" => Kind::Fraction,
        b"boolean-style" => Kind::Boolean,
        _ => return None,
    })
}

/// A worksheet while its rows are being scanned.
struct Table {
    name: String,
    formats: SheetFormats,
    row: u32,
    col: u32,
    row_style: Option<String>,
    columns: Vec<Option<String>>,
    /// Formats found in the current row, so a repeated row can be copied.
    current: Vec<(u32, Arc<NumberFormat>)>,
    row_repeat: u32,
}

impl Table {
    fn new(name: String) -> Self {
        Self {
            name,
            formats: SheetFormats::default(),
            row: 0,
            col: 0,
            row_style: None,
            columns: Vec::new(),
            current: Vec::new(),
            row_repeat: 1,
        }
    }
}

/// Everything `content.xml` contributes, accumulated in one streaming pass.
struct Scan {
    /// Data style name to format code.
    number: HashMap<String, String>,
    /// Cell/row/column style name to data style name.
    cell: HashMap<String, String>,
    /// The ODF default for table cells, if the file sets one.
    default_cell: Option<String>,
    system: DateSystem,
    /// Parsed formats, keyed by their code: one parse per distinct style.
    parsed: HashMap<String, Option<Arc<NumberFormat>>>,
    table: Option<Table>,
    sheets: HashMap<String, SheetFormats>,
    budget: usize,
}

fn parse_content<R: BufRead>(reader: R) -> WorkbookFormats {
    let mut reader = Reader::from_reader(reader);
    // Whitespace inside `number:text` is significant (" USD"), but elsewhere
    // it is not worth keeping; the builder trims literals it does not want.
    reader.config_mut().trim_text(false);

    let mut scan = Scan {
        number: HashMap::new(),
        cell: HashMap::new(),
        default_cell: None,
        system: DateSystem::Date1900,
        parsed: HashMap::new(),
        table: None,
        sheets: HashMap::new(),
        budget: MAX_FORMATTED,
    };
    let mut stack: Vec<Vec<u8>> = Vec::new();
    let mut build: Option<Build> = None;
    let mut text: Option<String> = None;
    let mut buffer = Vec::new();

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) => {
                let local = element.local_name().as_ref().to_vec();
                scan.start(&element, &local, &stack, &mut build, &mut text);
                stack.push(local);
            }
            Ok(Event::Empty(element)) => {
                let local = element.local_name().as_ref().to_vec();
                let was_open = build.is_some();
                let mut no_text = None;
                scan.start(&element, &local, &stack, &mut build, &mut no_text);
                // A self-closing style definition is complete on its own (a
                // self-closing element never receives an End event); a
                // self-closing child belongs to a style that is still open.
                if !was_open {
                    if let Some(done) = build.take() {
                        scan.finish(done);
                    }
                }
            }
            Ok(Event::Text(event)) => {
                if let Some(text) = text.as_mut() {
                    if let Ok(decoded) = event.xml10_content() {
                        if let Ok(value) = quick_xml::escape::unescape(&decoded) {
                            text.push_str(&value);
                        }
                    }
                }
            }
            // A currency symbol is usually written as a character reference
            // (`&#8364;`), which arrives as its own event rather than as text.
            Ok(Event::GeneralRef(reference)) => {
                if let Some(text) = text.as_mut() {
                    match reference.resolve_char_ref() {
                        Ok(Some(ch)) => text.push(ch),
                        _ => {
                            if let Ok(name) = reference.decode() {
                                text.push_str(predefined(name.as_ref()));
                            }
                        }
                    }
                }
            }
            Ok(Event::CData(data)) => {
                if let Some(text) = text.as_mut() {
                    if let Ok(value) = data.decode() {
                        text.push_str(&value);
                    }
                }
            }
            Ok(Event::End(_)) => {
                if let Some(closing) = stack.pop() {
                    if text.is_some() && is_text_element(&closing) {
                        let literal = text.take().unwrap_or_default();
                        if let Some(build) = build.as_mut() {
                            build.push_text(&literal);
                        }
                    }
                    if build.as_ref().is_some_and(|b| stack.len() == b.depth) {
                        if let Some(done) = build.take() {
                            scan.finish(done);
                        }
                    }
                    scan.end(&closing);
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }

    WorkbookFormats {
        date_system: scan.system,
        sheets: scan.sheets,
    }
}

impl Scan {
    /// Handle an element that is just starting (or is self-closing).
    fn start(
        &mut self,
        element: &BytesStart,
        local: &[u8],
        stack: &[Vec<u8>],
        build: &mut Option<Build>,
        text: &mut Option<String>,
    ) {
        let parent = stack.last().map(Vec::as_slice);

        // Style definitions live in the document header, never in the body.
        let in_header = matches!(parent, Some(b"automatic-styles") | Some(b"styles"));
        if in_header {
            if let Some(kind) = kind_of(local) {
                if let Some(name) = attr(element, b"name") {
                    *build = Some(Build {
                        name,
                        kind,
                        code: String::new(),
                        depth: stack.len(),
                    });
                }
            } else if local == b"style" {
                if let (Some(name), Some(data)) =
                    (attr(element, b"name"), attr(element, b"data-style-name"))
                {
                    self.cell.insert(name, data);
                }
            } else if local == b"default-style"
                && attr(element, b"family").as_deref() == Some("table-cell")
            {
                self.default_cell = attr(element, b"data-style-name");
            }
        }

        if let Some(current) = build.as_mut() {
            if stack.len() == current.depth + 1 {
                if is_text_element(local) {
                    *text = Some(String::new());
                } else {
                    current.push_element(element, local);
                }
                return;
            }
        }

        if local == b"calculation-settings" {
            if let Some(null) = attr(element, b"null-date") {
                if null.starts_with("1904") {
                    self.system = DateSystem::Date1904;
                } else if null.starts_with("1899-12-30") {
                    self.system = DateSystem::Date1900;
                }
            }
        }

        self.start_table_element(element, local);
    }

    fn end(&mut self, local: &[u8]) {
        match local {
            b"table-row" => self.end_row(),
            b"table" => {
                if let Some(table) = self.table.take() {
                    if !table.formats.is_empty() {
                        self.sheets.insert(table.name, table.formats);
                    }
                }
            }
            _ => {}
        }
    }

    fn finish(&mut self, mut build: Build) {
        build.finish();
        if let Some(format) = NumberFormat::parse(&build.code, self.system) {
            self.number.insert(build.name, build.code.clone());
            self.parsed.insert(build.code, Some(format));
        }
    }

    /// Resolve a cell style name to a parsed format.
    fn format_for(&mut self, style: &str) -> Option<Arc<NumberFormat>> {
        let data = self.cell.get(style)?.clone();
        let code = self.number.get(&data)?.clone();
        if let Some(cached) = self.parsed.get(&code) {
            return cached.clone();
        }
        let parsed = NumberFormat::parse(&code, self.system);
        self.parsed.insert(code, parsed.clone());
        parsed
    }

    fn start_table_element(&mut self, element: &BytesStart, local: &[u8]) {
        match local {
            b"table" => {
                if let Some(name) = attr(element, b"name") {
                    self.table = Some(Table::new(name));
                }
            }
            b"table-column" => {
                let Some(table) = self.table.as_mut() else {
                    return;
                };
                let style = attr(element, b"style-name");
                let repeat = attr_u32(element, b"number-columns-repeated", 1);
                for _ in 0..repeat {
                    if table.columns.len() as u32 >= MAX_COLS {
                        break;
                    }
                    table.columns.push(style.clone());
                }
            }
            b"table-row" => {
                let Some(table) = self.table.as_mut() else {
                    return;
                };
                table.row_style = attr(element, b"style-name");
                table.row_repeat = attr_u32(element, b"number-rows-repeated", 1).clamp(1, MAX_ROWS);
                table.current.clear();
                table.col = 0;
            }
            b"table-cell" | b"covered-table-cell" => self.cell_at(element, local == b"table-cell"),
            _ => {}
        }
    }

    fn cell_at(&mut self, element: &BytesStart, has_content: bool) {
        // The column moves on even for a cell that is not worth remembering.
        let (row, start, repeat, style) = {
            let Some(table) = self.table.as_mut() else {
                return;
            };
            let repeat = attr_u32(element, b"number-columns-repeated", 1).min(MAX_COLS);
            let start = table.col;
            table.col = (table.col + repeat).min(MAX_COLS);
            let style = if has_content && has_value(element) {
                attr(element, b"style-name")
                    .or_else(|| table.columns.get(start as usize).cloned().flatten())
                    .or_else(|| table.row_style.clone())
                    .or_else(|| self.default_cell.clone())
            } else {
                None
            };
            (table.row, start, repeat, style)
        };
        if self.budget == 0 {
            return;
        }
        let Some(format) = style.and_then(|style| self.format_for(&style)) else {
            return;
        };
        let Some(table) = self.table.as_mut() else {
            return;
        };
        let mut written = 0usize;
        for offset in 0..repeat {
            let col = start + offset;
            if col >= MAX_COLS || row >= MAX_ROWS || written >= self.budget {
                break;
            }
            table
                .formats
                .insert(row as usize, col as usize, format.clone());
            table.current.push((col, format.clone()));
            written += 1;
        }
        self.budget -= written;
    }

    fn end_row(&mut self) {
        let Some(table) = self.table.as_mut() else {
            return;
        };
        let mut written = 0usize;
        // LibreOffice collapses runs of identical rows into one repeated row;
        // the formats belong to every row in that run.
        for extra in 1..table.row_repeat {
            let row = table.row.saturating_add(extra);
            if row >= MAX_ROWS || written >= self.budget {
                break;
            }
            for (col, format) in &table.current {
                if written >= self.budget {
                    break;
                }
                table
                    .formats
                    .insert(row as usize, *col as usize, format.clone());
                written += 1;
            }
        }
        table.row = table.row.saturating_add(table.row_repeat).min(MAX_ROWS);
        table.col = 0;
        table.row_style = None;
        table.current.clear();
        self.budget -= written;
    }
}

impl Build {
    fn push_element(&mut self, element: &BytesStart, local: &[u8]) {
        match local {
            b"number" => self.code.push_str(&number_pattern(element)),
            b"scientific-number" => self.code.push_str(&scientific_pattern(element)),
            b"fraction" => self.code.push_str(&fraction_pattern(element)),
            b"text-content" => self.code.push('@'),
            b"am-pm" => self.code.push_str("AM/PM"),
            _ => {
                if let Some(token) = date_token(element, local) {
                    self.code.push_str(token);
                }
            }
        }
    }

    fn push_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // A `%` in a percentage style is the scaling token, not a literal: it
        // must reach the format engine unquoted or the value is not scaled.
        if self.kind == Kind::Percentage && text.trim() == "%" {
            self.code.push('%');
        } else {
            self.code.push_str(&quote(text));
        }
    }

    fn finish(&mut self) {
        if self.kind == Kind::Percentage && !self.code.contains('%') {
            self.code.push('%');
        }
        if self.kind == Kind::Text && !self.code.contains('@') {
            self.code.insert(0, '@');
        }
    }
}

fn is_text_element(local: &[u8]) -> bool {
    matches!(local, b"text" | b"currency-symbol")
}

/// The five entities XML defines without a document type declaration.
fn predefined(name: &str) -> &'static str {
    match name {
        "amp" => "&",
        "lt" => "<",
        "gt" => ">",
        "quot" => "\"",
        "apos" => "'",
        _ => "",
    }
}

fn date_token(element: &BytesStart, local: &[u8]) -> Option<&'static str> {
    let long = attr(element, b"style").as_deref() == Some("long");
    let textual = matches!(attr(element, b"textual").as_deref(), Some("true"));
    Some(match local {
        b"year" => {
            if long {
                "yyyy"
            } else {
                "yy"
            }
        }
        b"month" if textual => {
            if long {
                "mmmm"
            } else {
                "mmm"
            }
        }
        b"month" => {
            if long {
                "mm"
            } else {
                "m"
            }
        }
        b"day" => {
            if long {
                "dd"
            } else {
                "d"
            }
        }
        b"day-of-week" => {
            if long {
                "dddd"
            } else {
                "ddd"
            }
        }
        b"hours" => {
            if long {
                "hh"
            } else {
                "h"
            }
        }
        b"minutes" => {
            if long {
                "mm"
            } else {
                "m"
            }
        }
        b"seconds" => {
            if long {
                "ss"
            } else {
                "s"
            }
        }
        b"am-pm" => "AM/PM",
        _ => return None,
    })
}

fn number_pattern(element: &BytesStart) -> String {
    let decimals = attr_u32(element, b"decimal-places", 0);
    let min_decimals = attr_u32(element, b"min-decimal-places", decimals).min(decimals);
    let min_integer = attr_u32(element, b"min-integer-digits", 1).max(1);
    let grouping = matches!(attr(element, b"grouping").as_deref(), Some("true"));

    let mut out = String::new();
    if grouping {
        out.push_str("#,##");
        out.push_str(&"0".repeat((min_integer - 1) as usize));
        out.push('0');
    } else {
        out.push_str(&"0".repeat(min_integer as usize));
    }
    if decimals > 0 {
        out.push('.');
        out.push_str(&"0".repeat(min_decimals as usize));
        out.push_str(&"#".repeat((decimals - min_decimals) as usize));
    }
    out
}

fn scientific_pattern(element: &BytesStart) -> String {
    let decimals = attr_u32(element, b"decimal-places", 2);
    let min_integer = attr_u32(element, b"min-integer-digits", 1).max(1);
    let exponent = attr_u32(element, b"exponent-digits", 2).max(1);
    format!(
        "{}.{}E+{}",
        "0".repeat(min_integer as usize),
        "0".repeat(decimals as usize),
        "0".repeat(exponent as usize)
    )
}

fn fraction_pattern(element: &BytesStart) -> String {
    let numerator = attr_u32(element, b"min-numerator-digits", 1).max(1);
    let denominator = match attr(element, b"denominator-value") {
        Some(value) => value,
        None => {
            let digits = attr_u32(element, b"min-denominator-digits", 1).max(1);
            "?".repeat(digits as usize)
        }
    };
    format!("# {}/{}", "?".repeat(numerator as usize), denominator)
}

/// Quote literal text so the format engine shows it as written.
fn quote(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        if ch == '"' {
            out.push_str("\"\"");
        } else {
            out.push(ch);
        }
    }
    out.push('"');
    out
}

fn attr_u32(element: &BytesStart, name: &[u8], default: u32) -> u32 {
    attr(element, name)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

/// Whether a cell carries a value at all; an empty styled cell is not worth
/// remembering, and skipping it keeps repeated empty cells cheap.
fn has_value(element: &BytesStart) -> bool {
    const VALUE_ATTRIBUTES: [&[u8]; 6] = [
        b"value",
        b"boolean-value",
        b"date-value",
        b"time-value",
        b"string-value",
        b"value-type",
    ];
    element.attributes().flatten().any(|a| {
        let local = a.key.local_name();
        VALUE_ATTRIBUTES.contains(&local.as_ref())
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::Cell;
    use chrono::NaiveDate;
    use std::io::Write;

    fn date(year: i32, month: u32, day: u32) -> Cell {
        Cell::DateTime(
            NaiveDate::from_ymd_opt(year, month, day)
                .unwrap()
                .and_hms_opt(0, 0, 0)
                .unwrap(),
        )
    }

    fn temp_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("excelscry-cell-styles");
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

    /// How the format found at a position renders a representative value.
    fn render_at(
        formats: &WorkbookFormats,
        sheet: &str,
        row: usize,
        col: usize,
        value: Cell,
    ) -> Option<String> {
        formats.sheets.get(sheet)?.get(row, col)?.render(&value)
    }

    #[test]
    fn cell_references_are_parsed_into_positions() {
        assert_eq!(parse_cell_ref("A1"), Some((0, 0)));
        assert_eq!(parse_cell_ref("C7"), Some((6, 2)));
        assert_eq!(parse_cell_ref("$AA$10"), Some((9, 26)));
        assert_eq!(parse_cell_ref("Sheet1!B2"), Some((1, 1)));
        assert_eq!(parse_cell_ref("7"), None);
        assert_eq!(parse_cell_ref("A"), None);
    }

    #[test]
    fn xlsx_custom_and_builtin_formats_are_read_by_position() {
        let styles = r##"<?xml version="1.0"?>
<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <numFmts count="2">
    <numFmt numFmtId="164" formatCode="#,##0.00 &quot;USD&quot;"/>
    <numFmt numFmtId="165" formatCode="0.00%"/>
  </numFmts>
  <cellStyleXfs count="1"><xf numFmtId="0"/></cellStyleXfs>
  <cellXfs count="4">
    <xf numFmtId="0" applyNumberFormat="0"/>
    <xf numFmtId="164" applyNumberFormat="1"/>
    <xf numFmtId="165" applyNumberFormat="1"/>
    <xf numFmtId="14" applyNumberFormat="1"/>
  </cellXfs>
</styleSheet>"##;
        let workbook = r#"<?xml version="1.0"?>
<workbook xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main"
          xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships">
  <workbookPr/>
  <sheets>
    <sheet name="Money" sheetId="1" r:id="rId1"/>
  </sheets>
</workbook>"#;
        let rels = r#"<?xml version="1.0"?>
<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships">
  <Relationship Id="rId1" Type="worksheet" Target="worksheets/sheet1.xml"/>
</Relationships>"#;
        let sheet = r#"<?xml version="1.0"?>
<worksheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <sheetData>
    <row r="1">
      <c r="A1" s="1"><v>12.5</v></c>
      <c r="B1" s="2"><v>0.5</v></c>
    </row>
    <row r="2">
      <c r="C2" s="3"><v>46031</v></c>
      <c r="D2"><v>7</v></c>
    </row>
  </sheetData>
</worksheet>"#;

        let path = package(
            "formats.xlsx",
            &[
                ("xl/workbook.xml", workbook),
                ("xl/_rels/workbook.xml.rels", rels),
                ("xl/styles.xml", styles),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );
        let found = read(&path);

        assert_eq!(
            render_at(&found, "Money", 0, 0, Cell::Float(12.5)).as_deref(),
            Some("12.50 USD")
        );
        assert_eq!(
            render_at(&found, "Money", 0, 1, Cell::Float(0.5)).as_deref(),
            Some("50.00%")
        );
        // A built-in id is resolved through Excel's table, not dropped.
        assert_eq!(
            render_at(&found, "Money", 1, 2, date(2026, 1, 9)).as_deref(),
            Some("1/9/26")
        );
        // An unformatted cell is simply absent.
        assert_eq!(render_at(&found, "Money", 1, 3, Cell::Float(7.0)), None);
    }

    #[test]
    fn a_1904_workbook_is_read_with_its_own_calendar() {
        let styles = r#"<styleSheet><cellXfs><xf numFmtId="14"/></cellXfs></styleSheet>"#;
        let workbook = r#"<workbook xmlns:r="r"><workbookPr date1904="1"/><sheets><sheet name="S" sheetId="1" r:id="rId1"/></sheets></workbook>"#;
        let rels = r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;
        let sheet = r#"<worksheet><sheetData><row r="1"><c r="A1" s="0"><v>0</v></c></row></sheetData></worksheet>"#;
        let path = package(
            "mac.xlsx",
            &[
                ("xl/workbook.xml", workbook),
                ("xl/_rels/workbook.xml.rels", rels),
                ("xl/styles.xml", styles),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );
        let found = read(&path);
        assert_eq!(found.date_system, DateSystem::Date1904);
        // The same calendar day is 1462 days earlier on this system, so a
        // format rendered with the wrong one would land in a different year.
        assert_eq!(
            render_at(&found, "S", 0, 0, date(2026, 1, 9)).as_deref(),
            Some("1/9/26")
        );
    }

    #[test]
    fn xlsx_worksheet_stride_matches_the_style_table() {
        // The style index is per cell; a cell without `r` still advances the
        // column, so the formats must not slide onto their neighbours.
        let styles = r#"<styleSheet><numFmts><numFmt numFmtId="164" formatCode="0.0"/></numFmts><cellXfs><xf numFmtId="0"/><xf numFmtId="164"/></cellXfs></styleSheet>"#;
        let workbook =
            r#"<workbook xmlns:r="r"><sheets><sheet name="S" r:id="rId1"/></sheets></workbook>"#;
        let rels = r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;
        let sheet = r#"<worksheet><sheetData><row r="1"><c><v>1</v></c><c s="1"><v>2.5</v></c></row></sheetData></worksheet>"#;
        let path = package(
            "stride.xlsx",
            &[
                ("xl/workbook.xml", workbook),
                ("xl/_rels/workbook.xml.rels", rels),
                ("xl/styles.xml", styles),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );
        let found = read(&path);
        assert_eq!(render_at(&found, "S", 0, 0, Cell::Float(1.0)), None);
        assert_eq!(
            render_at(&found, "S", 0, 1, Cell::Float(2.5)).as_deref(),
            Some("2.5")
        );
    }

    const ODS_CONTENT: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<office:document-content
    xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
    xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
    xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
    xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0"
    xmlns:number="urn:oasis:names:tc:opendocument:xmlns:datastyle:1.0">
  <office:automatic-styles>
    <number:currency-style style:name="N1">
      <number:currency-symbol>&#8364;</number:currency-symbol>
      <number:number number:decimal-places="2" number:min-integer-digits="1" number:grouping="true"/>
    </number:currency-style>
    <number:percentage-style style:name="N2">
      <number:number number:decimal-places="1" number:min-decimal-places="1"/>
      <number:text>%</number:text>
    </number:percentage-style>
    <number:date-style style:name="N3">
      <number:day number:style="long"/>
      <number:text>/</number:text>
      <number:month number:style="long"/>
      <number:text>/</number:text>
      <number:year number:style="long"/>
    </number:date-style>
    <number:time-style style:name="N4">
      <number:hours number:style="long"/>
      <number:text>:</number:text>
      <number:minutes number:style="long"/>
    </number:time-style>
    <style:style style:name="ce1" style:family="table-cell" style:data-style-name="N1"/>
    <style:style style:name="ce2" style:family="table-cell" style:data-style-name="N2"/>
    <style:style style:name="ce3" style:family="table-cell" style:data-style-name="N3"/>
    <style:style style:name="ce4" style:family="table-cell" style:data-style-name="N4"/>
  </office:automatic-styles>
  <office:body>
    <office:spreadsheet>
      <table:calculation-settings table:null-date="1899-12-30"/>
      <table:table table:name="Money">
        <table:table-row>
          <table:table-cell table:style-name="ce1" office:value-type="currency" office:value="1234.5"><text:p>1,234.50</text:p></table:table-cell>
          <table:table-cell table:style-name="ce2" office:value-type="percentage" office:value="0.125"><text:p>12.5%</text:p></table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell table:style-name="ce3" office:value-type="date" office:date-value="2026-01-09"><text:p/></table:table-cell>
          <table:table-cell table:style-name="ce4" office:value-type="time" office:time-value="PT09H30M00S"><text:p/></table:table-cell>
        </table:table-row>
        <table:table-row>
          <table:table-cell office:value-type="float" office:value="3"><text:p>3</text:p></table:table-cell>
        </table:table-row>
      </table:table>
    </office:spreadsheet>
  </office:body>
</office:document-content>"#;

    #[test]
    fn ods_number_styles_become_format_codes() {
        let path = package("styles.ods", &[("content.xml", ODS_CONTENT)]);
        let found = read(&path);
        assert_eq!(
            render_at(&found, "Money", 0, 0, Cell::Float(1234.5)).as_deref(),
            Some("€1,234.50")
        );
        assert_eq!(
            render_at(&found, "Money", 0, 1, Cell::Float(0.125)).as_deref(),
            Some("12.5%")
        );
        assert_eq!(
            render_at(&found, "Money", 1, 0, date(2026, 1, 9)).as_deref(),
            Some("09/01/2026")
        );
        assert_eq!(
            render_at(
                &found,
                "Money",
                1,
                1,
                Cell::Time(chrono::NaiveTime::from_hms_opt(9, 30, 0).unwrap())
            )
            .as_deref(),
            Some("09:30")
        );
        // A plain cell has no format at all.
        assert_eq!(render_at(&found, "Money", 2, 0, Cell::Float(3.0)), None);
    }

    #[test]
    fn ods_built_in_defaults_and_repeats_are_honoured() {
        // A column style applies to cells that do not name one, and a repeated
        // row carries its formats to every row in the run.
        let content = r#"<office:document-content
            xmlns:office="o" xmlns:style="s" xmlns:table="t" xmlns:number="n" xmlns:text="x">
          <office:automatic-styles>
            <number:number-style style:name="N1"><number:number number:decimal-places="2"/></number:number-style>
            <style:style style:name="co1" style:family="table-column" style:data-style-name="N1"/>
          </office:automatic-styles>
          <office:body><office:spreadsheet>
            <table:table table:name="T">
              <table:table-column table:style-name="co1"/>
              <table:table-row table:number-rows-repeated="3">
                <table:table-cell office:value-type="float" office:value="2.5"><text:p/></table:table-cell>
              </table:table-row>
            </table:table>
          </office:spreadsheet></office:body>
        </office:document-content>"#;
        let path = package("repeat.ods", &[("content.xml", content)]);
        let found = read(&path);
        assert_eq!(
            render_at(&found, "T", 0, 0, Cell::Float(2.5)).as_deref(),
            Some("2.50")
        );
        assert_eq!(
            render_at(&found, "T", 1, 0, Cell::Float(2.5)).as_deref(),
            Some("2.50")
        );
        assert_eq!(
            render_at(&found, "T", 2, 0, Cell::Float(2.5)).as_deref(),
            Some("2.50")
        );
        assert_eq!(render_at(&found, "T", 3, 0, Cell::Float(2.5)), None);
    }

    #[test]
    fn packages_without_formats_yield_nothing() {
        let empty_styles = r#"<styleSheet><cellXfs><xf numFmtId="0"/></cellXfs></styleSheet>"#;
        let workbook =
            r#"<workbook xmlns:r="r"><sheets><sheet name="S" r:id="rId1"/></sheets></workbook>"#;
        let rels = r#"<Relationships><Relationship Id="rId1" Target="worksheets/sheet1.xml"/></Relationships>"#;
        let sheet = r#"<worksheet><sheetData><row r="1"><c r="A1"><v>1</v></c></row></sheetData></worksheet>"#;
        let path = package(
            "plain.xlsx",
            &[
                ("xl/workbook.xml", workbook),
                ("xl/_rels/workbook.xml.rels", rels),
                ("xl/styles.xml", empty_styles),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );
        assert!(read(&path).sheets.is_empty());

        // Not a package at all: no panic, no formats.
        let junk = temp_dir().join("not-a-package.xlsx");
        std::fs::write(&junk, b"plain text").unwrap();
        assert!(read(&junk).sheets.is_empty());
        assert!(read(std::path::Path::new("missing.ods")).sheets.is_empty());
    }

    #[test]
    fn literals_are_quoted_so_they_are_not_read_as_format_tokens() {
        assert_eq!(quote(" USD"), "\" USD\"");
        assert_eq!(quote("5\" screen"), "\"5\"\" screen\"");
        assert_eq!(quote("/"), "\"/\"");
    }
}
