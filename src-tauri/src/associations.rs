//! Windows shell integration: the "Scry Excel File" context-menu verb, the
//! file associations, and the Default Apps registration.
//!
//! Two separate things are happening here, and it is worth being precise about
//! what an installer can and cannot do on modern Windows:
//!
//! * **A context-menu verb is fully supported.** It is registered under
//!   `SystemFileAssociations\<ext>\shell\...` rather than under a ProgID, so
//!   "Scry Excel File" stays on the menu even when Excel or another viewer is
//!   the default handler.
//! * **Becoming the default viewer is the user's decision.** Since Windows 8
//!   the per-user `FileExts\<ext>\UserChoice` key is hash-protected, so no
//!   installer can silently take the association over — a write with a bad
//!   hash is simply ignored. ExcelScry therefore registers itself as a
//!   *candidate* (Capabilities + RegisteredApplications) and offers a button
//!   that opens the Default Apps page. It additionally writes the legacy
//!   `Software\Classes\<ext>` default, which Windows still honours on a profile
//!   that has no `UserChoice` yet, after backing up the previous value so that
//!   uninstalling is lossless.
//!
//! Macro-enabled workbooks (`.xlsm`) are registered alongside the rest and
//! read as plain data: nothing in ExcelScry executes or extracts the VBA
//! project, which makes it a safe way to look inside a workbook from an
//! untrusted source.
//!
//! OpenDocument spreadsheets (`.ods`) are registered too, and not only because
//! ExcelScry can write them: an `.ods` is read by the same viewer as an `.xlsx`,
//! so a spreadsheet from LibreOffice or Google Sheets gets the context menu and
//! the Default Apps entry that any Excel workbook gets.

use std::path::Path;

use serde::Serialize;

/// The context-menu label, exactly as requested.
pub const VERB_LABEL: &str = "Scry Excel File";
/// Registry key that holds the verb.
pub const VERB_KEY: &str = "ScryExcelFile";
/// `RegisteredApplications` value name, and the name used in the
/// `ms-settings:defaultapps?registeredAppUser=` deep link.
pub const APP_REG_NAME: &str = "ExcelScry";
const CAPABILITIES_PATH: &str = "Software\\ExcelScry\\ExcelScry\\Capabilities";
const BACKUP_PATH: &str = "Software\\ExcelScry\\Backup";

/// Extensions that get a ProgID, a context-menu verb and a Default Apps entry.
///
/// `(extension, ProgID, description)`. The app can *open* more formats than
/// this (`.xlsb`, `.xla`); these are the ones registered with the shell. Add a
/// row here — and a matching block plus a pair of `!insertmacro` lines in
/// `windows/hooks.nsh` — to extend it.
pub const EXTENSIONS: &[(&str, &str, &str)] = &[
    (".xlsx", "ExcelScry.xlsx", "ExcelScry Worksheet"),
    // Macro-enabled workbooks are readable because nothing here executes VBA;
    // see the note in the module docs.
    (
        ".xlsm",
        "ExcelScry.xlsm",
        "ExcelScry Macro-Enabled Workbook",
    ),
    (".xls", "ExcelScry.xls", "ExcelScry 97-2003 Worksheet"),
    // OpenDocument spreadsheets are viewable here, not just exportable: an
    // `.ods` from LibreOffice or Google Sheets opens in the same grid.
    (
        ".ods",
        "ExcelScry.ods",
        "ExcelScry OpenDocument Spreadsheet",
    ),
];

/// A single registry mutation, kept as data so the contract can be unit-tested
/// without writing to the real registry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegOp {
    SetString {
        key: String,
        name: String,
        value: String,
    },
    /// A `REG_NONE` value, which is what `OpenWithProgids` entries normally are.
    SetNone {
        key: String,
        name: String,
    },
    DeleteValue {
        key: String,
        name: String,
    },
    DeleteKey {
        key: String,
    },
}

/// The full registry contract for installing shell integration, relative to a
/// hive root (`HKCU` for the portable/self-register path, `HKLM` when the
/// installer runs per-machine). Kept in sync with `windows/hooks.nsh`.
pub fn register_ops(exe: &str) -> Vec<RegOp> {
    let command = command_for(Path::new(exe));
    let icon = format!("\"{exe}\",0");
    let mut ops = Vec::new();

    for (ext, progid, description) in EXTENSIONS {
        // The ProgID: what Explorer opens when this app owns the extension.
        ops.push(op_set(
            format!("Software\\Classes\\{progid}"),
            "",
            description,
        ));
        ops.push(op_set(
            format!("Software\\Classes\\{progid}"),
            "FriendlyTypeName",
            description,
        ));
        ops.push(op_set(
            format!("Software\\Classes\\{progid}\\DefaultIcon"),
            "",
            &icon,
        ));
        ops.push(op_set(
            format!("Software\\Classes\\{progid}\\shell\\open\\command"),
            "",
            &command,
        ));
        // Label the built-in open verb so the "Open with" list reads nicely.
        ops.push(op_set(
            format!("Software\\Classes\\{progid}\\shell\\open"),
            "",
            &format!("Open with {APP_REG_NAME}"),
        ));

        // Advertise the ProgID as an alternative handler for the extension.
        ops.push(RegOp::SetNone {
            key: format!("Software\\Classes\\{ext}\\OpenWithProgids"),
            name: progid.to_string(),
        });

        // The context-menu verb. This lives under SystemFileAssociations so it
        // is not tied to whichever ProgID currently owns the extension.
        let verb_key =
            format!("Software\\Classes\\SystemFileAssociations\\{ext}\\shell\\{VERB_KEY}");
        ops.push(op_set(verb_key.clone(), "", VERB_LABEL));
        ops.push(op_set(verb_key.clone(), "Icon", &icon));
        ops.push(op_set(format!("{verb_key}\\command"), "", &command));

        // Default Apps registration.
        ops.push(op_set(
            format!("{CAPABILITIES_PATH}\\FileAssociations"),
            ext,
            progid,
        ));
    }

    ops.push(op_set(CAPABILITIES_PATH, "ApplicationName", APP_REG_NAME));
    ops.push(op_set(
        CAPABILITIES_PATH,
        "ApplicationDescription",
        "ExcelScry - view Excel and OpenDocument spreadsheets",
    ));
    ops.push(op_set(
        "Software\\RegisteredApplications",
        APP_REG_NAME,
        CAPABILITIES_PATH,
    ));

    ops
}

/// The exact inverse of [`register_ops`].
pub fn unregister_ops() -> Vec<RegOp> {
    let mut ops = Vec::new();

    for (ext, progid, _) in EXTENSIONS {
        ops.push(RegOp::DeleteKey {
            key: format!("Software\\Classes\\{progid}"),
        });
        ops.push(RegOp::DeleteValue {
            key: format!("Software\\Classes\\{ext}\\OpenWithProgids"),
            name: progid.to_string(),
        });
        // Only our own verb is removed; other handlers' verbs are untouched.
        ops.push(RegOp::DeleteKey {
            key: format!("Software\\Classes\\SystemFileAssociations\\{ext}\\shell\\{VERB_KEY}"),
        });
        ops.push(RegOp::DeleteValue {
            key: format!("{CAPABILITIES_PATH}\\FileAssociations"),
            name: ext.to_string(),
        });
    }

    ops.push(RegOp::DeleteValue {
        key: "Software\\RegisteredApplications".to_string(),
        name: APP_REG_NAME.to_string(),
    });
    // Removes Capabilities and the Backup subkey together.
    ops.push(RegOp::DeleteKey {
        key: "Software\\ExcelScry".to_string(),
    });

    ops
}

fn op_set(key: impl Into<String>, name: &str, value: &str) -> RegOp {
    RegOp::SetString {
        key: key.into(),
        name: name.to_string(),
        value: value.to_string(),
    }
}

/// Build the `"exe" "%1"` command string used for the verb and the ProgID.
pub fn command_for(exe: &Path) -> String {
    format!("\"{}\" \"%1\"", exe.display())
}

/// Registration state for one extension.
///
/// Exposed per extension rather than as fixed fields so adding a format to
/// [`EXTENSIONS`] is all that is needed for the UI to show it.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExtensionStatus {
    pub ext: String,
    pub progid: String,
    /// What Windows currently opens this extension with, as a ProgID when known.
    pub current_handler: Option<String>,
    /// True when that handler is ExcelScry.
    pub is_default: bool,
    pub registered_for_user: bool,
    pub registered_machine: bool,
}

/// What the app knows about the current state of shell integration.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AssociationStatus {
    pub exe_path: String,
    /// Every extension's ProgID exists for the current user (portable
    /// self-registration).
    pub user_registered: bool,
    /// Every extension's ProgID exists machine-wide (the NSIS installer ran).
    pub machine_registered: bool,
    pub extensions: Vec<ExtensionStatus>,
    /// Deep link that opens the Default Apps page, pre-filtered to ExcelScry
    /// when a registration is present.
    pub settings_uri: String,
    /// Windows 11 hides legacy verbs behind "Show more options".
    pub context_menu_note: String,
}

/// Pick the `ms-settings:` deep link that actually works for this install.
/// `registeredAppMachine` only resolves for machine-wide registrations, and
/// `registeredAppUser` only for per-user ones.
pub fn settings_uri(user_registered: bool, machine_registered: bool) -> String {
    if user_registered {
        format!("ms-settings:defaultapps?registeredAppUser={APP_REG_NAME}")
    } else if machine_registered {
        format!("ms-settings:defaultapps?registeredAppMachine={APP_REG_NAME}")
    } else {
        "ms-settings:defaultapps".to_string()
    }
}

// ---------------------------------------------------------------------------
// Windows implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use super::*;

    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE, KEY_READ, REG_NONE};
    use winreg::{RegKey, RegValue};

    /// Apply a batch of operations to a hive root.
    fn apply(root: &RegKey, ops: &[RegOp]) -> Result<(), String> {
        for op in ops {
            let result = match op {
                RegOp::SetString { key, name, value } => root
                    .create_subkey(key)
                    .map_err(|e| format!("create {key}: {e}"))
                    .and_then(|(k, _)| {
                        k.set_value(name, value)
                            .map_err(|e| format!("set {key}\\{name}: {e}"))
                    }),
                RegOp::SetNone { key, name } => root
                    .create_subkey(key)
                    .map_err(|e| format!("create {key}: {e}"))
                    .and_then(|(k, _)| {
                        k.set_raw_value(
                            name,
                            &RegValue {
                                bytes: Vec::new().into(),
                                vtype: REG_NONE,
                            },
                        )
                        .map_err(|e| format!("set {key}\\{name}: {e}"))
                    }),
                // Cleanup is best-effort: a missing key is not a failure.
                RegOp::DeleteValue { key, name } => {
                    if let Ok(k) = root.open_subkey_with_flags(key, winreg::enums::KEY_ALL_ACCESS) {
                        let _ = k.delete_value(name);
                    }
                    Ok(())
                }
                RegOp::DeleteKey { key } => {
                    let _ = root.delete_subkey_all(key);
                    Ok(())
                }
            };
            result?;
        }
        Ok(())
    }

    /// Remember the extension's previous default handler, but only the first
    /// time, so repeated installs cannot overwrite the original with our own.
    fn backup_extension_default(root: &RegKey, ext: &str) -> Result<(), String> {
        let classes = root
            .open_subkey_with_flags("Software\\Classes", KEY_READ | winreg::enums::KEY_WRITE)
            .map_err(|e| format!("open Software\\Classes: {e}"))?;
        let previous: Option<String> = classes
            .open_subkey(ext)
            .ok()
            .and_then(|k| k.get_value::<String, _>("").ok());

        let (backup, _) = root
            .create_subkey(format!("{BACKUP_PATH}\\{ext}"))
            .map_err(|e| format!("create backup key: {e}"))?;

        // `HadValue` marks whether the extension had a default at all.
        if backup.get_value::<u32, _>("HadValue").is_err() {
            backup
                .set_value("HadValue", &u32::from(previous.is_some()))
                .map_err(|e| format!("write HadValue: {e}"))?;
            backup
                .set_value("Value", &previous.unwrap_or_default())
                .map_err(|e| format!("write Value: {e}"))?;
        }
        Ok(())
    }

    /// Restore whatever owned the extension before we did, then drop the backup.
    fn restore_extension_default(root: &RegKey, ext: &str) {
        let Ok(backup) = root.open_subkey(format!("{BACKUP_PATH}\\{ext}")) else {
            return;
        };
        let had_value = backup.get_value::<u32, _>("HadValue").unwrap_or(0) != 0;
        let previous: String = backup.get_value("Value").unwrap_or_default();

        if let Ok(classes) =
            root.open_subkey_with_flags("Software\\Classes", KEY_READ | winreg::enums::KEY_WRITE)
        {
            if let Ok(ext_key) = classes.open_subkey_with_flags(ext, winreg::enums::KEY_ALL_ACCESS)
            {
                let ours = EXTENSIONS
                    .iter()
                    .find(|(e, _, _)| *e == ext)
                    .map(|(_, progid, _)| *progid);
                let current: Option<String> = ext_key.get_value::<String, _>("").ok();
                // Only undo the change if the extension still points at us.
                if current.as_deref() == ours {
                    if had_value {
                        let _ = ext_key.set_value("", &previous);
                    } else {
                        let _ = ext_key.delete_value("");
                    }
                }
            }
        }
    }

    /// Tell Explorer that associations changed. Without this an already-running
    /// Explorer may keep serving a stale context menu.
    fn notify_shell() {
        use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};
        // SAFETY: SHChangeNotify with SHCNF_IDLIST takes no items; both item
        // pointers are null, which the API documents as valid for this flag.
        unsafe {
            SHChangeNotify(
                SHCNE_ASSOCCHANGED as i32,
                SHCNF_IDLIST,
                std::ptr::null(),
                std::ptr::null(),
            );
        }
    }

    pub fn exe_path() -> Option<std::path::PathBuf> {
        std::env::current_exe().ok()
    }

    fn hive(hklm: bool) -> RegKey {
        if hklm {
            RegKey::predef(HKEY_LOCAL_MACHINE)
        } else {
            RegKey::predef(HKEY_CURRENT_USER)
        }
    }

    /// Register for the current user only, which needs no elevation. This is
    /// what the portable build uses.
    pub fn register_current_user() -> Result<String, String> {
        let exe = exe_path().ok_or("Cannot determine the ExcelScry executable path.")?;
        let exe_str = exe.display().to_string();
        let root = hive(false);
        apply(&root, &register_ops(&exe_str))?;

        let mut notes = Vec::new();
        for (ext, progid, _) in EXTENSIONS {
            if let Err(e) = backup_extension_default(&root, ext) {
                notes.push(format!("could not back up {ext}: {e}"));
                continue;
            }
            // Best effort: honoured where no hash-protected UserChoice exists.
            if let Ok(classes) = root
                .open_subkey_with_flags("Software\\Classes", KEY_READ | winreg::enums::KEY_WRITE)
            {
                if let Ok((ext_key, _)) = classes.create_subkey(ext) {
                    if let Err(e) = ext_key.set_value("", progid) {
                        notes.push(format!("could not set default for {ext}: {e}"));
                    }
                }
            }
        }

        notify_shell();
        Ok(if notes.is_empty() {
            "Registered for the current user.".to_string()
        } else {
            format!("Registered with warnings: {}", notes.join("; "))
        })
    }

    /// Undo [`register_current_user`], restoring any backed-up default.
    pub fn unregister_current_user() -> Result<String, String> {
        let root = hive(false);
        for (ext, _, _) in EXTENSIONS {
            restore_extension_default(&root, ext);
        }
        apply(&root, &unregister_ops())?;
        notify_shell();
        Ok("Removed the ExcelScry context menu and file associations.".to_string())
    }

    fn progid_registered(hklm: bool, progid: &str) -> bool {
        hive(hklm)
            .open_subkey(format!("Software\\Classes\\{progid}"))
            .is_ok()
    }

    /// The ProgID Windows currently uses for an extension, if any.
    fn current_handler(ext: &str) -> Option<String> {
        let hkcu = hive(false);
        // 1. The user's hash-protected choice wins whenever it exists.
        if let Ok(k) = hkcu.open_subkey(format!(
            "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\FileExts\\{ext}\\UserChoice"
        )) {
            if let Ok(progid) = k.get_value::<String, _>("ProgId") {
                if !progid.is_empty() {
                    return Some(progid);
                }
            }
        }
        // 2. Then the per-user class mapping, 3. then the machine one.
        for hklm in [false, true] {
            if let Ok(k) = hive(hklm).open_subkey(format!("Software\\Classes\\{ext}")) {
                if let Ok(v) = k.get_value::<String, _>("") {
                    if !v.is_empty() {
                        return Some(v);
                    }
                }
            }
        }
        None
    }

    pub fn status() -> AssociationStatus {
        let extensions: Vec<ExtensionStatus> = EXTENSIONS
            .iter()
            .map(|(ext, progid, _)| {
                let current = current_handler(ext);
                ExtensionStatus {
                    ext: (*ext).to_string(),
                    progid: (*progid).to_string(),
                    is_default: handler_is_ours(current.as_deref(), progid),
                    current_handler: current,
                    registered_for_user: progid_registered(false, progid),
                    registered_machine: progid_registered(true, progid),
                }
            })
            .collect();

        let user_registered = extensions.iter().all(|e| e.registered_for_user);
        let machine_registered = extensions.iter().all(|e| e.registered_machine);

        AssociationStatus {
            exe_path: exe_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            user_registered,
            machine_registered,
            extensions,
            settings_uri: settings_uri(user_registered, machine_registered),
            context_menu_note: "On Windows 11 the entry appears under \"Show more options\"."
                .to_string(),
        }
    }

    /// True when the handler ProgID belongs to us.
    fn handler_is_ours(handler: Option<&str>, progid: &str) -> bool {
        handler.is_some_and(|h| h.eq_ignore_ascii_case(progid))
    }
}

#[cfg(not(windows))]
mod imp {
    use super::AssociationStatus;
    use std::path::PathBuf;

    pub fn exe_path() -> Option<PathBuf> {
        std::env::current_exe().ok()
    }

    pub fn register_current_user() -> Result<String, String> {
        Err("Shell integration is only implemented for Windows.".to_string())
    }

    pub fn unregister_current_user() -> Result<String, String> {
        Err("Shell integration is only implemented for Windows.".to_string())
    }

    pub fn status() -> AssociationStatus {
        AssociationStatus {
            exe_path: exe_path()
                .map(|p| p.display().to_string())
                .unwrap_or_default(),
            user_registered: false,
            machine_registered: false,
            extensions: super::EXTENSIONS
                .iter()
                .map(|(ext, progid, _)| super::ExtensionStatus {
                    ext: (*ext).to_string(),
                    progid: (*progid).to_string(),
                    current_handler: None,
                    is_default: false,
                    registered_for_user: false,
                    registered_machine: false,
                })
                .collect(),
            settings_uri: super::settings_uri(false, false),
            context_menu_note: "Shell integration is only implemented for Windows.".to_string(),
        }
    }
}

pub use imp::{register_current_user, status, unregister_current_user};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn context_menu_verb_uses_the_requested_label() {
        let ops = register_ops("C:\\Apps\\ExcelScry.exe");
        let label = ops.iter().find(|op| {
            matches!(op, RegOp::SetString { key, name, .. }
                if name.is_empty()
                    && key == "Software\\Classes\\SystemFileAssociations\\.xlsx\\shell\\ScryExcelFile")
        });
        match label {
            Some(RegOp::SetString { value, .. }) => assert_eq!(value, "Scry Excel File"),
            other => panic!("verb label not registered, got {other:?}"),
        }
    }

    #[test]
    fn every_supported_extension_gets_a_verb_progid_and_capability() {
        let ops = register_ops("C:\\Apps\\ExcelScry.exe");
        for (ext, progid, _) in EXTENSIONS {
            let verb_cmd = format!(
                "Software\\Classes\\SystemFileAssociations\\{ext}\\shell\\{VERB_KEY}\\command"
            );
            assert!(
                ops.iter()
                    .any(|op| matches!(op, RegOp::SetString { key, name, .. }
                    if *key == verb_cmd && name.is_empty())),
                "missing context-menu command for {ext}"
            );
            assert!(
                ops.iter()
                    .any(|op| matches!(op, RegOp::SetNone { key, name }
                    if *key == format!("Software\\Classes\\{ext}\\OpenWithProgids")
                        && name == progid)),
                "missing OpenWithProgids entry for {ext}"
            );
            assert!(
                ops.iter()
                    .any(|op| matches!(op, RegOp::SetString { key, name, .. }
                    if *key == format!("{CAPABILITIES_PATH}\\FileAssociations") && name == ext)),
                "missing capability for {ext}"
            );
        }
    }

    #[test]
    fn open_command_quotes_the_exe_and_the_file_argument() {
        let ops = register_ops("C:\\Program Files\\ExcelScry\\ExcelScry.exe");
        let cmd = ops
            .iter()
            .find_map(|op| match op {
                RegOp::SetString { key, value, name }
                    if name.is_empty()
                        && key == "Software\\Classes\\ExcelScry.xlsx\\shell\\open\\command" =>
                {
                    Some(value)
                }
                _ => None,
            })
            .expect("open command registered");
        // Both the executable path (which contains a space) and %1 must be
        // quoted, or Windows will not launch the app with the file.
        assert_eq!(
            cmd,
            "\"C:\\Program Files\\ExcelScry\\ExcelScry.exe\" \"%1\""
        );
    }

    #[test]
    fn unregister_is_the_exact_inverse_of_register() {
        let exe = "C:\\Apps\\ExcelScry.exe";
        assert_eq!(command_for(Path::new(exe)), format!("\"{exe}\" \"%1\""));

        let ops = register_ops(exe);
        let undo = unregister_ops();

        // Each advert must be removed under its own ProgID; using the wrong
        // name would leave ExcelScry on the Open-with list after uninstall.
        for (ext, progid, _) in EXTENSIONS {
            let key = format!("Software\\Classes\\{ext}\\OpenWithProgids");
            assert!(
                undo.iter()
                    .any(|op| matches!(op, RegOp::DeleteValue { key: k, name }
                    if *k == key && name == progid)),
                "OpenWithProgids entry for {ext} is not removed"
            );
        }

        // Everything else register creates must be covered by a delete: either
        // an ancestor key being removed wholesale, or the exact value.
        for created in ops.iter().filter_map(|op| match op {
            RegOp::SetString { key, .. } | RegOp::SetNone { key, .. } => Some(key.clone()),
            _ => None,
        }) {
            if created.ends_with("OpenWithProgids") {
                continue;
            }
            let covered = undo.iter().any(|op| match op {
                RegOp::DeleteKey { key } => created.starts_with(key.as_str()),
                RegOp::DeleteValue { key, .. } => &created == key,
                _ => false,
            });
            assert!(
                covered,
                "registry key not cleaned up on uninstall: {created}"
            );
        }
    }

    #[test]
    fn macro_enabled_workbooks_are_registered_like_any_other() {
        let ops = register_ops("C:\\Apps\\ExcelScry.exe");
        let verb = "Software\\Classes\\SystemFileAssociations\\.xlsm\\shell\\ScryExcelFile";
        assert!(
            ops.iter()
                .any(|op| matches!(op, RegOp::SetString { key, name, value }
                if key == verb && name.is_empty() && value == "Scry Excel File")),
            ".xlsm does not get the context-menu verb"
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, RegOp::SetString { key, name, .. }
                if *key == format!("{verb}\\command") && name.is_empty())),
            ".xlsm verb has no command"
        );
        assert!(
            ops.iter().any(|op| matches!(op, RegOp::SetNone { key, name }
                if *key == "Software\\Classes\\.xlsm\\OpenWithProgids" && name == "ExcelScry.xlsm")),
            ".xlsm is not advertised as a handler"
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, RegOp::SetString { key, name, .. }
                if *key == format!("{CAPABILITIES_PATH}\\FileAssociations") && name == ".xlsm")),
            ".xlsm has no Default Apps capability"
        );
    }

    #[test]
    fn opendocument_spreadsheets_get_the_same_shell_integration() {
        // `.ods` is a format ExcelScry reads, not just one it writes, so it has
        // to reach the context menu and Default Apps like an Excel workbook.
        let ops = register_ops("C:\\Apps\\ExcelScry.exe");
        let verb = "Software\\Classes\\SystemFileAssociations\\.ods\\shell\\ScryExcelFile";
        assert!(
            ops.iter()
                .any(|op| matches!(op, RegOp::SetString { key, name, value }
                if key == verb && name.is_empty() && value == "Scry Excel File")),
            ".ods does not get the context-menu verb"
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, RegOp::SetString { key, name, .. }
                if *key == format!("{verb}\\command") && name.is_empty())),
            ".ods verb has no command"
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, RegOp::SetNone { key, name }
                if *key == "Software\\Classes\\.ods\\OpenWithProgids" && name == "ExcelScry.ods")),
            ".ods is not advertised as a handler"
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, RegOp::SetString { key, name, .. }
                if *key == format!("{CAPABILITIES_PATH}\\FileAssociations") && name == ".ods")),
            ".ods has no Default Apps capability"
        );
        assert!(
            ops.iter()
                .any(|op| matches!(op, RegOp::SetString { key, name, value }
                if key == "Software\\Classes\\ExcelScry.ods" && name.is_empty()
                    && value == "ExcelScry OpenDocument Spreadsheet")),
            ".ods has no ProgID description"
        );
    }

    #[test]
    fn the_nsis_hooks_write_the_same_contract_as_the_runtime() {
        // The installer and the portable build register the shell integration
        // through two different mechanisms, and nothing compiles one against
        // the other. A format added to EXTENSIONS and forgotten in hooks.nsh
        // would be registered for portable installs and invisible to installed
        // ones, so the two are compared here.
        let script = std::fs::read_to_string(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("windows")
                .join("hooks.nsh"),
        )
        .expect("windows/hooks.nsh should be readable");

        for (ext, progid, description) in EXTENSIONS {
            assert!(
                script.contains(&format!("\"{ext}\"")),
                "hooks.nsh does not mention {ext}"
            );
            assert!(
                script.contains(&format!("\"{progid}\"")),
                "hooks.nsh does not register the {progid} ProgID"
            );
            assert!(
                script.contains(&format!("\"{description}\"")),
                "hooks.nsh does not describe {ext} as {description}"
            );
        }

        let associates = script.matches("!insertmacro ScryAssociate ").count();
        let disassociates = script.matches("!insertmacro ScryDisassociate ").count();
        assert_eq!(
            associates,
            EXTENSIONS.len(),
            "hooks.nsh registers {associates} extensions, the runtime knows {}",
            EXTENSIONS.len()
        );
        assert_eq!(
            disassociates,
            EXTENSIONS.len(),
            "hooks.nsh unregisters {disassociates} extensions, the runtime knows {}",
            EXTENSIONS.len()
        );
    }

    #[test]
    fn every_registered_extension_has_a_distinct_progid() {
        let mut progids: Vec<&str> = EXTENSIONS.iter().map(|(_, p, _)| *p).collect();
        progids.sort_unstable();
        let before = progids.len();
        progids.dedup();
        assert_eq!(before, progids.len(), "two extensions share a ProgID");
        assert!(
            EXTENSIONS.iter().any(|(ext, _, _)| *ext == ".xlsm"),
            "macro-enabled workbooks are not in the registration list"
        );
        assert!(
            EXTENSIONS.iter().any(|(ext, _, _)| *ext == ".ods"),
            "OpenDocument spreadsheets are not in the registration list"
        );
    }

    #[test]
    fn settings_deep_link_matches_the_registration_kind() {
        // machine and user registrations use different parameters; mixing them
        // silently produces a Settings page that cannot find the app.
        assert_eq!(
            settings_uri(true, true),
            "ms-settings:defaultapps?registeredAppUser=ExcelScry"
        );
        assert_eq!(
            settings_uri(false, true),
            "ms-settings:defaultapps?registeredAppMachine=ExcelScry"
        );
        assert_eq!(settings_uri(false, false), "ms-settings:defaultapps");
    }
}
