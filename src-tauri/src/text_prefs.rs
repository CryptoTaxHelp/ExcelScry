//! Remembering how each delimited file was last read.
//!
//! A `.csv` is bytes plus a convention, and the convention a particular file
//! uses is worth keeping: an export that is semicolon-separated Windows-1252
//! every week should not have to be fixed every week. The file's path, made
//! absolute and canonical where the filesystem allows, is the key; what is
//! stored is only what was chosen by hand, never what detection worked out, so
//! a file with nothing to remember keeps being read by its own bytes.
//!
//! This is a convenience, not a record the reader depends on: a store that
//! cannot be read or written is ignored, and the file opens with detection as
//! it would have without one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

use crate::text_table::{Delimiter, Dialect, Encoding};

/// Where the choices live, inside this build's per-user data directory.
const STORE_FILE: &str = "text-dialects.json";

/// One writer at a time: a read-modify-write from two opens at once would
/// otherwise lose one of them.
static LOCK: Mutex<()> = Mutex::new(());

/// The choices remembered for one file. Both are optional because the user may
/// have fixed only the delimiter, leaving the encoding to detection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delimiter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
}

impl Entry {
    /// The entry that records `dialect`, as labels the store can read back.
    pub fn from_dialect(dialect: Dialect) -> Self {
        Entry {
            delimiter: dialect.delimiter.map(|chosen| chosen.label().to_string()),
            encoding: dialect.encoding.map(|chosen| chosen.label().to_string()),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.delimiter.is_none() && self.encoding.is_none()
    }

    fn delimiter(&self) -> Option<Delimiter> {
        self.delimiter.as_deref().and_then(Delimiter::from_label)
    }

    fn encoding(&self) -> Option<Encoding> {
        self.encoding.as_deref().and_then(Encoding::from_label)
    }
}

/// The dialect a file should be read with.
///
/// A first open has nothing to ask for, so the store fills in the parts that
/// would otherwise be detection. A choice made in the bar is complete on its
/// own: choosing *Auto* for one part has to take effect now, not be undone by
/// what was remembered before it, so an explicit request uses exactly what it
/// names — and it is exactly what gets stored.
pub fn resolve(requested: Dialect, remembered: &Entry, explicit: bool) -> Dialect {
    if explicit {
        return requested;
    }
    Dialect {
        delimiter: requested.delimiter.or_else(|| remembered.delimiter()),
        encoding: requested.encoding.or_else(|| remembered.encoding()),
    }
}

/// What was remembered for `path`, or nothing.
pub fn lookup(directory: &Path, path: &Path) -> Entry {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    read(directory).files.remove(&key(path)).unwrap_or_default()
}

/// Remember `entry` for `path`, or forget it when the entry says nothing.
pub fn remember(directory: &Path, path: &Path, entry: Entry) {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = read(directory);
    let key = key(path);
    if entry.is_empty() {
        store.files.remove(&key);
    } else {
        store.files.insert(key, entry);
    }
    // A choice that cannot be written still applies to this session; failing to
    // open the file over it would be worse than forgetting it next time.
    let _ = write(directory, &store);
}

/// The key a file is remembered under.
///
/// Canonical where the filesystem can say, so the same file opened through
/// another spelling, a relative path or a symlink is the same file. Windows
/// paths are folded to one case because its filesystems are not case-sensitive.
fn key(path: &Path) -> String {
    let resolved: PathBuf = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let text = resolved.to_string_lossy().into_owned();
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    files: BTreeMap<String, Entry>,
}

/// Read the store, treating anything unreadable as empty.
fn read(directory: &Path) -> Store {
    std::fs::read_to_string(directory.join(STORE_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write the store through a temporary file and a rename, so a write that does
/// not finish leaves the previous choices rather than half of a new set.
fn write(directory: &Path, store: &Store) -> Result<(), String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let text = serde_json::to_string_pretty(store)
        .map_err(|error| format!("could not write up the dialect choices: {error}"))?;
    let path = directory.join(STORE_FILE);
    let temporary = path.with_extension("new");
    std::fs::write(&temporary, format!("{text}\n"))
        .map_err(|error| format!("could not write {}: {error}", temporary.display()))?;
    std::fs::rename(&temporary, &path)
        .map_err(|error| format!("could not replace {}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn directory(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("excelscry-text-prefs").join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory for the store");
        dir
    }

    fn file(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        let mut file = std::fs::File::create(&path).expect("a fixture");
        file.write_all(b"a;b\n").expect("contents");
        path
    }

    fn chosen(delimiter: Option<&str>, encoding: Option<&str>) -> Entry {
        Entry {
            delimiter: delimiter.map(str::to_string),
            encoding: encoding.map(str::to_string),
        }
    }

    #[test]
    fn a_choice_is_remembered_for_the_file_it_was_made_for() {
        let dir = directory("remembered");
        let sales = file(&dir, "sales.csv");
        let costs = file(&dir, "costs.csv");

        remember(
            &dir,
            &sales,
            chosen(Some("semicolon"), Some("Windows-1252")),
        );

        assert_eq!(
            lookup(&dir, &sales),
            chosen(Some("semicolon"), Some("Windows-1252"))
        );
        assert_eq!(lookup(&dir, &costs), Entry::default());
    }

    #[test]
    fn the_choice_survives_being_written_and_read_again() {
        let dir = directory("round-trip");
        let path = file(&dir, "report.csv");
        remember(&dir, &path, chosen(Some("pipe"), Some("UTF-16 LE")));

        // A fresh process would read the file, not a cached map.
        let store = read(&dir);
        let entry = store.files.get(&key(&path)).expect("the entry");
        assert_eq!(entry.delimiter(), Some(Delimiter::Pipe));
        assert_eq!(entry.encoding(), Some(Encoding::Utf16Le));
    }

    #[test]
    fn forgetting_leaves_no_entry_behind() {
        let dir = directory("forgotten");
        let path = file(&dir, "data.csv");
        remember(&dir, &path, chosen(Some("tab"), None));
        assert!(!lookup(&dir, &path).is_empty());

        remember(&dir, &path, Entry::default());

        assert_eq!(lookup(&dir, &path), Entry::default());
        assert!(read(&dir).files.is_empty());
    }

    #[test]
    fn the_last_choice_for_a_file_replaces_the_earlier_one() {
        let dir = directory("replaced");
        let path = file(&dir, "data.csv");
        remember(&dir, &path, chosen(Some("semicolon"), None));
        remember(&dir, &path, chosen(None, Some("windows-1252")));

        assert_eq!(lookup(&dir, &path), chosen(None, Some("windows-1252")));
    }

    #[test]
    fn a_damaged_store_is_ignored_rather_than_breaking_an_open() {
        let dir = directory("damaged");
        let path = file(&dir, "data.csv");
        std::fs::write(dir.join(STORE_FILE), b"{ not json").unwrap();

        assert_eq!(lookup(&dir, &path), Entry::default());

        // The next choice replaces the damaged file.
        remember(&dir, &path, chosen(Some("semicolon"), None));
        assert_eq!(lookup(&dir, &path).delimiter(), Some(Delimiter::Semicolon));
    }

    #[test]
    fn an_entry_that_names_an_unknown_value_is_ignored_for_that_part() {
        let dir = directory("unknown");
        let path = file(&dir, "data.csv");
        remember(&dir, &path, chosen(Some("tilde"), Some("ebcdic")));

        let entry = lookup(&dir, &path);
        assert_eq!(entry.delimiter(), None);
        assert_eq!(entry.encoding(), None);
        assert!(!entry.is_empty(), "the strings are still recorded");
    }

    #[test]
    fn a_first_open_takes_what_was_remembered() {
        let remembered = chosen(Some("semicolon"), Some("Windows-1252"));

        let auto = resolve(Dialect::default(), &remembered, false);
        assert_eq!(auto.delimiter, Some(Delimiter::Semicolon));
        assert_eq!(auto.encoding, Some(Encoding::Windows1252));

        let partial = resolve(
            Dialect::from_labels(None, Some("utf-8")).unwrap(),
            &remembered,
            false,
        );
        assert_eq!(partial.delimiter, Some(Delimiter::Semicolon));
        assert_eq!(partial.encoding, Some(Encoding::Utf8));
    }

    #[test]
    fn an_explicit_choice_uses_exactly_what_it_names() {
        let remembered = chosen(Some("semicolon"), Some("Windows-1252"));

        // Choosing Auto for one part takes effect now, rather than being filled
        // back in from the store.
        let auto_delimiter = resolve(
            Dialect::from_labels(None, Some("utf-8")).unwrap(),
            &remembered,
            true,
        );
        assert_eq!(auto_delimiter.delimiter, None);
        assert_eq!(auto_delimiter.encoding, Some(Encoding::Utf8));

        let auto_both = resolve(Dialect::default(), &remembered, true);
        assert_eq!(auto_both.delimiter, None);
        assert_eq!(auto_both.encoding, None);
    }

    #[test]
    fn nonzero_remembered_parts_do_not_leak_into_an_explicit_choice() {
        let remembered = chosen(Some("pipe"), None);
        let chosen_now = resolve(
            Dialect::from_labels(Some("tab"), None).unwrap(),
            &remembered,
            true,
        );
        assert_eq!(chosen_now.delimiter, Some(Delimiter::Tab));
        assert_eq!(chosen_now.encoding, None);
    }

    #[test]
    fn a_dialect_is_stored_under_the_names_the_bar_shows() {
        let entry =
            Entry::from_dialect(Dialect::from_labels(Some("semicolon"), Some("utf-16be")).unwrap());
        assert_eq!(entry, chosen(Some("semicolon"), Some("UTF-16 BE")));
        assert_eq!(entry.delimiter(), Some(Delimiter::Semicolon));
        assert_eq!(entry.encoding(), Some(Encoding::Utf16Be));
    }

    #[test]
    fn an_empty_store_and_an_empty_request_change_nothing() {
        let dialect = resolve(Dialect::default(), &Entry::default(), false);
        assert_eq!(dialect.delimiter, None);
        assert_eq!(dialect.encoding, None);
    }

    #[test]
    fn the_same_file_reached_by_another_spelling_is_the_same_key() {
        let dir = directory("key");
        let path = file(&dir, "data.csv");
        let dotted = dir.join(".").join("data.csv");

        remember(&dir, &path, chosen(Some("semicolon"), None));

        assert_eq!(
            lookup(&dir, &dotted).delimiter(),
            Some(Delimiter::Semicolon)
        );
    }
}
