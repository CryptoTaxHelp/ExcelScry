//! Remembering where each file was left.
//!
//! A workbook is rarely read from its first row: the reason to open it again is
//! usually to carry on where the last look ended. The sheet, the selected cell
//! and the scroll offset are kept per file, keyed the way the dialect memory
//! keys them — the absolute path, folded to one case on Windows — and an entry
//! records the file's length and modification time, so a different file put at
//! the same path starts at its own top-left instead of inheriting a position
//! that belonged to another.
//!
//! The column widths the reader dragged belong to the same memory: widening a
//! column is work done to read the file, and it is the same work next time. They
//! are stored per sheet inside the entry, so switching tabs gives each tab back
//! its own widths rather than lending one sheet's shape to another.
//!
//! This is a convenience, not a record the reader depends on: a store that
//! cannot be read or written is ignored, and the file opens at A1 as it would
//! have without one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

/// Where the positions live, inside this build's per-user data directory.
const STORE_FILE: &str = "views.json";

/// One writer at a time: a read-modify-write from two windows at once would
/// otherwise lose one of them.
static LOCK: Mutex<()> = Mutex::new(());

/// The columns a reader dragged on one sheet, in column order.
///
/// Column widths are a sheet's own: a tab with wide description columns is not
/// a tab with narrow dates, so what was dragged on one is put back only there.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SheetWidths {
    pub sheet: usize,
    pub columns: Vec<(u32, f64)>,
}

/// Where the grid was in one file.
///
/// Rows and columns are the grid's own, counted from zero as it counts them;
/// `top` and `left` are the scroll offsets in CSS pixels, which is the unit the
/// body scrolls by. `widths` holds the columns the reader dragged, per sheet.
#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ViewState {
    pub sheet: usize,
    pub row: u32,
    pub col: u32,
    pub top: f64,
    pub left: f64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub widths: Vec<SheetWidths>,
}

/// What a file looked like when its position was remembered.
///
/// Length and modification time are enough to notice an edit, or a different
/// file put at the same path, without reading the file a second time just to
/// open it. A missing or unreadable file has no stamp, so its entry is dropped.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct Stamp {
    len: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    modified: Option<u64>,
}

impl Stamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Stamp {
            len: metadata.len(),
            modified: metadata
                .modified()
                .ok()
                .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
                .and_then(|elapsed| u64::try_from(elapsed.as_nanos()).ok()),
        })
    }

    fn matches(&self, path: &Path) -> bool {
        Stamp::of(path).as_ref() == Some(self)
    }
}

/// A position and the file state it was recorded against.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    #[serde(flatten)]
    view: ViewState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stamp: Option<Stamp>,
}

/// Where `path` was left, or nothing.
///
/// An entry whose file has changed since it was written is removed rather than
/// returned: a new export at the same path should open at its own top, not at a
/// row that meant something in the file that used to be there.
pub fn lookup(directory: &Path, path: &Path) -> Option<ViewState> {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = read(directory);
    let key = key(path);
    let current = store.files.get(&key).is_some_and(|stored| {
        stored
            .stamp
            .as_ref()
            .is_some_and(|stamp| stamp.matches(path))
    });
    if !current {
        if store.files.remove(&key).is_some() {
            // A stale entry should not linger in the store; if this write
            // fails it will simply be dropped again on the next open.
            let _ = write(directory, &store);
        }
        return None;
    }
    store.files.get(&key).map(|stored| stored.view.clone())
}

/// Remember where `path` was left.
///
/// A position that cannot be written still applies to this session; failing to
/// open the file over it would be worse than forgetting it next time.
pub fn remember(directory: &Path, path: &Path, view: ViewState) {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = read(directory);
    let key = key(path);
    match Stamp::of(path) {
        Some(stamp) => {
            store.files.insert(
                key,
                Stored {
                    view,
                    stamp: Some(stamp),
                },
            );
        }
        // Without a stamp this could never be shown to still belong to the
        // file, so storing it would only leave an entry lookup has to drop.
        None => {
            store.files.remove(&key);
        }
    }
    let _ = write(directory, &store);
}

/// The key a file is remembered under.
///
/// Canonical where the filesystem can say, so the same file opened through
/// another spelling, a relative path or a symlink is the same file. Windows
/// paths are folded to one case because its filesystems are not case-sensitive.
fn key(path: &Path) -> String {
    // The file itself may be gone by the time an entry is looked up. Its parent
    // can still be canonical, so a deleted file finds the entry it left rather
    // than keeping it forever under a spelling the next lookup cannot reach.
    let resolved: PathBuf = std::fs::canonicalize(path).unwrap_or_else(|_| {
        let parent = match path.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        match (std::fs::canonicalize(parent), path.file_name()) {
            (Ok(parent), Some(name)) => parent.join(name),
            _ => path.to_path_buf(),
        }
    });
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
    files: BTreeMap<String, Stored>,
}

/// Read the store, treating anything unreadable as empty.
fn read(directory: &Path) -> Store {
    std::fs::read_to_string(directory.join(STORE_FILE))
        .ok()
        .and_then(|text| serde_json::from_str(&text).ok())
        .unwrap_or_default()
}

/// Write the store through a temporary file and a rename, so a write that does
/// not finish leaves the previous positions rather than half of a new set.
fn write(directory: &Path, store: &Store) -> Result<(), String> {
    std::fs::create_dir_all(directory)
        .map_err(|error| format!("could not create {}: {error}", directory.display()))?;
    let text = serde_json::to_string_pretty(store)
        .map_err(|error| format!("could not write up the view positions: {error}"))?;
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
    use std::time::{Duration, SystemTime};

    fn directory(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join("excelscry-views").join(name);
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

    fn view(row: u32, col: u32) -> ViewState {
        ViewState {
            sheet: 1,
            row,
            col,
            top: 1820.0,
            left: 96.0,
            widths: Vec::new(),
        }
    }

    #[test]
    fn a_position_survives_being_written_and_read_again() {
        let dir = directory("round-trip");
        let path = file(&dir, "report.xlsx");
        remember(&dir, &path, view(40, 3));

        // A fresh process would read the file, not a cached map.
        let store = read(&dir);
        let stored = store.files.get(&key(&path)).expect("the entry");
        assert_eq!(stored.view, view(40, 3));
        assert!(stored.stamp.is_some(), "the file state is recorded too");
        assert_eq!(lookup(&dir, &path), Some(view(40, 3)));
    }

    #[test]
    fn a_file_that_was_never_open_has_no_position() {
        let dir = directory("unknown");
        let path = file(&dir, "report.xlsx");

        assert_eq!(lookup(&dir, &path), None);
    }

    #[test]
    fn a_position_is_remembered_for_the_file_it_was_left_in() {
        let dir = directory("per-file");
        let sales = file(&dir, "sales.xlsx");
        let costs = file(&dir, "costs.xlsx");

        remember(&dir, &sales, view(12, 2));

        assert_eq!(lookup(&dir, &sales), Some(view(12, 2)));
        assert_eq!(lookup(&dir, &costs), None);
    }

    #[test]
    fn the_last_position_for_a_file_replaces_the_earlier_one() {
        let dir = directory("replaced");
        let path = file(&dir, "report.xlsx");
        remember(&dir, &path, view(5, 1));
        remember(&dir, &path, view(60, 4));

        assert_eq!(lookup(&dir, &path), Some(view(60, 4)));
        assert_eq!(read(&dir).files.len(), 1);
    }

    #[test]
    fn an_edited_file_starts_from_its_own_top() {
        let dir = directory("edited");
        let path = file(&dir, "report.xlsx");
        remember(&dir, &path, view(40, 3));

        std::fs::write(&path, b"a,b,c\nd,e,f\n").expect("new contents");

        assert_eq!(lookup(&dir, &path), None);
        assert!(
            read(&dir).files.is_empty(),
            "the stale entry is removed from the store"
        );
    }

    #[test]
    fn a_file_that_is_gone_forgets_its_position() {
        let dir = directory("gone");
        let path = file(&dir, "report.xlsx");
        remember(&dir, &path, view(40, 3));
        std::fs::remove_file(&path).expect("remove the fixture");

        assert_eq!(lookup(&dir, &path), None);
        assert!(read(&dir).files.is_empty());
    }

    #[test]
    fn a_file_put_at_the_same_path_is_not_given_the_old_position() {
        let dir = directory("swapped");
        let path = file(&dir, "report.xlsx");
        remember(&dir, &path, view(40, 3));

        // The same path and length, but a later timestamp: the position was
        // recorded in the file that was here before this one.
        std::fs::remove_file(&path).expect("remove the fixture");
        let replacement = file(&dir, "report.xlsx");
        let later = SystemTime::now() + Duration::from_secs(2);
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .expect("open the replacement")
            .set_modified(later)
            .expect("set a later timestamp");

        assert_eq!(lookup(&dir, &replacement), None);
    }

    #[test]
    fn an_entry_without_a_file_state_is_not_trusted() {
        let dir = directory("legacy");
        let path = file(&dir, "report.xlsx");
        let mut store = Store::default();
        store.files.insert(
            key(&path),
            Stored {
                view: view(40, 3),
                stamp: None,
            },
        );
        write(&dir, &store).expect("write an old-style entry");

        assert_eq!(lookup(&dir, &path), None);
        assert!(read(&dir).files.is_empty());
    }

    #[test]
    fn a_damaged_store_is_ignored_rather_than_breaking_an_open() {
        let dir = directory("damaged");
        let path = file(&dir, "report.xlsx");
        std::fs::write(dir.join(STORE_FILE), b"{ not json").unwrap();

        assert_eq!(lookup(&dir, &path), None);

        // The next position replaces the damaged file.
        remember(&dir, &path, view(3, 0));
        assert_eq!(lookup(&dir, &path), Some(view(3, 0)));
    }

    #[test]
    fn the_same_file_reached_by_another_spelling_is_the_same_position() {
        let dir = directory("key");
        let path = file(&dir, "report.xlsx");
        let dotted = dir.join(".").join("report.xlsx");

        remember(&dir, &path, view(40, 3));

        assert_eq!(lookup(&dir, &dotted), Some(view(40, 3)));
    }

    #[test]
    fn a_position_on_another_sheet_is_stored_as_that_sheet() {
        let dir = directory("sheet");
        let path = file(&dir, "report.xlsx");
        let mut where_we_were = view(7, 0);
        where_we_were.sheet = 3;
        remember(&dir, &path, where_we_were);

        assert_eq!(
            lookup(&dir, &path).map(|stored| stored.sheet),
            Some(3),
            "the active sheet is part of where the file was left"
        );
    }

    #[test]
    fn the_widths_a_reader_dragged_travel_with_the_position() {
        let dir = directory("widths");
        let path = file(&dir, "report.xlsx");
        let mut where_we_were = view(40, 3);
        where_we_were.widths = vec![
            SheetWidths {
                sheet: 0,
                columns: vec![(1, 220.0), (4, 96.0)],
            },
            SheetWidths {
                sheet: 2,
                columns: vec![(0, 64.0)],
            },
        ];

        remember(&dir, &path, where_we_were.clone());

        // A fresh process reads the store back, widths and all.
        assert_eq!(lookup(&dir, &path), Some(where_we_were.clone()));
        let store = read(&dir);
        let stored = store.files.get(&key(&path)).expect("the entry");
        assert_eq!(stored.view.widths, where_we_were.widths);
    }

    #[test]
    fn a_position_without_widths_reads_as_having_none() {
        let dir = directory("legacy-entry");
        let path = file(&dir, "report.xlsx");
        // An entry written before widths were remembered: a position and the
        // file state, and no `widths` key at all.
        let mut store = Store::default();
        store.files.insert(
            key(&path),
            Stored {
                view: view(4, 1),
                stamp: Stamp::of(&path),
            },
        );
        write(&dir, &store).expect("an old-style entry");
        let text = std::fs::read_to_string(dir.join(STORE_FILE)).expect("the store");
        assert!(
            !text.contains("\"widths\""),
            "nothing was dragged in this entry"
        );

        // It still says where the file was left; only the widths are missing,
        // so the sheet keeps the widths the file itself gives it.
        let stored = lookup(&dir, &path).expect("a position that is still current");
        assert_eq!((stored.row, stored.col), (4, 1));
        assert!(stored.widths.is_empty());
    }
}
