//! Checking that the update manifest is the one this project published.
//!
//! `latest.json` tells an installed copy that a version exists, where its
//! installer lives, and what the release says about itself. The updater verifies
//! the *installer* against the public key in `tauri.conf.json`, and with
//! `requireSignedVersion` on, that signature also binds the version the manifest
//! announced. The manifest itself, though, arrives over TLS, and TLS says only
//! that the bytes came from GitHub - not that GitHub's bytes are the ones this
//! project put there. Anything holding a token that may push to this repository
//! can rewrite a release page, and a local intermediary with a certificate the
//! machine trusts can rewrite the response body unnoticed. Either could point
//! the app at a different, genuinely signed release, or change the notes the
//! user is shown before they agree to install anything.
//!
//! So the release job signs the manifest too, with the same key as the updater
//! artifacts, and publishes the signature beside it as `latest.json.sig`. This
//! module fetches both, checks the signature over the manifest's own bytes, and
//! only then hands the manifest to the updater. The endpoint still has to serve
//! the right bytes; it no longer gets to decide what they say.
//!
//! A manifest may also carry a `keys` block - keys to start accepting, and keys
//! to stop - which is how the signing key is replaced. Changing the key in
//! `tauri.conf.json` alone would need a release that every installed copy
//! installs, from a key the copy already has, before the new key means anything
//! anywhere; a roll inside the signed manifest instead reaches a copy with the
//! release it is already installing, so a key can be handed over, and a key
//! that has leaked can be retired, without one. What a manifest can do to the
//! trust is merge-only: it adds and retires, and nothing it says puts a retired
//! key back.
//!
//! The checks are the updater's own, applied to the manifest instead of to the
//! installer: the signature first, because the trusted comment - and the version
//! in it - means nothing until the signature that covers it has been verified,
//! and then that version against the one the manifest announces.

use std::path::{Path, PathBuf};
use std::time::Duration;

use base64::Engine as _;
use minisign_verify::{PublicKey, Signature};
use reqwest::Url;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use tauri::{utils::config::BundleType, AppHandle, Manager};
use tauri_plugin_updater::Config;

/// The file, beside this build's other per-user data, that a key roll is
/// recorded in.
const KEY_STORE: &str = "updater-keys.json";

/// What this build calls itself when it asks for the manifest.
const USER_AGENT: &str = concat!("ExcelScry/", env!("CARGO_PKG_VERSION"));

/// How long the manifest and its signature may take, together.
///
/// The updater leaves this to `reqwest`'s default, which is no timeout at all.
/// That is tolerable for a request nobody is waiting on, but these are two small
/// files the status bar is waiting on: "checking…" for ever is worse than saying
/// the release could not be reached.
const TIMEOUT: Duration = Duration::from_secs(30);

/// The release manifest the configured endpoint serves, once the signature
/// published beside it has been checked.
///
/// The document returned is the one that was verified, so it can be handed
/// straight to [`tauri_plugin_updater::Updater::restore_update`], with nothing
/// in between having had a chance to change it. The key roll it carries is
/// applied before it is returned, so a release that signs its installer with a
/// key it has just handed over is one this copy can install - and a release
/// that retires a key means this copy stops accepting it.
///
/// # Errors
///
/// A message written to be shown as it is: the endpoint is missing or
/// unreachable, the manifest or its signature could not be fetched, the
/// signature does not cover the manifest, the manifest is not a release
/// manifest, or its key roll could not be carried out. None of those is a
/// reason to install anything.
pub async fn fetch(app: &AppHandle) -> Result<Verified, String> {
    let config = config(app)?;
    let mut trust = Trust::for_app(app, &config.pubkey)?;
    fetch_with(&config, &app.package_info().version.to_string(), &mut trust).await
}

/// The half of [`fetch`] that talks to the network, over settings given to it
/// rather than read from a running app, so a test can point it at a server it
/// runs itself, and over a trust given to it the same way.
async fn fetch_with(
    config: &Config,
    current_version: &str,
    trust: &mut Trust,
) -> Result<Verified, String> {
    let manifest_url = endpoint(config, current_version)?;
    let signature_url = signature_url(&manifest_url)?;

    install_tls_provider();
    let mut client = reqwest::Client::builder()
        .user_agent(USER_AGENT)
        .timeout(TIMEOUT);
    // The updater honours these, and this module stands in for its request: a
    // build that had to set one would otherwise check for updates differently
    // from the way it installs them.
    if config.dangerous_accept_invalid_certs {
        client = client.danger_accept_invalid_certs(true);
    }
    if config.dangerous_accept_invalid_hostnames {
        client = client.danger_accept_invalid_hostnames(true);
    }
    let client = client
        .build()
        .map_err(|error| format!("Could not prepare the update check: {error}"))?;

    let document = get(&client, manifest_url.as_str()).await.map_err(|error| {
        format!(
            "The update manifest at {manifest_url} {}.",
            error.describe()
        )
    })?;

    let signature = match get(&client, signature_url.as_str()).await {
        Ok(bytes) => String::from_utf8(bytes)
            .map_err(|_| "The update manifest's signature is not text.".to_string())?,
        // A release published before the manifest was signed, and a release
        // whose signing step failed, both look exactly like this: the manifest
        // is there and nothing vouches for it. Saying so beats a bare 404.
        Err(error) if error.is_missing() => {
            return Err(format!(
                "The release publishes no signed manifest ({signature_url}), so its manifest was \
                 not used."
            ));
        }
        Err(error) => {
            return Err(format!(
                "The update manifest's signature at {signature_url} {}.",
                error.describe()
            ));
        }
    };

    let document = verify(&document, &signature, trust, config.require_signed_version)?;

    // Before the manifest is used for anything, and after its signature has
    // been checked: what it says about keys is covered by that signature, and
    // the key it hands over is one this copy is about to accept an installer
    // from.
    trust.adopt(&document)?;

    Ok(Verified {
        document,
        trust: trust.clone(),
    })
}

/// The updater settings from `tauri.conf.json`, parsed with the updater's own
/// type.
///
/// The plugin deserializes the same `plugins.updater` block, so reading it
/// through its [`Config`] is what keeps this module from having an opinion of
/// its own about the endpoint, the public key, or `requireSignedVersion`.
fn config(app: &AppHandle) -> Result<Config, String> {
    app.config()
        .plugins
        .0
        .get("updater")
        .cloned()
        .ok_or_else(|| {
            "This build has no updater settings, so it cannot check for updates.".to_string()
        })
        .and_then(|value| {
            serde_json::from_value(value)
                .map_err(|error| format!("This build's updater settings are not usable: {error}"))
        })
}

/// The updater's endpoint, with its `{{...}}` variables resolved.
///
/// The updater substitutes these on the way into its own request, and this
/// module makes that request instead, so it resolves them the same way, with the
/// same values. A URL without variables, which is what this project configures,
/// passes through untouched.
fn endpoint(config: &Config, current_version: &str) -> Result<Url, String> {
    let base = config
        .endpoints
        .first()
        .ok_or_else(|| "This build has no update endpoint configured.".to_string())?;

    // `{{target}}` is the operating system on its own and `{{arch}}` the
    // architecture, which is how the updater splits the target string it reports
    // - `windows-x86_64`.
    let target = tauri_plugin_updater::target().ok_or_else(|| {
        "This build cannot be updated: its platform has no updater target.".to_string()
    })?;
    let (os, arch) = target.split_once('-').ok_or_else(|| {
        format!("This build cannot be updated: its updater target {target:?} is malformed.")
    })?;
    let installer = installer_name().unwrap_or("unknown");

    // `+` is not a legal version character in a URL path, so the updater
    // percent-encodes it here too.
    let version = current_version.replace('+', "%2B");

    // `Url` percent-encodes the braces of a variable when it parses a path, so a
    // template in the path arrives as `%7B%7Btarget%7D%7D` while one in a query
    // string keeps its braces. Both spellings have to be replaced, which is why
    // the updater substitutes eight forms rather than four.
    let text = base
        .to_string()
        .replace("%7B%7Bcurrent_version%7D%7D", &version)
        .replace("%7B%7Btarget%7D%7D", os)
        .replace("%7B%7Barch%7D%7D", arch)
        .replace("%7B%7Bbundle_type%7D%7D", installer)
        .replace("{{current_version}}", &version)
        .replace("{{target}}", os)
        .replace("{{arch}}", arch)
        .replace("{{bundle_type}}", installer);

    Url::parse(&text).map_err(|error| format!("This build's update endpoint is not a URL: {error}"))
}

/// The name the updater gives the package type this build is installed from, as
/// a `{{bundle_type}}` variable spells it - `nsis` for the installer this
/// project publishes.
fn installer_name() -> Option<&'static str> {
    match tauri::utils::platform::bundle_type()? {
        BundleType::Nsis => Some("nsis"),
        BundleType::Msi => Some("msi"),
        BundleType::AppImage => Some("appimage"),
        BundleType::Deb => Some("deb"),
        BundleType::Rpm => Some("rpm"),
        BundleType::App => Some("app"),
        // A disk image is not something the updater installs from.
        BundleType::Dmg => None,
    }
}

/// Where a manifest's signature is looked for: beside it, with `.sig` appended.
///
/// That is the name the release job publishes, and it is the only thing that can
/// be derived from the endpoint, so a signature is expected there whatever else
/// a server may offer. The suffix goes on the end of the path rather than the
/// end of the URL, so a query string or fragment survives.
fn signature_url(manifest: &Url) -> Result<Url, String> {
    let text = manifest.as_str();
    let end = text.find(['?', '#']).unwrap_or(text.len());
    let mut signed = String::with_capacity(text.len() + 4);
    signed.push_str(&text[..end]);
    signed.push_str(".sig");
    signed.push_str(&text[end..]);
    Url::parse(&signed).map_err(|error| {
        format!("Could not work out where the manifest's signature lives: {error}")
    })
}

/// The process-wide rustls provider, installed before the first request.
///
/// The updater links rustls without a provider and installs one on the way into
/// its own first request; this module gets there first. Installing it twice is
/// harmless - the second attempt returns an error that is ignored.
///
/// Where this sits matters: `reqwest` panics while building a client if no
/// provider has been installed, so it has to come before the client above is
/// built. The loopback tests in this module fail if it is ever moved after it.
fn install_tls_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// Why a request for one of the two files did not produce one.
enum Fetch {
    /// The server answered, but not with a successful status.
    Status(u16),
    /// There was no answer at all: no network, no DNS, a refused connection.
    Transport(String),
}

impl Fetch {
    /// How the request failed, for a caller that has already named the URL.
    fn describe(&self) -> String {
        match self {
            Fetch::Status(code) => format!("answered {code}"),
            Fetch::Transport(reason) => format!("could not be reached: {reason}"),
        }
    }

    fn is_missing(&self) -> bool {
        matches!(self, Fetch::Status(404))
    }
}

/// Fetch a URL and hand back its bytes, without interpreting them.
///
/// The manifest is verified as the bytes that arrived, so nothing here may
/// re-encode them: a signature covers bytes, and a document that has been parsed
/// and written out again is not the one that was signed.
async fn get(client: &reqwest::Client, url: &str) -> Result<Vec<u8>, Fetch> {
    let response = client
        .get(url)
        .send()
        .await
        .map_err(|error| Fetch::Transport(error.to_string()))?;
    let status = response.status();
    if !status.is_success() {
        return Err(Fetch::Status(status.as_u16()));
    }
    response
        .bytes()
        .await
        .map(|bytes| bytes.to_vec())
        .map_err(|error| Fetch::Transport(error.to_string()))
}

/// Check a manifest against the signature published beside it.
///
/// Returns the manifest as a document, so a caller cannot reach the fields
/// without having come through here.
fn verify(
    document: &[u8],
    signature: &str,
    trust: &Trust,
    require_signed_version: bool,
) -> Result<Value, String> {
    let signature = minisign_signature(signature)?;

    // The updater accepts minisign's legacy non-prehashed form as well as the
    // prehashed one `tauri build` writes, and so does this: which generation of
    // the signer produced a signature says nothing about whether it is the right
    // signature, and the updater is the one that has to accept the artifacts.
    trust.check(document, &signature)?;

    // Only now, with the global signature over the trusted comment verified, is
    // that comment worth reading: it is attacker-controlled data until then.
    let signed_for = signed_version(signature.trusted_comment());

    let document: Value = serde_json::from_slice(document)
        .map_err(|error| format!("The update manifest is not a release manifest: {error}"))?;
    let announced = document
        .get("version")
        .and_then(Value::as_str)
        .ok_or_else(|| "The update manifest announces no version.".to_string())?;

    match signed_for {
        Some(signed_for) if !same_version(signed_for, announced) => Err(format!(
            "The update manifest was signed for {signed_for} but announces {announced}, so it was \
             not used."
        )),
        None if require_signed_version => Err(
            "The update manifest was signed without a version, which this build requires."
                .to_string(),
        ),
        _ => Ok(document),
    }
}

/// The key a field in this build's trust names.
///
/// A field carries either base64 of a minisign public key file - a comment line
/// and the key, which is how `tauri signer generate` writes it, how the updater
/// reads the `pubkey` in `tauri.conf.json`, and how this project writes a key
/// into a release's roll - or the base64 of the key on its own.
fn public_key(field: &str) -> Result<PublicKey, String> {
    parse_public_key(field, KEY_WORDING).map(|(key, _)| key)
}

/// What to call a key this build was shipped with or has adopted, in a message.
const KEY_WORDING: &str = "A key this build trusts for updates";

/// The public key `field` names, and the id minisign identifies it by.
///
/// `what` names the field in a message, so that a key that does not parse is
/// reported as the wrong thing in the place it was written.
fn parse_public_key(field: &str, what: &str) -> Result<(PublicKey, [u8; 8]), String> {
    let base64 = public_key_base64(field, what)?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(&base64)
        .map_err(|error| format!("{what} is not base64: {error}"))?;
    if decoded.len() != 42 {
        return Err(format!(
            "{what} is {} bytes, not the 42 of a minisign public key.",
            decoded.len()
        ));
    }
    let key =
        PublicKey::from_base64(&base64).map_err(|error| format!("{what} is unusable: {error}"))?;

    // The two algorithm bytes, then the id, then the key itself: the id is what
    // a signature names, and so what a retirement is recorded against.
    let mut id = [0u8; 8];
    id.copy_from_slice(&decoded[2..10]);
    Ok((key, id))
}

/// The base64 of a minisign public key, from a field that carries either the
/// whole public key file or the key on its own.
fn public_key_base64(field: &str, what: &str) -> Result<String, String> {
    let field = field.trim();
    if field.is_empty() {
        return Err(format!("{what} is empty."));
    }

    // The file form: base64 of a comment line and then the key. A field that
    // decodes to text which is not a key file is the key itself, or is not a
    // key at all - either way, the caller's own decoding settles it.
    if let Ok(text) = decode_base64(field, what) {
        let mut lines = text.lines().map(str::trim).filter(|line| !line.is_empty());
        if let (Some(comment), Some(key)) = (lines.next(), lines.next()) {
            if comment.starts_with("untrusted comment:") {
                return Ok(key.to_string());
            }
        }
    }

    Ok(field.to_string())
}

/// The id of the key a signature names.
///
/// A manifest's `signature` field for a platform is the same encoding a `.sig`
/// file has: base64 of a minisign signature, whose first base64 line begins
/// with the two-byte algorithm marker and then the eight-byte id of the key
/// that made it.
fn signature_key_id(field: &str) -> Option<[u8; 8]> {
    let text = decode_base64(field.trim(), "A signature").ok()?;
    let line = text.lines().nth(1)?.trim();
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(line)
        .ok()?;
    if decoded.len() != 74 {
        return None;
    }
    let mut id = [0u8; 8];
    id.copy_from_slice(&decoded[2..10]);
    Some(id)
}

/// A manifest whose signature has been checked, and the trust it left behind.
///
/// The document is handed on exactly as it was verified: nothing that reads a
/// field out of it has had a chance to change one, and the key roll it carries
/// has already been taken.
#[derive(Debug)]
pub struct Verified {
    document: Value,
    trust: Trust,
}

impl Verified {
    /// The manifest itself, for the updater to read.
    pub fn into_document(self) -> Value {
        self.document
    }

    /// The key this platform's installer is signed with, when this build trusts
    /// it.
    ///
    /// A release may sign its installer with a key the manifest handed over in
    /// the same breath, which is what makes a rotation something an installed
    /// copy carries out on its own. `None` means the manifest names no artifact
    /// for this platform, carries no usable signature for one, or names a key
    /// this build does not trust: the updater is then left with the key this
    /// build ships with, and refuses anything that does not match it.
    pub fn installer_key(&self) -> Option<String> {
        let target = tauri_plugin_updater::target()?;
        let signature = self
            .document
            .get("platforms")?
            .get(target)?
            .get("signature")?
            .as_str()?;
        self.trust.key_for_signature(signature)
    }
}

/// The keys this build will accept a release from.
///
/// The anchor is the public key compiled into this build. A signed manifest can
/// do two things to it: hand over another key, which this build then accepts as
/// well, and retire a key, which it stops accepting. That is what lets the
/// signing key be replaced - because it is due for replacement, or because it
/// has leaked - without the release that does it being signed by the key being
/// given up.
///
/// A roll is merge-only, and retirements are matched by key id rather than by
/// the bytes of the key, because the id is what a signature names: nothing an
/// older manifest says puts a retired key back, and a build that later compiles
/// in a retired key still refuses it.
#[derive(Debug, Clone, Default)]
struct Trust {
    /// The key from `tauri.conf.json`, in the encoding that field uses.
    configured: String,
    /// Keys adopted from signed manifests, in the order they arrived.
    adopted: Vec<String>,
    /// Keys that signed manifests have retired, as those manifests named them.
    retired: Vec<String>,
    /// Where a roll is recorded; `None` when this build has nowhere to keep it.
    store: Option<PathBuf>,
}

/// What a key roll leaves behind: the keys this build has adopted, and the ones
/// a signed manifest has retired.
#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    #[serde(default)]
    adopted: Vec<String>,
    #[serde(default)]
    retired: Vec<String>,
}

impl Trust {
    /// The trust this build starts a check with: the key it ships with, plus
    /// whatever a signed manifest has handed over before.
    ///
    /// A record that cannot be read refuses the check rather than being ignored.
    /// It is the only place a retirement is written down, so carrying on
    /// without it would quietly start trusting a key this build had been told
    /// to stop trusting - and the message names the file to delete to go back
    /// to the key this build ships with.
    fn for_app(app: &AppHandle, configured: &str) -> Result<Self, String> {
        let directory = app.path().app_local_data_dir().map_err(|error| {
            format!("This build has nowhere to keep a record of updater keys: {error}")
        })?;
        Trust::read(&directory.join(KEY_STORE), configured)
    }

    /// The trust a record at `path` describes, for a build whose own key is
    /// `configured`.
    fn read(path: &Path, configured: &str) -> Result<Self, String> {
        let mut trust = Trust {
            configured: configured.to_string(),
            store: Some(path.to_path_buf()),
            ..Trust::default()
        };

        // A build that has never been handed a key has no record, which is the
        // ordinary case rather than a problem.
        if !path.exists() {
            return Ok(trust);
        }

        let text = std::fs::read_to_string(path).map_err(|error| unreadable(path, error))?;
        let store: Store = serde_json::from_str(&text).map_err(|error| unreadable(path, error))?;

        // Every entry has to still be a key: the entry that does not parse
        // might be the retirement of the key this build ships with.
        for key in store.adopted.iter().chain(store.retired.iter()) {
            parse_public_key(key, "A key in this build's record of updater keys")
                .map_err(|error| unreadable(path, error))?;
        }

        trust.adopted = store.adopted;
        trust.retired = store.retired;
        Ok(trust)
    }

    /// The keys a release may be signed by, in the order they are tried: the one
    /// this build ships with first, then the ones it has adopted, with anything
    /// retired left out.
    fn accepted(&self) -> Vec<&str> {
        let retired = self.retired_ids();
        std::iter::once(self.configured.as_str())
            .chain(self.adopted.iter().map(String::as_str))
            .filter(|key| match key_id_of(key) {
                Ok(id) => !retired.contains(&id),
                // A key that does not parse can verify nothing. It stays on the
                // list so that the failure is reported as a key this build
                // cannot use, rather than as no key at all.
                Err(_) => true,
            })
            .collect()
    }

    /// The ids of the keys this build has been told to stop trusting.
    fn retired_ids(&self) -> Vec<[u8; 8]> {
        self.retired
            .iter()
            .filter_map(|key| key_id_of(key).ok())
            .collect()
    }

    /// Check a manifest's own bytes against every key this build still trusts.
    ///
    /// A build with one key reports the failure the way it always has; one that
    /// has been handed more says how many were tried, because that is the
    /// sentence someone reading a rotation wants.
    fn check(&self, document: &[u8], signature: &Signature) -> Result<(), String> {
        let keys = self.accepted();
        if keys.is_empty() {
            return Err(
                "Every key this build trusts for updates has been retired, so the manifest was \
                 not checked against one."
                    .to_string(),
            );
        }

        // The failure of the last key tried is the one reported, which for the
        // single-key build above is the only one there is.
        let mut failed = String::new();
        for field in &keys {
            let outcome = public_key(field).and_then(|key| {
                key.verify(document, signature, true)
                    .map_err(|error| error.to_string())
            });
            match outcome {
                Ok(()) => return Ok(()),
                Err(error) => failed = error,
            }
        }

        Err(match keys.len() {
            1 => format!(
                "The update manifest is not the one this project signed ({failed}), so it was not \
                 used."
            ),
            keys => format!(
                "The update manifest is not the one this project signed, by any of the {keys} keys \
                 this build trusts ({failed}), so it was not used."
            ),
        })
    }

    /// Take the key roll a verified manifest carries.
    ///
    /// Only a manifest whose signature has been checked reaches here, and only
    /// what it adds and retires is taken: the lists are merged into the record,
    /// never replaced by it.
    fn adopt(&mut self, document: &Value) -> Result<(), String> {
        let Some(roll) = document.get("keys") else {
            return Ok(());
        };
        let roll = roll.as_object().ok_or_else(|| {
            "The update manifest's key roll is not made of named lists, so the manifest was not \
             used."
                .to_string()
        })?;
        let added = key_list(roll.get("added"), "added")?;
        let retired = key_list(roll.get("retired"), "retired")?;

        // Every key named has to be one, in either list: a roll that names
        // something else is a roll this build cannot carry out.
        for key in added.iter().chain(retired.iter()) {
            parse_public_key(key, "A key in the update manifest's key roll")?;
        }

        let mut next = self.clone();
        for key in added {
            // A roll that names the key this build already ships with adds
            // nothing: the record is for the keys that arrived in a manifest.
            if key != self.configured && !next.adopted.contains(&key) {
                next.adopted.push(key);
            }
        }
        for key in retired {
            if !next.retired.contains(&key) {
                next.retired.push(key);
            }
        }

        // A roll that retired everything would leave nothing to check the next
        // release against - and the release that says so would be checked with
        // the key it is giving up.
        if next.accepted().is_empty() {
            return Err(
                "The update manifest retires every key this build trusts, so it was not used."
                    .to_string(),
            );
        }

        // Most releases carry no roll, and one that repeats a roll that has
        // already been acted on has nothing new in it: the record is written
        // when it changes, and not otherwise.
        if next.adopted == self.adopted && next.retired == self.retired {
            return Ok(());
        }

        next.save().map_err(|error| {
            format!(
                "The update manifest hands over updater keys this build could not record \
                 ({error}), so it was not used."
            )
        })?;
        *self = next;
        Ok(())
    }

    /// Write the record beside this build's other per-user data.
    ///
    /// Through a temporary file and a rename, so a write that does not finish
    /// leaves the previous record rather than half of a new one.
    fn save(&self) -> Result<(), String> {
        let Some(path) = &self.store else {
            return Ok(());
        };
        let describe = |action: &str, error: std::io::Error| {
            format!("could not {action} {}: {error}", path.display())
        };
        if let Some(directory) = path.parent() {
            std::fs::create_dir_all(directory)
                .map_err(|error| describe("create the folder for", error))?;
        }

        let store = Store {
            adopted: self.adopted.clone(),
            retired: self.retired.clone(),
        };
        let text = serde_json::to_string_pretty(&store)
            .map_err(|error| format!("could not write up the keys: {error}"))?;

        let temporary = path.with_extension("new");
        std::fs::write(&temporary, format!("{text}\n"))
            .map_err(|error| describe("write", error))?;
        std::fs::rename(&temporary, path).map_err(|error| describe("replace", error))?;
        Ok(())
    }

    /// The key an installer's signature names, when it is one this build still
    /// trusts.
    fn key_for_signature(&self, field: &str) -> Option<String> {
        let id = signature_key_id(field)?;
        self.accepted()
            .into_iter()
            .find(|key| key_id_of(key).ok() == Some(id))
            .map(str::to_string)
    }
}

/// The id minisign identifies a public key by.
fn key_id_of(field: &str) -> Result<[u8; 8], String> {
    parse_public_key(field, KEY_WORDING).map(|(_, id)| id)
}

/// The keys a manifest's key roll lists under `name`.
fn key_list(value: Option<&Value>, name: &str) -> Result<Vec<String>, String> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let entries = value.as_array().ok_or_else(|| {
        format!(
            "The update manifest's key roll has an `{name}` that is not a list, so the manifest \
             was not used."
        )
    })?;
    entries
        .iter()
        .map(|entry| {
            entry.as_str().map(str::to_string).ok_or_else(|| {
                format!(
                    "The update manifest's key roll has an entry in `{name}` that is not a key, so \
                     the manifest was not used."
                )
            })
        })
        .collect()
}

/// Why a record cannot be used, said in terms of the file to delete.
fn unreadable(path: &Path, error: impl std::fmt::Display) -> String {
    format!(
        "This build's record of updater keys at {} could not be read ({error}). Delete that file to \
         fall back to the key this build ships with.",
        path.display()
    )
}

/// The signature published beside the manifest.
///
/// A `.sig` file is base64 of a minisign signature, as `tauri signer sign`
/// writes it - the same encoding a manifest carries for an installer in its
/// `signature` field.
fn minisign_signature(field: &str) -> Result<Signature, String> {
    let text = decode_base64(field.trim(), "The manifest's signature")?;
    Signature::decode(&text)
        .map_err(|error| format!("The manifest's signature is unusable: {error}"))
}

/// Base64-decode a field into the text it carries.
fn decode_base64(value: &str, what: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(value)
        .map_err(|error| format!("{what} is not valid base64: {error}"))?;
    String::from_utf8(bytes).map_err(|_| format!("{what} is not text."))
}

/// The version a signature was made for, from its trusted comment.
///
/// `tauri signer sign --app-version` writes that comment as tab separated
/// fields - `timestamp:…<tab>file:latest.json<tab>version:0.1.2` - and the
/// updater reads the same field out of an installer's signature. `None` means
/// the signature records no version at all, which only a signer that was not
/// told one produces.
fn signed_version(comment: &str) -> Option<&str> {
    comment
        .split('\t')
        .find_map(|field| field.strip_prefix("version:"))
}

/// Whether the version a signature records names the version a manifest
/// announces.
///
/// A tag carries a `v` and a manifest does not, so a leading `v` is trimmed from
/// both sides. Beyond that the two are compared as written: the release pipeline
/// writes both numbers from one file, so anything else is a mismatch worth
/// reporting rather than a spelling to reconcile.
fn same_version(signed_for: &str, announced: &str) -> bool {
    signed_for.trim().trim_start_matches('v') == announced.trim().trim_start_matches('v')
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The updater's public key, read out of the configuration this build ships
    /// with rather than from a copy of it: a copy would keep passing after the
    /// key in this build was rotated.
    fn configured_key() -> String {
        let config: Value = serde_json::from_str(include_str!("../tauri.conf.json")).unwrap();
        config["plugins"]["updater"]["pubkey"]
            .as_str()
            .expect("tauri.conf.json carries no updater pubkey")
            .to_string()
    }

    /// The trust a build starts a check with before any release has handed it a
    /// key: the key it ships with, and nowhere to record anything, so a test
    /// that adopts a key leaves no file behind.
    fn trust_of(pubkey: &str) -> Trust {
        Trust {
            configured: pubkey.to_string(),
            ..Trust::default()
        }
    }

    /// A key roll as a manifest carries one, for the tests that drive the trust
    /// itself rather than a fetch.
    fn roll(added: &[&str], retired: &[&str]) -> Value {
        serde_json::json!({ "keys": { "added": added, "retired": retired } })
    }

    /// A key nobody holds: the shape of a minisign public key - the two
    /// algorithm bytes, eight id bytes, then a key - in the file form this
    /// project writes.
    fn bogus_key() -> String {
        let mut bytes = vec![0x45u8, 0x64];
        bytes.extend((0..40u8).map(|i| i.wrapping_mul(7).wrapping_add(3)));
        let key = base64::engine::general_purpose::STANDARD.encode(&bytes);
        base64::engine::general_purpose::STANDARD.encode(format!(
            "untrusted comment: minisign public key: 0000000000000000\n{key}\n"
        ))
    }

    /// A manifest with `signature` where the installer's signature goes, for
    /// the platform this build is.
    fn document_with_installer(signature: &str) -> Value {
        let target = tauri_plugin_updater::target().expect("this build has an updater target");
        let mut platforms = serde_json::Map::new();
        platforms.insert(target, serde_json::json!({ "signature": signature }));
        serde_json::json!({ "version": "0.1.2", "platforms": platforms })
    }

    /// Where a test's record of adopted keys goes: a directory of its own in the
    /// temporary directory, so that tests do not read each other's.
    fn record_path(name: &str) -> std::path::PathBuf {
        let directory = std::env::temp_dir().join(format!(
            "excelscry-updater-keys-{}-{name}",
            std::process::id()
        ));
        std::fs::create_dir_all(&directory).expect("a temporary directory for the record");
        directory.join(KEY_STORE)
    }

    /// A release manifest signed by this project's own release key, and the
    /// signature published beside it.
    ///
    /// Both were produced once with the private half of the key in
    /// `tauri.conf.json`:
    ///
    /// ```text
    /// cargo tauri signer sign --app-version 0.1.2 latest.json
    /// ```
    ///
    /// The signature covers the document's own bytes, which is why the document
    /// is written as one line plus a newline rather than kept as a file a
    /// checkout might rewrite the line endings of: the constant below has to
    /// reproduce those bytes exactly for the signature to verify at all. The
    /// signature is the `.sig` file's own content, unaltered.
    ///
    /// The `platforms` entry is a stand-in - a URL and an uploaded artifact
    /// signature that no test here reads. The installer it points at is verified
    /// by the updater, during a download.
    const SIGNED_MANIFEST: &str = concat!(
        "{\"version\":\"0.1.2\",\"pub_date\":\"2026-10-16T12:00:00Z\",",
        "\"notes\":\"### Fixed\\n- A workbook with an empty sheet no longer closes the grid.\",",
        "\"platforms\":{\"windows-x86_64\":{",
        "\"url\":\"https://github.com/CryptoTaxHelp/ExcelScry/releases/download/v0.1.2/",
        "ExcelScry_0.1.2_x64-setup.exe\",",
        "\"signature\":\"dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkK\"",
        "}}}",
        "\n",
    );

    /// The same bytes, signed for `0.1.2` - the release above.
    const SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVTM3UxK3pUQzNwMHZYSEQzZlV6SjFiMXJlNEx0Vjg3ZHdsSmhiSFlIOGlTTDRPclI4akNkb1RGVXBkSVJjMGJNeThNRnVvV0lIbWR5dHE1eUpxSGxTZUtZZU0vbmg1K2d3PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkyMTYwNjY5CWZpbGU6bGF0ZXN0Lmpzb24JdmVyc2lvbjowLjEuMgpoZ2wzYXBWY3M0SXF5YSsvanV6aHBUbUNiaWFMemQ4WTVCam5mKy9vTTBUTEpXQXF4bTJjcHNoNnlrQUZtNG1OczZKRzV0dWM1QXZwVXNJUnQxNm1DUT09Cg==";

    /// The same bytes, signed by the same key but stamped `0.1.1`: what a
    /// rewritten manifest would need, and cannot get.
    const SIGNATURE_FOR_ANOTHER_VERSION: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVTM3UxK3pUQzNwMHA3Yy9yWk8zR0lwYnQ5akVJVWpxMkVzM2Fuck13UkJ0U3BtYSsvS0RvRVFqYXVRMTNWOHFHN3lGTVZTSXJDalJ4MFE3Z1JXeGdvUmRlTXVCbHlsUkFVPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkyMTYwNjY5CWZpbGU6bGF0ZXN0Lmpzb24JdmVyc2lvbjowLjEuMQpVV3lsMGNHSUhMTkx4OHZ1YXJIYVhNWHhibFp1K1ZTOWs2cE5TejR6OVpVaG5vb1pUZ3hnM0ZmNzVlUFRwdTF6RDBDRHRJSGFQeUVKeHRDVHlUbUFCdz09Cg==";

    /// The same bytes, signed with no `--app-version` at all.
    const SIGNATURE_WITHOUT_VERSION: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVTM3UxK3pUQzNwMG1BRzA4eU5xamRSa1I5bEFFeTNqTG9SYk9DeGo0TWJjNEg5T0dsUURpbHlkdDkyQSt0enJxTTZjSHdIOCtOUnROVDF4Z0sxU0ltRUNJbXU5bnpPQ0FJPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkyMTYwNjY5CWZpbGU6bGF0ZXN0Lmpzb24KczRobWhiaWF4S1l2Uy9sdG90OG4yVStwZW0yc3ZDck9VQWVlRnVMaVgzdTRNbnRvV2lURkY4UFY4SkdST0xCMGN6eGtuc094R0VGQ0R5ZVBpWWVFQVE9PQo=";

    /// The same bytes, signed for `0.1.2` by a key this build does not trust.
    const SIGNATURE_FROM_ANOTHER_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVRVjBWYldEWklVaVVBZkdQT2ZobzF5UEVJL0UrNmNrQnpJZ0tHOWV3aitVeGVZZnFkR1BHS3JIRWNVQ1N4bTNHT2VwRVpsY0pFYmZKVFdmZFZYVGVDQWlBOWpwUDNCbkE4PQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkyMTYwNjY5CWZpbGU6bGF0ZXN0Lmpzb24JdmVyc2lvbjowLjEuMgpLSk1CQy91dHlEa0NocjhpMTFEWitVVEtJSFJ1cTg0aU5CM3o1Y1p2TGxraDBwZ2dKZE9FM1FTQ1Y5cHdNcTRIcE9oNC92YzRpcGpYNlFBeXlRcUFBQT09Cg==";

    /// A release manifest that carries a key roll, signed by the same key as
    /// [`SIGNED_MANIFEST`].
    ///
    /// It is what a release that replaces the signing key publishes: it hands
    /// over the public half of a key generated for the fixture, whose private
    /// half was discarded because nothing here has to be signed with it, and it
    /// retires the key this build ships with - so the very same bytes stop
    /// verifying once the roll has been taken.
    ///
    /// Produced once with the private half of the key in `tauri.conf.json`, the
    /// way the release job signs `latest.json`, so the signature is
    /// reproducible by the documented procedure:
    ///
    /// ```text
    /// cargo tauri signer sign --app-version 0.1.2 latest.json
    /// ```
    ///
    /// The `platforms` entry is the same stand-in the other fixtures carry.
    const MANIFEST_WITH_A_KEY_ROLL: &str = concat!(
        "{\"version\":\"0.1.2\",\"pub_date\":\"2026-10-16T12:00:00Z\",\"notes\":\"###",
        " Changed\\n- A release can hand the app a new updater key, and retire one, ",
        "so a key that has leaked is replaced without a bridging ",
        "release.\",\"platforms\":{\"windows-x86_64\":{\"url\":\"https://github.com/CryptoTaxHelp/ExcelScry/releases/download/v0.1.2/ExcelScry_0.1.2_x64-setup.exe\",\"signature\":\"dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkK\"}},\"keys\":{\"added\":[\"dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEIzMzNGMUFCMUEwQzUxQzAKUldUQVVRd2FxL0V6czczZDg2Yll1Q1R3WUEvaXY2QmVBejh0TTZRdFlQbmJ6QTZZcEJsMjJjaFoK\"],\"retired\":[\"dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEQyRTkyRDRDQjM1RkJCQjcKUldTM3UxK3pUQzNwMHQ2amI5SXp6NWpxSjduM0d1NG1pcVdSeWlQRVpwKzNFRnp6NGFGTjZqcTMK\"]}}\n"
    );
    const KEY_ROLL_SIGNATURE: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IHNpZ25hdHVyZSBmcm9tIHRhdXJpIHNlY3JldCBrZXkKUlVTM3UxK3pUQzNwMHRtd2hNREhrL2QyeTNObHhMNjIyV0swTVhxbDBScnNwT29IdHMzRy96cnBOQko3aHFaMUFod2lZMmtldUxxemhUUXRwanp3eUxPY1lCdUtRRUJNOXdRPQp0cnVzdGVkIGNvbW1lbnQ6IHRpbWVzdGFtcDoxNzkyMTY1MjQ1CWZpbGU6bGF0ZXN0Lmpzb24JdmVyc2lvbjowLjEuMgozV0dnM3FzSzZUQmUwcmUyTWRTbS9GVENZTTIySmZJMExQUlh4REw5VkJBRFEvZVN2TFMzKzVISnhoUE1zdWhvY0g3MTRRd21nTGtveFRXbWRTeG5DUT09Cg==";
    const ROLLED_FORWARD_KEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEIzMzNGMUFCMUEwQzUxQzAKUldUQVVRd2FxL0V6czczZDg2Yll1Q1R3WUEvaXY2QmVBejh0TTZRdFlQbmJ6QTZZcEJsMjJjaFoK";

    #[test]
    fn a_signed_manifest_is_accepted() {
        let document = verify(
            SIGNED_MANIFEST.as_bytes(),
            SIGNATURE,
            &trust_of(&configured_key()),
            true,
        )
        .expect("a manifest signed by the release key should verify");
        assert_eq!(document["version"], "0.1.2");
        assert_eq!(
            document["platforms"]["windows-x86_64"]["url"],
            "https://github.com/CryptoTaxHelp/ExcelScry/releases/download/v0.1.2/\
             ExcelScry_0.1.2_x64-setup.exe"
        );
    }

    #[test]
    fn an_edited_manifest_is_rejected() {
        let edited = SIGNED_MANIFEST.replace("0.1.2", "9.9.9");
        assert_ne!(edited, SIGNED_MANIFEST, "the edit has to change the bytes");
        let error = verify(
            edited.as_bytes(),
            SIGNATURE,
            &trust_of(&configured_key()),
            true,
        )
        .expect_err("a manifest no signature covers should be refused");
        assert!(error.contains("not the one this project signed"), "{error}");
    }

    #[test]
    fn a_signature_from_another_key_is_rejected() {
        let error = verify(
            SIGNED_MANIFEST.as_bytes(),
            SIGNATURE_FROM_ANOTHER_KEY,
            &trust_of(&configured_key()),
            true,
        )
        .expect_err("a signature from an untrusted key should be refused");
        assert!(error.contains("different key"), "{error}");
    }

    #[test]
    fn a_signature_for_another_version_is_rejected() {
        let error = verify(
            SIGNED_MANIFEST.as_bytes(),
            SIGNATURE_FOR_ANOTHER_VERSION,
            &trust_of(&configured_key()),
            true,
        )
        .expect_err("a manifest announcing a version it was not signed for should be refused");
        assert!(
            error.contains("signed for 0.1.1 but announces 0.1.2"),
            "{error}"
        );
    }

    #[test]
    fn a_signature_without_a_version_is_only_accepted_when_the_config_says_so() {
        let error = verify(
            SIGNED_MANIFEST.as_bytes(),
            SIGNATURE_WITHOUT_VERSION,
            &trust_of(&configured_key()),
            true,
        )
        .expect_err("an unversioned signature should be refused while requireSignedVersion is on");
        assert!(error.contains("signed without a version"), "{error}");

        // The same rule the updater applies to an installer's signature: with
        // `requireSignedVersion` off, a signature that records no version is
        // accepted, and the manifest's own bytes are still what was signed.
        let document = verify(
            SIGNED_MANIFEST.as_bytes(),
            SIGNATURE_WITHOUT_VERSION,
            &trust_of(&configured_key()),
            false,
        )
        .expect("the signature covers the manifest, version or not");
        assert_eq!(document["version"], "0.1.2");
    }

    #[test]
    fn the_signature_lives_beside_the_manifest() {
        let manifest =
            Url::parse("https://example.com/releases/latest/download/latest.json").unwrap();
        assert_eq!(
            signature_url(&manifest).unwrap().as_str(),
            "https://example.com/releases/latest/download/latest.json.sig"
        );

        // A query string is not part of the name, so the suffix goes before it.
        let query = Url::parse("https://example.com/update?channel=stable").unwrap();
        assert_eq!(
            signature_url(&query).unwrap().as_str(),
            "https://example.com/update.sig?channel=stable"
        );
    }

    #[test]
    fn endpoint_variables_are_resolved_the_way_the_updater_resolves_them() {
        let config: Config = serde_json::from_value(serde_json::json!({
            "endpoints": [
                "https://example.com/{{target}}/{{arch}}/{{current_version}}/{{bundle_type}}"
            ],
            "pubkey": "unused",
        }))
        .unwrap();

        let target = tauri_plugin_updater::target().expect("this build has an updater target");
        let (os, arch) = target
            .split_once('-')
            .expect("a target is an operating system and an architecture");
        let installer = installer_name().unwrap_or("unknown");
        assert_eq!(
            endpoint(&config, "0.1.2").unwrap().as_str(),
            format!("https://example.com/{os}/{arch}/0.1.2/{installer}")
        );
    }

    /// A loopback server that answers the files a test asks for, and 404 for
    /// everything else.
    ///
    /// Enough of HTTP/1.1 to answer one request per connection, which is all
    /// the checks below need: what is under test is what the app does with the
    /// answer, not the protocol.
    fn serve(files: &[(&str, &str)]) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        let files: Vec<(String, String)> = files
            .iter()
            .map(|(path, body)| ((*path).to_string(), (*body).to_string()))
            .collect();

        std::thread::spawn(move || {
            use std::io::{Read as _, Write as _};
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let mut request = [0u8; 2048];
                let read = stream.read(&mut request).unwrap_or(0);
                let request = String::from_utf8_lossy(&request[..read]).into_owned();
                let path = request.split_whitespace().nth(1).unwrap_or_default();
                let body = files
                    .iter()
                    .find(|(name, _)| name == path)
                    .map(|(_, body)| body.as_str());
                let (status, reason, body) = match body {
                    Some(body) => ("200", "OK", body),
                    None => ("404", "Not Found", ""),
                };
                let response = format!(
                    "HTTP/1.1 {status} {reason}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(response.as_bytes());
                let _ = stream.write_all(body.as_bytes());
                let _ = stream.flush();
            }
        });

        port
    }

    /// A configuration pointing at a loopback server, the way `tauri.conf.json`
    /// would point at the release page.
    fn test_config(port: u16) -> Config {
        serde_json::from_value(serde_json::json!({
            "endpoints": [format!("http://127.0.0.1:{port}/latest.json")],
            "pubkey": configured_key(),
            "requireSignedVersion": true,
            // Plain HTTP to a loopback server, which is the point of the test:
            // the updater only allows that in a development build, and saying so
            // here keeps the test honest in a release one too.
            "dangerousInsecureTransportProtocol": true,
        }))
        .unwrap()
    }

    #[test]
    fn the_manifest_is_fetched_and_checked_over_the_network() {
        let port = serve(&[
            ("/latest.json", SIGNED_MANIFEST),
            ("/latest.json.sig", SIGNATURE),
        ]);

        let document = tauri::async_runtime::block_on(fetch_with(
            &test_config(port),
            "0.1.1",
            &mut trust_of(&configured_key()),
        ))
        .expect("a signed release served over the network should be accepted")
        .into_document();
        assert_eq!(document["version"], "0.1.2");
        assert_eq!(
            document["notes"],
            "### Fixed\n- A workbook with an empty sheet no longer closes the grid."
        );
    }

    #[test]
    fn a_release_that_cannot_be_fetched_says_which_file_and_why() {
        let nothing_at_all = serve(&[]);
        let error = tauri::async_runtime::block_on(fetch_with(
            &test_config(nothing_at_all),
            "0.1.1",
            &mut trust_of(&configured_key()),
        ))
        .expect_err("a manifest that is not there should be reported");
        assert!(
            error.starts_with("The update manifest at http://127.0.0.1:"),
            "{error}"
        );
        assert!(error.contains("answered 404"), "{error}");
    }

    #[test]
    fn a_release_with_no_signed_manifest_is_refused_over_the_network() {
        let only_the_manifest = serve(&[("/latest.json", SIGNED_MANIFEST)]);
        let error = tauri::async_runtime::block_on(fetch_with(
            &test_config(only_the_manifest),
            "0.1.1",
            &mut trust_of(&configured_key()),
        ))
        .expect_err("a manifest with no signature beside it should be refused");
        assert!(error.contains("publishes no signed manifest"), "{error}");

        // And a manifest whose bytes were changed after signing, with the
        // signature still where it was.
        let edited = SIGNED_MANIFEST.replace("0.1.2", "9.9.9");
        let served_edited = serve(&[("/latest.json", &edited), ("/latest.json.sig", SIGNATURE)]);
        let error = tauri::async_runtime::block_on(fetch_with(
            &test_config(served_edited),
            "0.1.1",
            &mut trust_of(&configured_key()),
        ))
        .expect_err("a manifest nothing signed should be refused");
        assert!(error.contains("not the one this project signed"), "{error}");
    }

    #[test]
    fn the_signed_version_comes_from_the_trusted_comment() {
        assert_eq!(
            signed_version("timestamp:1760000000\tfile:latest.json\tversion:0.1.2"),
            Some("0.1.2")
        );
        assert_eq!(
            signed_version("timestamp:1760000000\tfile:latest.json"),
            None,
            "a signature that records no version has none to read"
        );
    }

    #[test]
    fn versions_match_across_a_tag_prefix() {
        assert!(same_version("0.1.2", "0.1.2"));
        assert!(same_version("v0.1.2", "0.1.2"));
        assert!(!same_version("0.1.2", "0.1.3"));
        assert!(
            !same_version("0.1.2", "0.1.20"),
            "0.1.2 is not a prefix match"
        );
    }

    #[test]
    fn a_manifest_without_a_roll_leaves_the_trust_alone() {
        let configured = configured_key();
        let mut trust = trust_of(&configured);
        let document = verify(SIGNED_MANIFEST.as_bytes(), SIGNATURE, &trust, true).unwrap();
        trust.adopt(&document).expect("there is nothing to do");
        assert!(trust.adopted.is_empty() && trust.retired.is_empty());
        assert_eq!(trust.accepted(), vec![configured.as_str()]);
    }

    #[test]
    fn a_roll_is_taken_from_a_manifest_signed_with_the_key_it_retires() {
        // The bytes are checked before the roll is taken, which is what makes a
        // release the last thing an old key can sign.
        let mut trust = trust_of(&configured_key());
        let document = verify(
            MANIFEST_WITH_A_KEY_ROLL.as_bytes(),
            KEY_ROLL_SIGNATURE,
            &trust,
            true,
        )
        .expect("a roll signed by the key it retires has to verify first");
        assert_eq!(document["keys"]["added"][0], ROLLED_FORWARD_KEY);

        trust
            .adopt(&document)
            .expect("the roll should be carried out");
        assert_eq!(trust.adopted, vec![ROLLED_FORWARD_KEY.to_string()]);
        assert_eq!(trust.retired, vec![configured_key()]);
        assert_eq!(trust.accepted(), vec![ROLLED_FORWARD_KEY]);

        // The key that signed the manifest is one this build will not take a
        // release from any more - by either fixture.
        for (document, signature) in [
            (MANIFEST_WITH_A_KEY_ROLL, KEY_ROLL_SIGNATURE),
            (SIGNED_MANIFEST, SIGNATURE),
        ] {
            let error = verify(document.as_bytes(), signature, &trust, true)
                .expect_err("a retired key must not be accepted");
            assert!(error.contains("not the one this project signed"), "{error}");
        }
    }

    #[test]
    fn an_adopted_key_is_what_a_manifest_is_checked_with() {
        // A build whose own key verifies nothing, handed the key that signed
        // this fixture: what verifies it is the key that arrived in a manifest.
        let mut trust = trust_of(&bogus_key());
        let error = verify(SIGNED_MANIFEST.as_bytes(), SIGNATURE, &trust, true)
            .expect_err("the key this build ships with cannot check the fixture");
        assert!(error.contains("not the one this project signed"), "{error}");

        trust
            .adopt(&roll(&[&configured_key()], &[]))
            .expect("a roll that adds a key is one this build can carry out");
        let document = verify(SIGNED_MANIFEST.as_bytes(), SIGNATURE, &trust, true)
            .expect("the key the manifest handed over is the one it is checked with");
        assert_eq!(document["version"], "0.1.2");
    }

    #[test]
    fn a_roll_that_would_leave_nothing_is_refused() {
        let mut trust = trust_of(&configured_key());
        let error = trust
            .adopt(&roll(&[], &[&configured_key()]))
            .expect_err("retiring the only key this build trusts has to be refused");
        assert!(error.contains("retires every key"), "{error}");

        // And it was not carried out: the key still checks a release.
        assert!(
            trust.retired.is_empty(),
            "nothing should have been recorded"
        );
        verify(SIGNED_MANIFEST.as_bytes(), SIGNATURE, &trust, true)
            .expect("the key this build ships with is still trusted");
    }

    #[test]
    fn a_roll_is_merged_into_what_is_known() {
        let configured = configured_key();
        let mut trust = trust_of(&configured);
        trust
            .adopt(&roll(&[ROLLED_FORWARD_KEY], &[]))
            .expect("the key should be adopted");

        // A release that repeats a roll it has already handed over changes
        // nothing, and one that names the key this build ships with adds
        // nothing either.
        trust
            .adopt(&roll(&[ROLLED_FORWARD_KEY], &[]))
            .expect("the same roll again");
        trust
            .adopt(&roll(&[&configured], &[]))
            .expect("the build's own key is already trusted");
        assert_eq!(trust.adopted, vec![ROLLED_FORWARD_KEY.to_string()]);
        assert_eq!(
            trust.accepted(),
            vec![configured.as_str(), ROLLED_FORWARD_KEY]
        );

        // Nor does one that names only the keys it knows about: a manifest
        // cannot drop a key that an earlier manifest handed over.
        trust
            .adopt(&roll(&[&configured], &[]))
            .expect("a roll with nothing new in it");
        assert!(trust.accepted().contains(&ROLLED_FORWARD_KEY));
    }

    #[test]
    fn a_roll_naming_something_that_is_not_a_key_is_refused() {
        let mut trust = trust_of(&configured_key());
        for nonsense in [
            roll(&["not base64 at all"], &[]),
            roll(&[], &["not base64 at all"]),
        ]
        .into_iter()
        .chain([
            // A key this build could accept, but not a list.
            serde_json::json!({ "keys": { "added": ROLLED_FORWARD_KEY } }),
            serde_json::json!({ "keys": { "added": [7] } }),
            serde_json::json!({ "keys": "added" }),
        ]) {
            let error = trust
                .adopt(&nonsense)
                .expect_err("this roll cannot be carried out");
            assert!(error.contains("key roll"), "{error}");
        }
        assert!(
            trust.adopted.is_empty() && trust.retired.is_empty(),
            "nothing from a refused roll is recorded"
        );
    }

    #[test]
    fn a_roll_is_recorded_and_read_back() {
        let path = record_path("recorded");
        let configured = configured_key();

        let mut trust = Trust::read(&path, &configured).expect("no record yet is not a problem");
        trust
            .adopt(&roll(&[ROLLED_FORWARD_KEY], &[&configured]))
            .expect("the roll should be carried out");

        // A later run of the same build reads the same trust back - including
        // the retirement of the key that build ships with.
        let reread = Trust::read(&path, &configured).expect("the record should be readable");
        assert_eq!(reread.adopted, vec![ROLLED_FORWARD_KEY.to_string()]);
        assert_eq!(reread.retired, vec![configured]);
        assert_eq!(reread.accepted(), vec![ROLLED_FORWARD_KEY]);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn a_record_that_cannot_be_read_refuses_rather_than_forgetting_a_retirement() {
        let path = record_path("damaged");
        let configured = configured_key();

        std::fs::write(&path, "{ not json").unwrap();
        let error = Trust::read(&path, &configured).expect_err("a damaged record is not usable");
        assert!(error.contains("could not be read"), "{error}");
        assert!(
            error.contains("Delete that file"),
            "the message has to say how to get back to the key this build ships with: {error}"
        );

        // An entry that is no longer a key is refused the same way: the entry
        // that stopped parsing might be the retirement of that key.
        std::fs::write(&path, r#"{"retired": ["garbage"]}"#).unwrap();
        let error = Trust::read(&path, &configured).expect_err("a damaged entry is not usable");
        assert!(error.contains("could not be read"), "{error}");

        // A record that is only a list of the keys it has adopted is fine, and
        // the keys in it are not read as anything else.
        std::fs::write(
            &path,
            serde_json::json!({ "adopted": [ROLLED_FORWARD_KEY] }).to_string(),
        )
        .unwrap();
        let trust = Trust::read(&path, &configured).expect("an adopted key is a key");
        assert_eq!(
            trust.accepted(),
            vec![configured.as_str(), ROLLED_FORWARD_KEY]
        );

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn the_installer_key_is_the_trusted_key_that_signed_it() {
        // The key that signed this fixture, from the build's own copy.
        let verified = Verified {
            document: document_with_installer(SIGNATURE),
            trust: trust_of(&configured_key()),
        };
        assert_eq!(verified.installer_key(), Some(configured_key()));

        // A build whose own key is not that one, handed it by a roll: the
        // installer is checked with the key that arrived in a manifest.
        let mut trust = trust_of(&bogus_key());
        trust
            .adopt(&roll(&[&configured_key()], &[]))
            .expect("the key should be adopted");
        let verified = Verified {
            document: document_with_installer(SIGNATURE),
            trust,
        };
        assert_eq!(verified.installer_key(), Some(configured_key()));

        // Once that key is retired, the same installer signature is one this
        // build will not check a download with.
        let mut trust = trust_of(&configured_key());
        trust
            .adopt(&roll(&[ROLLED_FORWARD_KEY], &[]))
            .expect("the key should be adopted");
        trust
            .adopt(&roll(&[], &[&configured_key()]))
            .expect("the build's own key can be retired while another is trusted");
        let verified = Verified {
            document: document_with_installer(SIGNATURE),
            trust,
        };
        assert_eq!(verified.installer_key(), None);

        // A manifest with no signature for this platform, or none that names a
        // key, says nothing about which key to use: the updater is left with
        // the one this build ships with.
        for document in [
            serde_json::json!({}),
            document_with_installer("not a signature"),
            document_with_installer(""),
        ] {
            let verified = Verified {
                document,
                trust: trust_of(&configured_key()),
            };
            assert_eq!(verified.installer_key(), None);
        }
    }

    #[test]
    fn the_updater_still_reads_a_manifest_that_carries_a_roll() {
        // The app hands the verified document to the updater as it arrived, and
        // the updater deserializes it into its own type: the roll has to be a
        // field it does not know about rather than one that makes the release
        // unreadable.
        let document: Value =
            serde_json::from_str(MANIFEST_WITH_A_KEY_ROLL.trim()).expect("the fixture is JSON");
        let release: tauri_plugin_updater::RemoteRelease = serde_json::from_value(document)
            .expect("the updater has to read a manifest that carries a key roll");

        assert_eq!(release.version.to_string(), "0.1.2");
        let target = tauri_plugin_updater::target().expect("this build has an updater target");
        assert_eq!(
            release.download_url(&target).unwrap().as_str(),
            "https://github.com/CryptoTaxHelp/ExcelScry/releases/download/v0.1.2/\
             ExcelScry_0.1.2_x64-setup.exe"
        );
        assert!(release.signature(&target).is_ok());
    }

    #[test]
    fn a_key_roll_served_over_the_network_is_taken() {
        let port = serve(&[
            ("/latest.json", MANIFEST_WITH_A_KEY_ROLL),
            ("/latest.json.sig", KEY_ROLL_SIGNATURE),
        ]);
        let mut trust = trust_of(&configured_key());

        let verified =
            tauri::async_runtime::block_on(fetch_with(&test_config(port), "0.1.1", &mut trust))
                .expect("a signed release that hands over a key should be accepted");

        // The document goes on as it arrived, roll and all, and the trust it
        // left behind is the one the roll asked for.
        assert_eq!(
            verified.into_document()["keys"]["added"][0],
            ROLLED_FORWARD_KEY
        );
        assert_eq!(trust.accepted(), vec![ROLLED_FORWARD_KEY]);
    }
}
