# Changelog

All notable changes to ExcelScry are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The `Unreleased` section collects work for the next release; a release moves
those entries into a version section and tags the commit that holds it.

## [Unreleased]

### Added

- **The update manifest is signed, and the app checks it before believing it.**
  A manifest that arrives over TLS proves only where the bytes came from, not
  that they are the ones this project published, so the release job now signs
  `latest.json` with the same key as the installers and publishes the signature
  beside it as `latest.json.sig`. `src-tauri/src/manifest.rs` fetches both,
  verifies the signature over the manifest's own bytes against the key in
  `tauri.conf.json`, and only then hands the document to the updater
  (`restore_update`) - so the version, the notes and the installer's URL are the
  signed ones rather than whatever the endpoint said. A manifest with no
  signature beside it, or one whose signature does not cover it, is refused
  rather than installed. The app's tests include a manifest signed by the
  project's own release key, and the four ways such a signature can fail.
- **What's new, before it is installed.** The notes the release manifest carries
  — the same text the release page shows — are rendered in
  **Settings › Updates** as headings, bullets, emphasis and links, so an update
  can be read inside the app before anything is downloaded. They arrive with the
  check that already ran; nothing extra is fetched. The notes are Markdown rather
  than HTML, so they are built out of elements and text nodes: a note containing
  markup shows the characters, not the markup. A link is the exception, and the
  one thing a release body may ask the app to *do*: only `http` and `https` are
  links at all, and clicking one opens it in the default browser rather than
  replacing the app with it.
- **A published release is checked before anyone updates from it.** The
  `verify-release` job runs after the release job and asks the endpoint the app
  asks - `latest.json`, fetched from `tauri.conf.json` - verifying the signature
  published beside it over the manifest's own bytes, then downloading the
  installer the manifest points at and verifying its signature against the
  public key in that same file. Both are checked by the rules the app applies:
  the key id, the signature, the global signature over the trusted comment, and
  the version that comment records. Every other asset the workflow publishes is
  checked in the same run: the `.sig` published beside the installer has to be
  the signature the manifest embeds, and the portable executable has to be the
  release being verified, read out of its own version resource - nothing signs
  it, so a portable build left behind by an earlier release is otherwise
  invisible. A manifest that is missing, that is unsigned, that still belongs to
  an older release, that carries an installer whose bytes do not match its
  signature, or a portable build from another release fails the run for that tag
  instead of a user's update check.
- **A release can hand the app a new updater key, and retire one.** The signing
  key can be replaced - because it is due for replacement, or because it has
  leaked - without a release that every installed copy has to take first. The
  manifest may carry a `keys` roll: keys to start accepting, and keys to retire.
  The app takes it only from a manifest whose signature has already been checked,
  and merges it into a record kept beside its own per-user data
  (`%LOCALAPPDATA%\com.excelscry.viewer\updater-keys.json`), so a retirement
  survives a restart, applies to the key the build itself ships with, and cannot
  be undone by an older manifest. A roll that names something that is not a key,
  or that would leave nothing trusted, is refused whole - the release carrying it
  would otherwise be the last one those copies could update from. The installer
  is verified with whichever trusted key the release names for it, so the release
  doing the rotation can sign its installer with the new key. The roll lives in
  `src-tauri/updater-keys.json`, is written into `latest.json` by
  `scripts/manifest_keys.py` before the release job signs it, and is checked
  against that same file on every push and again against the published release.
- **A release is one command.** `python scripts/release.py 0.2.0` bumps the four
  copies of the version, moves the `Unreleased` entries into a dated section,
  commits, tags and pushes — and refuses to start unless the tree is clean, the
  branch is in step with its upstream, the version is ahead of the current one
  and there are entries to release. It prints the notes the release page will
  carry before pushing, and `--dry-run` stops before anything is written.
- **The release flow is checked, not described.** `release.py` cuts a release of
  this repository, so nothing in CI could run it, and what it refuses was only
  ever asserted in prose. `scripts/test_release.py` runs it somewhere it is safe
  to: a temporary checkout of the real `src-tauri/` and `scripts/`, with a
  changelog holding unreleased entries and a bare remote to push to, in which
  every refusal the flow is supposed to make is checked — an uncommitted change,
  a branch ahead of or behind its remote, a version that is not ahead, a version
  that is not a version, a tag that already exists, a changelog with nothing
  unreleased, one that already names the version — along with what `--dry-run`
  leaves alone, and a full release end to end: the four copies of the version,
  the moved changelog section, the files the release commit carries, the
  annotated tag, both pushes, and the report a rejected tag push leaves behind.
  The scripts are copied rather than reimplemented, so the code under test is
  the code in the checkout, and the run fails if this repository's `HEAD` or
  `git status` moved while it was going.

### Changed

- **A release is published only once its manifest is signed, and the workflow
  cannot publish one that is not.** The release job
  builds the release as a draft, and makes it visible in its last step: the
  endpoint the app asks resolves to the latest release that is neither a draft
  nor a pre-release, so while the manifest is being signed that endpoint still
  answers with the previous release's signed manifest. Everything the release
  will serve is attached to the draft first - the installer, its signature,
  `latest.json` with the key roll in it, and `latest.json.sig` - and the two
  manifest files are then read back out of the draft and checked
  ([`verify_release.py --signature-only`](scripts/verify_release.py)) before
  publishing. A release job that fails anywhere before that leaves a draft no
  copy can see, rather than a public release with an unsigned manifest that no
  copy can update from. Since nothing that runs *after* publication can tell
  whether the manifest was briefly unsigned, the ordering itself is checked on
  every push by [`scripts/verify_workflow.py`](scripts/verify_workflow.py): read
  the release job, and refuse a workflow that would build a published release
  rather than a draft, sign the manifest after publishing it, publish without
  checking what is about to become visible, or let another job create a release
  of its own.
- **Installing asks first.** The status-bar pill and the **Settings › Updates**
  button used to install on a single click; both now open a confirmation showing
  what is new in the version on offer - the release's own opening points, a few
  at a time - along with what installing does. Nothing is downloaded until that
  is answered, cancelling costs nothing, and the full notes are a click away in
  **Settings › Updates**.

## [0.1.1] - 2026-10-16

### Added

- **Self-updating from the project's GitHub releases.** ExcelScry asks the
  latest release for its update manifest when it starts and offers a newer
  version in the status bar; **Settings › Updates** shows the running version,
  the release manifest, a manual check and the install button. Nothing is
  downloaded until it is asked for, the installer is verified against the
  release signing key before anything runs, and it must have been signed for the
  version the manifest named — so a rewritten manifest cannot point the app at
  an older, validly signed release. A failed check stays quiet at start-up; a
  failed verification is reported and changes nothing.

### Fixed

- **The Windows integration dialog lists every extension ExcelScry registers,
  `.xlsm` included.** It asked the backend for a `.xlsx` handler and an `.xls`
  handler — fields that never existed — so both rows always read *no handler
  recorded* while the registry said otherwise, and macro-enabled workbooks,
  which are registered exactly like the other two, were not mentioned at all.
  The rows are now built from the registration table, so the dialog cannot fall
  behind what the app actually registers.

## [0.1.0] - 2026-10-16

First release: a read-only viewer for Excel workbooks and OpenDocument
spreadsheets, with a package trust inspector, ODS export, and a Windows shell
integration. Nothing in a workbook is ever executed, extracted or written.

### Added

- **Reading** of `.xlsx`, `.xlsm`, `.xlsb`, `.xls`, `.xla` and `.ods` — every
  format [calamine](https://crates.io/crates/calamine) supports.
- **Virtualised grid**: only the cells in view exist in the DOM, fetched in
  tiles as you scroll, so a 500,000-row sheet stays responsive and memory stays
  bound by the tile cache rather than the size of the sheet.
- **Spreadsheet-style cell rendering**: numbers right-aligned without an `f64`
  round trip for large integers, dates as `YYYY-MM-DD`, times as clock times,
  durations past 24 hours as `36:00:00` rather than wrapping, error values in
  red, booleans centred. Excel's day zero (`1899-12-30`) is treated as a time of
  day rather than a date.
- **Trust inspector** that reads the *package* rather than the cells and reports
  five signals — macros, external links, data connections, embedded objects and
  ActiveX controls — as a status-bar badge that opens a detail dialog. The
  report is tri-state, so `null` means *not inspected for this format* and never
  *absent*; a legacy `.xls` is an OLE2 compound file with no member list, so
  only its macros can be checked and the badge says so.
- **A standing status-bar note** for a workbook that actually carries macros:
  *Macros present — ExcelScry will not run them*. The badge never shrinks away
  and the sentence does not clip; the file name and readout ellipsize instead,
  and below 720px the sentence steps aside rather than pushing the bar into
  overflow.
- **ODS export** of the open workbook, keeping values as real numbers, dates,
  times, durations, booleans and strings, with the first used row styled bold.
- **Windows shell integration**: a *Scry Excel File* context-menu entry, file
  association registration for `.xlsx`, `.xlsm` and `.xls`, a deep link to
  Windows' Default Apps page pre-filtered to ExcelScry, and an uninstall that
  restores the previous `UserChoice`.
- **Single instance**: opening another workbook reuses the running window. A
  path on the command line opens it directly, which is what the context-menu
  entry uses.
- **Keyboard and pointer navigation**: arrow keys, `PageUp`/`PageDown`,
  `Home`/`End`, sheet tabs, per-cell copy with `Ctrl+C`, `Ctrl+O` to open,
  drag-and-drop, and a cell readout bar.
- **Two build outputs**: a single-file portable executable with the front end
  embedded, and a per-machine NSIS installer (LZMA, WebView2 bootstrapper,
  installed from `scripts/build.sh` or `scripts/build.ps1`).

### Development

- **CI** on every push: `cargo fmt --check`, `cargo clippy -- -D warnings` and
  `cargo test`, plus a Windows job that builds the installer and uploads it as a
  workflow artifact. Both jobs run on Windows, because the shell integration
  needs `winreg` and `windows-sys`.
- **A test suite** covering the ODS round trip, the day-zero time rule, the
  registry contract, display formatting and the package inspector, including
  packages built in memory and a needle straddling a read boundary.
- **Sample workbooks** and a browser UI harness, generated by `scripts/make_*`,
  covering a genuine macro-enabled `.xlsm`, a file carrying external links, a
  data connection, an embedded object and an ActiveX control.
- **Version bookkeeping**: `scripts/bump_version.py` writes the version to every
  copy or fails when they disagree, and neither build script holds a version
  literal.
- **Editor policy**: `.editorconfig` and `.gitattributes` agree on line endings
  and indentation.
