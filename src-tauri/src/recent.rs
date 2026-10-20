//! Remembering the files that were opened recently.
//!
//! The welcome screen is useful only if the file someone worked in yesterday
//! is one click away. Paths are stored newest first, keyed the way the text
//! dialect memory keys them, so another spelling of the same file does not
//! appear twice. An entry whose file is gone is dropped when the list is read:
//! the list is a shortcut to a file, not a record that it still exists.
//!
//! Like the dialect store, this is a convenience. An unreadable store is an
//! empty list, and failing to write one does not fail an open.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use serde::{Deserialize, Serialize};

/// Where the paths live, inside this build's per-user data directory.
const STORE_FILE: &str = "recent-files.json";

/// Enough to cover the last few sessions without turning the welcome screen
/// into a file browser.
const LIMIT: usize = 8;

/// One writer at a time, so two opens cannot lose one another's entries.
static LOCK: Mutex<()> = Mutex::new(());

/// One path the welcome screen can offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RecentFile {
    pub path: String,
    pub name: String,
}

/// The files worth offering, newest first.
///
/// Missing files, duplicates and entries with no readable name are left out;
/// the store is rewritten when any of those were there.
pub fn list(directory: &Path) -> Vec<RecentFile> {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = read(directory);
    let before = store.files.len();
    let mut seen = HashSet::new();
    store.files.retain(|path| {
        let path = Path::new(path);
        path.is_file() && seen.insert(key(path))
    });
    store.files.truncate(LIMIT);
    if store.files.len() != before {
        let _ = write(directory, &store);
    }
    store
        .files
        .iter()
        .map(|path| RecentFile {
            name: name(Path::new(path)),
            path: path.clone(),
        })
        .collect()
}

/// Put `path` at the front of the list, dropping whatever is no longer there.
pub fn remember(directory: &Path, path: &Path) {
    if !path.is_file() {
        return;
    }
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = read(directory);
    let opened = key(path);
    store.files.retain(|existing| {
        let existing = Path::new(existing);
        existing.is_file() && key(existing) != opened
    });
    store.files.insert(0, display(path));
    store.files.truncate(LIMIT);
    // A list that cannot be written still serves this session; failing to open
    // the file over it would be worse than forgetting the path next time.
    let _ = write(directory, &store);
}

/// The path as the welcome screen should show and reopen it.
fn display(path: &Path) -> String {
    std::path::absolute(path)
        .unwrap_or_else(|_| path.to_path_buf())
        .to_string_lossy()
        .into_owned()
}

/// The name under which a path is compared with another spelling of itself.
fn key(path: &Path) -> String {
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

fn name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| path.to_string_lossy().into_owned())
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    files: Vec<String>,
}

/// Read the store, treating anything unreadable as empty.
fn read(directory: &Path) -> Store {
    std::fs::read_to_string(directory.join(STORE_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write through a temporary file and a rename, so a write that does not
/// finish leaves the previous list rather than half of a new one.
fn write(directory: &Path, store: &Store) -> Result<(), String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let text = serde_json::to_string_pretty(store)
        .map_err(|error| format!("could not write up the recent files: {error}"))?;
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
        let dir = std::env::temp_dir()
            .join("excelscry-recent-files")
            .join(name);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory for the store");
        dir
    }

    fn file(directory: &Path, name: &str) -> PathBuf {
        let path = directory.join(name);
        let mut file = std::fs::File::create(&path).expect("a fixture");
        file.write_all(b"a,b\n").expect("contents");
        path
    }

    fn names(files: &[RecentFile]) -> Vec<&str> {
        files.iter().map(|file| file.name.as_str()).collect()
    }

    #[test]
    fn the_newest_open_comes_first_and_reopening_moves_it_up() {
        let dir = directory("order");
        let sales = file(&dir, "sales.xlsx");
        let costs = file(&dir, "costs.csv");

        remember(&dir, &sales);
        remember(&dir, &costs);
        remember(&dir, &sales);

        let listed = list(&dir);
        assert_eq!(names(&listed), ["sales.xlsx", "costs.csv"]);
        assert_eq!(
            Path::new(&listed[0].path),
            std::path::absolute(&sales).unwrap()
        );
    }

    #[test]
    fn a_file_that_is_gone_is_not_offered_or_kept() {
        let dir = directory("gone");
        let present = file(&dir, "present.xlsx");
        let gone = file(&dir, "gone.xlsx");
        remember(&dir, &present);
        remember(&dir, &gone);

        std::fs::remove_file(&gone).expect("remove the fixture");

        assert_eq!(names(&list(&dir)), ["present.xlsx"]);
        assert_eq!(read(&dir).files.len(), 1);
    }

    #[test]
    fn the_list_is_capped_and_keeps_the_newest() {
        let dir = directory("capped");
        for index in 0..(LIMIT + 2) {
            let path = file(&dir, &format!("file-{index}.xlsx"));
            remember(&dir, &path);
        }

        let listed = list(&dir);
        assert_eq!(listed.len(), LIMIT);
        assert_eq!(listed[0].name, format!("file-{}.xlsx", LIMIT + 1));
        assert!(
            listed
                .iter()
                .all(|file| file.name != "file-0.xlsx" && file.name != "file-1.xlsx"),
            "the oldest entries should have fallen off"
        );
    }

    #[test]
    fn another_spelling_of_the_same_file_is_one_entry() {
        let dir = directory("same-file");
        let path = file(&dir, "report.xlsx");
        let dotted = dir.join(".").join("report.xlsx");

        remember(&dir, &path);
        remember(&dir, &dotted);

        assert_eq!(names(&list(&dir)), ["report.xlsx"]);
    }

    #[test]
    fn a_damaged_store_is_an_empty_list_rather_than_a_failed_open() {
        let dir = directory("damaged");
        let path = file(&dir, "report.xlsx");
        std::fs::write(dir.join(STORE_FILE), b"{ not json").unwrap();

        assert!(list(&dir).is_empty());

        // The next open replaces the damaged file.
        remember(&dir, &path);
        assert_eq!(names(&list(&dir)), ["report.xlsx"]);
    }

    #[test]
    fn a_path_that_is_not_a_file_is_not_remembered() {
        let dir = directory("missing");
        let path = dir.join("never-existed.xlsx");

        remember(&dir, &path);

        assert!(list(&dir).is_empty());
        assert!(read(&dir).files.is_empty());
    }
}
