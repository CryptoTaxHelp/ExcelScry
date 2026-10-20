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

use crate::model::{CellLook, SheetFormats, SheetLooks};
use crate::number_format::NumberFormat;

/// Bound on the formatted cells remembered for one workbook, mirroring the
/// reader's own cell budget so a hostile package cannot exhaust memory here.
const MAX_FORMATTED: usize = 2_000_000;

/// Excel's own column and row limits, used to clamp repeated ODF cells.
const MAX_ROWS: u32 = 1_048_576;
const MAX_COLS: u32 = 16_384;

/// What a package says about cell formatting: how each cell is displayed, and
/// how it looks.
#[derive(Debug, Default)]
pub struct WorkbookFormats {
    /// The calendar serial dates are counted on.
    pub date_system: DateSystem,
    /// Formats per sheet *name*: that is how sheets are identified when the
    /// rows are read, and it survives a package whose sheet order differs.
    pub sheets: HashMap<String, SheetFormats>,
    /// Bold, italic and colours per sheet name, the same way.
    pub looks: HashMap<String, SheetLooks>,
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
    let theme = theme_colour(zip, &targets);
    let table = parse_styles(&styles, out.date_system, &theme);
    if table.formats.iter().all(Option::is_none) && table.looks.iter().all(Option::is_none) {
        // Every style is `General` and plain: no worksheet needs scanning.
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
        let mut looks = SheetLooks::default();
        read_sheet(
            BufReader::new(entry),
            &table.formats,
            &table.looks,
            &mut found,
            &mut looks,
            &mut budget,
        );
        if !found.is_empty() {
            out.sheets.insert(name.clone(), found);
        }
        if !looks.is_empty() {
            out.looks.insert(name, looks);
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

/// Formats and appearances of each `cellXfs` entry, by style index.
struct StyleTable {
    formats: Vec<Option<Arc<NumberFormat>>>,
    looks: Vec<Option<Arc<CellLook>>>,
}

/// A colour before the theme palette and indexed palette are known.
#[derive(Debug, Clone)]
enum ColourRef {
    Rgb(String),
    Theme(u32, f64),
    Indexed(u32),
}

#[derive(Debug, Default, Clone)]
struct RawFont {
    bold: bool,
    italic: bool,
    colour: Option<ColourRef>,
}

#[derive(Debug, Default)]
struct RawFill {
    pattern: Option<String>,
    foreground: Option<ColourRef>,
    background: Option<ColourRef>,
}

/// Parse `xl/styles.xml`: the format and look of every style index.
fn parse_styles(xml: &[u8], system: DateSystem, theme: &[Option<String>]) -> StyleTable {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut custom: HashMap<u32, String> = HashMap::new();
    let mut xfs: Vec<(u32, u32, u32)> = Vec::new();
    let mut fonts: Vec<RawFont> = Vec::new();
    let mut fills: Vec<RawFill> = Vec::new();
    let mut indexed: Vec<Option<String>> = Vec::new();
    let mut font: Option<RawFont> = None;
    let mut fill: Option<RawFill> = None;
    let mut in_fonts = false;
    let mut in_fills = false;
    let mut in_cell_xfs = false;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                match element.local_name().as_ref() {
                    b"cellXfs" => in_cell_xfs = true,
                    b"fonts" => in_fonts = true,
                    b"fills" => in_fills = true,
                    b"font" if in_fonts => font = Some(RawFont::default()),
                    b"fill" if in_fills => fill = Some(RawFill::default()),
                    b"patternFill" => {
                        if let Some(current) = fill.as_mut() {
                            current.pattern = attr(&element, b"patternType");
                        }
                    }
                    b"fgColor" => {
                        if let Some(current) = fill.as_mut() {
                            current.foreground = colour_ref(&element);
                        }
                    }
                    b"bgColor" => {
                        if let Some(current) = fill.as_mut() {
                            current.background = colour_ref(&element);
                        }
                    }
                    b"b" => {
                        if let Some(current) = font.as_mut() {
                            current.bold = enabled(&element);
                        }
                    }
                    b"i" => {
                        if let Some(current) = font.as_mut() {
                            current.italic = enabled(&element);
                        }
                    }
                    b"color" => {
                        if let Some(current) = font.as_mut() {
                            current.colour = colour_ref(&element);
                        }
                    }
                    b"rgbColor" => {
                        let value = attr(&element, b"rgb").and_then(|v| normalise_rgb(&v));
                        indexed.push(value);
                    }
                    b"numFmt" => {
                        let id = attr(&element, b"numFmtId").and_then(|v| v.parse::<u32>().ok());
                        if let (Some(id), Some(code)) = (id, attr(&element, b"formatCode")) {
                            custom.insert(id, code);
                        }
                    }
                    b"xf" if in_cell_xfs => xfs.push((
                        format_id(&element),
                        id_attr(&element, b"fontId"),
                        id_attr(&element, b"fillId"),
                    )),
                    _ => {}
                }
            }
            Ok(Event::End(element)) => match element.local_name().as_ref() {
                b"cellXfs" => in_cell_xfs = false,
                b"fonts" => in_fonts = false,
                b"fills" => in_fills = false,
                b"font" if in_fonts => {
                    if let Some(current) = font.take() {
                        fonts.push(current);
                    }
                }
                b"fill" if in_fills => {
                    if let Some(current) = fill.take() {
                        fills.push(current);
                    }
                }
                _ => {}
            },
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }

    // The workbook's normal font is font 0, and it almost always names a
    // colour (Excel writes `theme="1"`). Treating that as an explicit colour
    // would give every plain cell a look, so the default is subtracted: a cell
    // is only coloured when it differs from what the sheet already draws.
    let default_colour = fonts
        .first()
        .and_then(|font| font.colour.as_ref())
        .and_then(|colour| resolve_colour(colour, theme, &indexed));

    let (formats, looks) = xfs
        .into_iter()
        .map(|(num_fmt, font_id, fill_id)| {
            let code = custom
                .get(&num_fmt)
                .cloned()
                .or_else(|| ssfmt::format_code_from_id(num_fmt).map(str::to_string));
            let format = code.and_then(|code| NumberFormat::parse(&code, system));

            let font = fonts.get(font_id as usize);
            let fill = fills.get(fill_id as usize);
            let background = fill
                .filter(|f| f.pattern.as_deref() != Some("none"))
                .and_then(|f| f.foreground.as_ref().or(f.background.as_ref()))
                .and_then(|colour| resolve_colour(colour, theme, &indexed));
            let color = font
                .and_then(|f| f.colour.as_ref())
                .and_then(|colour| resolve_colour(colour, theme, &indexed))
                .filter(|colour| Some(colour) != default_colour.as_ref());
            let look = CellLook {
                bold: font.is_some_and(|f| f.bold),
                italic: font.is_some_and(|f| f.italic),
                color,
                background,
            };
            let look = (!look.is_plain()).then(|| Arc::new(look));
            (format, look)
        })
        .unzip();

    StyleTable { formats, looks }
}

/// Whether a toggle such as `<b/>` is on. Excel writes `<b/>` for on and
/// `val="0"` for off.
fn enabled(element: &BytesStart) -> bool {
    !matches!(attr(element, b"val").as_deref(), Some("0") | Some("false"))
}

fn id_attr(element: &BytesStart, name: &[u8]) -> u32 {
    attr(element, name)
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0)
}

/// A colour reference from an OOXML colour element, if it names one.
fn colour_ref(element: &BytesStart) -> Option<ColourRef> {
    if matches!(attr(element, b"auto").as_deref(), Some("1") | Some("true")) {
        return None;
    }
    if let Some(rgb) = attr(element, b"rgb").and_then(|v| normalise_rgb(&v)) {
        return Some(ColourRef::Rgb(rgb));
    }
    if let Some(theme) = attr(element, b"theme").and_then(|v| v.trim().parse::<u32>().ok()) {
        let tint = attr(element, b"tint")
            .and_then(|v| v.trim().parse::<f64>().ok())
            .unwrap_or(0.0);
        return Some(ColourRef::Theme(theme, tint));
    }
    attr(element, b"indexed")
        .and_then(|v| v.trim().parse::<u32>().ok())
        .map(ColourRef::Indexed)
}

/// `FFRRGGBB` or `RRGGBB` as `#RRGGBB`.
fn normalise_rgb(value: &str) -> Option<String> {
    let hex = value.trim().trim_start_matches('#');
    let hex = match hex.len() {
        8 => &hex[2..],
        6 => hex,
        _ => return None,
    };
    if !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(format!("#{}", hex.to_ascii_uppercase()))
}

/// An ODF colour attribute as `#RRGGBB`; `transparent` and names have no RGB
/// value this app can carry.
fn odf_colour(value: &str) -> Option<String> {
    normalise_rgb(value)
}

/// Resolve a colour through the theme and indexed palettes.
fn resolve_colour(
    colour: &ColourRef,
    theme: &[Option<String>],
    indexed: &[Option<String>],
) -> Option<String> {
    match colour {
        ColourRef::Rgb(rgb) => Some(rgb.clone()),
        ColourRef::Theme(index, tint) => theme
            .get(*index as usize)
            .and_then(|value| value.as_ref())
            .map(|base| tinted(base, *tint)),
        ColourRef::Indexed(index) => indexed
            .get(*index as usize)
            .and_then(|value| value.clone())
            .or_else(|| default_indexed(*index)),
    }
}

/// Excel's standard 64-entry palette, for files that use indexed colours
/// without overriding the table.
pub(crate) fn default_indexed(index: u32) -> Option<String> {
    const PALETTE: [&str; 64] = [
        "000000", "FFFFFF", "FF0000", "00FF00", "0000FF", "FFFF00", "FF00FF", "00FFFF", "000000",
        "FFFFFF", "FF0000", "00FF00", "0000FF", "FFFF00", "FF00FF", "00FFFF", "800000", "008000",
        "000080", "808000", "800080", "008080", "C0C0C0", "808080", "9999FF", "993366", "FFFFCC",
        "CCFFFF", "660066", "FF8080", "0066CC", "CCCCFF", "000080", "FF00FF", "FFFF00", "00FFFF",
        "800080", "800000", "008080", "0000FF", "00CCFF", "CCFFFF", "CCFFCC", "FFFF99", "99CCFF",
        "FF99CC", "CC99FF", "FFCC99", "3366FF", "33CCCC", "99CC00", "FFCC00", "FF9900", "FF6600",
        "666699", "969696", "003366", "339966", "003300", "333300", "993300", "993366", "333399",
        "333333",
    ];
    match index {
        64 => Some("#000000".to_string()),
        65 => Some("#FFFFFF".to_string()),
        i => PALETTE.get(i as usize).map(|hex| format!("#{hex}")),
    }
}

/// Apply Excel's tint to a base colour, in HSL space.
fn tinted(base: &str, tint: f64) -> String {
    let Some(hex) = base.strip_prefix('#') else {
        return base.to_string();
    };
    if tint == 0.0 || hex.len() != 6 {
        return base.to_string();
    }
    let channel = |start: usize| u8::from_str_radix(&hex[start..start + 2], 16).unwrap_or(0);
    let (r, g, b) = (channel(0), channel(2), channel(4));
    let (h, l, s) = rgb_to_hls(r, g, b);
    let l = if tint < 0.0 {
        l * (1.0 + tint)
    } else {
        l * (1.0 - tint) + tint
    };
    let (r, g, b) = hls_to_rgb(h, l.clamp(0.0, 1.0), s);
    format!("#{r:02X}{g:02X}{b:02X}")
}

fn rgb_to_hls(r: u8, g: u8, b: u8) -> (f64, f64, f64) {
    let (r, g, b) = (r as f64 / 255.0, g as f64 / 255.0, b as f64 / 255.0);
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let l = (max + min) / 2.0;
    if (max - min).abs() < f64::EPSILON {
        return (0.0, l, 0.0);
    }
    let d = max - min;
    let s = if l > 0.5 {
        d / (2.0 - max - min)
    } else {
        d / (max + min)
    };
    let h = if max == r {
        ((g - b) / d + if g < b { 6.0 } else { 0.0 }) / 6.0
    } else if max == g {
        ((b - r) / d + 2.0) / 6.0
    } else {
        ((r - g) / d + 4.0) / 6.0
    };
    (h, l, s)
}

fn hls_to_rgb(h: f64, l: f64, s: f64) -> (u8, u8, u8) {
    if s == 0.0 {
        let v = (l * 255.0).round() as u8;
        return (v, v, v);
    }
    let m2 = if l < 0.5 {
        l * (1.0 + s)
    } else {
        l + s - l * s
    };
    let m1 = 2.0 * l - m2;
    let hue = |mut t: f64| {
        if t < 0.0 {
            t += 1.0;
        }
        if t > 1.0 {
            t -= 1.0;
        }
        let v = if t * 6.0 < 1.0 {
            m1 + (m2 - m1) * t * 6.0
        } else if t * 2.0 < 1.0 {
            m2
        } else if t * 3.0 < 2.0 {
            m1 + (m2 - m1) * (2.0 / 3.0 - t) * 6.0
        } else {
            m1
        };
        (v * 255.0).round().clamp(0.0, 255.0) as u8
    };
    (hue(h + 1.0 / 3.0), hue(h), hue(h - 1.0 / 3.0))
}

/// The theme's twelve colours, in the order Excel's `theme` attribute uses.
fn theme_colour<R: Read + Seek>(
    zip: &mut ZipArchive<R>,
    targets: &HashMap<String, String>,
) -> Vec<Option<String>> {
    let Some(path) = targets
        .values()
        .find(|target| target.to_ascii_lowercase().contains("theme"))
        .and_then(|target| worksheet_path(target))
    else {
        return Vec::new();
    };
    let Some(xml) = part(zip, &path) else {
        return Vec::new();
    };
    parse_theme(&xml)
}

fn parse_theme(xml: &[u8]) -> Vec<Option<String>> {
    let mut reader = Reader::from_reader(xml);
    reader.config_mut().trim_text(true);
    let mut buffer = Vec::new();
    let mut out = vec![None; 12];
    let mut in_scheme = false;
    let mut slot: Option<(Vec<u8>, usize)> = None;

    loop {
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) | Ok(Event::Empty(element)) => {
                let local = element.local_name().as_ref().to_vec();
                if local == b"clrScheme" {
                    in_scheme = true;
                } else if in_scheme {
                    if let Some(index) = theme_slot(&local) {
                        slot = Some((local.clone(), index));
                    } else if let Some((_, index)) = slot.as_ref() {
                        if local == b"srgbClr" {
                            if let Some(value) =
                                attr(&element, b"val").and_then(|v| normalise_rgb(&v))
                            {
                                out[*index] = Some(value);
                            }
                        } else if local == b"sysClr" {
                            if let Some(value) = attr(&element, b"lastClr")
                                .or_else(|| attr(&element, b"val"))
                                .and_then(|v| normalise_rgb(&v))
                            {
                                out[*index] = Some(value);
                            }
                        }
                    }
                }
            }
            Ok(Event::End(element)) => {
                let local = element.local_name().as_ref().to_vec();
                if local == b"clrScheme" {
                    break;
                }
                if slot.as_ref().is_some_and(|(name, _)| *name == local) {
                    slot = None;
                }
            }
            Ok(Event::Eof) | Err(_) => break,
            _ => {}
        }
        buffer.clear();
    }
    out
}

fn theme_slot(local: &[u8]) -> Option<usize> {
    Some(match local {
        b"lt1" => 0,
        b"dk1" => 1,
        b"lt2" => 2,
        b"dk2" => 3,
        b"accent1" => 4,
        b"accent2" => 5,
        b"accent3" => 6,
        b"accent4" => 7,
        b"accent5" => 8,
        b"accent6" => 9,
        b"hlink" => 10,
        b"folHlink" => 11,
        _ => return None,
    })
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
    looks: &[Option<Arc<CellLook>>],
    out: &mut SheetFormats,
    out_looks: &mut SheetLooks,
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
                        let format = style
                            .and_then(|index| formats.get(index))
                            .and_then(Option::as_ref);
                        let look = style
                            .and_then(|index| looks.get(index))
                            .and_then(Option::as_ref);
                        if format.is_none() && look.is_none() {
                            continue;
                        }
                        if *budget == 0 {
                            return;
                        }
                        *budget -= 1;
                        if let Some(format) = format {
                            out.insert(position.0 as usize, position.1 as usize, format.clone());
                        }
                        if let Some(look) = look {
                            out_looks.insert(
                                position.0 as usize,
                                position.1 as usize,
                                look.clone(),
                            );
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
    /// A time style that does not wrap at 24 hours (`[h]:mm:ss`).
    elapsed: bool,
    /// Whether the section contains a digit pattern. A percentage style with
    /// only literal text (`"-"` for zero) is not scaled.
    has_number: bool,
    /// The colour the style gives its number text, as `[Red]` does.
    colour: Option<String>,
    /// `style:map` conditions and the data styles they select.
    maps: Vec<(String, String)>,
}

/// A data style as `content.xml` defines it.
struct NumberStyle {
    code: String,
    /// The data style a negative-value `style:map` selects, if there is one.
    negative: Option<String>,
    /// The data style a zero-value `style:map` selects, if there is one.
    zero: Option<String>,
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
    looks: SheetLooks,
    row: u32,
    col: u32,
    row_style: Option<String>,
    columns: Vec<Option<String>>,
    /// Formats found in the current row, so a repeated row can be copied.
    current: Vec<(u32, Arc<NumberFormat>)>,
    /// Appearances found in the current row, copied the same way.
    current_looks: Vec<(u32, Arc<CellLook>)>,
    row_repeat: u32,
}

impl Table {
    fn new(name: String) -> Self {
        Self {
            name,
            formats: SheetFormats::default(),
            looks: SheetLooks::default(),
            row: 0,
            col: 0,
            row_style: None,
            columns: Vec::new(),
            current: Vec::new(),
            current_looks: Vec::new(),
            row_repeat: 1,
        }
    }
}

/// The appearance a `style:style` defines, before its parent chain is walked.
#[derive(Debug, Clone, Default)]
struct RawLook {
    parent: Option<String>,
    bold: Option<bool>,
    italic: Option<bool>,
    color: Option<String>,
    background: Option<String>,
}

/// The key a `style:default-style` is stored under; it is not a cell style
/// name, so it cannot collide with one.
const DEFAULT_LOOK: &str = "";

/// A `style:style` being read.
struct LookBuild {
    name: String,
    depth: usize,
    look: RawLook,
}

/// How many parents to follow before deciding a package's style chain is a
/// loop. Excel and Calc nest a handful deep at most.
const MAX_LOOK_DEPTH: u8 = 16;

/// Everything `content.xml` contributes, accumulated in one streaming pass.
struct Scan {
    /// Cell/row/column style name to data style name.
    cell: HashMap<String, String>,
    /// Data style name to the format code it defines, and the style it maps
    /// negative values to.
    number: HashMap<String, NumberStyle>,
    /// The ODF default for table cells, if the file sets one.
    default_cell: Option<String>,
    system: DateSystem,
    /// Parsed formats, keyed by their code: one parse per distinct style.
    parsed: HashMap<String, Option<Arc<NumberFormat>>>,
    table: Option<Table>,
    sheets: HashMap<String, SheetFormats>,
    sheet_looks: HashMap<String, SheetLooks>,
    budget: usize,
    /// `table-cell` styles by name, as written, and the default style.
    raw_looks: HashMap<String, RawLook>,
    /// Resolved appearances, cached by style name.
    resolved_looks: HashMap<String, Option<Arc<CellLook>>>,
    /// The cell style currently being defined.
    look_build: Option<LookBuild>,
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
        sheet_looks: HashMap::new(),
        budget: MAX_FORMATTED,
        raw_looks: HashMap::new(),
        resolved_looks: HashMap::new(),
        look_build: None,
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
                // A self-closing cell style has no End event of its own.
                scan.finish_self_closing_look(&local, stack.len());
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
                    scan.end(&closing, stack.len());
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
        looks: scan.sheet_looks,
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
                        // Calc marks a duration this way: the hours of
                        // `[HH]:MM:SS` keep counting past a day.
                        elapsed: attr(element, b"truncate-on-overflow").as_deref() == Some("false"),
                        has_number: false,
                        colour: None,
                        maps: Vec::new(),
                    });
                }
            }
            if local == b"style" {
                if let (Some(name), Some(data)) =
                    (attr(element, b"name"), attr(element, b"data-style-name"))
                {
                    self.cell.insert(name, data);
                }
                // The same `<style:style>` can carry a number format and an
                // appearance; both are kept.
                if attr(element, b"family").as_deref() == Some("table-cell") {
                    if let Some(name) = attr(element, b"name") {
                        self.look_build = Some(LookBuild {
                            name,
                            depth: stack.len(),
                            look: RawLook {
                                parent: attr(element, b"parent-style-name"),
                                ..RawLook::default()
                            },
                        });
                    }
                }
            }
            if local == b"default-style"
                && attr(element, b"family").as_deref() == Some("table-cell")
            {
                self.default_cell = attr(element, b"data-style-name");
                self.look_build = Some(LookBuild {
                    name: DEFAULT_LOOK.to_string(),
                    depth: stack.len(),
                    look: RawLook {
                        parent: attr(element, b"parent-style-name"),
                        ..RawLook::default()
                    },
                });
            }
        }

        // The properties of the cell style being defined are its direct
        // children; the same element names appear elsewhere, so the depth is
        // what tells them apart.
        if let Some(current) = self.look_build.as_mut() {
            if stack.len() == current.depth + 1 {
                match local {
                    b"table-cell-properties" => {
                        current.look.background =
                            attr(element, b"background-color").and_then(|value| odf_colour(&value));
                    }
                    b"text-properties" => {
                        current.look.bold = attr(element, b"font-weight").map(|v| {
                            matches!(v.trim().to_ascii_lowercase().as_str(), "bold" | "bolder")
                                || v.trim().parse::<u32>().is_ok_and(|weight| weight >= 600)
                        });
                        current.look.italic = attr(element, b"font-style").map(|v| {
                            matches!(v.trim().to_ascii_lowercase().as_str(), "italic" | "oblique")
                        });
                        current.look.color =
                            attr(element, b"color").and_then(|value| odf_colour(&value));
                    }
                    _ => {}
                }
            }
        }

        if let Some(current) = build.as_mut() {
            if stack.len() == current.depth + 1 {
                // `<style:map style:condition="value()<0" .../>` is how ODF
                // carries the extra sections of a format code.
                // `local` is the local name, so `style:map` arrives as `map`.
                if local == b"map" {
                    let condition = attr(element, b"condition").unwrap_or_default();
                    if let Some(applied) = attr(element, b"apply-style-name") {
                        current.maps.push((condition, applied));
                    }
                    return;
                }
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

    fn end(&mut self, local: &[u8], depth: usize) {
        if matches!(local, b"style" | b"default-style")
            && self.look_build.as_ref().is_some_and(|b| b.depth == depth)
        {
            self.end_look();
        }
        match local {
            b"table-row" => self.end_row(),
            b"table" => {
                if let Some(table) = self.table.take() {
                    if !table.formats.is_empty() {
                        self.sheets.insert(table.name.clone(), table.formats);
                    }
                    if !table.looks.is_empty() {
                        self.sheet_looks.insert(table.name, table.looks);
                    }
                }
            }
            _ => {}
        }
    }

    /// Complete a `<style:style/>` that never receives an End event.
    fn finish_self_closing_look(&mut self, local: &[u8], depth: usize) {
        if matches!(local, b"style" | b"default-style")
            && self.look_build.as_ref().is_some_and(|b| b.depth == depth)
        {
            self.end_look();
        }
    }

    /// Store the cell style that just closed, if it was one.
    fn end_look(&mut self) {
        if let Some(done) = self.look_build.take() {
            self.raw_looks.insert(done.name, done.look);
        }
    }

    /// Resolve a style name to its appearance, walking parents once.
    fn look_for(&mut self, style: &str) -> Option<Arc<CellLook>> {
        if let Some(cached) = self.resolved_looks.get(style) {
            return cached.clone();
        }
        let resolved = self.resolve_look(style, 0);
        self.resolved_looks
            .insert(style.to_string(), resolved.clone());
        resolved
    }

    fn resolve_look(&mut self, style: &str, depth: u8) -> Option<Arc<CellLook>> {
        self.look_for_depth(style, depth)
    }

    /// Recursion through a parent chain, without caching intermediate names:
    /// they are few, and a cycle is stopped by `depth`.
    fn look_for_depth(&mut self, style: &str, depth: u8) -> Option<Arc<CellLook>> {
        if depth > MAX_LOOK_DEPTH {
            return None;
        }
        let raw = self.raw_looks.get(style)?.clone();
        let mut look = raw
            .parent
            .as_deref()
            .and_then(|parent| self.look_for_depth(parent, depth + 1))
            .map(|parent| (*parent).clone())
            .unwrap_or_default();
        if let Some(bold) = raw.bold {
            look.bold = bold;
        }
        if let Some(italic) = raw.italic {
            look.italic = italic;
        }
        if raw.color.is_some() {
            look.color = raw.color;
        }
        if raw.background.is_some() {
            look.background = raw.background;
        }
        (!look.is_plain()).then(|| Arc::new(look))
    }

    fn finish(&mut self, mut build: Build) {
        build.finish();
        let negative = build
            .maps
            .iter()
            .find(|(condition, _)| is_negative(condition))
            .map(|(_, applied)| applied.clone());
        let zero = build
            .maps
            .iter()
            .find(|(condition, _)| is_zero(condition))
            .map(|(_, applied)| applied.clone());
        if let Some(format) = NumberFormat::parse(&build.code, self.system) {
            self.parsed.insert(build.code.clone(), Some(format));
        }
        // A style with no content is what a `style:map` points at when a
        // section hides a value (a hidden zero), so it is kept even though it
        // parses to no format of its own. Dropping it would quietly unhide
        // the value it was mapped from.
        self.number.insert(
            build.name,
            NumberStyle {
                code: build.code,
                negative,
                zero,
            },
        );
    }

    /// Resolve a cell style name to a parsed format.
    fn format_for(&mut self, style: &str) -> Option<Arc<NumberFormat>> {
        let data = self.cell.get(style)?.clone();
        let base = self.number.get(&data)?;
        let mut code = base.code.clone();
        // A mapped negative section is a second format code section, which is
        // how the rest of the app already reads Excel's own `pos;neg` codes.
        if let Some(negative) = &base.negative {
            if let Some(mapped) = self.number.get(negative) {
                code = format!("{code};{}", mapped.code);
            }
        }
        if let Some(zero) = &base.zero {
            // A zero section is Excel's third; if the package mapped it
            // without a negative one, the positive code has to stand in so
            // the zero does not slide into the negative slot.
            if base.negative.is_none() {
                code = format!("{code};{}", base.code);
            }
            if let Some(mapped) = self.number.get(zero) {
                code = format!("{code};{}", mapped.code);
            }
        }
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
                table.current_looks.clear();
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
            let style = if has_content {
                // An explicit style is kept even on a cell with no value: a
                // filled blank cell is part of the table's appearance. A
                // column, row or default style is only followed when the cell
                // has a value, so a million empty cells are not materialised.
                match attr(element, b"style-name") {
                    Some(style) => Some(style),
                    None if has_value(element) => table
                        .columns
                        .get(start as usize)
                        .cloned()
                        .flatten()
                        .or_else(|| table.row_style.clone())
                        .or_else(|| self.default_cell.clone()),
                    None => None,
                }
            } else {
                None
            };
            (table.row, start, repeat, style)
        };
        if self.budget == 0 {
            return;
        }
        let format = style.as_ref().and_then(|style| self.format_for(style));
        let look = style
            .as_ref()
            .and_then(|style| self.look_for(style))
            .or_else(|| self.look_for(DEFAULT_LOOK));
        if format.is_none() && look.is_none() {
            return;
        }
        let Some(table) = self.table.as_mut() else {
            return;
        };
        let mut written = 0usize;
        for offset in 0..repeat {
            let col = start + offset;
            if col >= MAX_COLS || row >= MAX_ROWS || written >= self.budget {
                break;
            }
            if let Some(format) = &format {
                table
                    .formats
                    .insert(row as usize, col as usize, format.clone());
                table.current.push((col, format.clone()));
                written += 1;
            }
            if let Some(look) = &look {
                if written >= self.budget {
                    break;
                }
                table.looks.insert(row as usize, col as usize, look.clone());
                table.current_looks.push((col, look.clone()));
                written += 1;
            }
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
            for (col, look) in &table.current_looks {
                if written >= self.budget {
                    break;
                }
                table
                    .looks
                    .insert(row as usize, *col as usize, look.clone());
                written += 1;
            }
        }
        table.row = table.row.saturating_add(table.row_repeat).min(MAX_ROWS);
        table.col = 0;
        table.row_style = None;
        table.current.clear();
        table.current_looks.clear();
        self.budget -= written;
    }
}

impl Build {
    fn push_element(&mut self, element: &BytesStart, local: &[u8]) {
        match local {
            b"text-properties" => {
                self.colour = attr(element, b"color").and_then(|value| odf_colour(&value));
            }
            b"number" => {
                self.has_number = true;
                self.code.push_str(&number_pattern(element));
            }
            b"scientific-number" => {
                self.has_number = true;
                self.code.push_str(&scientific_pattern(element));
            }
            b"fraction" => {
                self.has_number = true;
                self.code.push_str(&fraction_pattern(element));
            }
            b"text-content" => self.code.push('@'),
            b"am-pm" => self.code.push_str("AM/PM"),
            // Brackets are what make the format engine (and Excel) read the
            // leading hour field as elapsed rather than as a clock time. Only
            // the first one gets them: `[h]:[mm]` would count total minutes
            // as well, which is not what the file means.
            b"hours" | b"minutes" | b"seconds" if self.elapsed => {
                self.elapsed = false;
                if let Some(token) = date_token(element, local) {
                    self.code.push('[');
                    self.code.push_str(token);
                    self.code.push(']');
                }
            }
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
        if self.kind == Kind::Percentage && self.has_number && !self.code.contains('%') {
            self.code.push('%');
        }
        // ODF carries a section's colour as text properties on its number
        // style; Excel spells the same thing as a leading `[Red]` section
        // token, which is what the rest of the app reads.
        if let Some(name) = self.colour.as_deref().and_then(colour_token) {
            self.code.insert_str(0, &format!("[{name}]"));
        }
        if self.kind == Kind::Text && !self.code.contains('@') {
            if self.code.is_empty() {
                // A text style with nothing in it shows nothing at all, which
                // is how the fourth section of `0;;;` hides text. Spelled as
                // the four empty sections Excel uses for the same thing.
                self.code = ";;;\"\"".to_string();
            } else if self.code.contains('"') {
                // A text style with only literals shows those literals for
                // text values. Excel writes that as a fourth section, whose
                // empty first three also hide the numbers the style was never
                // meant for.
                self.code = format!(";;;{}", self.code);
            } else {
                // Nothing but a colour, or nothing at all: the cell's own text
                // is what the style shows.
                self.code.push('@');
            }
        }
    }
}

/// Whether a `style:map` condition selects negative values.
///
/// Calc writes `value()<0`; the spacing around the operator varies between
/// writers, and `<` is usually escaped as `&lt;` in the file itself.
fn is_negative(condition: &str) -> bool {
    let compact: String = condition.chars().filter(|c| !c.is_whitespace()).collect();
    compact.contains("<0")
}

/// Whether a `style:map` condition selects zero, as `value()=0` does. Calc
/// spaces the operator freely, so whitespace is removed first.
fn is_zero(condition: &str) -> bool {
    let compact: String = condition.chars().filter(|c| !c.is_whitespace()).collect();
    compact
        .strip_prefix("value()=")
        .is_some_and(|value| value.parse::<f64>().is_ok_and(|value| value == 0.0))
}

/// The Excel format token for a colour ODF wrote as RGB: a name where one
/// exists, otherwise the palette's `ColorN`.
fn colour_token(hex: &str) -> Option<String> {
    if let Some(name) = match hex {
        "#000000" => Some("Black"),
        "#0000FF" => Some("Blue"),
        "#00FFFF" => Some("Cyan"),
        "#00FF00" => Some("Green"),
        "#FF00FF" => Some("Magenta"),
        "#FF0000" => Some("Red"),
        "#FFFFFF" => Some("White"),
        "#FFFF00" => Some("Yellow"),
        _ => None,
    } {
        return Some(name.to_string());
    }
    color_index_for_hex(hex).map(|index| format!("Color{index}"))
}

/// The palette index behind Excel's one-based `[ColorN]` token.
///
/// `Color1`..`Color8` are the first eight palette entries; `Color9` onwards
/// skip the eight system-colour duplicates in the xlsx palette.
pub(crate) fn color_palette_index(token: u8) -> Option<u32> {
    match token {
        1..=8 => Some(u32::from(token - 1)),
        9..=56 => Some(u32::from(token) + 7),
        _ => None,
    }
}

fn color_index_for_hex(hex: &str) -> Option<u8> {
    (1..=56u8).find(|token| {
        color_palette_index(*token)
            .and_then(default_indexed)
            .as_deref()
            == Some(hex)
    })
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
    fn ods_style_maps_restore_the_negative_section() {
        // Calc writes a second data style and a `style:map` for the negative
        // section of `#,##0.00;(#,##0.00)`; the two must read back as one
        // two-section code. The same file also carries a duration: a time
        // style that does not overflow is `[h]:mm:ss`, not `hh:mm:ss`.
        let content = r##"<office:document-content
            xmlns:office="o" xmlns:style="s" xmlns:table="t" xmlns:number="n" xmlns:text="x" xmlns:fo="f">
          <office:automatic-styles>
            <number:number-style style:name="N1">
              <style:text-properties fo:color="#0000ff"/>
              <style:map style:condition="value()&lt;0" style:apply-style-name="N1neg"/>
              <style:map style:condition="value()=0" style:apply-style-name="N1zero"/>
              <number:number number:decimal-places="2" number:min-decimal-places="2" number:min-integer-digits="1" number:grouping="true"/>
            </number:number-style>
            <number:number-style style:name="N1neg">
              <style:text-properties fo:color="#ff0000"/>
              <number:text>(</number:text>
              <number:number number:decimal-places="2" number:min-decimal-places="2" number:min-integer-digits="1" number:grouping="true"/>
              <number:text>)</number:text>
            </number:number-style>
            <number:number-style style:name="N1zero">
              <number:text>-</number:text>
            </number:number-style>
            <number:time-style style:name="T1" number:truncate-on-overflow="false">
              <number:hours number:style="long"/>
              <number:text>:</number:text>
              <number:minutes number:style="long"/>
              <number:text>:</number:text>
              <number:seconds number:style="long"/>
            </number:time-style>
            <style:style style:name="ce1" style:family="table-cell" style:data-style-name="N1"/>
            <style:style style:name="ce2" style:family="table-cell" style:data-style-name="T1"/>
          </office:automatic-styles>
          <office:body><office:spreadsheet>
            <table:table table:name="T">
              <table:table-row>
                <table:table-cell table:style-name="ce1" office:value-type="float" office:value="1234.5"><text:p/></table:table-cell>
                <table:table-cell table:style-name="ce2" office:value-type="time" office:time-value="PT36H0M0S"><text:p/></table:table-cell>
              </table:table-row>
            </table:table>
          </office:spreadsheet></office:body>
        </office:document-content>"##;
        let found = read(&package("maps.ods", &[("content.xml", content)]));

        assert_eq!(
            render_at(&found, "T", 0, 0, Cell::Float(1234.5)).as_deref(),
            Some("1,234.50")
        );
        assert_eq!(
            render_at(&found, "T", 0, 0, Cell::Float(-1234.5)).as_deref(),
            Some("(1,234.50)"),
            "the mapped negative section was not used"
        );
        assert_eq!(
            render_at(&found, "T", 0, 0, Cell::Float(0.0)).as_deref(),
            Some("-"),
            "the mapped zero section was not used"
        );
        let code = found.sheets["T"].get(0, 0).unwrap().code();
        assert!(
            code.contains("[Blue]") && code.contains("[Red]"),
            "the section colours were not restored: {code}"
        );
        assert_eq!(
            render_at(
                &found,
                "T",
                0,
                1,
                Cell::Duration(chrono::Duration::hours(36))
            )
            .as_deref(),
            Some("36:00:00"),
            "a non-overflowing time style is a duration"
        );
    }

    #[test]
    fn ods_empty_zero_style_hides_the_zero() {
        // A hide-zero format has an empty third section, which Calc writes as a
        // data style with no parts at all. Following the map must keep that
        // empty section, or the zero would fall back to the positive one.
        let content = r##"<office:document-content
            xmlns:office="o" xmlns:style="s" xmlns:table="t" xmlns:number="n" xmlns:text="x">
          <office:automatic-styles>
            <number:number-style style:name="N1">
              <number:number number:decimal-places="0" number:min-integer-digits="1" number:grouping="true"/>
              <style:map style:condition="value()=0" style:apply-style-name="N1zero"/>
            </number:number-style>
            <number:number-style style:name="N1zero"/>
            <style:style style:name="ce1" style:family="table-cell" style:data-style-name="N1"/>
          </office:automatic-styles>
          <office:body><office:spreadsheet>
            <table:table table:name="T">
              <table:table-row>
                <table:table-cell table:style-name="ce1" office:value-type="float" office:value="0"><text:p/></table:table-cell>
              </table:table-row>
            </table:table>
          </office:spreadsheet></office:body>
        </office:document-content>"##;
        let found = read(&package("hidden-zero.ods", &[("content.xml", content)]));

        assert_eq!(
            render_at(&found, "T", 0, 0, Cell::Float(0.0)).as_deref(),
            Some(""),
            "an empty zero section must leave the zero blank"
        );
        assert_eq!(
            render_at(&found, "T", 0, 0, Cell::Float(12.0)).as_deref(),
            Some("12"),
            "other values still use the positive section"
        );
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

    fn look_at<'a>(
        found: &'a WorkbookFormats,
        sheet: &str,
        row: usize,
        col: usize,
    ) -> Option<&'a CellLook> {
        found.looks.get(sheet)?.get(row, col)
    }

    #[test]
    fn xlsx_fonts_fills_theme_and_indexed_colours_are_read() {
        let styles = r##"<styleSheet xmlns="http://schemas.openxmlformats.org/spreadsheetml/2006/main">
  <fonts count="4">
    <font><sz val="11"/><color theme="1"/></font>
    <font><b/><i/><color rgb="FFFF0000"/></font>
    <font><color indexed="10"/></font>
    <font><color theme="4"/></font>
  </fonts>
  <fills count="3">
    <fill><patternFill patternType="none"/></fill>
    <fill><patternFill patternType="solid"><fgColor rgb="FFCCE5FF"/></patternFill></fill>
    <fill><patternFill patternType="solid"><fgColor indexed="13"/></patternFill></fill>
  </fills>
  <cellXfs count="4">
    <xf numFmtId="0" fontId="0" fillId="0"/>
    <xf numFmtId="0" fontId="1" fillId="1"/>
    <xf numFmtId="0" fontId="2" fillId="0"/>
    <xf numFmtId="0" fontId="3" fillId="2"/>
  </cellXfs>
</styleSheet>"##;
        let theme = r##"<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main">
  <a:themeElements><a:clrScheme name="Office">
    <a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1>
    <a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1>
    <a:dk2><a:srgbClr val="44546A"/></a:dk2>
    <a:lt2><a:srgbClr val="E7E6E6"/></a:lt2>
    <a:accent1><a:srgbClr val="4472C4"/></a:accent1>
    <a:accent2><a:srgbClr val="ED7D31"/></a:accent2>
    <a:accent3><a:srgbClr val="A5A5A5"/></a:accent3>
    <a:accent4><a:srgbClr val="FFC000"/></a:accent4>
    <a:accent5><a:srgbClr val="5B9BD5"/></a:accent5>
    <a:accent6><a:srgbClr val="70AD47"/></a:accent6>
    <a:hlink><a:srgbClr val="0563C1"/></a:hlink>
    <a:folHlink><a:srgbClr val="954F72"/></a:folHlink>
  </a:clrScheme></a:themeElements>
</a:theme>"##;
        let workbook =
            r#"<workbook xmlns:r="r"><sheets><sheet name="S" r:id="rId1"/></sheets></workbook>"#;
        let rels = r#"<Relationships>
  <Relationship Id="rId1" Target="worksheets/sheet1.xml"/>
  <Relationship Id="rId2" Type="theme" Target="theme/theme1.xml"/>
</Relationships>"#;
        let sheet = r#"<worksheet><sheetData><row r="1">
  <c r="A1" s="1"><v>a</v></c>
  <c r="B1" s="2"><v>b</v></c>
  <c r="C1" s="3"><v>c</v></c>
  <c r="D1"><v>d</v></c>
</row></sheetData></worksheet>"#;
        let path = package(
            "looks.xlsx",
            &[
                ("xl/workbook.xml", workbook),
                ("xl/_rels/workbook.xml.rels", rels),
                ("xl/styles.xml", styles),
                ("xl/theme/theme1.xml", theme),
                ("xl/worksheets/sheet1.xml", sheet),
            ],
        );
        let found = read(&path);

        let first = look_at(&found, "S", 0, 0).expect("A1 should carry a look");
        assert!(first.bold && first.italic, "font toggles were lost");
        assert_eq!(first.color.as_deref(), Some("#FF0000"));
        assert_eq!(first.background.as_deref(), Some("#CCE5FF"));

        assert_eq!(
            look_at(&found, "S", 0, 1).and_then(|l| l.color.as_deref()),
            Some("#FF0000"),
            "an indexed colour should use the standard palette"
        );
        let themed = look_at(&found, "S", 0, 2).expect("C1 should carry a look");
        assert_eq!(themed.color.as_deref(), Some("#4472C4"));
        assert_eq!(themed.background.as_deref(), Some("#FFFF00"));
        assert!(
            look_at(&found, "S", 0, 3).is_none(),
            "a plain cell has no look"
        );
    }

    #[test]
    fn ods_cell_styles_inherit_from_their_parent() {
        let content = r##"<office:document-content
            xmlns:office="urn:oasis:names:tc:opendocument:xmlns:office:1.0"
            xmlns:style="urn:oasis:names:tc:opendocument:xmlns:style:1.0"
            xmlns:fo="urn:oasis:names:tc:opendocument:xmlns:xsl-fo-compatible:1.0"
            xmlns:text="urn:oasis:names:tc:opendocument:xmlns:text:1.0"
            xmlns:table="urn:oasis:names:tc:opendocument:xmlns:table:1.0">
          <office:automatic-styles>
            <style:style style:name="ce1" style:family="table-cell">
              <style:table-cell-properties fo:background-color="#FFFF00"/>
              <style:text-properties fo:font-weight="bold" fo:color="#FF0000"/>
            </style:style>
            <style:style style:name="ce2" style:family="table-cell" style:parent-style-name="ce1">
              <style:text-properties fo:font-style="italic"/>
            </style:style>
          </office:automatic-styles>
          <office:body><office:spreadsheet>
            <table:table table:name="T">
              <table:table-row>
                <table:table-cell table:style-name="ce1" office:value-type="string" office:string-value="a"><text:p>a</text:p></table:table-cell>
                <table:table-cell table:style-name="ce2" office:value-type="string" office:string-value="b"><text:p>b</text:p></table:table-cell>
              </table:table-row>
            </table:table>
          </office:spreadsheet></office:body>
        </office:document-content>"##;
        let found = read(&package("looks.ods", &[("content.xml", content)]));

        let first = look_at(&found, "T", 0, 0).expect("ce1 should be read");
        assert!(first.bold);
        assert_eq!(first.color.as_deref(), Some("#FF0000"));
        assert_eq!(first.background.as_deref(), Some("#FFFF00"));

        let second = look_at(&found, "T", 0, 1).expect("ce2 should be read");
        assert!(second.bold, "the parent's weight should be inherited");
        assert!(second.italic, "its own slant should be applied");
        assert_eq!(second.color.as_deref(), Some("#FF0000"));
        assert_eq!(second.background.as_deref(), Some("#FFFF00"));
    }

    #[test]
    fn colour_tokens_use_excels_one_based_palette() {
        assert_eq!(color_palette_index(1), Some(0), "Color1 is the first entry");
        assert_eq!(color_palette_index(8), Some(7));
        assert_eq!(
            color_palette_index(9),
            Some(16),
            "Color9 skips the duplicates"
        );
        assert_eq!(color_palette_index(10), Some(17));
        assert_eq!(
            default_indexed(17).as_deref(),
            Some("#008000"),
            "Color10 is dark green"
        );
        assert_eq!(colour_token("#008000").as_deref(), Some("Color10"));
        assert_eq!(colour_token("#FF0000").as_deref(), Some("Red"));
        assert!(color_palette_index(0).is_none());
        assert!(color_palette_index(57).is_none());
    }

    #[test]
    fn tints_lighten_and_darken_without_touching_hue() {
        assert_eq!(tinted("#FFFFFF", 0.5), "#FFFFFF");
        assert_eq!(tinted("#FF0000", -0.5), "#800000");
        assert_eq!(tinted("#000000", 0.5), "#808080");
        assert_eq!(tinted("#4472C4", 0.0), "#4472C4");
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
