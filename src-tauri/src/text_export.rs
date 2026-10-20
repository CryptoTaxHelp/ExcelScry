//! Writing a UTF-8 copy of a delimited file read in another encoding.
//!
//! This is a byte-for-byte conversion of the file's text, not a re-export of
//! the parsed grid: quotes, delimiters and line endings stay exactly as the
//! source wrote them, while the characters become UTF-8. The original file is
//! never touched.

use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::text_table::{self, Encoding};

/// What the UI is told after a UTF-8 copy is written.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Utf8CopySummary {
    pub path: String,
    pub bytes: u64,
}

/// Write `source`, decoded with `encoding`, to `destination` as UTF-8.
///
/// The text goes through the same decoder the grid used, so a Windows-1252 or
/// UTF-16 file becomes the characters that were shown. No byte order mark is
/// written: this is UTF-8, not a particular tool's flavour of it.
pub fn write_utf8_copy(
    source: &Path,
    encoding: Encoding,
    destination: &Path,
) -> Result<Utf8CopySummary, String> {
    if encoding == Encoding::Utf8 {
        return Err("This file was already read as UTF-8.".to_string());
    }
    if same_file(source, destination) {
        return Err(
            "Choose a different name: this saves a copy and leaves the original alone.".to_string(),
        );
    }

    let bytes = std::fs::read(source)
        .map_err(|error| format!("Cannot read \"{}\": {error}", source.display()))?;
    let text = text_table::decode_bytes(&bytes, encoding);
    let utf8 = text.as_bytes();
    std::fs::write(destination, utf8)
        .map_err(|error| format!("Cannot write \"{}\": {error}", destination.display()))?;

    Ok(Utf8CopySummary {
        path: destination.display().to_string(),
        bytes: utf8.len() as u64,
    })
}

/// Whether two paths name the same file, including through another spelling.
fn same_file(first: &Path, second: &Path) -> bool {
    normalized(first) == normalized(second)
}

fn normalized(path: &Path) -> String {
    let resolved: PathBuf = std::fs::canonicalize(path)
        .or_else(|_| std::path::absolute(path))
        .unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy().into_owned();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// The encoding a Western file that is not UTF-8 is in.
    const WINDOWS_1252: Encoding = Encoding::Legacy(encoding_rs::WINDOWS_1252);

    fn directory(name: &str) -> PathBuf {
        let dir = std::env::temp_dir()
            .join("excelscry-text-export")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory for the copy");
        dir
    }

    fn file(directory: &Path, name: &str) -> PathBuf {
        directory.join(name)
    }

    fn write(path: &Path, bytes: &[u8]) {
        let mut file = std::fs::File::create(path).expect("a fixture");
        file.write_all(bytes).expect("contents");
    }

    #[test]
    fn windows_1252_becomes_the_utf8_the_grid_showed() {
        let dir = directory("windows-1252");
        let source = file(&dir, "names.csv");
        let target = file(&dir, "names-utf8.csv");
        write(&source, b"name,caf\xe9\r\nAda,na\xefve\r\n");

        let summary = write_utf8_copy(&source, WINDOWS_1252, &target).expect("copy");

        assert_eq!(
            std::fs::read(&target).unwrap(),
            "name,café\r\nAda,naïve\r\n".as_bytes()
        );
        assert_eq!(summary.bytes, std::fs::metadata(&target).unwrap().len());
        assert_eq!(
            std::fs::read(&source).unwrap(),
            b"name,caf\xe9\r\nAda,na\xefve\r\n",
            "the original is untouched"
        );
    }

    #[test]
    fn a_utf16_file_becomes_utf8_without_its_bom() {
        let dir = directory("utf16");
        let source = file(&dir, "report.tsv");
        let target = file(&dir, "report-utf8.tsv");
        let mut bytes = vec![0xFF, 0xFE];
        bytes.extend("name\tcafé\r\n".encode_utf16().flat_map(u16::to_le_bytes));
        write(&source, &bytes);

        write_utf8_copy(&source, Encoding::Utf16Le, &target).expect("copy");

        assert_eq!(std::fs::read(&target).unwrap(), "name\tcafé\r\n".as_bytes());
    }

    #[test]
    fn quoting_and_line_endings_survive_the_conversion() {
        let dir = directory("shape");
        let source = file(&dir, "quoted.csv");
        let target = file(&dir, "quoted-utf8.csv");
        write(&source, b"\"a;b\",\"say \"\"hi\"\"\"\r\ncaf\xe9,d\r\n");

        write_utf8_copy(&source, WINDOWS_1252, &target).expect("copy");

        assert_eq!(
            std::fs::read(&target).unwrap(),
            "\"a;b\",\"say \"\"hi\"\"\"\r\ncafé,d\r\n".as_bytes()
        );
    }

    #[test]
    fn a_file_already_read_as_utf8_is_refused() {
        let dir = directory("already-utf8");
        let source = file(&dir, "plain.csv");
        let target = file(&dir, "plain-utf8.csv");
        write(&source, b"a,b\n");

        let error = write_utf8_copy(&source, Encoding::Utf8, &target).unwrap_err();

        assert!(error.contains("already read as UTF-8"), "{error}");
        assert!(!target.exists());
    }

    #[test]
    fn the_original_cannot_be_the_destination() {
        let dir = directory("same-file");
        let source = file(&dir, "plain.csv");
        write(&source, b"caf\xe9\n");

        let error = write_utf8_copy(&source, WINDOWS_1252, &source).unwrap_err();

        assert!(error.contains("different name"), "{error}");
        assert_eq!(std::fs::read(&source).unwrap(), b"caf\xe9\n");
    }
}
