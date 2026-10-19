//! Reading delimited text tables (`.csv`, `.tsv`) into the grid's own model.
//!
//! A delimited file has no workbook, no cell types and no styles: it is a grid
//! of strings, and that is what this produces. Every value stays
//! [`Cell::Text`], including ones that look numeric, because a viewer has to
//! show `000123` and `1E5` the way the file spells them. Inferring types would
//! rewrite the data before it reaches the grid — and an export would then hold
//! values the file did not.
//!
//! The parser is the shape RFC 4180 describes, with the allowances real files
//! need: a quoted field may contain the delimiter, doubled quotes and line
//! breaks; `CRLF`, `LF` and `CR` all end a record; a UTF-8 BOM is skipped;
//! UTF-16 with a BOM (what Excel's "Unicode Text" export writes) is decoded;
//! and rows may be ragged.

use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader, Cursor, Read};
use std::path::Path;

use crate::model::{Cell, LoadedWorkbook, SheetData, SheetFormats};
use crate::trust::{Trust, TrustReport};
use crate::workbook::{format_label, MAX_CELLS};

/// The reader's own sheet limits, so a text file cannot describe more than a
/// workbook could.
const MAX_ROWS: usize = 1_048_576;
const MAX_COLS: usize = 16_384;

/// How much of the first record is inspected when choosing a delimiter.
const SNIFF_BYTES: usize = 8 * 1024;

/// Which delimiter a file's extension asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Comma,
    Tab,
}

/// The delimited formats this module reads, or `None` for anything else.
pub fn kind_of(path: &Path) -> Option<Kind> {
    match path.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "csv" => Some(Kind::Comma),
        "tsv" | "tab" => Some(Kind::Tab),
        _ => None,
    }
}

/// Load a delimited text file as a one-sheet workbook.
pub fn load(path: &Path, meta: &Metadata, kind: Kind) -> Result<LoadedWorkbook, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open \"{}\": {e}", path.display()))?;
    let mut reader = BufReader::new(file);
    let prefix = reader
        .fill_buf()
        .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?
        .to_vec();

    let delimiter = match kind {
        Kind::Tab => b'\t',
        // `.csv` says comma, but a file written in a locale that uses `;` as
        // the decimal separator is common enough that the first record decides.
        Kind::Comma => sniff(&prefix[..prefix.len().min(SNIFF_BYTES)]),
    };

    let table = match bom(&prefix) {
        Some(Bom::Utf8) => {
            reader.consume(3);
            parse(&mut reader, delimiter, MAX_CELLS)
        }
        Some(Bom::Utf16Le) | Some(Bom::Utf16Be) => {
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?;
            let text = decode_utf16(&bytes[2..], matches!(bom(&prefix), Some(Bom::Utf16Le)));
            parse(Cursor::new(text.into_bytes()), delimiter, MAX_CELLS)
        }
        None => parse(&mut reader, delimiter, MAX_CELLS),
    }
    .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?;

    let used_cells = table
        .rows
        .iter()
        .map(|row| row.iter().filter(|cell| !cell.is_empty()).count())
        .sum();
    let cols = table.rows.iter().map(Vec::len).max().unwrap_or(0);

    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.display().to_string());

    Ok(LoadedWorkbook {
        path: path.display().to_string(),
        file_name,
        format: format_label(path),
        file_size: meta.len(),
        sheets: vec![SheetData {
            name: sheet_name(path),
            rows: table.rows,
            cols,
            formats: SheetFormats::default(),
        }],
        truncated: table.truncated,
        total_cells: MAX_CELLS - table.budget,
        used_cells,
        // A text file is not a package: there is nothing in it that could carry
        // macros, links, connections, objects or controls.
        trust: TrustReport::new(Trust::plain_text()),
    })
}

/// The rows a parse produced, and what had to be dropped to produce them.
struct Table {
    rows: Vec<Vec<Cell>>,
    row: Vec<Cell>,
    field: Vec<u8>,
    budget: usize,
    truncated: bool,
    /// Whether the record being built has seen a delimiter or any content, so
    /// a trailing newline is not mistaken for another record.
    open: bool,
}

/// Parse a stream of delimited records.
fn parse<R: Read>(mut reader: R, delimiter: u8, budget: usize) -> std::io::Result<Table> {
    let mut table = Table {
        rows: Vec::new(),
        row: Vec::new(),
        field: Vec::new(),
        budget,
        truncated: false,
        open: false,
    };
    let mut buffer = vec![0u8; 64 * 1024];
    let mut quoted = false;
    let mut at_field_start = true;
    let mut skip_lf = false;

    loop {
        let read = reader.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        let mut index = 0;
        while index < read {
            let byte = buffer[index];

            // A CRLF pair is one line ending: the `\r` ended the record and
            // the `\n` that follows it belongs to the same break.
            if skip_lf {
                skip_lf = false;
                if byte == b'\n' {
                    index += 1;
                    continue;
                }
            }

            if quoted {
                if byte == b'"' {
                    // A doubled quote is the escape for a quote character.
                    if index + 1 < read && buffer[index + 1] == b'"' {
                        table.field.push(b'"');
                        index += 2;
                        continue;
                    }
                    quoted = false;
                    at_field_start = false;
                    index += 1;
                    continue;
                }
                table.field.push(byte);
                index += 1;
                continue;
            }

            match byte {
                // A quote only opens a quoted field when it is the first thing
                // in it; anywhere else it is an ordinary character.
                b'"' if at_field_start && table.field.is_empty() => {
                    quoted = true;
                    at_field_start = false;
                    table.open = true;
                }
                b if b == delimiter => {
                    if !table.push_field() {
                        table.flush();
                        return Ok(table);
                    }
                    at_field_start = true;
                    table.open = true;
                }
                b'\n' | b'\r' => {
                    if !table.push_row() {
                        return Ok(table);
                    }
                    at_field_start = true;
                    table.open = false;
                    skip_lf = byte == b'\r';
                }
                b => {
                    table.field.push(b);
                    at_field_start = false;
                    table.open = true;
                }
            }
            index += 1;
        }
    }

    // A file that does not end with a line break still ends its last record;
    // one that does has nothing left to add.
    if table.open || !table.field.is_empty() {
        table.push_row();
    }
    Ok(table)
}

impl Table {
    /// Finish the current field. `false` means the cell budget is spent, and
    /// the caller should stop reading.
    fn push_field(&mut self) -> bool {
        if self.row.len() >= MAX_COLS {
            self.truncated = true;
            self.field.clear();
            return true;
        }
        if self.budget == 0 {
            self.truncated = true;
            return false;
        }
        self.budget -= 1;
        let cell = if self.field.is_empty() {
            Cell::Empty
        } else {
            Cell::Text(String::from_utf8_lossy(&self.field).into_owned())
        };
        self.field.clear();
        self.row.push(cell);
        true
    }

    /// Finish the current record. `false` means the cell budget is spent.
    fn push_row(&mut self) -> bool {
        if !self.push_field() {
            self.flush();
            return false;
        }
        self.flush()
    }

    /// Keep the row built so far, without adding a field for it.
    fn flush(&mut self) -> bool {
        if self.row.is_empty() {
            return true;
        }
        if self.rows.len() >= MAX_ROWS {
            self.truncated = true;
            self.row.clear();
            return false;
        }
        self.rows.push(std::mem::take(&mut self.row));
        true
    }
}

/// What byte order mark the file starts with.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Bom {
    Utf8,
    Utf16Le,
    Utf16Be,
}

fn bom(prefix: &[u8]) -> Option<Bom> {
    if prefix.starts_with(&[0xEF, 0xBB, 0xBF]) {
        Some(Bom::Utf8)
    } else if prefix.starts_with(&[0xFF, 0xFE]) {
        Some(Bom::Utf16Le)
    } else if prefix.starts_with(&[0xFE, 0xFF]) {
        Some(Bom::Utf16Be)
    } else {
        None
    }
}

/// Decode UTF-16 code units without their byte order mark.
fn decode_utf16(bytes: &[u8], little_endian: bool) -> String {
    let units: Vec<u16> = bytes
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            if little_endian {
                u16::from_le_bytes(*pair)
            } else {
                u16::from_be_bytes(*pair)
            }
        })
        .collect();
    String::from_utf16_lossy(&units)
}

/// Choose the delimiter from the first record.
///
/// Characters inside a quoted field are not candidates: a header like
/// `"Name, first",Age` is two records either way, and the comma belongs to the
/// name. Ties and an empty first record fall back to a comma.
fn sniff(first_record: &[u8]) -> u8 {
    const CANDIDATES: &[u8; 4] = b",;\t|";
    let mut counts = [0usize; 4];
    let mut quoted = false;
    let mut index = 0;

    while index < first_record.len() {
        let byte = first_record[index];
        if quoted {
            if byte == b'"' {
                if first_record.get(index + 1) == Some(&b'"') {
                    index += 2;
                    continue;
                }
                quoted = false;
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' => quoted = true,
            b'\n' | b'\r' => break,
            b => {
                if let Some(slot) = CANDIDATES.iter().position(|candidate| *candidate == b) {
                    counts[slot] += 1;
                }
            }
        }
        index += 1;
    }

    let best = counts.iter().copied().max().unwrap_or(0);
    if best == 0 {
        return b',';
    }
    counts
        .iter()
        .position(|count| *count == best)
        .map(|slot| CANDIDATES[slot])
        .unwrap_or(b',')
}

/// A worksheet name derived from the file name, capped as Excel caps them.
fn sheet_name(path: &Path) -> String {
    let stem = path
        .file_stem()
        .map(|stem| stem.to_string_lossy().into_owned())
        .unwrap_or_default();
    let cleaned: String = stem
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '[' | ']' | '*' | '?' => '_',
            c => c,
        })
        .collect();
    let trimmed = cleaned.trim();
    let name = if trimmed.is_empty() { "Sheet" } else { trimmed };
    name.chars().take(31).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn parse_text(text: &str, delimiter: u8) -> Table {
        parse(Cursor::new(text.as_bytes()), delimiter, MAX_CELLS).expect("parse")
    }

    fn displayed(table: &Table) -> Vec<Vec<String>> {
        table
            .rows
            .iter()
            .map(|row| row.iter().map(Cell::display).collect())
            .collect()
    }

    fn temp_file(name: &str, contents: &[u8]) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join("excelscry-text-tables");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(contents).unwrap();
        path
    }

    #[test]
    fn records_become_rows_and_columns() {
        let table = parse_text("Region,Product\nNorth,Widget\n", b',');
        assert_eq!(
            displayed(&table),
            vec![
                vec!["Region".to_string(), "Product".to_string()],
                vec!["North".to_string(), "Widget".to_string()],
            ]
        );
        assert!(!table.truncated);
    }

    #[test]
    fn quoted_fields_keep_delimiters_quotes_and_line_breaks() {
        let text = "a,\"one, two\"\n\"said \"\"hi\"\"\",\"two\nlines\"\n";
        let table = parse_text(text, b',');
        assert_eq!(
            displayed(&table),
            vec![
                vec!["a".to_string(), "one, two".to_string()],
                vec!["said \"hi\"".to_string(), "two\nlines".to_string()],
            ]
        );
    }

    #[test]
    fn a_quote_in_the_middle_of_a_field_is_ordinary_text() {
        let table = parse_text("5\" screen,cheap\n", b',');
        assert_eq!(displayed(&table)[0][0], "5\" screen");
    }

    #[test]
    fn line_endings_do_not_add_an_empty_record() {
        for text in ["a,b", "a,b\n", "a,b\r\n", "a,b\r"] {
            let table = parse_text(text, b',');
            assert_eq!(
                displayed(&table),
                vec![vec!["a".to_string(), "b".to_string()]]
            );
        }
    }

    #[test]
    fn a_trailing_delimiter_keeps_its_empty_field() {
        let table = parse_text("a,b,\n", b',');
        assert_eq!(displayed(&table)[0], vec!["a", "b", ""]);
        assert!(matches!(table.rows[0][2], Cell::Empty));
    }

    #[test]
    fn empty_fields_are_empty_cells() {
        let table = parse_text("a,,c\n", b',');
        assert_eq!(table.rows[0][1], Cell::Empty);
        assert_eq!(table.rows[0].len(), 3);
    }

    #[test]
    fn a_blank_line_is_a_record_of_its_own() {
        let table = parse_text("a\n\nb\n", b',');
        assert_eq!(table.rows.len(), 3);
        assert!(table.rows[1].iter().all(Cell::is_empty));
    }

    #[test]
    fn values_stay_text_even_when_they_look_numeric() {
        // `000123` must not become 123, and `1E5` must not become 100000.
        let table = parse_text("000123,1E5,15/01/2026,TRUE\n", b',');
        assert_eq!(
            table.rows[0],
            vec![
                Cell::Text("000123".into()),
                Cell::Text("1E5".into()),
                Cell::Text("15/01/2026".into()),
                Cell::Text("TRUE".into()),
            ]
        );
    }

    #[test]
    fn ragged_rows_keep_their_own_width() {
        let table = parse_text("a,b,c\nd,e\n", b',');
        assert_eq!(table.rows[0].len(), 3);
        assert_eq!(table.rows[1].len(), 2);
    }

    #[test]
    fn a_tab_file_splits_on_tabs_and_keeps_commas() {
        let table = parse_text("a\tb, c\n1\t2, 3\n", b'\t');
        assert_eq!(displayed(&table)[0], vec!["a", "b, c"]);
        assert_eq!(displayed(&table)[1], vec!["1", "2, 3"]);
    }

    #[test]
    fn a_semicolon_file_is_detected_from_its_first_record() {
        assert_eq!(sniff(b"Name;Age;City\n"), b';');
        assert_eq!(sniff(b"Name\tAge\n"), b'\t');
        assert_eq!(sniff(b"Name,Age,City\n"), b',');
        // Commas inside a quoted header do not compete with the real delimiter.
        assert_eq!(sniff(b"\"Name, first\";Age\n"), b';');
        // Nothing to go on: the extension's comma.
        assert_eq!(sniff(b"Name\n"), b',');
        assert_eq!(sniff(b""), b',');
    }

    #[test]
    fn the_budget_stops_a_file_that_is_too_large() {
        let table = parse(Cursor::new(b"a,b,c\nd,e,f\n"), b',', 4).expect("parse");
        assert!(table.truncated);
        assert_eq!(table.budget, 0);
        // The cells that fit are kept, in order.
        assert_eq!(displayed(&table)[0], vec!["a", "b", "c"]);
        assert_eq!(displayed(&table)[1], vec!["d"]);
    }

    #[test]
    fn a_file_that_fits_is_not_reported_as_truncated() {
        let table = parse(Cursor::new(b"a,b\nc,d\n"), b',', 4).expect("parse");
        assert!(!table.truncated);
        assert_eq!(table.budget, 0);
    }

    #[test]
    fn a_utf8_bom_is_not_part_of_the_first_value() {
        let path = temp_file("bom.csv", b"\xEF\xBB\xBFname,age\nAda,36\n");
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Comma).expect("load");
        assert_eq!(wb.sheets[0].cell(0, 0), &Cell::Text("name".into()));
        assert_eq!(wb.sheets[0].cell(1, 1), &Cell::Text("36".into()));
        assert_eq!(wb.format, "Comma-Separated Values (csv)");
    }

    #[test]
    fn utf16_text_is_decoded() {
        // Excel's "Unicode Text" export: little-endian with a BOM.
        let mut bytes = vec![0xFF, 0xFE];
        for unit in "name\tage\nAda\t36\n".encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        let path = temp_file("unicode.tsv", &bytes);
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Tab).expect("load");
        assert_eq!(wb.sheets[0].cell(0, 0), &Cell::Text("name".into()));
        assert_eq!(wb.sheets[0].cell(1, 0), &Cell::Text("Ada".into()));
        assert_eq!(wb.sheets[0].cell(1, 1), &Cell::Text("36".into()));
    }

    #[test]
    fn invalid_utf8_is_shown_rather_than_dropped() {
        let path = temp_file("latin1.csv", b"caf\xE9,1\n");
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Comma).expect("load");
        assert_eq!(
            wb.sheets[0].cell(0, 0),
            &Cell::Text("caf\u{FFFD}".into()),
            "an undecodable byte should not lose the whole cell"
        );
    }

    #[test]
    fn the_sheet_is_named_after_the_file() {
        assert_eq!(sheet_name(Path::new("C:\\data\\sales.csv")), "sales");
        assert_eq!(sheet_name(Path::new("a/b:c.csv")), "b_c");
        assert_eq!(sheet_name(Path::new("x.tsv")), "x");
        assert_eq!(
            sheet_name(Path::new(&format!("{}.csv", "n".repeat(40)))).len(),
            31
        );
    }

    #[test]
    fn extensions_choose_the_reader() {
        assert_eq!(kind_of(Path::new("a.csv")), Some(Kind::Comma));
        assert_eq!(kind_of(Path::new("a.CSV")), Some(Kind::Comma));
        assert_eq!(kind_of(Path::new("a.tsv")), Some(Kind::Tab));
        assert_eq!(kind_of(Path::new("a.tab")), Some(Kind::Tab));
        assert_eq!(kind_of(Path::new("a.xlsx")), None);
        assert_eq!(kind_of(Path::new("a")), None);
    }

    #[test]
    fn plain_text_has_nothing_to_inspect() {
        let path = temp_file("plain.csv", b"a,b\n");
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Comma).expect("load");
        assert_eq!(wb.trust.badge, "No macros or links");
        assert_eq!(wb.trust.level, crate::trust::TrustLevel::Clean);
        assert!(wb.trust.hazards.is_empty());
        assert!(wb.trust.unchecked.is_empty());
    }
}
