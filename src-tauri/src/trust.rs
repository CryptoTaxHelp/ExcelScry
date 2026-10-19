//! What else a workbook carries besides cells.
//!
//! A viewer is often pointed at a file from a source the user does not trust,
//! so the UI says up front whether the package carries macros, links to other
//! workbooks, external data connections, embedded objects or ActiveX controls —
//! the parts that can run code or reach the network when the file is later
//! opened in Excel or LibreOffice.
//!
//! Nothing here executes or extracts any of it. Inspection reads a package's
//! member list, plus two element names in an ODF `content.xml`; cell data is
//! never touched.
//!
//! The report is deliberately tri-state. `None` means *not inspected for this
//! format*, never *absent*: reporting that a legacy `.xls` has no external
//! links when nothing looked for them would be worse than saying nothing.

use std::fs::File;
use std::io::{BufReader, Read, Seek};
use std::path::Path;

use calamine::{Reader, Sheets};
use serde::Serialize;
use zip::ZipArchive;

// --- OOXML parts ------------------------------------------------------------
//
// Paths are the OPC part names from ECMA-376. `.xlsb` uses the same layout with
// binary parts, so a name that differs between the two is listed in both forms.

/// The VBA project, i.e. the macros.
const OOXML_VBA_PROJECT: &str = "xl/vbaProject.bin";
/// A digital signature over the VBA project. Present only alongside a project.
const OOXML_VBA_SIGNATURE: &str = "xl/vbaProjectSignature.bin";
/// Excel 4.0 macro sheets: macros without a VBA project.
const OOXML_MACRO_SHEETS: &str = "xl/macrosheets/";
/// Workbooks and documents this one pulls values from.
const OOXML_EXTERNAL_LINKS: &str = "xl/externalLinks/";
/// External data connections (OLE DB, ODBC, web queries).
const OOXML_CONNECTIONS: [&str; 2] = ["xl/connections.xml", "xl/connections.bin"];
/// OLE objects stored inside the workbook.
const OOXML_EMBEDDINGS: &str = "xl/embeddings/";
/// ActiveX controls, which are executable content.
const OOXML_ACTIVEX: &str = "xl/activeX/";

// --- ODF members -----------------------------------------------------------

/// ODF keeps macros as package members rather than in an OLE2 VBA project.
const ODF_MACRO_DIRS: [&str; 2] = ["Basic/", "Scripts/"];
/// A linked cell range in another document.
const ODF_TABLE_SOURCE: &[u8] = b"<table:table-source";
/// ODF's equivalent of an external data connection.
const ODF_DATABASE_RANGE: &[u8] = b"<table:database-range";

/// What a package contains, one field per signal.
///
/// `None` always means *this was not inspected*, so the UI can say
/// "not checked" instead of implying a clean result.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Trust {
    pub macros: Option<bool>,
    pub external_links: Option<usize>,
    pub connections: Option<bool>,
    pub embedded_objects: Option<usize>,
    pub activex: Option<bool>,
}

impl Trust {
    /// Nothing known: used when the package could not be read at all.
    pub const fn unknown() -> Self {
        Self {
            macros: None,
            external_links: None,
            connections: None,
            embedded_objects: None,
            activex: None,
        }
    }

    /// A delimited text file: there is no package, so there is nothing that
    /// could carry macros, links, connections, objects or controls. Unlike
    /// [`Trust::unknown`], this is a real answer rather than a missing one.
    pub const fn plain_text() -> Self {
        Self {
            macros: Some(false),
            external_links: Some(0),
            connections: Some(false),
            embedded_objects: Some(0),
            activex: Some(false),
        }
    }

    /// Short labels for the signals that came back positive.
    fn hazards(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.macros == Some(true) {
            out.push("Macros".to_string());
        }
        if let Some(n) = self.external_links.filter(|n| *n > 0) {
            out.push(count_label(n, "external link", "external links"));
        }
        if self.connections == Some(true) {
            out.push("Data connections".to_string());
        }
        if let Some(n) = self.embedded_objects.filter(|n| *n > 0) {
            out.push(count_label(n, "embedded object", "embedded objects"));
        }
        if self.activex == Some(true) {
            out.push("ActiveX controls".to_string());
        }
        out
    }

    /// Names of the signals this format gave us no way to check.
    fn unchecked(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.macros.is_none() {
            out.push("macros".to_string());
        }
        if self.external_links.is_none() {
            out.push("external links".to_string());
        }
        if self.connections.is_none() {
            out.push("data connections".to_string());
        }
        if self.embedded_objects.is_none() {
            out.push("embedded objects".to_string());
        }
        if self.activex.is_none() {
            out.push("ActiveX controls".to_string());
        }
        out
    }
}

/// How alarming the report is, and therefore how the badge is coloured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum TrustLevel {
    /// Something is there that can run code or reach the network.
    Warn,
    /// Checked, and nothing was found.
    Clean,
    /// Nothing was found, but not every signal could be checked.
    Partial,
}

/// How many hazards the badge names before it summarises the rest.
const BADGE_ITEMS: usize = 2;

/// The report the UI renders: a ready-made summary plus the detail behind it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct TrustReport {
    /// One short label per hazard present. Empty means none were found.
    pub hazards: Vec<String>,
    /// Signals this format does not let us check, listed so the UI never
    /// implies they came back clean.
    pub unchecked: Vec<String>,
    /// Badge text: the hazards (capped), or what was checked and came back
    /// empty. `hazards` always carries the full list.
    pub badge: String,
    pub level: TrustLevel,
    pub detail: Trust,
}

impl TrustReport {
    pub fn new(detail: Trust) -> Self {
        let hazards = detail.hazards();
        let unchecked = detail.unchecked();
        let level = if !hazards.is_empty() {
            TrustLevel::Warn
        } else if unchecked.is_empty() {
            TrustLevel::Clean
        } else {
            TrustLevel::Partial
        };
        // When nothing is wrong, say what was actually looked at rather than a
        // blanket "clean".
        let badge = if !hazards.is_empty() {
            // The badge shares a narrow status bar with the file name, so cap
            // the list; the tooltip and the dialog show all of it.
            if hazards.len() <= BADGE_ITEMS {
                hazards.join(", ")
            } else {
                format!(
                    "{}, +{} more",
                    hazards[..BADGE_ITEMS].join(", "),
                    hazards.len() - BADGE_ITEMS
                )
            }
        } else if detail.macros == Some(false) && detail.external_links == Some(0) {
            "No macros or links".to_string()
        } else if detail.macros == Some(false) {
            "No macros".to_string()
        } else if detail.external_links == Some(0) {
            "No external links".to_string()
        } else {
            "Not inspected".to_string()
        };
        Self {
            hazards,
            unchecked,
            badge,
            level,
            detail,
        }
    }
}

/// Inspect a workbook that calamine has already opened.
///
/// This never fails: if the package cannot be read the report says so, rather
/// than getting in the way of viewing the file.
pub fn inspect(path: &Path, book: &mut Sheets<BufReader<File>>) -> Trust {
    // An OOXML or ODF workbook is a zip package, so its parts can be listed
    // directly. Anything else — a legacy `.xls` is an OLE2 compound file — has
    // no member list, and calamine's VBA answer is all there is.
    match open_zip(path) {
        Some(zip) => inspect_zip(zip),
        None => Trust {
            macros: legacy_macros(book),
            ..Trust::unknown()
        },
    }
}

fn open_zip(path: &Path) -> Option<ZipArchive<BufReader<File>>> {
    let file = File::open(path).ok()?;
    ZipArchive::new(BufReader::new(file)).ok()
}

/// ODF packages always carry a manifest; OOXML never does.
fn inspect_zip<R: Read + Seek>(zip: ZipArchive<R>) -> Trust {
    let is_odf = zip.file_names().any(|name| name == "META-INF/manifest.xml");
    if is_odf {
        odf(zip)
    } else {
        ooxml(zip)
    }
}

fn ooxml<R: Read + Seek>(zip: ZipArchive<R>) -> Trust {
    let names: Vec<String> = zip.file_names().map(str::to_owned).collect();
    let has = |name: &str| names.iter().any(|n| n == name);

    Trust {
        macros: Some(
            has(OOXML_VBA_PROJECT)
                || has(OOXML_VBA_SIGNATURE)
                || names.iter().any(|n| n.starts_with(OOXML_MACRO_SHEETS)),
        ),
        external_links: Some(count_under(&names, OOXML_EXTERNAL_LINKS)),
        connections: Some(OOXML_CONNECTIONS.iter().any(|c| has(c))),
        embedded_objects: Some(count_under(&names, OOXML_EMBEDDINGS)),
        activex: Some(count_under(&names, OOXML_ACTIVEX) > 0),
    }
}

fn odf<R: Read + Seek>(mut zip: ZipArchive<R>) -> Trust {
    let names: Vec<String> = zip.file_names().map(str::to_owned).collect();
    let macros = Some(
        names
            .iter()
            .any(|n| ODF_MACRO_DIRS.iter().any(|dir| n.starts_with(dir))),
    );

    // The links and database ranges are element names inside content.xml.
    let (links, connections) = match zip.by_name("content.xml") {
        Ok(file) => match count_all(file, &[ODF_TABLE_SOURCE, ODF_DATABASE_RANGE]) {
            Ok(counts) => (Some(counts[0]), Some(counts[1] > 0)),
            Err(_) => (None, None),
        },
        Err(_) => (None, None),
    };

    Trust {
        macros,
        external_links: links,
        connections,
        // An ODF package has no ActiveX, and we do not guess at embedded
        // objects from directory names.
        embedded_objects: None,
        activex: None,
    }
}

/// Macros in a legacy `.xls`, which calamine reads as an OLE2 compound file.
fn legacy_macros(book: &mut Sheets<BufReader<File>>) -> Option<bool> {
    match book.vba_project() {
        Ok(Some(_)) => Some(true),
        // `vba_project` only fails after it has located the VBA storage, so an
        // error still means a macro project is there — it just would not parse.
        Err(_) => Some(true),
        Ok(None) => Some(false),
    }
}

/// Count the parts directly under `prefix`, ignoring their `.rels` sidecars so
/// a single part is not counted twice.
fn count_under(names: &[String], prefix: &str) -> usize {
    names
        .iter()
        .filter(|n| n.starts_with(prefix))
        .filter(|n| !n[prefix.len()..].starts_with("_rels/"))
        .count()
}

fn count_label(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

/// Count occurrences of each needle in a stream.
///
/// Only a window is buffered, because a real `content.xml` can be enormous and
/// only two element names are needed from it. A needle that straddles a read
/// boundary is counted exactly once: each needle restarts scanning far enough
/// back to catch a straddling match, and no further, so anything already
/// counted inside the carried-over bytes is skipped.
fn count_all<R: Read>(mut reader: R, needles: &[&[u8]]) -> std::io::Result<Vec<usize>> {
    let longest = needles.iter().map(|n| n.len()).max().unwrap_or(0);
    let mut counts = vec![0usize; needles.len()];
    if longest == 0 {
        return Ok(counts);
    }

    let mut buf = vec![0u8; 64 * 1024];
    let mut carry: Vec<u8> = Vec::new();
    loop {
        let read = reader.read(&mut buf)?;
        if read == 0 {
            return Ok(counts);
        }

        let already = carry.len();
        let mut window = std::mem::take(&mut carry);
        window.extend_from_slice(&buf[..read]);

        for (i, needle) in needles.iter().enumerate() {
            let from = (already + 1).saturating_sub(needle.len()).min(window.len());
            counts[i] += count_bytes(&window[from..], needle);
        }

        let keep = longest.saturating_sub(1).min(window.len());
        carry = window[window.len() - keep..].to_vec();
    }
}

fn count_bytes(haystack: &[u8], needle: &[u8]) -> usize {
    if needle.is_empty() || haystack.len() < needle.len() {
        return 0;
    }
    haystack
        .windows(needle.len())
        .filter(|window| *window == needle)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, Write};
    use zip::write::SimpleFileOptions;
    use zip::ZipWriter;

    const WORKBOOK_PARTS: [(&str, &str); 3] = [
        ("[Content_Types].xml", "<Types/>"),
        ("xl/workbook.xml", "<workbook/>"),
        ("xl/worksheets/sheet1.xml", "<sheetData/>"),
    ];

    fn zip_of(entries: &[(&str, &str)]) -> Vec<u8> {
        let mut writer = ZipWriter::new(Cursor::new(Vec::new()));
        for (name, body) in entries {
            writer
                .start_file(*name, SimpleFileOptions::default())
                .expect("start_file");
            writer.write_all(body.as_bytes()).expect("write_all");
        }
        writer.finish().expect("finish").into_inner()
    }

    fn ooxml_of(extra: &[(&str, &str)]) -> Trust {
        let mut entries = WORKBOOK_PARTS.to_vec();
        entries.extend_from_slice(extra);
        let bytes = zip_of(&entries);
        ooxml(ZipArchive::new(Cursor::new(bytes)).expect("archive"))
    }

    fn odf_of(content: &str, extra: &[(&str, &str)]) -> Trust {
        let mut entries = vec![
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
            ("META-INF/manifest.xml", "<manifest/>"),
            ("content.xml", content),
        ];
        entries.extend_from_slice(extra);
        let bytes = zip_of(&entries);
        odf(ZipArchive::new(Cursor::new(bytes)).expect("archive"))
    }

    #[test]
    fn a_plain_workbook_reports_every_signal_clean() {
        let trust = ooxml_of(&[]);
        assert_eq!(trust.macros, Some(false));
        assert_eq!(trust.external_links, Some(0));
        assert_eq!(trust.connections, Some(false));
        assert_eq!(trust.embedded_objects, Some(0));
        assert_eq!(trust.activex, Some(false));

        let report = TrustReport::new(trust);
        assert_eq!(report.level, TrustLevel::Clean);
        assert_eq!(report.badge, "No macros or links");
        assert!(report.hazards.is_empty());
        assert!(report.unchecked.is_empty());
    }

    #[test]
    fn a_vba_project_is_reported_as_macros() {
        let trust = ooxml_of(&[(OOXML_VBA_PROJECT, "not really a project")]);
        assert_eq!(trust.macros, Some(true));

        let report = TrustReport::new(trust);
        assert_eq!(report.level, TrustLevel::Warn);
        assert_eq!(report.badge, "Macros");
        assert_eq!(report.hazards, vec!["Macros"]);
    }

    #[test]
    fn a_signature_alone_still_means_a_vba_project() {
        assert_eq!(ooxml_of(&[(OOXML_VBA_SIGNATURE, "sig")]).macros, Some(true));
    }

    #[test]
    fn excel_4_macro_sheets_count_as_macros() {
        assert_eq!(
            ooxml_of(&[("xl/macrosheets/sheet1.xml", "<macrosheet/>")]).macros,
            Some(true)
        );
    }

    #[test]
    fn external_links_are_counted_without_their_rels_sidecars() {
        let trust = ooxml_of(&[
            ("xl/externalLinks/externalLink1.xml", "<externalLink/>"),
            (
                "xl/externalLinks/_rels/externalLink1.xml.rels",
                "<Relationships/>",
            ),
            ("xl/externalLinks/externalLink2.xml", "<externalLink/>"),
            (
                "xl/externalLinks/_rels/externalLink2.xml.rels",
                "<Relationships/>",
            ),
        ]);
        assert_eq!(trust.external_links, Some(2));
        assert_eq!(TrustReport::new(trust).badge, "2 external links");
    }

    #[test]
    fn a_single_external_link_reads_in_the_singular() {
        let trust = ooxml_of(&[("xl/externalLinks/externalLink1.xml", "<externalLink/>")]);
        assert_eq!(TrustReport::new(trust).badge, "1 external link");
    }

    #[test]
    fn connections_are_reported_in_either_part_form() {
        assert_eq!(
            ooxml_of(&[(OOXML_CONNECTIONS[0], "<connections/>")]).connections,
            Some(true)
        );
        assert_eq!(
            ooxml_of(&[(OOXML_CONNECTIONS[1], "\u{0}\u{1}")]).connections,
            Some(true)
        );
        assert_eq!(ooxml_of(&[]).connections, Some(false));
    }

    #[test]
    fn embedded_objects_and_activex_are_detected() {
        let trust = ooxml_of(&[
            ("xl/embeddings/oleObject1.bin", "\u{0}"),
            ("xl/embeddings/oleObject2.bin", "\u{0}"),
            ("xl/activeX/activeX1.xml", "<ax/>"),
            ("xl/activeX/_rels/activeX1.xml.rels", "<Relationships/>"),
        ]);
        assert_eq!(trust.embedded_objects, Some(2));
        assert_eq!(trust.activex, Some(true));
        assert_eq!(
            TrustReport::new(trust).badge,
            "2 embedded objects, ActiveX controls"
        );
    }

    #[test]
    fn a_short_list_of_hazards_is_not_capped() {
        let trust = ooxml_of(&[
            (OOXML_VBA_PROJECT, "x"),
            ("xl/embeddings/oleObject1.bin", "x"),
        ]);
        assert_eq!(TrustReport::new(trust).badge, "Macros, 1 embedded object");
    }

    #[test]
    fn every_hazard_is_named_in_one_badge() {
        let trust = ooxml_of(&[
            (OOXML_VBA_PROJECT, "x"),
            ("xl/externalLinks/externalLink1.xml", "x"),
            ("xl/externalLinks/externalLink2.xml", "x"),
            (OOXML_CONNECTIONS[0], "x"),
            ("xl/embeddings/oleObject1.bin", "x"),
            ("xl/activeX/activeX1.xml", "x"),
        ]);
        let report = TrustReport::new(trust);
        assert_eq!(report.level, TrustLevel::Warn);
        assert_eq!(
            report.hazards,
            vec![
                "Macros",
                "2 external links",
                "Data connections",
                "1 embedded object",
                "ActiveX controls",
            ]
        );
        // The badge is capped so it does not crowd the status bar, but every
        // hazard is still named in the list the dialog shows.
        assert_eq!(report.badge, "Macros, 2 external links, +3 more");
        assert!(report.unchecked.is_empty());
    }

    #[test]
    fn the_odf_manifest_selects_the_odf_rules() {
        let bytes = zip_of(&[
            ("mimetype", "application/vnd.oasis.opendocument.spreadsheet"),
            ("META-INF/manifest.xml", "<manifest/>"),
            ("content.xml", "<office:document-content/>"),
            ("Basic/Standard/Module1.xml", "<script/>"),
        ]);
        let trust = inspect_zip(ZipArchive::new(Cursor::new(bytes)).expect("archive"));
        // Only the ODF rules look in Basic/, so this shows the dispatch followed
        // the manifest rather than anything about the file's name.
        assert_eq!(trust.macros, Some(true));
        assert_eq!(trust.embedded_objects, None);
    }

    #[test]
    fn a_zip_without_a_manifest_is_treated_as_ooxml() {
        // The dispatcher keys off the ODF manifest, not the file extension.
        let bytes = zip_of(&WORKBOOK_PARTS);
        let trust = inspect_zip(ZipArchive::new(Cursor::new(bytes)).expect("archive"));
        assert_eq!(trust.macros, Some(false));
        assert_eq!(trust.external_links, Some(0));
    }

    #[test]
    fn odf_macros_are_found_in_the_basic_and_script_directories() {
        let basic = odf_of(
            "<office:document-content/>",
            &[("Basic/Standard/Module1.xml", "<script/>")],
        );
        assert_eq!(basic.macros, Some(true));
        assert_eq!(TrustReport::new(basic).badge, "Macros");

        let scripts = odf_of(
            "<office:document-content/>",
            &[("Scripts/javascript/script.js", "x")],
        );
        assert_eq!(scripts.macros, Some(true));
        assert_eq!(
            odf_of("<office:document-content/>", &[]).macros,
            Some(false)
        );
    }

    #[test]
    fn odf_links_and_database_ranges_come_from_content_xml() {
        let trust = odf_of(
            r#"<table:table-source xlink:href="file:///other.ods"/>
               <table:table-source xlink:href="file:///third.ods"/>
               <table:database-range/>"#,
            &[],
        );
        assert_eq!(trust.external_links, Some(2));
        assert_eq!(trust.connections, Some(true));
        assert_eq!(
            TrustReport::new(trust).badge,
            "2 external links, Data connections"
        );
    }

    #[test]
    fn odf_reports_the_signals_it_cannot_check_as_unchecked() {
        let trust = odf_of("<office:document-content/>", &[]);
        assert_eq!(trust.embedded_objects, None);
        assert_eq!(trust.activex, None);

        let report = TrustReport::new(trust);
        // Nothing was found, but two signals went unexamined, so the badge must
        // not read as a clean bill of health.
        assert_eq!(report.level, TrustLevel::Partial);
        assert_eq!(report.badge, "No macros or links");
        assert_eq!(
            report.unchecked,
            vec!["embedded objects", "ActiveX controls"]
        );
    }

    #[test]
    fn a_legacy_compound_file_leaves_everything_but_macros_unchecked() {
        let trust = Trust {
            macros: Some(false),
            ..Trust::unknown()
        };
        let report = TrustReport::new(trust);
        assert_eq!(report.level, TrustLevel::Partial);
        assert_eq!(report.badge, "No macros");
        assert_eq!(
            report.unchecked,
            vec![
                "external links",
                "data connections",
                "embedded objects",
                "ActiveX controls"
            ]
        );
    }

    #[test]
    fn plain_text_reports_everything_clean() {
        // A `.csv` is not a package: nothing in it can carry these signals.
        let report = TrustReport::new(Trust::plain_text());
        assert_eq!(report.level, TrustLevel::Clean);
        assert_eq!(report.badge, "No macros or links");
        assert!(report.hazards.is_empty());
        assert!(report.unchecked.is_empty());
    }

    #[test]
    fn a_package_that_could_not_be_read_says_so() {
        let report = TrustReport::new(Trust::unknown());
        assert_eq!(report.level, TrustLevel::Partial);
        assert_eq!(report.badge, "Not inspected");
        assert_eq!(report.unchecked.len(), 5);
        assert!(report.hazards.is_empty());
    }

    #[test]
    fn a_needle_straddling_a_read_boundary_is_counted_exactly_once() {
        let needle = b"<table:table-source";
        for offset in [
            0,
            64 * 1024 - 5,
            64 * 1024 - 1,
            64 * 1024,
            2 * 64 * 1024 - 3,
        ] {
            let mut body = vec![b'a'; offset];
            body.extend_from_slice(needle);
            body.extend_from_slice(b"tail");
            let counts = count_all(Cursor::new(body), &[needle]).expect("count");
            assert_eq!(counts, vec![1], "needle at offset {offset}");
        }
    }

    #[test]
    fn needles_of_different_lengths_do_not_double_count() {
        let short = b"<table:table-source";
        let long = b"<table:database-range";
        for offset in [64 * 1024 - 3, 64 * 1024 + 7, 3 * 64 * 1024] {
            let mut body = vec![b' '; offset];
            body.extend_from_slice(short);
            body.extend_from_slice(long);
            let counts = count_all(Cursor::new(body), &[short, long]).expect("count");
            assert_eq!(counts, vec![1, 1], "needles at offset {offset}");
        }
    }

    #[test]
    fn every_occurrence_is_counted() {
        let body = b"<table:table-source a <table:table-source b <table:table-source".to_vec();
        let counts = count_all(Cursor::new(body), &[ODF_TABLE_SOURCE]).expect("count");
        assert_eq!(counts, vec![3]);
    }

    #[test]
    fn counting_an_empty_stream_is_not_an_error() {
        let counts = count_all(Cursor::new(Vec::new()), &[ODF_TABLE_SOURCE]).expect("count");
        assert_eq!(counts, vec![0]);
    }
}
