// Hide the console window in release builds; a grid viewer should not flash a
// terminal when Windows launches it from a file association.
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod associations;
mod model;
mod ods_export;
mod workbook;

use std::path::{Path, PathBuf};
use std::sync::Mutex;

use tauri::{Emitter, Manager, State};
use tauri_plugin_dialog::DialogExt;

use associations::AssociationStatus;
use model::{RowsPage, WorkbookInfo};
use ods_export::ExportSummary;

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
}

// ---------------------------------------------------------------------------
// Opening files
// ---------------------------------------------------------------------------

/// Parse a spreadsheet and make it the open workbook.
#[tauri::command]
async fn open_workbook(path: String, state: State<'_, AppState>) -> Result<WorkbookInfo, String> {
    let requested = PathBuf::from(&path);
    let loaded = tauri::async_runtime::spawn_blocking(move || workbook::load(&requested))
        .await
        .map_err(|e| format!("Could not read the file: {e}"))??;

    let info = loaded.info();
    *state
        .workbook
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(loaded);
    Ok(info)
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
            &["xlsx", "xlsm", "xlsb", "xls", "xla", "ods"],
        )
        .add_filter("Excel workbook", &["xlsx", "xlsm", "xls"])
        .blocking_pick_file();

    match picked {
        None => Ok(None),
        Some(file) => file
            .into_path()
            .map(|p| Some(p.display().to_string()))
            .map_err(|e| format!("Could not use the selected file: {e}")),
    }
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
    state.with_workbook(|wb| wb.page(sheet, row_start, row_count, col_start, col_count))?
        .ok_or_else(|| "That worksheet no longer exists.".to_string())
}

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
    let mut builder = tauri::Builder::default().plugin(tauri_plugin_dialog::init());

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
            initial_file,
            close_workbook,
            get_rows,
            export_ods,
            association_status,
            install_associations,
            remove_associations,
            open_default_apps_settings,
            reveal_in_explorer,
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
    fn first_file_arg_ignores_flags_and_the_executable() {
        let exe = std::env::current_exe().unwrap().display().to_string();
        let args = vec![
            exe.clone(),
            "--flag".to_string(),
            "does-not-exist-98765.xlsx".to_string(),
        ];
        assert_eq!(first_file_arg(&args), None);

        // A real file is picked up even when it follows flags.
        let args = vec![exe, "--flag".to_string(), std::env::temp_dir().display().to_string()];
        // A directory is not a file, so this stays None.
        assert_eq!(first_file_arg(&args), None);
    }
}
