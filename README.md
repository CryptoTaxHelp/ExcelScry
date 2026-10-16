# ExcelScry

A fast desktop viewer for Excel workbooks and OpenDocument spreadsheets, built
with Rust and Tauri 2. It renders sheets in a virtualised grid, exports any open
workbook to **ODS**, and ships as both a **portable executable** and an **NSIS
installer** that adds a *Scry Excel File* entry to the Windows context menu and
registers ExcelScry with Windows' Default Apps list.

![ExcelScry viewing a workbook](docs/screenshot.png)

## What it does

- **Reads** `.xlsx`, `.xlsm`, `.xlsb`, `.xls`, `.xla` and `.ods` (everything
  [calamine](https://crates.io/crates/calamine) supports).
- **Says what the file carries before you open it elsewhere.** A badge in the
  status bar reports macros, external links, external data connections,
  embedded objects and ActiveX controls, and opens a dialog with the detail.
- **Never runs any of it.** An `.xlsm` is opened as plain data: the VBA project
  inside `xl/vbaProject.bin` is not executed, extracted or written anywhere,
  and the inspector only reads the package's member list. That makes ExcelScry
  a safe way to look inside a workbook from a source you do not trust, and
  `.xlsm` gets the same shell registration as the other Excel formats.
- **Exports** the open workbook to `.ods` with values kept as real numbers,
  dates, times, booleans and strings, so the result opens as a genuine
  spreadsheet rather than a wall of text. The first used row is styled bold.
- **Virtualised grid**: only the cells in view exist in the DOM and cells are
  fetched in tiles as you scroll, so a 500,000-row sheet stays responsive.
- Cell types are rendered the way a spreadsheet does: numbers right-aligned,
  dates as `YYYY-MM-DD`, times as clock times, durations past 24 hours as
  `36:00:00` (never wrapped), errors in red, booleans centred.
- Sheet tabs, keyboard navigation, per-cell copy (`Ctrl+C`), `Ctrl+O` to open,
  drag-and-drop, and a cell readout bar.
- Opens in a single instance: double-clicking another workbook reuses the
  running window.
- Accepts a file path on the command line, which is what the context-menu entry
  uses.

## What the badge reports

Opening a workbook inspects the *package* rather than the cells, because a
viewer is often pointed at a file from a source you do not trust. Five signals
are reported:

| Signal | Where it is found |
|---|---|
| Macros | `xl/vbaProject.bin` or its signature, Excel 4.0 `xl/macrosheets/`, ODF `Basic/` and `Scripts/`, or calamine's VBA probe for a legacy `.xls` |
| External links | `xl/externalLinks/` parts, or `<table:table-source>` in an ODF `content.xml` |
| Data connections | `xl/connections.xml` / `.bin`, or `<table:database-range>` in ODF |
| Embedded objects | `xl/embeddings/` parts |
| ActiveX controls | `xl/activeX/` parts |

The report is deliberately tri-state: `null` means *not inspected for this
format*, never *absent*. A legacy `.xls` is an OLE2 compound file with no member
list to read, so only its macros can be checked — and the badge says exactly
that instead of implying the rest came back clean. Embedded objects and ActiveX
are treated the same way in an `.ods`, where there is nothing reliable to look
for.

![The status bar badge and the detail dialog](docs/trust-badge.png)

A workbook that actually carries macros gets one thing more: a standing line in
the status bar, *Macros present — ExcelScry will not run them*, so the
reassuring half of the message is there without opening the dialog. The badge
never shrinks away, and that sentence does not clip either — the file name and
the trailing readout ellipsize instead. Only on a window too narrow for the
whole line (below 720px, just above the point where the bar would overflow) does
the sentence step aside, leaving the badge to name the hazard.

![The macro note in the status bar](docs/macro-note.png)

Nothing is ever executed, extracted or written to disk: the inspector reads part
names, plus two element names from an ODF `content.xml`, which is streamed
through a window rather than buffered because it can be very large. This is a
description of a file, not a malware scan.

## Requirements

| | |
|---|---|
| OS | Windows 10/11 (the shell integration is Windows-specific) |
| Rust | 1.77+ with the `x86_64-pc-windows-msvc` toolchain |
| C++ build tools | MSVC (Visual Studio Build Tools with the C++ workload) |
| WebView2 | Preinstalled on Windows 10/11; the installer can bootstrap it |
| Node.js | **Not required** — the front end is plain HTML/CSS/JS |

## Building

```bash
cargo tauri build
```

This is the command behind `scripts/build.sh` (bash) and `scripts/build.ps1`
(PowerShell), which additionally collect the two deliverables into `dist/`:

```bash
./scripts/build.sh          # or:  powershell -File scripts/build.ps1
```

Both scripts insist on the **updater signing key**: `createUpdaterArtifacts`
is on, so every build signs its own updater artifact and the bundler refuses to
produce one without the private half of the key in `tauri.conf.json`. The
scripts take `TAURI_SIGNING_PRIVATE_KEY` from the environment (which is how CI
passes the repository secret) or fall back to `~/.tauri/ExcelScry.key`, and stop
with a pointer to [Updates](#updates) when neither is there. A build is either
signed or it fails — never quietly unsigned.

### Outputs

| Artifact | Path |
|---|---|
| Portable executable | `dist/ExcelScry-portable/ExcelScry.exe` |
| NSIS installer | `dist/ExcelScry-0.2.0-setup.exe` |
| Raw build products | `src-tauri/target/release/ExcelScry.exe`, `src-tauri/target/release/bundle/nsis/` |

The **portable** build is a single self-contained `.exe` — the front end is
embedded in the binary, so there is nothing to install and no loose assets. It
still needs the WebView2 runtime, which is already present on Windows 10/11.

## Windows integration

The installer (`bundle.windows.nsis.installerHooks` → [hooks.nsh](src-tauri/windows/hooks.nsh))
performs a per-machine install and, at the end of it, writes:

- a **context-menu verb** — `Scry Excel File` — under
  `Software\Classes\SystemFileAssociations\.xlsx\shell\ScryExcelFile`, and the
  same for `.xlsm` and `.xls`. Registering it under `SystemFileAssociations`
  rather than under a ProgID means the entry stays on the menu **even when
  Excel or another viewer is the default**;
- a ProgID (`ExcelScry.xlsx`, `ExcelScry.xlsm`, `ExcelScry.xls`) with its open
  command and icon;
- an `OpenWithProgids` advert so ExcelScry appears on the *Open with* list;
- a `RegisteredApplications` + `Capabilities` entry so ExcelScry shows up in
  **Settings › Apps › Default apps**.

Everything the installer writes is removed again on uninstall.

### About "make it the default viewer"

The installer does everything Windows permits, but **no installer can silently
take over an existing file association on Windows 8 or later**: the per-user
choice lives in `FileExts\.xlsx\UserChoice`, which is hash-protected, and a
write that Windows cannot verify is ignored. Tools that fake the hash exist but
are unsupported and break on updates.

So ExcelScry does the honest version of it:

1. It registers itself as a **candidate** handler (ProgID, `OpenWithProgids`,
   Capabilities).
2. It writes the legacy `Software\Classes\.xlsx` default, which Windows *does*
   honour on a profile with no `UserChoice` yet — after backing up the previous
   value, so uninstalling restores it.
3. The **Windows integration** dialog (the gear button) shows the current state —
   the executable, whether the registration is per-user or per-machine, and what
   Windows currently opens **each registered extension** with (`.xlsx`, `.xlsm`
   and `.xls`) — and offers *Make ExcelScry the default…*, which opens the
   Default Apps page pre-filtered to ExcelScry for the one-click confirmation.

On Windows 11 the new context menu is collapsed; the entry appears under
**Show more options** (or `Shift+F10`).

`Add to context menu` in the same dialog performs the equivalent registration
for the current user only, which needs no elevation — that is the path for the
portable build. The registry layout is defined once, as data, in
[associations.rs](src-tauri/src/associations.rs) and unit-tested against the
same contract the NSIS hooks write.

## Usage

```bash
ExcelScry.exe                       # empty window, open a file from the toolbar
ExcelScry.exe "C:\path\book.xlsx"   # open a workbook straight away
```

- **Open file** / `Ctrl+O` — choose a workbook.
- **Export to ODS** — writes an `.ods` next to the original (you can pick a
  different location and the extension is corrected for you).
- **Reload** — re-read the file from disk.
- Arrow keys, `PageUp`/`PageDown`, `Home`/`End` — move the selection.
- `Ctrl+C` — copy the selected cell's text.

## Updates

ExcelScry updates itself from its own GitHub releases. At start-up it fetches
`https://github.com/CryptoTaxHelp/ExcelScry/releases/latest/download/latest.json`
— the `latest.json` asset `tauri-action` attaches to every release the workflow
builds — and, if that names a newer version, the status bar offers it as
**Update to *x.y.z***. Nothing is downloaded until that is clicked. The same
state lives in **Settings › Updates**: *This build*, *Release manifest*, a
manual *Check for updates*, *Install update* when there is one, and — when an
update is available — the release's own notes, so what changed can be read
before anything is downloaded. Installing asks once more: the status-bar pill
and the dialog's button open a confirmation carrying the release's opening
points, and nothing is downloaded until it is answered. A start-up check that
cannot reach GitHub says nothing; a manual one reports the reason.

Installing means: fetch the manifest, verify it, download the installer, verify
that too, hand it to Windows. The verification is the point of the arrangement,
and both signatures are checked against a key this build trusts: the `pubkey` in
`src-tauri/tauri.conf.json`, and any key a signed manifest has handed over since
([The signing key](#the-signing-key)):

- **The manifest is signed.** `latest.json.sig` is published beside `latest.json`
  on the release page, and the app verifies it over the manifest's own bytes
  *before it reads a field out of it*
  ([manifest.rs](src-tauri/src/manifest.rs)). Nothing else here would matter if
  the version, the notes and the installer's URL were whatever the endpoint
  happened to serve: TLS proves those bytes came from GitHub, not that GitHub's
  bytes are the ones this project put there.
- **The installer is signed.** Its `signature` — a
  [minisign](https://jedisct1.github.io/minisign/) signature over the installer
  — is checked while it is downloaded and **before it is allowed to run**,
  against the key the manifest names for this platform: the one this build ships
  with, or one the same manifest handed over. Anyone who can push to this
  repository can replace an asset; only the holder of the private key can
  produce an installer those bytes will accept.
- **Both signatures record the version they were made for**, and both are
  compared with the version the manifest announces: the manifest's by the app,
  the installer's by the updater, whose `requireSignedVersion` is on. `tauri
  build` and the release job stamp that version into the signatures' trusted
  comments, and the release job refuses a tag that disagrees with the manifest —
  so the numbers agree on every release this app can update from. Without that
  binding, a genuinely signed manifest and a genuinely signed installer could be
  paired across releases.
- **A release can replace the key.** The manifest may carry a `keys` block: keys
  to start accepting, and keys to retire. It is only taken from a manifest whose
  signature has already been checked, and it is merged into a record kept beside
  the app's own data, so nothing an older manifest says puts a retired key back.
  That is what makes a key that has leaked something the next release retires by
  saying so — rather than something every installed copy has to be walked
  forward from first.

An update that cannot be verified is one the app will not use: a manifest with
no signature beside it, or one whose signature does not cover it, is reported in
the status bar like any other failed check.

There is no moment when a copy can be offered an unsigned one. The release job
builds the release as a **draft** — the endpoint the app asks resolves to the
latest release that is neither a draft nor a pre-release, so while the manifest
is being signed that endpoint is still answering with the previous release's
signed manifest — and the manifest, its signature and the key roll in it are
read back out of the draft and checked before the release is published. The
reveal is the last step of the job, so a build that fails anywhere before it
leaves a draft no copy can see, rather than a public release nobody could update
from ([Continuous integration](#continuous-integration)).

The notes are the release body, so they arrive with the manifest and are covered
by its signature like every other field in it. They are still rendered as text —
headings, bullets and emphasis from the shapes this project's own notes use, with
`createElement` and `createTextNode` rather than `innerHTML`, and everything else
left literal — because a release body is Markdown, not HTML. A note containing
markup shows the characters, not the markup.

A link is the one thing a release body can ask the app to *do*, so it is the one
shape handled with care. Only `http` and `https` are links: `javascript:`, `data:`
and `file:` stay the characters someone typed. Clicking a link is handled in the
front end, so the window never navigates away from the app, and the address is
checked again in Rust and opened with the shell — in the default browser, not in
this window.

Every release is checked this way once it is published: the `verify-release` job
in CI fetches `latest.json` and the signature beside it from the endpoint above,
verifies both the manifest and the installer it points at against a key this
build trusts, and holds the key roll the manifest carries to the one this
repository publishes ([Continuous integration](#continuous-integration)), so a
release an installed copy could not use — or one handing over a key nobody
agreed to — fails its own run rather than a user's update check.

A failed download is reported in the status bar and changes nothing: the
signature mismatch, the version mismatch and the unsigned case each have their
own sentence. On Windows the installer is a per-machine NSIS package, so
finishing an update means the usual elevation prompt; it shows a progress bar
(`installMode: "passive"`), closes the running app and starts the new version.

### The signing key

One keypair, generated once:

```bash
cargo tauri signer generate -w ~/.tauri/ExcelScry.key
```

- the **public** half is committed as `plugins.updater.pubkey` in
  [tauri.conf.json](src-tauri/tauri.conf.json);
- the **private** half lives outside the repository (`~/.tauri/ExcelScry.key`,
  and `*.key` is gitignored as a second line of defence) and in the
  `TAURI_SIGNING_PRIVATE_KEY` **repository secret** that the release and
  installer jobs read. It was generated with an empty password, so no
  `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` is needed; set both if that ever changes.

Rotate by generating a new keypair, putting its **public** half in
[`src-tauri/updater-keys.json`](src-tauri/updater-keys.json), and letting a
release publish that roll — `scripts/manifest_keys.py` writes it into
`latest.json` before the release job signs it, and an installed copy takes it
from there. Committing the new key to `tauri.conf.json` is then only a matter of
how *new* builds start out, and can wait until the copies in the field have been
handed the key.

The two lists are the roll. `added` is published in every release until the key
it names is the one in `tauri.conf.json`, because a copy learns it from whichever
release it happens to check; `retired` stays listed for the same reason, and one
release is enough to retire a key — a copy that checks the endpoint after it was
published acts on it, and merges it into what it already knows rather than
replacing it. The record of all that is
`%LOCALAPPDATA%\com.excelscry.viewer\updater-keys.json`, which is per-user, is
not part of the installation, and is the file to delete to go back to the key a
build ships with.

A copy that never checks for updates again keeps the trust it had, so retiring a
key that has leaked and *keeping* every copy updateable are in tension: the
release that retires it reaches the copies that ask, and the stragglers it does
not reach keep trusting a compromised key — which is the reason to retire it
promptly rather than wait for them, and the reason a copy that has fallen far
enough behind may have to be updated by hand.

### What a release has to carry

The updater works from the first release built by this workflow; a release
published before it has no `latest.json`, so the endpoint answers 404 and the
start-up check stays quiet by design. The release job sets
`uploadUpdaterJson: true` (plus `uploadUpdaterSignatures`, `uploadPlainBinary`,
and `updaterJsonPreferNsis: true` because the manifest is built from the NSIS
`.sig`), which is what puts the installer, its signature, the portable executable
and `latest.json` on the release page, and `releaseDraft: true`, which is what
keeps that page out of reach until the two steps after it have signed the
manifest and checked it back:

| Asset | What the app does with it |
|---|---|
| `latest.json` | `version`, `notes`, `pub_date`, a `platforms` map keyed `windows-x86_64` (and `windows-x86_64-nsis`), and the `keys` roll the release is publishing |
| `ExcelScry_<version>_x64-setup.exe` | the installer an update downloads and runs |
| `ExcelScry_<version>_x64-setup.exe.sig` | the signature `latest.json` embeds for that platform |
| `latest.json.sig` | the signature over `latest.json` itself, which the app checks before it uses anything in it |
| `ExcelScry_windows_x64.exe` | nothing: this is the portable build, which the app never updates from, so nothing is signed for it. Its version resource is the one checkable thing it carries, and the release check holds it to the version the manifest announces |

## Repository layout

```
ui/                     front end: index.html, styles.css, app.js (no bundler)
src-tauri/
  updater-keys.json     the key roll releases publish: keys handed over, keys retired
  src/
  main.rs               Tauri commands, window setup, single-instance handling
  model.rs              cell model and per-type display formatting
  workbook.rs           reading via calamine, absolute-coordinate normalisation
  ods_export.rs         writing ODS via spreadsheet-ods
  trust.rs              package inspection: macros, links, connections, objects
  update.rs             update check and install, on tauri-plugin-updater
  manifest.rs           fetching the update manifest, checking its signature, and
                        the key roll a release carries
  associations.rs       Windows registry contract for the shell integration
  windows/hooks.nsh     NSIS installer hooks (context menu, associations)
scripts/
  make_icons.py         generates src-tauri/icons/* from code
  make_sample.py        generates samples/ExcelScry-sample.xlsx
  make_macro_sample.py  generates samples/ExcelScry-macro-sample.xlsm
  make_links_sample.py  generates samples/ExcelScry-links-sample.xlsx
  make_ui_harness.py    generates test/ui-harness.html
  bump_version.py       writes the version to every copy, or fails when they disagree
  release_notes.py      builds the release notes from the changelog and git history
  release.py            bumps, moves the changelog, commits, tags and pushes
  test_release.py       runs that release flow against a throwaway repository
  verify_release.py     checks that a published release can update an installed copy,
                        and that its key roll is the one this repository publishes
  manifest_keys.py      writes that key roll into the manifest before it is signed
  verify_workflow.py    checks that no release can be published before its manifest
                        is signed
  build.sh / build.ps1  build and collect artifacts
  capture-window.ps1    screenshot a running app window (verification aid)
samples/                sample workbooks covering the awkward cases: one with a
                        genuine macro-enabled .xlsm, and one carrying external
                        links, a data connection, an embedded object and an
                        ActiveX control
```

## Tests

```bash
cd src-tauri && cargo test        # 74 unit tests
cd src-tauri && cargo clippy --all-targets
```

The tests cover the parts that are easy to get subtly wrong:

- an **ODS round trip** that exports a sheet and reads it back, asserting that
  numbers stay numeric, that a date keeps its calendar day, that 36 hours stays
  a 36-hour duration, and that a block that starts at row 5 / column C is still
  at row 5 / column C afterwards;
- `parse_iso` treating Excel's day zero (`1899-12-30`) as a **time of day** —
  ODF stores a bare time as an ISO duration and calamine returns it as a
  datetime on that date, which would otherwise render as `1899-12-30 09:30:00`;
- the update manifest's signature, against a manifest signed by this project's
  own release key: an edited manifest, one signed by another key, one signed for
  a version it does not announce and one signed without a version at all are each
  refused, and the endpoint's variables are resolved the way the updater
  resolves them;
- the **key roll** a release may carry, against a manifest signed with the very
  key it retires: taking the roll leaves the new key trusted and the old one
  refused for both fixtures, an adopted key is what a manifest is then checked
  with, a roll naming something that is not a key or retiring everything is
  refused without being recorded, a retirement is written to disk and read back
  by the next run (and a damaged record refuses rather than forgetting one), and
  the updater's own manifest type still reads a document that carries a roll;
- the registry contract: the verb label, the quoted `"exe" "%1"` command, that
  `.xlsm` is registered exactly like `.xlsx` and `.xls`, and that uninstall
  removes exactly what install created;
- display formatting for large integers (no `f64` round trip), fractional
  numbers, times and durations;
- the package inspector, driven from zips built in memory: each signal is found
  and counted, `.rels` sidecars are not double counted, the ODF rules are chosen
  from the package manifest rather than the file name, and a needle that
  straddles a read boundary in a streamed `content.xml` is counted once.

`python scripts/make_macro_sample.py` builds a **genuine macro-enabled** `.xlsm`
to test against: it rewrites the content type to
`application/vnd.ms-excel.sheet.macroEnabled.main+xml` and adds a real OLE2
`xl/vbaProject.bin` part, so the viewer is exercised against a macro-enabled
package rather than a renamed `.xlsx`. The VBA streams in the fixture are
placeholders, not runnable macros — deliberately, since nothing reads them.
[docs/xlsm-verification.html](docs/xlsm-verification.html) shows it opening.
`python scripts/make_links_sample.py` builds the companion fixture, adding the
external links, connection, embedded object and ActiveX control that the badge
reports on.

### UI harness

The app's front end lives inside the executable, so the UI cannot be driven by
ordinary browser tooling. `python scripts/make_ui_harness.py` inlines
`ui/styles.css` and `ui/app.js` into `test/ui-harness.html` together with a mock
of the Tauri bridge that reproduces the IPC contract exactly (including
`get_rows` returning the same clamped camelCase page shape as the Rust side).
Open that file in a browser to exercise the grid, tabs, virtual scrolling,
dialogs and escaping without a build. It is a development tool and is not
shipped.

## Continuous integration

[.github/workflows/ci.yml](.github/workflows/ci.yml) runs on every push, every
pull request, and on demand from the Actions tab. Every job uses
`windows-latest`, because the app is Windows-only — the shell integration is
built on `winreg` and `windows-sys`, and a Linux runner would need the
`webkit2gtk` development packages before it could build at all.

**`checks`** runs, with a read-only token and a cached `target/`:

```bash
cd src-tauri
cargo fmt --all --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
python scripts/bump_version.py --check
python scripts/manifest_keys.py --check
python scripts/verify_workflow.py
python scripts/test_release.py
```

So formatting is enforced, a new clippy warning fails the build, and `--locked`
makes the committed `Cargo.lock` part of the contract. **Run `cargo fmt` before
pushing** — the check reports a diff rather than rewriting files for you. The
same job runs `python scripts/bump_version.py --check`, so a version bumped in
one file and missed in another fails CI instead of shipping a mislabelled
installer.

The same job also reads the release path itself. `python
scripts/manifest_keys.py --check` holds the key roll to the rules an installed
copy applies to it, and
[`scripts/verify_workflow.py`](scripts/verify_workflow.py) reads the release job's
steps and refuses a workflow that could make a release visible before its
manifest was signed — a draft instead of a published release, a signing step
moved after the publishing one, a publishing step that no longer checks what it
is about to reveal, or another job asking the action for a release of its own.
That check exists because nothing else can make it: the `verify-release` job runs
once the release is public and passes either way, so a step moved in a pull
request would be invisible until a copy asked the endpoint at exactly the wrong
moment.

The same job also runs the release flow itself, on a repository made for it.
`release.py` cuts a release of *this* project, so it can never be exercised on
this checkout; [`scripts/test_release.py`](scripts/test_release.py) copies the
real `src-tauri/` and `scripts/` into a temporary repository with a bare remote
to push to, and there checks every refusal the flow is supposed to make, what
`--dry-run` leaves alone, and a full release end to end — the four copies of the
version, the changelog section, the files the commit carries, the annotated tag
and the push. The scripts are copied rather than reimplemented, so what runs is
the code in this checkout, and the test fails if this repository's `HEAD` or
`git status` moved while it ran.

Rust is not the only thing with a formatting rule. `.editorconfig` sets UTF-8,
LF and a four-space indent — two for the front end, configuration, Markdown and
the NSIS hooks — and `.gitattributes` stores every text file normalised with LF
while checking `.ps1`, `.nsh`, `.bat` and `.cmd` out as CRLF. The two are a pair:
`.editorconfig` decides what an editor writes, `.gitattributes` what git stores,
and neither should be changed without the other.

**`mutants`** asks whether that test is still worth what it claims. It runs
[`scripts/mutate_release.py`](scripts/mutate_release.py), which breaks
`release.py` one deliberate way at a time — the branch-is-ahead-of-its-remote
check removed, the `Unreleased` entries left out of the new section, a
lightweight tag instead of an annotated one, `--resume` counting a commit it just
made as already pushed — and requires
[`scripts/test_release.py`](scripts/test_release.py) to fail for the mutation it
claims. A mutation the test passes, or one it fails on somewhere other than that
case, is a hole: the run reports it and exits 1, because a test that has stopped
catching something goes on passing, and would go on passing on the commit that
breaks the flow. Every mutation is applied to a repository a worker made for
itself out of this checkout — the test ends by comparing the `HEAD` and
`git status` of the root it runs from, so the copy has to be one — and never to
the working tree, so a run cannot leave a mutated `release.py` behind. It takes
minutes rather than seconds, which is why it is a job of its own beside `checks`
rather than a step in it, and it is skipped for a tag, whose branch push has
already been through it.

**`installer`** builds the app and bundles the NSIS installer through
[`tauri-apps/tauri-action`](https://github.com/tauri-apps/tauri-action), then
uploads the executables and bundles as a workflow artifact. It passes no
`tagName` or `releaseId`, so nothing is published and no repository write is
needed, and no updater files are uploaded — there is no release for them to
belong to. It does pass `TAURI_SIGNING_PRIVATE_KEY` from the repository secret,
because the build signs its updater artifact whether or not anyone publishes it.
A repository without that secret stops in a step of its own that says so, before
the build: a secret nobody set reaches the bundler as an *empty* variable rather
than a missing one, because that is what GitHub resolves an unset secret to, and
the complaint the bundler has for a missing key does not cover that — what comes
out instead is the key's decode failing with
`incorrect updater private key password: Missing comment in secret key`, which
blames the password rather than the key and never says a secret is missing. The
step is the same refusal `scripts/build.sh` and `scripts/build.ps1` make for a
local build, and it is what the `installer` job fails on while the secret is not
there.
Two things that are deliberately absent:

- **no Node/npm setup** — the front end is static files that `frontendDist`
  points straight at, and there is no `beforeBuildCommand` to run;
- **no NSIS install step** — `tauri-bundler` fetches and hash-verifies its own
  NSIS toolset (`NSIS_URL` → `nsis-3.13.zip`) into its cache, so a preinstalled
  `makensis` is not required.

**`release`** runs only for a `v*` tag and is the one job that writes to the
repository. It checks that the tag names the version in the manifest, builds the
portable executable and the installer, and has `tauri-action` create the GitHub
Release as a **draft**, with both attached — body from `scripts/release_notes.py`,
with GitHub's own generated notes appended. Alongside them it uploads the two
files the updater reads, `latest.json` and the installer's `.sig`, both built
from the same signed build. The `installer` job skips tag pushes, so a tag is
built once rather than twice.

A draft is invisible to the endpoint the app asks, which is what makes the
signing step safe to run after the build rather than inside it. Two steps follow,
because `tauri-action` cannot sign the manifest it writes:

- the first downloads `latest.json` from the draft, writes the repository's key
  roll into it, signs it with the version `bump_version.py --print` reports, and
  uploads `latest.json.sig` beside it. That is the signature an installed copy
  checks before it believes a word of the manifest ([Updates](#updates)), so this
  failing means the release cannot update anybody — and since the release is
  still a draft, it also means nobody is offered it. This is the one place in
  this workflow that installs the Tauri CLI itself: the CLI ships as a prebuilt
  npm binary, and `tauri-action` installs it the same way on this runner, but
  doing it explicitly means the step does not depend on what that action left
  behind;
- the second reads `latest.json` and `latest.json.sig` back out of the draft,
  runs [`scripts/verify_release.py`](scripts/verify_release.py) over them with
  `--signature-only` — the manifest's version, the signature over its bytes, and
  the key roll it carries, with nothing downloaded, because a draft's assets are
  not public — and only then publishes the release as the latest one. The check
  is there rather than in the `verify-release` job below because it is the last
  thing that happens before the manifest becomes reachable: everything a copy
  can see, from the first moment it can see anything, has been checked.

**`verify-release`** runs after `release`, and only when it succeeded: publishing
the files and being updateable from them are different questions.
[`scripts/verify_release.py`](scripts/verify_release.py) fetches `latest.json`
and the signature published beside it from the endpoint the app uses, checks
that it announces the version being verified, verifies that signature over the
manifest's own bytes, and then downloads the installer the manifest points at
and verifies its signature too — for each of them the key id, the signature, the
global signature over the trusted comment and the version that comment records,
which are the checks the app itself makes. Every asset the workflow publishes is
checked, not only the ones an update passes through: the `.sig` published beside
the installer has to be the signature the manifest embeds, since that embedded
copy is the one an installed app verifies, and the portable executable has to be
the release that is being verified, read out of its own version resource —
nothing signs it, so a portable build left behind by an earlier release is
otherwise invisible. A manifest that is missing, that is unsigned, that still
belongs to an older release, that carries an installer whose bytes do not match
its signature, or a portable build from another release, fails the run for that
tag. It needs no token: the assets of a published release are public, and the key
it verifies against is committed in this repository.

## Releasing

`src-tauri/Cargo.toml` holds the version. Three other files repeat it —
`tauri.conf.json` (which names the installer), `Cargo.lock` (which cargo checks
under `--locked`) and the artifact table above — and a release is only correct
when all four agree. Neither build script holds a version literal, so a stale
copy cannot silently mislabel an installer.

```bash
python scripts/release.py 0.2.0 --dry-run    # say what it would do
python scripts/release.py 0.2.0              # do it
python scripts/release.py --resume           # finish a run that stopped part-way
```

That is the whole sequence in one command: bump every copy, move the
`Unreleased` entries in [CHANGELOG.md](CHANGELOG.md) into a dated section,
commit them, tag the commit and push both. The pushes come last, so nothing
leaves the machine until every check has passed, and everything before them is
local and reversible. A failure after them — a rejected tag push, say — is
reported with the one command that finishes the job. It refuses to start unless:

- the branch is in step with its upstream and the tree is clean — a release
  commit carries the version and nothing else;
- the new version is a semantic version ahead of the current one, with no such
  tag locally or on the remote;
- the `Unreleased` section has entries to move, and no section for the version
  is there already.

Before pushing it prints the notes the release page will carry, read from the
commit it just tagged, so they can be read before they are published rather than
after. `--dry-run` stops before anything is written.

**A release that fails part-way is finished, not repeated.** The version is
written before anything is committed, so a run that dies in between — a commit
that does not happen, a tag push the remote refuses, a machine that goes away
mid-push — leaves a repository that no longer reads as ready to release, because
the version is not ahead of itself any more, while nothing it did is wrong.
`--resume` takes that state and finishes it: it commits what is still
uncommitted, writes any copy of the version a killed run left behind, makes the
tag again if there is none, and pushes whatever the remote does not have, saying
which of those each step is first. Steps that are already done are left alone,
so it is safe to run again: on a release that is already out it reports that
there is nothing to finish and stops. It refuses to guess — a tag of that
version held elsewhere as a different tag object, a change the release did not
make, or a `HEAD` that is no release commit are refused rather than resolved —
and every failure that leaves a half-made release names this command in its own
message, so the way out is in the output rather than in this file.

Every push runs the same sequence in a throwaway repository:
[`scripts/test_release.py`](scripts/test_release.py) is what makes the refusals
above, the release itself, and the states a failed run leaves behind — finished
by `--resume`, or refused — checked behaviour rather than a description of one,
and [`scripts/mutate_release.py`](scripts/mutate_release.py) is what keeps that
test honest: it breaks the flow a mutation at a time, and a mutation the test
does not notice is a failure of its own.

The three pieces are still usable on their own:

```bash
python scripts/bump_version.py --check      # do the copies agree?
python scripts/bump_version.py 0.2.0        # write every copy at once
python scripts/release_notes.py             # the notes, without releasing
```

`--check` fails on a hardcoded version in either build script, and
`--check --tag v0.2.0` fails when a tag does not name the version in the
manifest — a release tag has to match the build it points at. After a bump,
`cargo metadata --manifest-path src-tauri/Cargo.toml --locked` should succeed;
it fails if the lock was left behind. `release_notes.py` prints the changelog
entry followed by every commit since the previous tag, straight from `git log`,
so the notes cannot drift from the repository. The release job runs both of
these — the tag check and the notes — which is why `release.py` runs them on the
commit it is about to push, and prints the notes it built rather than promising
they will be right.

**Let the tag publish it.** Pushing the tag starts the `release` job above,
which refuses to publish if the tag and the manifest disagree. The
`TAURI_SIGNING_PRIVATE_KEY` repository secret has to hold the private half of the
key in `tauri.conf.json` ([Updates](#updates) describes the pair). Without it the
release job fails rather than publishing an installer no installed copy of the
app would accept — and because the build fails before the release is created,
nothing is half-published: add the secret and use **Re-run failed jobs** on that
run, which needs no new commit or tag.

Once the release is published, the `verify-release` job checks what an installed
copy would get from it. If that fails, what is wrong is the release rather than
the check, and the release page is what every installed copy reads: republish
the assets, or remove the release, before anything updates from it. Nothing in
this script can do that for you — it stops at the push, and the push is what
starts all of it.

## Limitations

- **Cell formatting is not rendered.** Colours, fonts, column widths, merged
  cells and number formats are not exposed by calamine, so the grid uses a
  uniform look and a default date/number rendering. Values themselves are shown
  exactly.
- **Formulas are shown as their last cached value**, not as formulas.
- **Macros are never run.** A `.xlsm` opens as data; its VBA project is not
  executed, extracted or written anywhere. ExcelScry only ever reads.
- Workbooks are read into memory. Beyond **2,000,000 cells** the remainder is
  dropped and the status bar says so.
- A workbook whose sheets cannot be parsed fails with an explanatory message
  rather than a partial view.
- Password-protected workbooks are not supported; the error says so.
- Exports are value-only; the ODS file contains no formulas.
- **Updates only exist from the first release built by the workflow**, and only
  for a strictly newer version. Installed copies of an older, pre-updater
  release cannot update themselves; they have to be replaced by hand once.
- The update check is two requests to the `releases/latest/download` URL - the
  manifest and its signature - with no retry and no background polling: a
  machine that is offline at start-up simply does not offer the update until the
  next launch or a manual check.
