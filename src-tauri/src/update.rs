//! Updating from the GitHub release this build came from.
//!
//! The release manifest is the `latest.json` asset the release job attaches to
//! every release; the app asks for it at
//! `.../releases/latest/download/latest.json`, which always resolves to the most
//! recent published release.
//!
//! Two signatures stand between an answer from the network and a new version
//! being installed, and both are checked against a key this build trusts - the
//! public key in `tauri.conf.json`, and any key a signed manifest has handed
//! over since:
//!
//! * the manifest itself, by [`crate::manifest`], before anything it says is
//!   believed. TLS says the bytes came from GitHub; only that signature says
//!   they are the bytes this project published;
//! * the installer it points at, by the updater, while it is downloaded, with
//!   whichever of those keys signed it. A release that replaces the signing key
//!   can therefore hand the new key over and sign its installer with it, which
//!   is what makes a rotation something an installed copy carries out on its
//!   own.
//!
//! `requireSignedVersion` is on, so the installer also has to have been signed
//! *for the version the manifest announced*. A signature over an installer only
//! covers the installer, so without that check a rewritten manifest could pair a
//! new version number with the signature and URL of an older, genuinely signed
//! release and talk the app into reinstalling it. `tauri build` stamps the
//! version into the signature's trusted comment, the release job refuses a tag
//! that disagrees with the manifests, and the manifest is signed for the version
//! it announces - so all three numbers agree on every release this app can
//! update from.

use serde::Serialize;
use tauri::{AppHandle, Emitter};
use tauri_plugin_updater::UpdaterExt;

use crate::manifest;

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

/// An updater that checks a download against the key the release named, or -
/// when the manifest names none this build trusts, or none at all - the key this
/// build ships with, which is what the updater would have used on its own.
fn updater_for(
    app: &AppHandle,
    installer_key: Option<String>,
) -> Result<tauri_plugin_updater::Updater, tauri_plugin_updater::Error> {
    match installer_key {
        Some(key) => app.updater_builder().pubkey(key).build(),
        None => app.updater(),
    }
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
/// The manifest is fetched here rather than by the updater, so that the
/// signature published beside it can be checked first; `restore_update` then
/// applies the updater's own target selection and version comparison to the
/// document that was verified. It has always required an installer's signature
/// to bind the announced version, which is safe to insist on because the
/// announced version is now one this build checked.
///
/// `Ok(None)` is the ordinary "already current" answer. An error means the
/// question could not be asked at all - no network, no release yet, endpoint
/// unreachable, or a manifest nothing signed - which the UI reports without
/// treating it as a failure.
#[tauri::command]
pub async fn check_for_update(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    let verified = manifest::fetch(&app).await?;
    let updater = updater_for(&app, verified.installer_key()).map_err(describe)?;
    match updater
        .restore_update(verified.into_document())
        .map_err(describe)?
    {
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
/// The document is fetched and verified again rather than reused from the check
/// that offered the update, so an install follows a manifest that was verified
/// in this run: the offer and the install are separate invocations of the app,
/// and only the second one installs anything.
///
/// The installer's bytes are checked during the download against the key the
/// release names for this platform - the one this build ships with, or one the
/// same manifest handed over - so a package that does not match is never run:
/// the installer is only reached once that signature holds too.
///
/// On Windows the installer is then launched and takes over: it closes this
/// app, replaces it, and starts it again (`restart_after_install` defaults to
/// true). Exiting here is deliberate rather than restarting - the installer
/// brings the app back, and this is a single-instance app, so a restart of our
/// own would only race it.
#[tauri::command]
pub async fn install_update(app: AppHandle) -> Result<(), String> {
    let verified = manifest::fetch(&app).await?;
    let updater = updater_for(&app, verified.installer_key()).map_err(describe)?;
    let update = updater
        .restore_update(verified.into_document())
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
