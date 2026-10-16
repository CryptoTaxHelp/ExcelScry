//! Updating from the GitHub release this build came from.
//!
//! The release manifest is the `latest.json` asset `tauri-action` attaches to
//! every release built by the workflow; the app asks for it at
//! `.../releases/latest/download/latest.json`, which always resolves to the
//! most recent published release.
//!
//! The downloaded installer is verified against the public key in
//! `tauri.conf.json` before it is allowed to run. That key is the half that
//! matters here: anyone can replace an asset in a repository they can push to,
//! but only the holder of the private key can produce an installer this app
//! will accept.
//!
//! `requireSignedVersion` is on, so the artifact also has to have been signed
//! *for the version the manifest announced*. The manifest itself is not signed -
//! it arrives over TLS from a release anyone with push access could rewrite - so
//! without that check a crafted manifest could pair a new version number with
//! the signature and URL of an older, genuinely signed release and talk the app
//! into reinstalling it. `tauri build` stamps the version into the signature's
//! trusted comment, and the release job refuses a tag that disagrees with the
//! manifests, so the two numbers agree on every release this app can update
//! from.

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

/// An update the release manifest announced.
#[derive(Debug, Serialize)]
pub struct UpdateInfo {
    /// The version the release announces.
    pub version: String,
    /// The version running now, so the UI can say "0.1.0 -> 0.1.1".
    pub current_version: String,
    /// The release notes from the manifest, when it carries them.
    pub notes: Option<String>,
}

/// How far along a download is, emitted as `update-progress`.
#[derive(Debug, Clone, Copy, Serialize)]
pub struct UpdateProgress {
    pub downloaded: u64,
    /// `None` when the server sends no content length.
    pub total: Option<u64>,
}

/// Turn an updater failure into something worth putting in a status bar.
///
/// The plugin's own messages are decent but a couple of them are worth saying
/// in the app's terms: a signature mismatch is the one outcome a user of a
/// self-updating app should never have to read a stack trace about.
fn describe(error: tauri_plugin_updater::Error) -> String {
    use tauri_plugin_updater::Error;
    match error {
        Error::EmptyEndpoints => "This build has no update endpoint configured.".into(),
        Error::Minisign(_) => {
            "The download did not match its signature, so it was not installed.".into()
        }
        Error::SignedVersionMismatch { signed, announced } => format!(
            "The release manifest announced {announced}, but the update was signed for \
             {signed}. It was not installed."
        ),
        Error::MissingSignedVersion => {
            "The update was signed without a version, which this build requires.".into()
        }
        other => other.to_string(),
    }
}

/// The version this build reports.
///
/// Read from the bundle's own manifest rather than a string copied into the
/// front end, so the dialog names the version the installer wrote and the
/// start-up check compares against the same number the release was signed for.
#[tauri::command]
pub fn app_version(app: AppHandle) -> String {
    app.package_info().version.to_string()
}

/// Ask the release manifest whether a newer version exists.
///
/// `Ok(None)` is the ordinary "already current" answer. An error means the
/// question could not be asked at all - no network, no release yet, endpoint
/// unreachable - which the UI reports without treating it as a failure.
#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let updater = app.updater().map_err(describe)?;
    match updater.check().await.map_err(describe)? {
        Some(update) => Ok(Some(UpdateInfo {
            version: update.version.clone(),
            current_version: update.current_version.clone(),
            notes: update.body.clone(),
        })),
        None => Ok(None),
    }
}

/// Download the announced update, verify it, and hand it to its installer.
///
/// The bytes are checked against the configured public key during the
/// download, so a package that does not match is never run - the installer is
/// only reached once the signature holds.
///
/// On Windows the installer is then launched and takes over: it closes this
/// app, replaces it, and starts it again (`restart_after_install` defaults to
/// true). Exiting here is deliberate rather than restarting - the installer
/// brings the app back, and this is a single-instance app, so a restart of our
/// own would only race it.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let updater = app.updater().map_err(describe)?;
    let update = updater
        .check()
        .await
        .map_err(describe)?
        .ok_or("There is no update to install.")?;

    let progress_app = app.clone();
    let mut downloaded = 0u64;
    update
        .download_and_install(
            move |chunk, total| {
                downloaded += chunk as u64;
                let _ = progress_app.emit("update-progress", UpdateProgress { downloaded, total });
            },
            || {},
        )
        .await
        .map_err(describe)?;

    app.exit(0);
    Ok(())
}
