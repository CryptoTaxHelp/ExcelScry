// Hide the console window in release builds; a grid viewer should not flash a
// terminal when Windows launches it from a file association.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod associations;
mod cell_styles;
mod deps;
mod formula;
mod manifest;
mod model;
mod number_format;
mod ods_export;
mod ods_formats;
mod recent;
mod sheet_layout;
mod text_export;
mod text_prefs;
mod text_table;
mod trust;
mod update;
mod view_prefs;
mod workbook;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use associations::AssociationStatus;
use model::{RowsPage, WorkbookInfo};
use ods_export::ExportSummary;
use text_export::Utf8CopySummary;
use text_table::Encoding;

/// Everything the UI can ask for, plus the currently open workbook.
#[derive(Default)]
struct AppState {
    /// The parsed workbook, held so scrolling does not re-parse the file.
    workbook: Mutex<Option<model::LoadedWorkbook>>,
    /// A file handed to us on the command line, waiting for the UI to ask.
    pending_open: Mutex<Option<String>>,
}

impl AppState {
    fn with_workbook<T>(&self, f: impl FnOnce(&model::LoadedWorkbook) -> T) -> Result<T, String> {
        let guard = self
            .workbook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let wb = guard.as_ref().ok_or("No spreadsheet is open.")?;
        Ok(f(wb))
    }

    /// The same, for a command that changes the open workbook in place: a text
    /// table's sort order is a view of the rows, not a re-read of the file.
    fn with_workbook_mut<T>(
        &self,
        f: impl FnOnce(&mut model::LoadedWorkbook) -> T,
    ) -> Result<T, String> {
        let mut guard = self
            .workbook
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let wb = guard.as_mut().ok_or("No spreadsheet is open.")?;
        Ok(f(wb))
    }
}

// ---------------------------------------------------------------------------
// Opening files
// ---------------------------------------------------------------------------

/// Parse a spreadsheet and make it the open workbook.
///
/// `delimiter` and `encoding` are how the UI re-reads a `.csv`/`.tsv` that
/// opened wrong; empty means the file decides, which is what the first open
/// uses. They are validated here rather than trusted, since they arrive from
/// the webview. `dialect_explicit` says the UI is reporting a choice the user
/// just made, which is the only case worth remembering for this file.
#[tauri::command]
async fn open_workbook(
    path: String,
    delimiter: Option<String>,
    encoding: Option<String>,
    dialect_explicit: Option<bool>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<WorkbookInfo, String> {
    let requested = text_table::Dialect::from_labels(delimiter.as_deref(), encoding.as_deref())?;
    // A file read with choices before starts from them; detection only fills in
    // the parts nobody chose. Without a data directory the stores simply
    // remember nothing.
    let data_dir = app.path().app_local_data_dir().ok();
    let remembered = data_dir
        .as_deref()
        .map(|directory| text_prefs::lookup(directory, Path::new(&path)))
        .unwrap_or_default();
    let explicit = dialect_explicit.unwrap_or(false);
    let dialect = text_prefs::resolve(requested, &remembered, explicit);
    let requested_path = PathBuf::from(&path);
    let mut loaded =
        tauri::async_runtime::spawn_blocking(move || workbook::load_with(&requested_path, dialect))
            .await
            .map_err(|e| format!("Could not read the file: {e}"))??;

    // A successful open is what the welcome screen offers next time. This is
    // separate from the dialect memory: a workbook has no dialect, and a text
    // file opened with detection has no remembered choice either.
    if let Some(directory) = data_dir.as_deref() {
        recent::remember(directory, Path::new(&path));
    }

    // Only a choice made in the bar is stored; the first open of a file and a
    // reload must not rewrite what the store already says.
    if explicit {
        if let Some(directory) = data_dir.as_deref() {
            text_prefs::remember(
                directory,
                Path::new(&path),
                text_prefs::Entry::from_dialect(requested),
            );
        }
    }

    // A delimited table reopens in the order it was left in. The rows are put
    // back before the grid is told anything, so the first paint is the table
    // the reader left rather than a re-sort once its cells arrive.
    let restored_sort = if loaded.text.is_some() {
        let rows = loaded.sheets.first().map_or(0, |sheet| sheet.rows.len());
        let cols = loaded.sheets.first().map_or(0, |sheet| sheet.cols);
        restore_sort(&mut loaded, &remembered, rows, cols)
    } else {
        None
    };

    let mut info = loaded.info();
    // A delimited file reopens with the row that was pinned for it last time:
    // the rows above the header do not need finding again. The choice is a view
    // of the file, not a way to read it, so it is applied after the parse.
    if let Some(text) = info.text.as_mut() {
        let rows = info.sheets.first().map_or(0, |sheet| sheet.rows);
        text.header_row = stored_header_row(&remembered, rows);
        // The order the rows are already in, for the strip and the status bar.
        text.sort = restored_sort;
    }
    // And it reopens where it was left: the sheet, the cell and the scroll
    // offset, when the store still has them for this exact file.
    let extents: Vec<(usize, usize)> = info
        .sheets
        .iter()
        .map(|sheet| (sheet.rows, sheet.cols))
        .collect();
    info.view = data_dir
        .as_deref()
        .and_then(|directory| view_prefs::lookup(directory, Path::new(&path)))
        .and_then(|stored| fit_view(stored, &extents));
    *state
        .workbook
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(loaded);
    Ok(info)
}

/// Where the grid was left, fitted to the workbook that was just opened.
///
/// The store is trusted only as far as the file it was written for: the sheet
/// has to still be there, and a cell past the end of a sheet that turned out
/// shorter is pulled back to its last row or column rather than left pointing
/// at nothing. An offset that is not a number is not a position either.
fn fit_view(
    view: view_prefs::ViewState,
    sheets: &[(usize, usize)],
) -> Option<view_prefs::ViewState> {
    let &(rows, cols) = sheets.get(view.sheet)?;
    Some(view_prefs::ViewState {
        sheet: view.sheet,
        row: view.row.min(last_index(rows)),
        col: view.col.min(last_index(cols)),
        top: offset(view.top),
        left: offset(view.left),
        widths: fit_widths(view.widths, sheets),
    })
}

/// How narrow and how wide a dragged column may be, in CSS pixels.
///
/// The grid clamps a drag to the same range, so a stored value outside it came
/// from a hand-edited store rather than from the pointer.
const MIN_COL_WIDTH: f64 = 24.0;
const MAX_COL_WIDTH: f64 = 1600.0;

/// Enough dragged columns to have shaped a table by hand, without letting a
/// damaged store grow one view entry without bound.
const MAX_WIDTHS_PER_SHEET: usize = 512;

/// The column widths a stored view can still put back.
///
/// A sheet the workbook no longer has is not one whose widths can be restored,
/// and neither is a column past the end of a sheet that turned out narrower —
/// the same fitting the position itself gets. The list is sorted by column and
/// deduplicated so the grid's layout walk stays linear, whatever the store says.
fn fit_widths(
    widths: Vec<view_prefs::SheetWidths>,
    sheets: &[(usize, usize)],
) -> Vec<view_prefs::SheetWidths> {
    let mut fitted: Vec<view_prefs::SheetWidths> = widths
        .into_iter()
        .filter_map(|sheet| {
            let &(_, cols) = sheets.get(sheet.sheet)?;
            let mut columns: Vec<(u32, f64)> = sheet
                .columns
                .into_iter()
                .filter(|&(col, pixels)| {
                    usize::try_from(col).is_ok_and(|col| col < cols) && pixels.is_finite()
                })
                .map(|(col, pixels)| (col, pixels.clamp(MIN_COL_WIDTH, MAX_COL_WIDTH)))
                .collect();
            columns.sort_by_key(|&(col, _)| col);
            columns.dedup_by_key(|entry| entry.0);
            columns.truncate(MAX_WIDTHS_PER_SHEET);
            (!columns.is_empty()).then_some(view_prefs::SheetWidths {
                sheet: sheet.sheet,
                columns,
            })
        })
        .collect();
    // One entry per sheet, whatever the store listed, and in sheet order so
    // two files' views compare equal however they were written.
    fitted.sort_by_key(|sheet| sheet.sheet);
    fitted.dedup_by_key(|sheet| sheet.sheet);
    fitted
}

/// A scroll offset the grid can be put back at: a number, and not a negative
/// one, which is the top or left edge anyway.
fn offset(offset: f64) -> f64 {
    if offset.is_finite() {
        offset.max(0.0)
    } else {
        0.0
    }
}

/// The last index of an axis `count` long, as the grid numbers it.
fn last_index(count: usize) -> u32 {
    u32::try_from(count.saturating_sub(1)).unwrap_or(u32::MAX)
}

/// Put the rows of a delimited file back in the order they were left in.
///
/// Only a delimited file has an order to restore, and only for a column it
/// still has: a re-export that dropped the column opens in its own order
/// rather than refusing to open. The rows above the header keep their places,
/// exactly as they did when the order was chosen, and the sort itself is the
/// one the grid asks for by hand.
fn restore_sort(
    loaded: &mut model::LoadedWorkbook,
    entry: &text_prefs::Entry,
    rows: usize,
    cols: usize,
) -> Option<text_prefs::SortChoice> {
    let sort = entry.sort.clone()?;
    if sort.column >= cols {
        return None;
    }
    loaded
        .sort_text(0, sort_header(entry, rows), sort.column, sort.descending)
        .ok()?;
    Some(sort)
}

/// The header row a sort starts below, as the grid will pin it.
///
/// Nobody having chosen one means the first record pins, which is the grid's
/// own default; a file left unpinned sorts from its first row.
fn sort_header(entry: &text_prefs::Entry, rows: usize) -> Option<usize> {
    match stored_header_row(entry, rows) {
        None => Some(0),
        Some(None) => None,
        Some(Some(row)) => Some(row),
    }
}

/// The header row a stored choice names, as the UI reads it.
///
/// `None` means nobody chose one (the first record pins, as always),
/// `Some(None)` that the file was left unpinned, and `Some(Some(row))` a row.
/// A row the file no longer has is dropped rather than pinned past the end.
fn stored_header_row(entry: &text_prefs::Entry, rows: usize) -> Option<Option<usize>> {
    match entry.header_row {
        Some(Some(row)) if row >= rows => None,
        stored => stored,
    }
}

/// Remember the row pinned as the open delimited file's header.
///
/// The bar calls this whenever the pin changes, including when the file is
/// unpinned — which is itself a choice, and one that reopening should keep. A
/// pin that cannot be written still applies to this session; the store is a
/// convenience, never something an open depends on.
#[tauri::command]
fn remember_header(row: Option<u32>, app: tauri::AppHandle, state: State<'_, AppState>) {
    let Ok(directory) = app.path().app_local_data_dir() else {
        return;
    };
    let opened = state.with_workbook(|wb| {
        (
            wb.text.is_some(),
            wb.path.clone(),
            wb.sheets.first().map_or(0, |sheet| sheet.rows.len()),
        )
    });
    let Ok((true, path, rows)) = opened else {
        return;
    };
    // Only a row the file has can be its header; anything else is the pin
    // coming off.
    let header = row.map(|row| row as usize).filter(|row| *row < rows);
    text_prefs::remember_header(&directory, Path::new(&path), header);
}

/// Show the native file picker and open whatever is chosen.
#[tauri::command]
async fn pick_workbook(app: tauri::AppHandle) -> Result<Option<String>, String> {
    let picked = app
        .dialog()
        .file()
        .set_title("Open a spreadsheet")
        .add_filter(
            "Spreadsheets",
            &[
                "xlsx", "xlsm", "xlsb", "xls", "xla", "ods", "csv", "tsv", "tab",
            ],
        )
        .add_filter("Excel workbook", &["xlsx", "xlsm", "xls"])
        .add_filter("Text tables", &["csv", "tsv", "tab"])
        .blocking_pick_file();

    match picked {
        None => Ok(None),
        Some(file) => file
            .into_path()
            .map(|p| Some(p.display().to_string()))
            .map_err(|e| format!("Could not use the selected file: {e}")),
    }
}

/// The files the welcome screen should offer, newest first.
#[tauri::command]
fn recent_files(app: tauri::AppHandle) -> Vec<recent::RecentFile> {
    app.path()
        .app_local_data_dir()
        .map(|directory| recent::list(&directory))
        .unwrap_or_default()
}

/// The file Windows launched us with, e.g. from "Scry Excel File".
#[tauri::command]
fn initial_file(state: State<'_, AppState>) -> Option<String> {
    let mut pending = state
        .pending_open
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.take().filter(|p| Path::new(p).is_file())
}

#[tauri::command]
fn close_workbook(state: State<'_, AppState>) {
    *state
        .workbook
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

// ---------------------------------------------------------------------------
// Paging and exporting
// ---------------------------------------------------------------------------

/// A window of cells, addressed in absolute spreadsheet coordinates.
#[tauri::command]
fn get_rows(
    sheet: usize,
    row_start: usize,
    row_count: usize,
    col_start: usize,
    col_count: usize,
    state: State<'_, AppState>,
) -> Result<RowsPage, String> {
    state
        .with_workbook(|wb| wb.page(sheet, row_start, row_count, col_start, col_count))?
        .ok_or_else(|| "That worksheet no longer exists.".to_string())
}

/// Remember where the open file was left.
///
/// The grid calls this as the selection, the sheet, the scroll offset or a
/// column's width changes, so the next open of the same file starts there.
/// What arrives from the webview is fitted to the sheet it names rather than
/// trusted: a position past the end of the sheet is not one, neither is a
/// scroll offset that is not a number, and a width for a column the sheet does
/// not have is dropped.
#[tauri::command]
fn remember_view(view: view_prefs::ViewState, app: tauri::AppHandle, state: State<'_, AppState>) {
    let Ok(directory) = app.path().app_local_data_dir() else {
        return;
    };
    let Ok((path, extents)) = state.with_workbook(|wb| {
        (
            wb.path.clone(),
            wb.sheets
                .iter()
                .map(|sheet| (sheet.row_count(), sheet.cols))
                .collect::<Vec<_>>(),
        )
    }) else {
        return;
    };
    // A view of a sheet that is not there is not this workbook's view.
    let Some(view) = fit_view(view, &extents) else {
        return;
    };
    view_prefs::remember(&directory, Path::new(&path), view);
}

/// The cells a calculation reads, and the cells that read it.
///
/// The chain is followed on the sheet the cell is on: a reference to another
/// sheet is a real dependency, but not one this view can show without becoming
/// a workbook browser, so it is left out rather than shown as a cell here that
/// it is not.
#[tauri::command]
fn cell_dependencies(
    sheet: usize,
    row: usize,
    col: usize,
    state: State<'_, AppState>,
) -> Result<deps::Dependencies, String> {
    state.with_workbook(|wb| {
        wb.sheets
            .get(sheet)
            .map(|data| deps::of(data, row, col))
            .ok_or_else(|| "That worksheet no longer exists.".to_string())
    })?
}

/// Sort a delimited file's records by one of its named columns.
///
/// `headerRow` is the row the grid has pinned, so the records start below it
/// and a title or note above it is left alone. The file on disk is never
/// touched: this orders the rows in memory, and the order is remembered for
/// this file — `label` is the column's name as the strip showed it, so the next
/// open can say what the table is ordered by without reading a cell first.
#[tauri::command]
fn sort_text_rows(
    sheet: usize,
    header_row: Option<usize>,
    column: usize,
    descending: bool,
    label: String,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<(), String> {
    state.with_workbook_mut(|wb| wb.sort_text(sheet, header_row, column, descending))??;
    // The order is a view of the file, but one worth coming back to: it is kept
    // beside the dialect and the header row, on the same terms.
    let Ok(directory) = app.path().app_local_data_dir() else {
        return Ok(());
    };
    let Ok((path, cols)) = state.with_workbook(|wb| {
        (
            wb.path.clone(),
            wb.sheets.get(sheet).map_or(0, |data| data.cols),
        )
    }) else {
        return Ok(());
    };
    // A column the sheet does not have is not an order it can reopen in.
    if column >= cols {
        return Ok(());
    }
    text_prefs::remember_sort(
        &directory,
        Path::new(&path),
        text_prefs::SortChoice {
            column,
            descending,
            label: sort_label(&label),
        },
    );
    Ok(())
}

/// Put a delimited table back in the order its file has.
///
/// The rows are moved back in memory — nothing is read again, so the selection,
/// the scroll and the tiles that are already loaded all stay where they are —
/// and the chosen order is forgotten, since the file's own order is what a
/// first open does anyway.
#[tauri::command]
fn restore_text_order(
    sheet: usize,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<bool, String> {
    let moved = state.with_workbook_mut(|wb| {
        if wb.text.is_none() {
            return Err("Only a delimited text file has a file order.".to_string());
        }
        let data = wb
            .sheets
            .get_mut(sheet)
            .ok_or_else(|| "That worksheet no longer exists.".to_string())?;
        Ok(data.restore_file_order())
    })??;
    // The file's own order is the default, so there is nothing left to remember
    // once it is back: the next open is the file's too.
    if let Ok(directory) = app.path().app_local_data_dir() {
        if let Ok(path) = state.with_workbook(|wb| wb.path.clone()) {
            text_prefs::forget_sort(&directory, Path::new(&path));
        }
    }
    Ok(moved)
}

/// The column name stored with an order: one line, and short enough that a
/// hand-written request cannot fill the store with it.
fn sort_label(label: &str) -> String {
    label
        .trim()
        .chars()
        .filter(|ch| !ch.is_control())
        .take(MAX_SORT_LABEL)
        .collect()
}

/// Long enough for any column name a spreadsheet allows, and short enough that
/// a store of them stays a store of choices.
const MAX_SORT_LABEL: usize = 120;

/// Write the open workbook out as OpenDocument (.ods).
#[tauri::command]
async fn export_ods(
    destination: Option<String>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<ExportSummary, String> {
    let target = match destination {
        Some(d) if !d.trim().is_empty() => PathBuf::from(d),
        _ => {
            let suggested = state.with_workbook(|wb| default_ods_name(&wb.file_name))?;
            let picked = app
                .dialog()
                .file()
                .set_title("Export as OpenDocument spreadsheet")
                .set_file_name(&suggested)
                .add_filter("OpenDocument spreadsheet", &["ods"])
                .blocking_save_file();
            match picked {
                None => return Err("Export cancelled.".to_string()),
                Some(file) => file
                    .into_path()
                    .map_err(|e| format!("Could not use that location: {e}"))?,
            }
        }
    };

    // Make sure the file we hand over has the extension the user expects.
    let target = ensure_extension(target, "ods");

    let workbook = state
        .with_workbook(|wb| Ok::<_, String>(wb.clone()))
        .map_err(|e| e.to_string())??;

    tauri::async_runtime::spawn_blocking(move || ods_export::export(&workbook, &target))
        .await
        .map_err(|e| format!("Export failed: {e}"))?
}

/// Write a UTF-8 copy of a delimited file that was read in another encoding.
#[tauri::command]
async fn save_utf8_copy(
    destination: Option<String>,
    app: tauri::AppHandle,
    state: State<'_, AppState>,
) -> Result<Utf8CopySummary, String> {
    let (source, encoding, suggested) = state.with_workbook(|wb| {
        let text = wb
            .text
            .as_ref()
            .ok_or_else(|| "Only a delimited text file can be saved as UTF-8.".to_string())?;
        let encoding = Encoding::from_label(&text.encoding)
            .ok_or_else(|| format!("Unknown encoding {:?}.", text.encoding))?;
        if encoding == Encoding::Utf8 {
            return Err("This file was already read as UTF-8.".to_string());
        }
        Ok::<_, String>((
            PathBuf::from(&wb.path),
            encoding,
            utf8_copy_name(&wb.file_name),
        ))
    })??;

    let target = match destination {
        Some(path) if !path.trim().is_empty() => PathBuf::from(path),
        _ => {
            let picked = app
                .dialog()
                .file()
                .set_title("Save a UTF-8 copy")
                .set_file_name(&suggested)
                .add_filter("Delimited text", &["csv", "tsv", "tab"])
                .blocking_save_file();
            match picked {
                None => return Err("Save cancelled.".to_string()),
                Some(file) => file
                    .into_path()
                    .map_err(|e| format!("Could not use that location: {e}"))?,
            }
        }
    };

    tauri::async_runtime::spawn_blocking(move || {
        text_export::write_utf8_copy(&source, encoding, &target)
    })
    .await
    .map_err(|e| format!("Save failed: {e}"))?
}

fn utf8_copy_name(file_name: &str) -> String {
    let path = Path::new(file_name);
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "spreadsheet".to_string());
    match path.extension() {
        Some(extension) => format!("{stem}-utf8.{}", extension.to_string_lossy()),
        None => format!("{stem}-utf8"),
    }
}

fn default_ods_name(file_name: &str) -> String {
    let stem = Path::new(file_name)
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "workbook".to_string());
    format!("{stem}.ods")
}

fn ensure_extension(mut path: PathBuf, ext: &str) -> PathBuf {
    let has_ext = path
        .extension()
        .map(|e| e.eq_ignore_ascii_case(ext))
        .unwrap_or(false);
    if !has_ext {
        let mut name = path
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "workbook".to_string());
        name.push('.');
        name.push_str(ext);
        path.set_file_name(name);
    }
    path
}

// ---------------------------------------------------------------------------
// Windows shell integration
// ---------------------------------------------------------------------------

#[tauri::command]
fn association_status() -> AssociationStatus {
    associations::status()
}

#[tauri::command]
fn install_associations() -> Result<String, String> {
    associations::register_current_user()
}

#[tauri::command]
fn remove_associations() -> Result<String, String> {
    associations::unregister_current_user()
}

/// Open Windows' Default Apps page so the user can pick ExcelScry.
///
/// The URI is computed here rather than accepted from the webview, so the
/// renderer can never hand us an arbitrary command to launch.
#[tauri::command]
fn open_default_apps_settings() -> Result<(), String> {
    let status = associations::status();
    let uri = status.settings_uri;
    if !uri.starts_with("ms-settings:") {
        return Err("Refusing to open an unexpected URI.".to_string());
    }
    launch_detached("cmd", &["/C", "start", "", &uri])
}

/// Reveal a file in File Explorer.
#[tauri::command]
fn reveal_in_explorer(path: String) -> Result<(), String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("\"{}\" no longer exists.", p.display()));
    }
    launch_detached("explorer.exe", &[&format!("/select,{}", p.display())])
}

/// Open a link from the release notes in the user's own browser.
///
/// This is the one command whose argument comes from outside this build, and it
/// is the reason the checks are written out rather than assumed: the notes are
/// the release body, so a link in them is data, not an instruction.
///
/// Two things keep that from mattering. Only `http` and `https` are accepted, so
/// `javascript:`, `data:` and `file:` are refused rather than handed to Windows.
/// And the address goes to the shell's own "open this" call rather than to a
/// command interpreter: `cmd /C start`, which this app uses for its own
/// `ms-settings:` URI, would run the `&` in a URL as a second command.
///
/// The webview never navigates: the click is handled in the front end, which is
/// what puts the page in the browser instead of replacing the app with it.
#[tauri::command]
fn open_link(url: String) -> Result<(), String> {
    let target = web_link(&url).ok_or("That is not a link this app will open.")?;
    open_in_browser(target)
}

/// The address, if it is a web link this app will hand to Windows.
fn web_link(url: &str) -> Option<&str> {
    let trimmed = url.trim();
    let lowered = trimmed.to_ascii_lowercase();
    let rest = lowered
        .strip_prefix("https://")
        .or_else(|| lowered.strip_prefix("http://"))?;
    // A host has to be there: `https:///x` is not something to hand to the
    // shell. A control character would be a way to slip past something that
    // splits on lines; there is no such parser here, but a URL with one is not
    // a URL.
    if rest.is_empty() || rest.starts_with('/') || rest.chars().any(char::is_control) {
        return None;
    }
    Some(trimmed)
}

/// Hand the address to the shell, which opens it with whatever handles it.
#[cfg(windows)]
fn open_in_browser(url: &str) -> Result<(), String> {
    use windows_sys::Win32::UI::Shell::ShellExecuteW;
    use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    fn wide(text: &str) -> Vec<u16> {
        text.encode_utf16().chain(std::iter::once(0)).collect()
    }

    let operation = wide("open");
    let target = wide(url);
    // SAFETY: both strings are NUL-terminated and outlive the call (`wide`
    // returns owned buffers, kept alive by these bindings), the null window
    // handle means "no parent window", and the two `null` pointers are the
    // optional parameters, which ShellExecuteW documents as acceptable.
    let launched = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            operation.as_ptr(),
            target.as_ptr(),
            std::ptr::null(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW returns a value of 32 or less when it could not start
    // anything; above that it is a handle to what it started.
    if launched as isize <= 32 {
        Err(format!("Windows could not open {url}."))
    } else {
        Ok(())
    }
}

#[cfg(not(windows))]
fn open_in_browser(url: &str) -> Result<(), String> {
    Err(format!(
        "Opening {url} in a browser is not wired up on this platform."
    ))
}

/// Launch a helper process with no console window and without waiting.
fn launch_detached(program: &str, args: &[&str]) -> Result<(), String> {
    let mut command = std::process::Command::new(program);
    command.args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW: a GUI app spawning cmd.exe would otherwise flash a
        // console window on screen.
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
        .spawn()
        .map(|_| ())
        .map_err(|e| format!("Could not launch {program}: {e}"))
}

/// The first command-line argument that is an existing file.
fn first_file_arg(args: &[String]) -> Option<String> {
    args.iter()
        .skip(1)
        .find(|a| !a.starts_with('-') && Path::new(a.as_str()).is_file())
        .cloned()
}

fn main() {
    let mut builder = tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        // Reads `plugins.updater` from tauri.conf.json: the endpoint and the
        // public key an update has to be signed with.
        .plugin(tauri_plugin_updater::Builder::new().build());

    // Reuse a single window: a file viewer launched from a file association
    // should not pile up processes.
    #[cfg(windows)]
    {
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, argv, _cwd| {
            if let Some(path) = first_file_arg(&argv) {
                let _ = app.emit("open-file", path);
            }
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.unminimize();
                let _ = window.show();
                let _ = window.set_focus();
            }
        }));
    }

    builder
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            open_workbook,
            pick_workbook,
            recent_files,
            initial_file,
            close_workbook,
            get_rows,
            export_ods,
            save_utf8_copy,
            remember_header,
            remember_view,
            cell_dependencies,
            sort_text_rows,
            restore_text_order,
            association_status,
            install_associations,
            remove_associations,
            open_default_apps_settings,
            open_link,
            reveal_in_explorer,
            update::app_version,
            update::check_for_update,
            update::install_update,
        ])
        .setup(|app| {
            if let Some(path) = first_file_arg(&std::env::args().collect::<Vec<_>>()) {
                let state = app.state::<AppState>();
                *state
                    .pending_open
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(path);
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running ExcelScry");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ods_export_name_replaces_the_original_extension() {
        assert_eq!(default_ods_name("Budget 2026.xlsx"), "Budget 2026.ods");
        assert_eq!(default_ods_name("legacy.xls"), "legacy.ods");
        assert_eq!(default_ods_name("no-extension"), "no-extension.ods");
    }

    #[test]
    fn a_remembered_header_is_passed_on_only_while_the_file_has_that_row() {
        assert_eq!(stored_header_row(&text_prefs::Entry::default(), 5), None);
        let pinned = text_prefs::Entry {
            header_row: Some(Some(2)),
            ..text_prefs::Entry::default()
        };
        assert_eq!(stored_header_row(&pinned, 5), Some(Some(2)));
        // The file no longer has a third row: the first record pins instead of
        // a row index that would point past the end.
        assert_eq!(stored_header_row(&pinned, 2), None);
        // Unpinned is a choice, and one the UI has to see.
        let unpinned = text_prefs::Entry {
            header_row: Some(None),
            ..text_prefs::Entry::default()
        };
        assert_eq!(stored_header_row(&unpinned, 5), Some(None));
    }

    #[test]
    fn a_remembered_position_is_fitted_to_the_sheet_it_names() {
        let sheets = [(50, 8), (10, 2)];
        let remembered = view_prefs::ViewState {
            sheet: 1,
            row: 40,
            col: 6,
            top: 1200.0,
            left: 64.0,
            widths: Vec::new(),
        };

        // The second sheet has ten rows and two columns, so the cell is pulled
        // back to one it has; the sheet itself is still the one to reopen.
        let fitted = fit_view(remembered.clone(), &sheets).expect("a sheet the workbook has");
        assert_eq!((fitted.sheet, fitted.row, fitted.col), (1, 9, 1));
        assert_eq!((fitted.top, fitted.left), (1200.0, 64.0));

        // A sheet the workbook no longer has is not a place to reopen at.
        let gone = view_prefs::ViewState {
            sheet: 2,
            ..remembered.clone()
        };
        assert_eq!(fit_view(gone, &sheets), None);

        // An offset that is not a number is not a position; a negative one is
        // the top of the sheet, which is where the grid would land anyway.
        let broken = view_prefs::ViewState {
            top: f64::NAN,
            left: -5.0,
            ..remembered
        };
        let fitted = fit_view(broken, &sheets).expect("the sheet is still there");
        assert_eq!((fitted.top, fitted.left), (0.0, 0.0));
    }

    #[test]
    fn dragged_widths_are_fitted_to_the_sheets_and_columns_that_are_there() {
        let sheets = [(50, 8), (10, 2)];
        let widths = vec![
            view_prefs::SheetWidths {
                sheet: 1,
                columns: vec![(1, 240.0), (5, 90.0), (0, 12.0), (1, 300.0), (0, f64::NAN)],
            },
            view_prefs::SheetWidths {
                sheet: 4,
                columns: vec![(0, 200.0)],
            },
        ];

        let fitted = fit_widths(widths, &sheets);
        // Sheet 4 is no longer in the workbook and column 5 is past the end of
        // sheet 1; the NaN is not a width, the 12 px is widened to the
        // narrowest a drag can go, and a repeated column keeps its first entry
        // rather than making the grid lay the sheet out twice.
        assert_eq!(
            fitted,
            [view_prefs::SheetWidths {
                sheet: 1,
                columns: vec![(0, MIN_COL_WIDTH), (1, 240.0)],
            }]
        );
    }

    #[test]
    fn a_sort_starts_below_the_header_the_grid_pins() {
        // Nobody chose a header, so the first record pins and the records are
        // the rows below it — the grid's own default.
        assert_eq!(sort_header(&text_prefs::Entry::default(), 5), Some(0));

        // Left unpinned on purpose: the whole file is records.
        let unpinned = text_prefs::Entry {
            header_row: Some(None),
            ..text_prefs::Entry::default()
        };
        assert_eq!(sort_header(&unpinned, 5), None);

        let pinned = text_prefs::Entry {
            header_row: Some(Some(2)),
            ..text_prefs::Entry::default()
        };
        assert_eq!(sort_header(&pinned, 5), Some(2));

        // A row the file no longer has is not a header, so the first record
        // pins again rather than the sort starting past the end.
        assert_eq!(sort_header(&pinned, 2), Some(0));
    }

    #[test]
    fn a_remembered_order_is_put_back_when_the_file_opens() {
        let dir = std::env::temp_dir().join("excelscry-sort-restore");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a directory for the fixture");
        let path = dir.join("report.csv");
        std::fs::write(&path, "Units,Region\n9,North\n10,South\n2,East\n").expect("the fixture");

        let mut loaded =
            workbook::load_with(&path, text_table::Dialect::default()).expect("a delimited file");
        let rows = loaded.sheets.first().map_or(0, |sheet| sheet.rows.len());
        let cols = loaded.sheets.first().map_or(0, |sheet| sheet.cols);
        let entry = text_prefs::Entry {
            sort: Some(text_prefs::SortChoice {
                column: 0,
                descending: true,
                label: "Units".to_string(),
            }),
            ..text_prefs::Entry::default()
        };

        let restored = restore_sort(&mut loaded, &entry, rows, cols).expect("the order comes back");
        assert_eq!((restored.column, restored.descending), (0, true));
        // The first record is the header — nobody chose one — so the rows
        // below it are the records the order applies to, largest first.
        let units: Vec<String> = loaded.sheets[0].rows[1..]
            .iter()
            .map(|row| row[0].display())
            .collect();
        assert_eq!(units, ["10", "9", "2"]);

        // A file that opened sorted can still be put back in its own order:
        // the sort the open path applied left the same trail as a click does.
        assert!(loaded.sheets[0].restore_file_order());
        let units: Vec<String> = loaded.sheets[0].rows[1..]
            .iter()
            .map(|row| row[0].display())
            .collect();
        assert_eq!(units, ["9", "10", "2"], "the order the file has");

        // A column a re-export dropped is not an order the file can reopen in.
        let gone = text_prefs::Entry {
            sort: Some(text_prefs::SortChoice {
                column: 9,
                descending: true,
                label: "Dropped".to_string(),
            }),
            ..text_prefs::Entry::default()
        };
        assert_eq!(restore_sort(&mut loaded, &gone, rows, cols), None);
    }

    #[test]
    fn a_stored_column_name_is_one_short_line() {
        assert_eq!(sort_label("  Units\n "), "Units");
        assert_eq!(sort_label("a\tb"), "ab");
        assert_eq!(sort_label(&"x".repeat(500)).len(), MAX_SORT_LABEL);
    }

    #[test]
    fn a_utf8_copy_keeps_the_files_extension() {
        assert_eq!(utf8_copy_name("sales.csv"), "sales-utf8.csv");
        assert_eq!(utf8_copy_name("sales.tsv"), "sales-utf8.tsv");
        assert_eq!(utf8_copy_name("sales"), "sales-utf8");
    }

    #[test]
    fn export_extension_is_forced_to_ods() {
        assert_eq!(
            ensure_extension(PathBuf::from("C:\\out\\book"), "ods"),
            PathBuf::from("C:\\out\\book.ods")
        );
        assert_eq!(
            ensure_extension(PathBuf::from("C:\\out\\book.ODS"), "ods"),
            PathBuf::from("C:\\out\\book.ODS")
        );
        // A .xlsx name is corrected rather than silently written as xlsx.
        assert_eq!(
            ensure_extension(PathBuf::from("C:\\out\\book.xlsx"), "ods"),
            PathBuf::from("C:\\out\\book.xlsx.ods")
        );
    }

    #[test]
    fn web_links_are_the_two_schemes_a_browser_is_for() {
        assert_eq!(
            web_link("https://example.com/x"),
            Some("https://example.com/x")
        );
        assert_eq!(
            web_link("  http://example.com  "),
            Some("http://example.com")
        );
        // The scheme is matched however the release's author spelled it.
        assert_eq!(
            web_link("HTTPS://EXAMPLE.COM/a?b=1"),
            Some("HTTPS://EXAMPLE.COM/a?b=1")
        );
    }

    #[test]
    fn anything_that_is_not_a_web_link_is_refused() {
        for url in [
            "javascript:alert(1)",
            "data:text/html,<b>",
            "file:///C:/Windows/System32/calc.exe",
            "ms-settings:defaultapps",
            "vbscript:msgbox",
            "//example.com",
            "https://",
            "https:///no-host",
            "",
        ] {
            assert_eq!(web_link(url), None, "{url} should not be opened");
        }
        // A newline inside a URL is not a URL, and neither is a bare scheme.
        assert_eq!(web_link("https://example.com/a\nb"), None);
        assert_eq!(web_link("https://example.com/a\u{0}b"), None);
    }

    #[test]
    fn first_file_arg_ignores_flags_and_the_executable() {
        let exe = std::env::current_exe().unwrap().display().to_string();
        let args = vec![
            exe.clone(),
            "--flag".to_string(),
            "does-not-exist-98765.xlsx".to_string(),
        ];
        assert_eq!(first_file_arg(&args), None);

        // A real file is picked up even when it follows flags.
        let args = vec![
            exe,
            "--flag".to_string(),
            std::env::temp_dir().display().to_string(),
        ];
        // A directory is not a file, so this stays None.
        assert_eq!(first_file_arg(&args), None);
    }
}
