//! Remembering how each delimited file was last read.
//!
//! A `.csv` is bytes plus a convention, and the convention a particular file
//! uses is worth keeping: an export that is semicolon-separated Windows-1252
//! every week should not have to be fixed every week. The file's path, made
//! absolute and canonical where the filesystem allows, is the key. An entry
//! also records the file's length and modification time, and is dropped when
//! those no longer match, so a different file put at the same path starts from
//! detection. What is stored is only what was chosen by hand, never what
//! detection worked out, so a file with nothing to remember keeps being read
//! by its own bytes.
//!
//! The row pinned as the file's header lives here too. It is not part of the
//! dialect — nothing about it is read from the bytes — but it is the same kind
//! of choice for the same file, made in the same bar, and it is forgotten on
//! the same terms: an export replaced by a different one starts again from its
//! own first record.
//!
//! So does the order the records were put in. Sorting a table is work done to
//! read it — finding the biggest month, the newest order — and coming back to
//! the file a week later to find it in export order means doing that work
//! again. The column, the direction and the column's name as the strip showed
//! it are kept beside the dialect and the header row.
//!
//! This is a convenience, not a record the reader depends on: a store that
//! cannot be read or written is ignored, and the file opens with detection as
//! it would have without one.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use serde::{Deserialize, Serialize};

use crate::text_table::{Delimiter, Dialect, Encoding};

/// Where the choices live, inside this build's per-user data directory.
const STORE_FILE: &str = "text-dialects.json";

/// One writer at a time: a read-modify-write from two opens at once would
/// otherwise lose one of them.
static LOCK: Mutex<()> = Mutex::new(());

/// The order a delimited file's records were left in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SortChoice {
    /// The column the records are ordered by, counted from zero as the grid
    /// counts them.
    pub column: usize,
    pub descending: bool,
    /// The column's name as the strip showed it when the order was chosen, so
    /// the status bar can say what the table is ordered by before the cells it
    /// is read from have arrived. Absent in an entry written without one.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub label: String,
}

/// The choices remembered for one file. Each is optional because the user may
/// have fixed only the delimiter, leaving the encoding to detection.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delimiter: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub encoding: Option<String>,
    /// The row pinned as the header, counted from zero as the grid counts it.
    ///
    /// Three states, which is why this is an `Option` of an `Option`: absent
    /// when nobody has chosen one (the first record pins), `Some(None)` when the
    /// user unpinned the file deliberately — a headerless file should open
    /// headerless next time — and `Some(Some(row))` for a row.
    #[serde(
        default,
        deserialize_with = "deserialize_double_option",
        skip_serializing_if = "Option::is_none"
    )]
    pub header_row: Option<Option<usize>>,
    /// The order the records were left in, when one was chosen.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sort: Option<SortChoice>,
}

/// Read a field that is absent, `null`, or a value, as the three things they
/// are.
///
/// `serde` maps `null` to `None` for an `Option`, which would make "nobody
/// chose a header" and "unpinned on purpose" the same state — and the file
/// would come back with a header after being left without one.
fn deserialize_double_option<'de, D, T>(deserializer: D) -> Result<Option<Option<T>>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::Deserialize<'de>,
{
    Option::<T>::deserialize(deserializer).map(Some)
}

impl Entry {
    /// The entry that records `dialect`, as labels the store can read back.
    pub fn from_dialect(dialect: Dialect) -> Self {
        Entry {
            delimiter: dialect.delimiter.map(|chosen| chosen.label().to_string()),
            encoding: dialect.encoding.map(|chosen| chosen.label().to_string()),
            header_row: None,
            sort: None,
        }
    }

    pub fn is_empty(&self) -> bool {
        self.delimiter.is_none()
            && self.encoding.is_none()
            && self.header_row.is_none()
            && self.sort.is_none()
    }

    fn delimiter(&self) -> Option<Delimiter> {
        self.delimiter.as_deref().and_then(Delimiter::from_label)
    }

    fn encoding(&self) -> Option<Encoding> {
        self.encoding.as_deref().and_then(Encoding::from_label)
    }
}

/// What a file looked like when its dialect was remembered.
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

/// A choice and the file state it was made against.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    #[serde(flatten)]
    entry: Entry,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    stamp: Option<Stamp>,
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
///
/// An entry whose file has changed since it was made is removed rather than
/// returned: a different export at the same path should be read by its own
/// bytes, not by the choices the old one needed.
pub fn lookup(directory: &Path, path: &Path) -> Entry {
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
            // A stale choice should not linger in the store; if this write
            // fails it will simply be dropped again on the next open.
            let _ = write(directory, &store);
        }
        return Entry::default();
    }
    store
        .files
        .get(&key)
        .map(|stored| stored.entry.clone())
        .unwrap_or_default()
}

/// Remember `entry` for `path`, or forget it when the entry says nothing.
pub fn remember(directory: &Path, path: &Path, entry: Entry) {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = read(directory);
    let key = key(path);
    // The delimiter and encoding are chosen at one moment and the header row at
    // another, so replacing one must not throw the other away. Only an entry
    // that still describes this file is worth carrying over: one left by a
    // different file at the same path is not this file's header.
    let mut entry = entry;
    let previous = store
        .files
        .get(&key)
        .filter(|stored| {
            stored
                .stamp
                .as_ref()
                .is_some_and(|stamp| stamp.matches(path))
        })
        .map(|stored| stored.entry.clone());
    if entry.header_row.is_none() {
        entry.header_row = previous.as_ref().and_then(|stored| stored.header_row);
    }
    // Re-reading a file with another delimiter or encoding changes how it is
    // read, not which order the records were put in.
    if entry.sort.is_none() {
        entry.sort = previous.and_then(|stored| stored.sort);
    }
    if entry.is_empty() {
        store.files.remove(&key);
    } else if let Some(stamp) = Stamp::of(path) {
        store.files.insert(
            key,
            Stored {
                entry,
                stamp: Some(stamp),
            },
        );
    } else {
        // Without a stamp this could never be shown to still belong to the
        // file, so storing it would only leave an entry lookup has to drop.
        store.files.remove(&key);
    }
    // A choice that cannot be written still applies to this session; failing to
    // open the file over it would be worse than forgetting it next time.
    let _ = write(directory, &store);
}

/// Remember the order chosen for `path`, keeping how the file is read.
///
/// An entry left by a different file at the same path is not carried over: its
/// columns are not this file's, and neither is its order.
pub fn remember_sort(directory: &Path, path: &Path, sort: SortChoice) {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = read(directory);
    let key = key(path);
    let Some(stamp) = Stamp::of(path) else {
        // Without a stamp this could never be shown to belong to the file.
        store.files.remove(&key);
        let _ = write(directory, &store);
        return;
    };
    let mut entry = store
        .files
        .get(&key)
        .filter(|stored| stored.stamp.as_ref() == Some(&stamp))
        .map(|stored| stored.entry.clone())
        .unwrap_or_default();
    entry.sort = Some(sort);
    store.files.insert(
        key,
        Stored {
            entry,
            stamp: Some(stamp),
        },
    );
    // An order that cannot be written still applies to this session; failing to
    // open the file over it would be worse than forgetting it next time.
    let _ = write(directory, &store);
}

/// Forget the order chosen for `path`, keeping how the file is read.
///
/// The file's own order is the default, so going back to it leaves nothing to
/// remember. An entry that said nothing but which order the records were in
/// goes with it rather than lingering as an empty one.
pub fn forget_sort(directory: &Path, path: &Path) {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = read(directory);
    let key = key(path);
    let Some(stored) = store.files.get_mut(&key) else {
        return;
    };
    if stored.entry.sort.take().is_none() {
        return;
    }
    if stored.entry.is_empty() {
        store.files.remove(&key);
    }
    let _ = write(directory, &store);
}

/// Remember the header row chosen for `path`, keeping the dialect it is read
/// with.
///
/// `None` records that the file was unpinned on purpose, which reopens it with
/// no header rather than with its first record. An entry left by a different
/// file at the same path is not carried over: its rows are not this file's.
pub fn remember_header(directory: &Path, path: &Path, header_row: Option<usize>) {
    let _guard = LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let mut store = read(directory);
    let key = key(path);
    let Some(stamp) = Stamp::of(path) else {
        // Without a stamp this could never be shown to belong to the file.
        store.files.remove(&key);
        let _ = write(directory, &store);
        return;
    };
    let mut entry = store
        .files
        .get(&key)
        .filter(|stored| stored.stamp.as_ref() == Some(&stamp))
        .map(|stored| stored.entry.clone())
        .unwrap_or_default();
    entry.header_row = Some(header_row);
    store.files.insert(
        key,
        Stored {
            entry,
            stamp: Some(stamp),
        },
    );
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
    use std::time::{Duration, SystemTime};

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
            header_row: None,
            sort: None,
        }
    }

    fn by_units() -> SortChoice {
        SortChoice {
            column: 3,
            descending: true,
            label: "Units".to_string(),
        }
    }

    /// The entry for a file whose header is `header_row`, where `None` means
    /// the user unpinned it and `Some(None)` that nobody chose.
    fn with_header(mut entry: Entry, header_row: Option<usize>) -> Entry {
        entry.header_row = Some(header_row);
        entry
    }

    #[test]
    fn a_header_row_is_remembered_across_a_fresh_process() {
        let dir = directory("header");
        let path = file(&dir, "report.csv");
        remember_header(&dir, &path, Some(2));

        // A fresh process would read the file, not a cached map.
        let store = read(&dir);
        let stored = store.files.get(&key(&path)).expect("the entry");
        assert_eq!(stored.entry.header_row, Some(Some(2)));
        assert_eq!(lookup(&dir, &path).header_row, Some(Some(2)));
    }

    #[test]
    fn unpinning_a_file_is_remembered_as_a_choice() {
        let dir = directory("unpinned");
        let path = file(&dir, "raw.csv");
        remember_header(&dir, &path, None);

        // Absent would mean "nobody chose", which pins the first record again.
        assert_eq!(lookup(&dir, &path).header_row, Some(None));
        assert!(
            !lookup(&dir, &path).is_empty(),
            "an entry that only says 'no header' is still a choice"
        );
    }

    #[test]
    fn the_header_and_the_dialect_do_not_overwrite_each_other() {
        let dir = directory("both");
        let path = file(&dir, "report.csv");
        remember_header(&dir, &path, Some(2));
        remember(&dir, &path, chosen(Some("semicolon"), Some("Windows-1252")));

        assert_eq!(
            lookup(&dir, &path),
            with_header(chosen(Some("semicolon"), Some("Windows-1252")), Some(2))
        );

        // And the other way round: choosing the encoding again keeps the row.
        remember_header(&dir, &path, Some(3));
        remember(&dir, &path, chosen(Some("tab"), None));
        assert_eq!(
            lookup(&dir, &path),
            with_header(chosen(Some("tab"), None), Some(3))
        );
    }

    #[test]
    fn the_order_the_records_were_left_in_is_remembered() {
        let dir = directory("sort");
        let path = file(&dir, "report.csv");
        remember_sort(&dir, &path, by_units());

        assert_eq!(lookup(&dir, &path).sort, Some(by_units()));

        // Re-reading the file with another delimiter changes how it is read,
        // not the order its records are in.
        remember(&dir, &path, chosen(Some("semicolon"), None));
        assert_eq!(lookup(&dir, &path).sort, Some(by_units()));

        // A fresh process reads the file, not a cached map.
        let store = read(&dir);
        let stored = store.files.get(&key(&path)).expect("the entry");
        assert_eq!(stored.entry.sort, Some(by_units()));
    }

    #[test]
    fn the_order_and_the_header_are_choices_of_the_same_file() {
        let dir = directory("sort-and-header");
        let path = file(&dir, "report.csv");
        remember_header(&dir, &path, Some(2));
        remember_sort(&dir, &path, by_units());

        let entry = lookup(&dir, &path);
        assert_eq!(entry.header_row, Some(Some(2)));
        assert_eq!(entry.sort, Some(by_units()));
    }

    #[test]
    fn going_back_to_the_file_order_forgets_only_the_order() {
        let dir = directory("forget-sort");
        let path = file(&dir, "report.csv");
        remember_header(&dir, &path, Some(2));
        remember_sort(&dir, &path, by_units());

        forget_sort(&dir, &path);

        let entry = lookup(&dir, &path);
        assert_eq!(entry.sort, None);
        assert_eq!(
            entry.header_row,
            Some(Some(2)),
            "the header is still the header"
        );
    }

    #[test]
    fn an_entry_that_only_said_which_order_goes_with_it() {
        let dir = directory("forget-only-sort");
        let path = file(&dir, "report.csv");
        remember_sort(&dir, &path, by_units());

        forget_sort(&dir, &path);

        assert_eq!(lookup(&dir, &path), Entry::default());
        assert!(
            read(&dir).files.is_empty(),
            "there is nothing left to keep, so no empty entry lingers"
        );
    }

    #[test]
    fn an_edited_file_forgets_the_order_it_was_left_in() {
        let dir = directory("edited-sort");
        let path = file(&dir, "report.csv");
        remember_sort(&dir, &path, by_units());

        std::fs::write(&path, b"a;b\nc;d\n").expect("new contents");

        // The columns a new export has are its own, so they are not ordered by
        // what the old file was sorted on.
        assert_eq!(lookup(&dir, &path).sort, None);
    }

    #[test]
    fn an_edited_file_forgets_its_header_row() {
        let dir = directory("edited-header");
        let path = file(&dir, "report.csv");
        remember_header(&dir, &path, Some(2));

        std::fs::write(&path, b"a;b\nc;d\n").expect("new contents");

        assert_eq!(lookup(&dir, &path), Entry::default());
        // A new export at the same path does not inherit the old rows either.
        remember(&dir, &path, chosen(Some("semicolon"), None));
        assert_eq!(lookup(&dir, &path).header_row, None);
    }

    #[test]
    fn a_header_row_survives_the_store_being_read_back() {
        let dir = directory("header-json");
        let path = file(&dir, "report.csv");
        remember_header(&dir, &path, Some(4));
        remember(&dir, &path, chosen(None, Some("windows-1252")));

        let text = std::fs::read_to_string(dir.join(STORE_FILE)).expect("the store");
        let store: Store = serde_json::from_str(&text).expect("valid JSON");
        let stored = store.files.get(&key(&path)).expect("the entry");
        assert_eq!(stored.entry.header_row, Some(Some(4)));

        // "No header" is written as null rather than left out, so reopening
        // does not pin the first record again.
        remember_header(&dir, &path, None);
        let text = std::fs::read_to_string(dir.join(STORE_FILE)).expect("the store");
        assert!(text.contains("\"header_row\": null"), "{text}");
        let store: Store = serde_json::from_str(&text).expect("valid JSON");
        assert_eq!(
            store
                .files
                .get(&key(&path))
                .expect("the entry")
                .entry
                .header_row,
            Some(None)
        );
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
        let stored = store.files.get(&key(&path)).expect("the entry");
        assert_eq!(stored.entry.delimiter(), Some(Delimiter::Pipe));
        assert_eq!(stored.entry.encoding(), Some(Encoding::Utf16Le));
        assert!(stored.stamp.is_some(), "the file state is recorded too");
    }

    #[test]
    fn an_edited_file_forgets_the_choice_it_was_given() {
        let dir = directory("edited");
        let path = file(&dir, "data.csv");
        remember(&dir, &path, chosen(Some("semicolon"), Some("Windows-1252")));

        std::fs::write(&path, b"a|b|c\nd|e|f\n").expect("new contents");

        assert_eq!(lookup(&dir, &path), Entry::default());
        assert!(
            read(&dir).files.is_empty(),
            "the stale entry is removed from the store"
        );
    }

    #[test]
    fn a_file_that_is_gone_forgets_the_choice_it_was_given() {
        let dir = directory("gone");
        let path = file(&dir, "data.csv");
        remember(&dir, &path, chosen(Some("pipe"), None));
        std::fs::remove_file(&path).expect("remove the fixture");

        assert_eq!(lookup(&dir, &path), Entry::default());
        assert!(read(&dir).files.is_empty());
    }

    #[test]
    fn a_file_put_at_the_same_path_is_not_given_the_old_choice() {
        let dir = directory("replaced-file");
        let path = file(&dir, "data.csv");
        remember(&dir, &path, chosen(Some("tab"), None));

        // The same path and length, but a later timestamp: the old choices
        // were made for the file that was here before this one.
        std::fs::remove_file(&path).expect("remove the fixture");
        let replacement = file(&dir, "data.csv");
        let later = SystemTime::now() + Duration::from_secs(2);
        std::fs::File::options()
            .write(true)
            .open(&replacement)
            .expect("open the replacement")
            .set_modified(later)
            .expect("set a later timestamp");

        assert_eq!(lookup(&dir, &path), Entry::default());
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
    fn an_entry_without_a_file_state_is_not_trusted() {
        let dir = directory("legacy");
        let path = file(&dir, "data.csv");
        let mut store = Store::default();
        store.files.insert(
            key(&path),
            Stored {
                entry: chosen(Some("semicolon"), None),
                stamp: None,
            },
        );
        write(&dir, &store).expect("write an old-style entry");

        assert_eq!(lookup(&dir, &path), Entry::default());
        assert!(read(&dir).files.is_empty());
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
        assert_eq!(
            auto.encoding,
            Some(Encoding::Legacy(encoding_rs::WINDOWS_1252))
        );

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
