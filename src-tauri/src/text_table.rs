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
//! UTF-16 (with a BOM, or recognised by its high zero bytes without one) is
//! decoded; a file that is not UTF-8 is scored against the legacy encodings a
//! Windows tool writes, so a Shift_JIS or GBK export opens as its own
//! characters rather than as mojibake; and rows may be ragged.

use std::fs::{File, Metadata};
use std::io::{BufRead, BufReader, Cursor, Read, Seek, SeekFrom};
use std::path::Path;

use crate::model::{Cell, LoadedWorkbook, SheetData, SheetFormats, TextInfo};
use crate::trust::{Trust, TrustReport};
use crate::workbook::{format_label, MAX_CELLS};

/// The reader's own sheet limits, so a text file cannot describe more than a
/// workbook could.
const MAX_ROWS: usize = 1_048_576;
const MAX_COLS: usize = 16_384;

/// How much of the first record is inspected when choosing a delimiter.
const SNIFF_BYTES: usize = 8 * 1024;

/// How much of a file is read when the encoding has to be worked out.
///
/// UTF-8 and UTF-16 are settled by a handful of bytes; a legacy encoding is
/// settled by statistics, and statistics want text. The more of it the detector
/// sees, the less often a short or unusual file is attributed to the wrong
/// alphabet. A prefix is enough — a file's first sixty-four kilobytes name its
/// language — and it costs one read of a file that is about to be read whole
/// anyway.
const DETECT_BYTES: usize = 64 * 1024;

/// How many non-ASCII bytes there must be before the detector is consulted.
///
/// One accented letter in an otherwise ASCII file is not enough to choose
/// between the world's encodings: every single-byte one explains it, so the
/// statistics answer with whatever their tables happen to favour, which is not
/// evidence. Below this, the answer stays Windows-1252 — what such a file
/// almost always is.
const MIN_DETECT_NON_ASCII: usize = 8;

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

/// A delimiter the reader can be told to use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delimiter {
    Comma,
    Semicolon,
    Tab,
    Pipe,
}

impl Delimiter {
    fn byte(self) -> u8 {
        match self {
            Delimiter::Comma => b',',
            Delimiter::Semicolon => b';',
            Delimiter::Tab => b'\t',
            Delimiter::Pipe => b'|',
        }
    }

    /// The name the bar shows and the store keeps.
    pub fn label(self) -> &'static str {
        match self {
            Delimiter::Comma => "comma",
            Delimiter::Semicolon => "semicolon",
            Delimiter::Tab => "tab",
            Delimiter::Pipe => "pipe",
        }
    }

    /// The delimiter a name or a single character refers to.
    pub fn from_label(label: &str) -> Option<Self> {
        match label.trim().to_ascii_lowercase().as_str() {
            "comma" | "," => Some(Delimiter::Comma),
            "semicolon" | ";" => Some(Delimiter::Semicolon),
            "tab" | "\\t" => Some(Delimiter::Tab),
            "pipe" | "|" => Some(Delimiter::Pipe),
            _ => None,
        }
    }
}

/// The delimiter a byte is, for reporting what a file was read with.
fn delimiter_label(byte: u8) -> &'static str {
    match byte {
        b';' => "semicolon",
        b'\t' => "tab",
        b'|' => "pipe",
        _ => "comma",
    }
}

/// How to decode a delimited file's bytes.
///
/// UTF-8 and UTF-16 are handled here because both have a byte order mark to
/// honour and neither is a guess once it is recognised. Every other encoding a
/// file can arrive in — the code pages of Windows, the ISO-8859 series, KOI8,
/// and the Japanese, Chinese and Korean encodings — is a decoder `encoding_rs`
/// already has, so it is named rather than reimplemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoding {
    Utf8,
    Utf16Le,
    Utf16Be,
    /// Any other encoding `encoding_rs` knows, including Windows-1252.
    Legacy(&'static encoding_rs::Encoding),
}

impl Encoding {
    /// The name shown to the user, and reported over IPC.
    pub fn label(self) -> &'static str {
        match self {
            Encoding::Utf8 => "UTF-8",
            Encoding::Utf16Le => "UTF-16 LE",
            Encoding::Utf16Be => "UTF-16 BE",
            Encoding::Legacy(encoding) => pretty_name(encoding),
        }
    }

    /// The encoding a name or a label refers to.
    ///
    /// Everything the bar offers and everything the detector can answer is
    /// accepted, in either spelling: the bar sends `utf-16le` and `gbk`, and a
    /// person reading the bar sees `UTF-16 LE` and `GBK`.
    pub fn from_label(label: &str) -> Option<Self> {
        let trimmed = label.trim();
        // The name a person sees (`UTF-16 LE`) and the name the UI sends
        // (`utf-16le`) are both accepted, as separators differ between them.
        match trimmed
            .to_ascii_lowercase()
            .replace(['_', ' '], "-")
            .as_str()
        {
            "utf-8" | "utf8" => return Some(Encoding::Utf8),
            "utf-16le" | "utf-16-le" | "utf16le" => return Some(Encoding::Utf16Le),
            "utf-16be" | "utf-16-be" | "utf16be" => return Some(Encoding::Utf16Be),
            _ => {}
        }
        // `for_label` knows every label a browser accepts for an encoding:
        // `windows-1252`, `latin1`, `shift_jis`, `sjis`, `gbk`, `euc-kr`,
        // `koi8-r` and the rest. It also answers UTF-16 for `utf-16`, so those
        // three are mapped to the variants that handle their byte order marks.
        let encoding = encoding_rs::Encoding::for_label(trimmed.as_bytes())?;
        if encoding == encoding_rs::UTF_8 {
            Some(Encoding::Utf8)
        } else if encoding == encoding_rs::UTF_16LE {
            Some(Encoding::Utf16Le)
        } else if encoding == encoding_rs::UTF_16BE {
            Some(Encoding::Utf16Be)
        } else if encoding == encoding_rs::REPLACEMENT || encoding == encoding_rs::X_USER_DEFINED {
            // Neither is a text encoding: one throws characters away and the
            // other is a byte pass-through for scripts.
            None
        } else {
            Some(Encoding::Legacy(encoding))
        }
    }
}

/// The name the bar shows: the encoding's own name, with the Windows code pages
/// spelled the way the picker spells them.
fn pretty_name(encoding: &'static encoding_rs::Encoding) -> &'static str {
    match encoding.name() {
        "windows-1250" => "Windows-1250",
        "windows-1251" => "Windows-1251",
        "windows-1252" => "Windows-1252",
        "windows-1253" => "Windows-1253",
        "windows-1254" => "Windows-1254",
        "windows-1255" => "Windows-1255",
        "windows-1256" => "Windows-1256",
        "windows-1257" => "Windows-1257",
        "windows-1258" => "Windows-1258",
        "windows-874" => "Windows-874",
        // Shift_JIS, EUC-JP, ISO-2022-JP, GBK, Big5, EUC-KR, KOI8-U, IBM866 and
        // the ISO-8859 series are already spelled the way they are written.
        name => name,
    }
}

/// What the UI asked for, with `None` meaning "let the file decide".
#[derive(Debug, Clone, Copy, Default)]
pub struct Dialect {
    pub delimiter: Option<Delimiter>,
    pub encoding: Option<Encoding>,
}

impl Dialect {
    /// Read a dialect from the two names the UI sends, where empty means
    /// automatic.
    pub fn from_labels(delimiter: Option<&str>, encoding: Option<&str>) -> Result<Self, String> {
        let delimiter = match delimiter.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => Some(Delimiter::from_label(name).ok_or_else(|| {
                format!("Unknown delimiter {name:?}. Choose comma, semicolon, tab or pipe.")
            })?),
            None => None,
        };
        let encoding = match encoding.map(str::trim).filter(|name| !name.is_empty()) {
            Some(name) => Some(Encoding::from_label(name).ok_or_else(|| {
                format!("Unknown encoding {name:?}. Choose one of the encodings the bar lists.")
            })?),
            None => None,
        };
        Ok(Dialect {
            delimiter,
            encoding,
        })
    }
}

/// Load a delimited text file as a one-sheet workbook.
///
/// A chosen delimiter or encoding overrides detection for that part only, which
/// is what lets the user fix one guess without discarding the other. Detection
/// is the default: the extension asks for a comma or a tab, the first record can
/// disagree, and the bytes say whether a file is UTF-8, one of the UTF-16s (with
/// or without a mark) or Windows-1252.
pub fn load_with(
    path: &Path,
    meta: &Metadata,
    kind: Kind,
    dialect: Dialect,
) -> Result<LoadedWorkbook, String> {
    let file = File::open(path).map_err(|e| format!("Cannot open \"{}\": {e}", path.display()))?;
    let mut reader = BufReader::new(file);
    let prefix = reader
        .fill_buf()
        .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?
        .to_vec();

    let byte_order_mark = bom(&prefix);
    // Whether the encoding was the reader's guess; only a guess may be revised
    // once the bytes are read.
    let auto_encoding = dialect.encoding.is_none();
    let mut encoding = match dialect.encoding {
        Some(chosen) => chosen,
        None => match byte_order_mark {
            Some(Bom::Utf16Le) => Encoding::Utf16Le,
            Some(Bom::Utf16Be) => Encoding::Utf16Be,
            Some(Bom::Utf8) => Encoding::Utf8,
            None => {
                // Detection reads past the buffer the prefix came from, so the
                // reader starts over from the first byte afterwards.
                let sample = read_sample(&mut reader)
                    .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?;
                let guess = detect_encoding(&sample, sample.len() < DETECT_BYTES);
                reader
                    .seek(SeekFrom::Start(0))
                    .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?;
                guess
            }
        },
    };
    let delimiter = match dialect.delimiter {
        Some(chosen) => chosen.byte(),
        // `.csv` says comma, but a file written in a locale that uses `;` as
        // the decimal separator is common enough that the first record decides.
        None => match kind {
            Kind::Tab => b'\t',
            Kind::Comma => sniff(&prefix[..prefix.len().min(SNIFF_BYTES)]),
        },
    };

    let (table, undecodable) = match encoding {
        // Streamed, because UTF-8 is the common case and a large file should
        // not have to be held in memory twice.
        Encoding::Utf8 => {
            if byte_order_mark == Some(Bom::Utf8) {
                reader.consume(3);
            }
            let table = parse(&mut reader, delimiter, MAX_CELLS)
                .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?;
            if table.undecodable && auto_encoding {
                // The start of the file looked like UTF-8, but a later field
                // did not decode. The whole file is in memory a moment later,
                // so the detector can name the encoding that start did not
                // reveal - a Japanese export whose header is ASCII is the
                // common case - rather than assuming a Western code page.
                reader
                    .seek(SeekFrom::Start(0))
                    .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?;
                let mut bytes = Vec::new();
                reader
                    .read_to_end(&mut bytes)
                    .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?;
                encoding = detect_encoding(&bytes, true);
                let (text, undecodable) = decode(&bytes, encoding);
                let table = parse(Cursor::new(text.into_bytes()), delimiter, MAX_CELLS)
                    .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?;
                (table, undecodable)
            } else {
                let undecodable = table.undecodable;
                (table, undecodable)
            }
        }
        // Decoded whole, because the bytes have to become UTF-8 before the
        // parser - which works on bytes - sees them.
        _ => {
            let mut bytes = Vec::new();
            reader
                .read_to_end(&mut bytes)
                .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?;
            let (text, undecodable) = decode(&bytes, encoding);
            let table = parse(Cursor::new(text.into_bytes()), delimiter, MAX_CELLS)
                .map_err(|e| format!("Cannot read \"{}\": {e}", path.display()))?;
            (table, undecodable)
        }
    };

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
            looks: Default::default(),
            // Text has no column widths, row heights or merges of its own.
            layout: Default::default(),
            // A delimited file has no formulas either: every cell is a value.
            formulas: Default::default(),
        }],
        truncated: table.truncated,
        total_cells: MAX_CELLS - table.budget,
        used_cells,
        // A text file is not a package: there is nothing in it that could carry
        // macros, links, connections, objects or controls.
        trust: TrustReport::new(Trust::plain_text()),
        text: Some(TextInfo {
            delimiter: delimiter_label(delimiter).to_string(),
            encoding: encoding.label().to_string(),
            delimiter_chosen: dialect.delimiter.is_some(),
            encoding_chosen: dialect.encoding.is_some(),
            undecodable,
            // The row pinned as the header is a choice about the view, kept
            // per file by `text_prefs`; a fresh parse knows nothing of it.
            header_row: None,
        }),
    })
}

/// The rows a parse produced, and what had to be dropped to produce them.
struct Table {
    rows: Vec<Vec<Cell>>,
    row: Vec<Cell>,
    field: Vec<u8>,
    budget: usize,
    truncated: bool,
    /// Whether a field held bytes that are not UTF-8, and so was shown with
    /// replacement characters.
    undecodable: bool,
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
        undecodable: false,
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
            match std::str::from_utf8(&self.field) {
                Ok(text) => Cell::Text(text.to_owned()),
                Err(_) => {
                    // Shown rather than dropped, and remembered so the UI can
                    // offer the encoding this file is actually in.
                    self.undecodable = true;
                    Cell::Text(String::from_utf8_lossy(&self.field).into_owned())
                }
            }
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

/// The encoding a file without a byte order mark is in.
///
/// Three questions settle it, in this order. A UTF-16 file written by a Windows
/// tool usually has no BOM, but every ASCII character in it has a zero byte on
/// one side, which is a pattern UTF-8 text and binary data do not share. Failing
/// that, bytes that are valid UTF-8 are UTF-8. Anything else is a legacy
/// encoding, and which one is a statistical question: the bytes are scored
/// against the encodings a Windows tool can have written, so a Japanese export
/// is read as Shift_JIS and a Russian one as windows-1251 rather than both
/// being read as Windows-1252.
///
/// `complete` says whether `sample` is the whole file, or a prefix cut out of a
/// longer one — the detector treats the end of the stream differently from a
/// pause in it.
fn detect_encoding(sample: &[u8], complete: bool) -> Encoding {
    match utf16_without_bom(sample) {
        Some(encoding) => encoding,
        None => {
            // An ISO-2022-JP file is 7-bit ASCII: its Japanese lives in escape
            // sequences, so it is valid UTF-8 and the rule below would keep the
            // escapes in the cells as text. Only the detector recognises it,
            // and only if it is asked first.
            if sample.contains(&0x1B) {
                let guess = guess_legacy(sample, complete);
                if guess == encoding_rs::ISO_2022_JP {
                    return Encoding::Legacy(guess);
                }
            }
            if utf8_prefix(sample) {
                return Encoding::Utf8;
            }
            if non_ascii_bytes(sample) < MIN_DETECT_NON_ASCII {
                return Encoding::Legacy(encoding_rs::WINDOWS_1252);
            }
            Encoding::Legacy(guess_legacy(sample, complete))
        }
    }
}

/// How many bytes in a sample are not ASCII.
fn non_ascii_bytes(sample: &[u8]) -> usize {
    sample.iter().filter(|byte| **byte >= 0x80).count()
}

/// Read the bytes an encoding is chosen from.
fn read_sample<R: Read>(reader: &mut R) -> std::io::Result<Vec<u8>> {
    let mut sample = Vec::with_capacity(DETECT_BYTES);
    reader.take(DETECT_BYTES as u64).read_to_end(&mut sample)?;
    Ok(sample)
}

/// Score a file's bytes against the legacy encodings, and name the best fit.
///
/// The detector weighs each candidate by what it can decode, by the character
/// pairs that are common in the language the encoding serves, and by the ones
/// that never occur at all, so a file of Japanese text rules out the Korean and
/// Chinese encodings rather than merely fitting them less well. UTF-8 is not
/// offered as an answer: the caller has already established these bytes are not
/// UTF-8, and "no, really, Windows-1252" is the useful reply.
fn guess_legacy(sample: &[u8], complete: bool) -> &'static encoding_rs::Encoding {
    let mut detector = chardetng::EncodingDetector::new(chardetng::Iso2022JpDetection::Allow);
    detector.feed(sample, complete);
    detector.guess(None, chardetng::Utf8Detection::Deny)
}

/// Whether a prefix is UTF-8, allowing its last character to be cut in half.
///
/// The prefix is a fixed window into the file, so it can end in the middle of a
/// multi-byte character. That is not evidence against the file being UTF-8, and
/// treating it as evidence would send every large UTF-8 file to Windows-1252.
fn utf8_prefix(prefix: &[u8]) -> bool {
    match std::str::from_utf8(prefix) {
        Ok(_) => true,
        // `error_len` is `None` only when the input ended mid-character.
        Err(error) => error.error_len().is_none(),
    }
}

/// UTF-16 without a byte order mark, recognised by its zero high bytes.
///
/// For text up to U+00FF - numbers, Latin words, the accented letters of a
/// Western file - the byte on one side of every code unit is zero: the high
/// byte in little-endian text, the low byte in big-endian text. The side with
/// the zeros says which byte order, and the window is decoded and checked to be
/// text before it is trusted: binary data that happens to have zeros every
/// other byte decodes to control characters instead. Text whose characters
/// mostly need both bytes (Greek, Cyrillic, CJK) is not recognised here; the
/// picker is the answer for those.
fn utf16_without_bom(sample: &[u8]) -> Option<Encoding> {
    let bytes = &sample[..sample.len() - sample.len() % 2];
    let pairs = bytes.len() / 2;
    // Two units is enough for the pattern; a header-only file of a few
    // characters should still be recognised, and binary data is rejected by
    // the text check below rather than by its length.
    if pairs < 2 {
        return None;
    }
    let zeros_at = |offset: usize| {
        bytes
            .iter()
            .skip(offset)
            .step_by(2)
            .filter(|byte| **byte == 0)
            .count()
    };
    // Even bytes are the low half of each unit, odd bytes the high half.
    let low_zeros = zeros_at(0);
    let high_zeros = zeros_at(1);
    let little_endian = high_zeros * 2 >= pairs && low_zeros * 8 <= pairs;
    let big_endian = low_zeros * 2 >= pairs && high_zeros * 8 <= pairs;
    let encoding = match (little_endian, big_endian) {
        (true, false) => Encoding::Utf16Le,
        (false, true) => Encoding::Utf16Be,
        _ => return None,
    };
    let (text, replaced) = decode_utf16(bytes, encoding == Encoding::Utf16Le);
    (!replaced && looks_like_text(&text)).then_some(encoding)
}

/// Whether decoded text is text, rather than bytes that happened to decode.
///
/// A tab, a line break and a carriage return are the controls a table can hold;
/// anything else in C0 is a signature of binary data read as UTF-16.
fn looks_like_text(text: &str) -> bool {
    !text.is_empty()
        && text
            .chars()
            .all(|ch| !ch.is_control() || matches!(ch, '\t' | '\r' | '\n'))
}

/// Decode a byte range with an encoding, saying whether anything in it had to
/// be replaced.
/// Decode bytes the way a file read with `encoding` was decoded.
///
/// The copy writer uses this rather than its own decoder, so a UTF-8 copy is
/// made from exactly the text the grid showed.
pub(crate) fn decode_bytes(bytes: &[u8], encoding: Encoding) -> String {
    decode(bytes, encoding).0
}

fn decode(bytes: &[u8], encoding: Encoding) -> (String, bool) {
    match encoding {
        Encoding::Utf8 => match std::str::from_utf8(bytes) {
            Ok(text) => (text.to_owned(), false),
            Err(_) => (String::from_utf8_lossy(bytes).into_owned(), true),
        },
        Encoding::Utf16Le => decode_utf16(bytes, true),
        Encoding::Utf16Be => decode_utf16(bytes, false),
        // A UTF-8 BOM is stripped: a file that carries one was written by a
        // tool that meant UTF-8, and is being read this way because its bytes
        // turned out not to be. Single-byte encodings map every byte to a
        // character, so decoding cannot fail; the multi-byte ones report the
        // bytes no character sequence explains, which is what the bar's hint is
        // for.
        Encoding::Legacy(encoding) => {
            let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
            let (text, replaced) = encoding.decode_without_bom_handling(bytes);
            (text.into_owned(), replaced)
        }
    }
}

/// Decode UTF-16 code units, without their byte order mark if they have one.
fn decode_utf16(bytes: &[u8], little_endian: bool) -> (String, bool) {
    let bytes = if little_endian {
        bytes.strip_prefix(&[0xFF, 0xFE]).unwrap_or(bytes)
    } else {
        bytes.strip_prefix(&[0xFE, 0xFF]).unwrap_or(bytes)
    };
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
    match String::from_utf16(&units) {
        Ok(text) => (text, false),
        // Only an unpaired surrogate gets here; replacing it keeps the rest of
        // the field.
        Err(_) => (String::from_utf16_lossy(&units), true),
    }
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

    /// The default read, which is what an open from the command line does.
    fn load(
        path: &std::path::Path,
        meta: &std::fs::Metadata,
        kind: Kind,
    ) -> Result<LoadedWorkbook, String> {
        load_with(path, meta, kind, Dialect::default())
    }

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
    fn windows_1252_needs_no_picker() {
        // The shape of a legacy export: `é` as the single byte 0xE9, which is
        // not UTF-8 at all.
        let path = temp_file("legacy.csv", b"name,caf\xE9\nAda,th\xE9\n");
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Comma).expect("load");
        assert_eq!(wb.sheets[0].cell(0, 1), &Cell::Text("caf\u{E9}".into()));
        assert_eq!(wb.sheets[0].cell(1, 1), &Cell::Text("th\u{E9}".into()));
        let text = wb.text.expect("a text table reports its dialect");
        assert_eq!(text.encoding, "Windows-1252");
        assert!(!text.encoding_chosen, "nobody chose it; the file said so");
        assert!(!text.undecodable);
    }

    #[test]
    fn windows_1252_beyond_the_sniff_window_is_still_found() {
        // The inspected prefix is all ASCII and valid UTF-8, so the first read
        // streams as UTF-8 and only meets 0xE9 later. That byte is not a good
        // reason to show every other accented cell as a replacement character.
        let mut bytes = vec![b'a'; 9000];
        bytes.extend_from_slice(b",caf\xE9\n");
        let path = temp_file("late-latin1.csv", &bytes);
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Comma).expect("load");
        assert_eq!(wb.sheets[0].cell(0, 0).display().len(), 9000);
        assert_eq!(wb.sheets[0].cell(0, 1), &Cell::Text("caf\u{E9}".into()));
        let text = wb.text.expect("a text table reports its dialect");
        assert_eq!(text.encoding, "Windows-1252");
        assert!(!text.encoding_chosen);
        assert!(!text.undecodable);
    }

    #[test]
    fn a_utf8_character_cut_by_the_sniff_window_is_not_mistaken_for_latin1() {
        // The window ends between the two bytes of `é`; an incomplete sequence
        // is not an invalid one, and the file is still UTF-8.
        let mut bytes = vec![b'a'; 8191];
        bytes.extend_from_slice("\u{E9}".as_bytes());
        bytes.extend_from_slice(b",b\n");
        let path = temp_file("cut-utf8.csv", &bytes);
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Comma).expect("load");
        assert_eq!(wb.text.as_ref().unwrap().encoding, "UTF-8");
        let first = wb.sheets[0].cell(0, 0);
        assert!(matches!(first, Cell::Text(text) if text.ends_with('\u{E9}')));
        assert_eq!(wb.sheets[0].cell(0, 1), &Cell::Text("b".into()));
    }

    #[test]
    fn bom_less_utf16_is_detected_from_its_zero_bytes() {
        let little: Vec<u8> = "name\tage\nAda\t36\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let big: Vec<u8> = "name\tage\nAda\t36\n"
            .encode_utf16()
            .flat_map(u16::to_be_bytes)
            .collect();
        for (name, bytes, label) in [
            ("utf16le-bare.tsv", little, "UTF-16 LE"),
            ("utf16be-bare.tsv", big, "UTF-16 BE"),
        ] {
            let path = temp_file(name, &bytes);
            let meta = std::fs::metadata(&path).unwrap();
            let wb = load(&path, &meta, Kind::Tab).expect("load");
            assert_eq!(wb.sheets[0].cell(0, 0), &Cell::Text("name".into()));
            assert_eq!(wb.sheets[0].cell(1, 0), &Cell::Text("Ada".into()));
            let text = wb.text.expect("a text table reports its dialect");
            assert_eq!(text.encoding, label);
            assert!(!text.encoding_chosen);
            assert!(!text.undecodable);
        }
    }

    #[test]
    fn a_truncated_character_is_not_an_invalid_one() {
        assert!(utf8_prefix("caf\u{E9}".as_bytes()));
        // Half of `é` at the end of the sniff window is a valid UTF-8 prefix.
        assert!(utf8_prefix(b"caf\xC3"));
        assert_eq!(detect_encoding(b"caf\xC3", true), Encoding::Utf8);
        // A byte that cannot start a sequence is a real error, not a cut.
        assert!(!utf8_prefix(b"caf\xC3("));
        const WINDOWS_1252: Encoding = Encoding::Legacy(encoding_rs::WINDOWS_1252);
        assert_eq!(detect_encoding(b"caf\xC3(", true), WINDOWS_1252);
    }

    #[test]
    fn a_very_short_utf16_file_is_still_recognised() {
        let bytes: Vec<u8> = "a,b".encode_utf16().flat_map(u16::to_le_bytes).collect();
        let path = temp_file("tiny-utf16.csv", &bytes);
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Comma).expect("load");
        assert_eq!(wb.sheets[0].cell(0, 0), &Cell::Text("a".into()));
        assert_eq!(wb.sheets[0].cell(0, 1), &Cell::Text("b".into()));
        assert_eq!(wb.text.unwrap().encoding, "UTF-16 LE");
    }

    #[test]
    fn zero_bytes_that_are_not_text_are_not_read_as_utf16() {
        // Bytes with a zero every other one, but 0x80-0x89 are not ASCII: read
        // as UTF-16 they are control characters, so this is not a table.
        let bytes = [
            0x80, 0x00, 0x81, 0x00, 0x82, 0x00, 0x83, 0x00, 0x84, 0x00, 0x85, 0x00, 0x86, 0x00,
            0x87, 0x00, 0x88, 0x00, 0x89, 0x00,
        ];
        assert_eq!(utf16_without_bom(&bytes), None);
        assert_eq!(
            detect_encoding(&bytes, true),
            Encoding::Legacy(encoding_rs::WINDOWS_1252)
        );
    }

    #[test]
    fn an_explicit_utf8_read_says_it_could_not_decode() {
        // A choice is respected: choosing UTF-8 for a legacy file shows the
        // replacement characters and lets the bar say why.
        let path = temp_file("latin1-chosen.csv", b"caf\xE9,1\n");
        let meta = std::fs::metadata(&path).unwrap();
        let dialect = Dialect::from_labels(None, Some("utf-8")).unwrap();
        let wb = load_with(&path, &meta, Kind::Comma, dialect).expect("load");
        assert_eq!(
            wb.sheets[0].cell(0, 0),
            &Cell::Text("caf\u{FFFD}".into()),
            "an undecodable byte should not lose the whole cell"
        );
        let text = wb.text.expect("a text table reports its dialect");
        assert_eq!(text.encoding, "UTF-8");
        assert!(text.encoding_chosen);
        assert!(
            text.undecodable,
            "the UI offers another encoding only if the read says it may be wrong"
        );
    }

    #[test]
    fn a_utf8_file_with_accents_stays_utf8() {
        let path = temp_file("utf8-accents.csv", "name,caf\u{E9}\n".as_bytes());
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Comma).expect("load");
        assert_eq!(wb.sheets[0].cell(0, 1), &Cell::Text("caf\u{E9}".into()));
        let text = wb.text.expect("a text table reports its dialect");
        assert_eq!(text.encoding, "UTF-8");
        assert!(!text.encoding_chosen);
        assert!(!text.undecodable);
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
    fn a_chosen_delimiter_replaces_detection() {
        // Detection would pick the semicolon; choosing a comma is the user
        // saying the file is one column after all.
        let path = temp_file("chosen-comma.csv", b"a;b\n1;2\n");
        let meta = std::fs::metadata(&path).unwrap();
        let dialect = Dialect::from_labels(Some("comma"), None).unwrap();
        let wb = load_with(&path, &meta, Kind::Comma, dialect).expect("load");
        assert_eq!(wb.sheets[0].cols, 1);
        let text = wb.text.expect("a text table reports its dialect");
        assert_eq!(text.delimiter, "comma");
        assert!(text.delimiter_chosen);
        assert!(!text.encoding_chosen);
        assert_eq!(text.encoding, "UTF-8");
    }

    #[test]
    fn a_tab_file_can_be_read_as_semicolon_separated() {
        // A `.tsv` that is really semicolon-separated: the extension's tab
        // leaves it in one column until the user says otherwise.
        let path = temp_file("actually-semicolon.tsv", b"a;b;c\n1;2;3\n");
        let meta = std::fs::metadata(&path).unwrap();
        let detected = load(&path, &meta, Kind::Tab).expect("load");
        assert_eq!(detected.sheets[0].cols, 1);

        let dialect = Dialect::from_labels(Some("semicolon"), None).unwrap();
        let chosen = load_with(&path, &meta, Kind::Tab, dialect).expect("load");
        assert_eq!(chosen.sheets[0].cols, 3);
        assert_eq!(chosen.sheets[0].cell(0, 1), &Cell::Text("b".into()));
        assert_eq!(chosen.sheets[0].cell(1, 2), &Cell::Text("3".into()));
    }

    #[test]
    fn a_file_decoded_as_windows_1252_keeps_its_accents() {
        let path = temp_file("cp1252.csv", b"name,caf\xE9\n");
        let meta = std::fs::metadata(&path).unwrap();
        let dialect = Dialect::from_labels(None, Some("windows-1252")).unwrap();
        let wb = load_with(&path, &meta, Kind::Comma, dialect).expect("load");
        assert_eq!(wb.sheets[0].cell(0, 1), &Cell::Text("caf\u{E9}".into()));
        let text = wb.text.expect("a text table reports its dialect");
        assert_eq!(text.encoding, "Windows-1252");
        assert!(text.encoding_chosen);
        assert!(!text.undecodable);
    }

    #[test]
    fn an_explicit_utf16_encoding_needs_no_byte_order_mark() {
        let bytes: Vec<u8> = "name\tage\nAda\t36\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes)
            .collect();
        let path = temp_file("utf16-bare.tsv", &bytes);
        let meta = std::fs::metadata(&path).unwrap();
        let dialect = Dialect::from_labels(Some("tab"), Some("utf-16le")).unwrap();
        let wb = load_with(&path, &meta, Kind::Tab, dialect).expect("load");
        assert_eq!(wb.sheets[0].cell(0, 0), &Cell::Text("name".into()));
        assert_eq!(wb.sheets[0].cell(1, 1), &Cell::Text("36".into()));
        assert_eq!(wb.text.unwrap().delimiter, "tab");
    }

    #[test]
    fn the_dialect_is_reported_in_the_workbook_description() {
        let path = temp_file("reported.csv", b"a;b\n");
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Comma).expect("load");
        let text = wb.info().text.expect("the UI needs it to offer the picker");
        assert_eq!(text.delimiter, "semicolon");
        assert_eq!(text.encoding, "UTF-8");
        assert!(!text.delimiter_chosen);
        assert!(!text.undecodable);
    }

    #[test]
    fn dialect_labels_are_validated() {
        assert!(Dialect::from_labels(None, None).is_ok());
        // A display name and a UI value both name the same encoding.
        assert_eq!(Encoding::from_label("UTF-16 LE"), Some(Encoding::Utf16Le));
        assert_eq!(Encoding::from_label("utf-16le"), Some(Encoding::Utf16Le));
        assert_eq!(
            Encoding::from_label("Windows-1252"),
            Some(Encoding::Legacy(encoding_rs::WINDOWS_1252))
        );
        // A name from a language the old list never covered, in either
        // spelling: what the bar sends, and what the bar shows.
        assert_eq!(
            Encoding::from_label("shift_jis"),
            Some(Encoding::Legacy(encoding_rs::SHIFT_JIS))
        );
        assert_eq!(
            Encoding::from_label("Shift_JIS"),
            Some(Encoding::Legacy(encoding_rs::SHIFT_JIS))
        );
        assert_eq!(
            Encoding::from_label("gbk").map(Encoding::label),
            Some("GBK")
        );
        assert_eq!(
            Encoding::from_label("utf-16"),
            Some(Encoding::Utf16Le),
            "a bare UTF-16 label is the little-endian one"
        );
        assert_eq!(Encoding::from_label("replacement"), None);
        assert!(Dialect::from_labels(Some(""), Some(" ")).is_ok());
        assert!(Dialect::from_labels(Some(";"), Some("utf_16le")).is_ok());
        let err = Dialect::from_labels(Some("tilde"), None).unwrap_err();
        assert!(err.contains("Unknown delimiter"), "unexpected: {err}");
        let err = Dialect::from_labels(None, Some("ebcdic")).unwrap_err();
        assert!(err.contains("Unknown encoding"), "unexpected: {err}");
    }

    /// A file's bytes the way its own encoding wrote them.
    fn encoded(encoding: &'static encoding_rs::Encoding, text: &str) -> Vec<u8> {
        let (bytes, _, unmappable) = encoding.encode(text);
        assert!(
            !unmappable,
            "the fixture has to be writable in its encoding"
        );
        bytes.into_owned()
    }

    /// The Japanese, Chinese, Korean and other single-byte exports a Windows
    /// tool writes when nobody asked for UTF-8, and the name the bar should end
    /// up showing for each.
    ///
    /// Two answers are compatible rather than identical, and are pinned here so
    /// a change of detector is noticed: KOI8-R text decodes identically in
    /// KOI8-U, and the Hebrew code page is recognised as the visual ISO-8859-8
    /// that carries the same letters.
    fn legacy_samples() -> Vec<(
        &'static str,
        &'static encoding_rs::Encoding,
        &'static str,
        &'static str,
    )> {
        const JA: &str = "商品コード,商品名,単価,数量,備考\nA-1001,りんご,150,12,青森県産の新鮮な果物です\nB-2002,みかん,80,40,愛媛県産の甘い柑橘類\nC-3003,ぶどう,320,8,山梨県産の高級品\nD-4004,もも,280,15,福島県産の贈答用\nE-5005,すいか,1200,3,熊本県産の大玉\nF-6006,いちご,400,20,栃木県産の甘酸っぱい苺\n";
        const ZH: &str = "商品编号,商品名称,单价,数量,备注\nA-1001,苹果,150,12,山东烟台的新鲜水果\nB-2002,橘子,80,40,湖南常德的甜橙\nC-3003,葡萄,320,8,新疆吐鲁番的无核葡萄\nD-4004,桃子,280,15,北京平谷的鲜桃\nE-5005,西瓜,1200,3,海南岛的大西瓜\nF-6006,草莓,400,20,辽宁丹东的红颜草莓\n";
        const ZH_TW: &str = "商品編號,商品名稱,單價,數量,備註\nA-1001,蘋果,150,12,山東煙台的新鮮水果\nB-2002,橘子,80,40,湖南常德的甜橙\nC-3003,葡萄,320,8,新疆吐魯番的無核葡萄\nD-4004,桃子,280,15,北京平谷的鮮桃\nE-5005,西瓜,1200,3,海南島的大西瓜\nF-6006,草莓,400,20,遼寧丹東的紅顏草莓\n";
        const KO: &str = "상품코드,상품명,단가,수량,비고\nA-1001,사과,150,12,경상북도 청송의 신선한 과일\nB-2002,감귤,80,40,제주도에서 온 달콤한 귤\nC-3003,포도,320,8,충청북도 영동의 포도\nD-4004,복숭아,280,15,경기도 이천의 복숭아\nE-5005,수박,1200,3,전라북도 고창의 수박\nF-6006,딸기,400,20,충청남도 논산의 딸기\n";
        const RU: &str = "Код,Наименование,Цена,Количество,Примечание\nA-1001,Яблоко,150,12,Свежие фрукты из Краснодара\nB-2002,Мандарин,80,40,Сладкие цитрусовые из Сочи\nC-3003,Виноград,320,8,Виноград из Дагестана\nD-4004,Персик,280,15,Персики из Крыма\nE-5005,Арбуз,1200,3,Большой арбуз из Астрахани\nF-6006,Земляника,400,20,Сладкая клубника из Липецка\n";
        const PL: &str = "Kod,Nazwa,Cena,Ilość,Uwagi\nA-1001,Jabłko,150,12,Świeże owoce z Grójca\nB-2002,Mandarynka,80,40,Słodkie cytrusy z Hiszpanii\nC-3003,Winogrono,320,8,Kiść winogron z Włoch\nD-4004,Brzoskwinia,280,15,Soczysta brzoskwinia z Sandomierza\nE-5005,Arbuz,1200,3,Duży arbuz z Węgier\nF-6006,Truskawka,400,20,Słodka truskawka z Kaszub\n";
        const EL: &str = "Κωδικός,Όνομα,Τιμή,Ποσότητα,Σχόλια\nA-1001,Μήλο,150,12,Φρέσκα φρούτα από την Πέλλα\nB-2002,Μανταρίνι,80,40,Γλυκά εσπεριδοειδή από την Άρτα\nC-3003,Σταφύλι,320,8,Σταφύλια από τη Νεμέα\nD-4004,Ροδάκινο,280,15,Ροδάκινα από τη Βέροια\nE-5005,Καρπούζι,1200,3,Μεγάλο καρπούζι από την Ηλεία\nF-6006,Φράουλα,400,20,Γλυκές φράουλες από την Καβάλα\n";
        const HE: &str = "קוד,שם,מחיר,כמות,הערות\nA-1001,תפוח,150,12,פירות טריים מהגליל\nB-2002,מנדרינה,80,40,הדרים מתוקים מהשרון\nC-3003,ענב,320,8,ענבים מכרמיאל\nD-4004,אפרסק,280,15,אפרסקים מהגולן\nE-5005,אבטיח,1200,3,אבטיח גדול מהערבה\nF-6006,תות,400,20,תותים מתוקים מהשרון\n";
        const AR: &str = "الرمز,الاسم,السعر,الكمية,ملاحظات\nA-1001,تفاح,150,12,فواكه طازجة من لبنان\nB-2002,يوسفي,80,40,حمضيات حلوة من المغرب\nC-3003,عنب,320,8,عنب من تونس\nD-4004,خوخ,280,15,خوخ من سوريا\nE-5005,بطيخ,1200,3,بطيخ كبير من مصر\nF-6006,فراولة,400,20,فراولة حلوة من الأردن\n";
        vec![
            ("shift-jis", encoding_rs::SHIFT_JIS, JA, "Shift_JIS"),
            ("euc-jp", encoding_rs::EUC_JP, JA, "EUC-JP"),
            ("iso-2022-jp", encoding_rs::ISO_2022_JP, JA, "ISO-2022-JP"),
            ("gbk", encoding_rs::GBK, ZH, "GBK"),
            ("big5", encoding_rs::BIG5, ZH_TW, "Big5"),
            ("euc-kr", encoding_rs::EUC_KR, KO, "EUC-KR"),
            (
                "windows-1251",
                encoding_rs::WINDOWS_1251,
                RU,
                "Windows-1251",
            ),
            ("koi8-r", encoding_rs::KOI8_R, RU, "KOI8-U"),
            ("iso-8859-2", encoding_rs::ISO_8859_2, PL, "ISO-8859-2"),
            (
                "windows-1253",
                encoding_rs::WINDOWS_1253,
                EL,
                "Windows-1253",
            ),
            ("windows-1255", encoding_rs::WINDOWS_1255, HE, "ISO-8859-8"),
            (
                "windows-1256",
                encoding_rs::WINDOWS_1256,
                AR,
                "Windows-1256",
            ),
        ]
    }

    #[test]
    fn a_legacy_file_opens_as_the_characters_it_holds() {
        for (name, encoding, text, _) in legacy_samples() {
            let path = temp_file(&format!("{name}.csv"), &encoded(encoding, text));
            let meta = std::fs::metadata(&path).unwrap();
            let wb = load(&path, &meta, Kind::Comma).expect("load");
            let expected: Vec<Vec<String>> = text
                .lines()
                .map(|line| line.split(',').map(str::to_owned).collect())
                .collect();
            let shown: Vec<Vec<String>> = wb.sheets[0]
                .rows
                .iter()
                .map(|row| row.iter().map(Cell::display).collect())
                .collect();
            assert_eq!(shown, expected, "{name} was misread");
            let info = wb.text.expect("a text table reports its dialect");
            assert!(!info.undecodable, "{name} should decode completely");
            assert!(!info.encoding_chosen, "nobody chose it; the bytes said it");
        }
    }

    #[test]
    fn the_bar_names_the_encoding_the_bytes_turned_out_to_be() {
        for (name, encoding, text, label) in legacy_samples() {
            let path = temp_file(&format!("named-{name}.csv"), &encoded(encoding, text));
            let meta = std::fs::metadata(&path).unwrap();
            let wb = load(&path, &meta, Kind::Comma).expect("load");
            assert_eq!(wb.text.unwrap().encoding, label, "{name}");
        }
    }

    #[test]
    fn every_encoding_the_bar_offers_is_one_it_can_read_back() {
        // The picker in ui/index.html, value by value. The bar sends the value
        // and reads the name back when the file is reopened, so a value the
        // backend rejects — or one whose name it cannot find again — would leave
        // the select on Auto and re-read the file with detection instead.
        for value in [
            "utf-8",
            "utf-16le",
            "utf-16be",
            "windows-1252",
            "windows-1250",
            "iso-8859-2",
            "iso-8859-4",
            "windows-1251",
            "koi8-r",
            "koi8-u",
            "iso-8859-5",
            "ibm866",
            "windows-1253",
            "iso-8859-7",
            "windows-1254",
            "windows-1255",
            "iso-8859-8",
            "windows-1256",
            "iso-8859-6",
            "windows-1257",
            "iso-8859-13",
            "windows-1258",
            "windows-874",
            "shift_jis",
            "euc-jp",
            "iso-2022-jp",
            "gbk",
            "gb18030",
            "big5",
            "euc-kr",
        ] {
            let encoding = Encoding::from_label(value)
                .unwrap_or_else(|| panic!("the bar offers {value}, which is not accepted"));
            let label = encoding.label();
            assert_eq!(
                Encoding::from_label(label),
                Some(encoding),
                "{value} does not read back from the name {label:?}"
            );
        }
    }

    #[test]
    fn a_handful_of_accented_bytes_is_not_a_language_sample() {
        // Two bytes a Central European code page explains as well as a Western
        // one: with this little text there is nothing to choose between them,
        // so the answer stays the Western default rather than following noise.
        assert_eq!(
            detect_encoding(b"a,\x8a\x9a\n", true),
            Encoding::Legacy(encoding_rs::WINDOWS_1252)
        );
    }

    #[test]
    fn a_chosen_legacy_encoding_is_used_as_chosen() {
        // Detection finds Shift_JIS on its own here; choosing it explicitly is
        // how a file the detector gets wrong is read by hand.
        let (_, encoding, text, _) = legacy_samples()
            .into_iter()
            .find(|(name, ..)| *name == "shift-jis")
            .expect("a Japanese sample");
        let path = temp_file("chosen-shift-jis.csv", &encoded(encoding, text));
        let meta = std::fs::metadata(&path).unwrap();
        let dialect = Dialect::from_labels(Some("comma"), Some("shift_jis")).unwrap();
        let wb = load_with(&path, &meta, Kind::Comma, dialect).expect("load");
        assert_eq!(wb.sheets[0].cell(0, 0), &Cell::Text("商品コード".into()));
        let info = wb.text.expect("a text table reports its dialect");
        assert_eq!(info.encoding, "Shift_JIS");
        assert!(info.encoding_chosen);
        assert!(!info.undecodable);
    }

    #[test]
    fn a_cjk_export_whose_header_is_ascii_is_still_read_as_its_own_encoding() {
        // The inspected prefix is all ASCII, so the first read streams as UTF-8
        // and only meets the Japanese bytes later. Those bytes are not a reason
        // to show the file as Western mojibake.
        let (_, encoding, text, _) = legacy_samples()
            .into_iter()
            .find(|(name, ..)| *name == "shift-jis")
            .expect("a Japanese sample");
        let mut bytes = vec![b'a'; DETECT_BYTES + 1024];
        bytes.push(b'\n');
        bytes.extend_from_slice(&encoded(encoding, text));
        let path = temp_file("late-japanese.csv", &bytes);
        let meta = std::fs::metadata(&path).unwrap();
        let wb = load(&path, &meta, Kind::Comma).expect("load");
        assert_eq!(wb.sheets[0].cell(1, 0), &Cell::Text("商品コード".into()));
        let info = wb.text.expect("a text table reports its dialect");
        assert_eq!(info.encoding, "Shift_JIS");
        assert!(!info.encoding_chosen);
        assert!(!info.undecodable);
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
