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

### Outputs

| Artifact | Path |
|---|---|
| Portable executable | `dist/ExcelScry-portable/ExcelScry.exe` |
| NSIS installer | `dist/ExcelScry-0.1.0-setup.exe` |
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
3. The **Windows integration** dialog (the gear button) shows the current state
   and offers *Make ExcelScry the default…*, which opens the Default Apps page
   pre-filtered to ExcelScry for the one-click confirmation.

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

## Repository layout

```
ui/                     front end: index.html, styles.css, app.js (no bundler)
src-tauri/src/
  main.rs               Tauri commands, window setup, single-instance handling
  model.rs              cell model and per-type display formatting
  workbook.rs           reading via calamine, absolute-coordinate normalisation
  ods_export.rs         writing ODS via spreadsheet-ods
  trust.rs              package inspection: macros, links, connections, objects
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
  build.sh / build.ps1  build and collect artifacts
  capture-window.ps1    screenshot a running app window (verification aid)
samples/                sample workbooks covering the awkward cases: one with a
                        genuine macro-enabled .xlsm, and one carrying external
                        links, a data connection, an embedded object and an
                        ActiveX control
```

## Tests

```bash
cd src-tauri && cargo test        # 49 unit tests
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
pull request, and on demand from the Actions tab. Both jobs use
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
```

So formatting is enforced, a new clippy warning fails the build, and `--locked`
makes the committed `Cargo.lock` part of the contract. **Run `cargo fmt` before
pushing** — the check reports a diff rather than rewriting files for you. The
same job runs `python scripts/bump_version.py --check`, so a version bumped in
one file and missed in another fails CI instead of shipping a mislabelled
installer.

Rust is not the only thing with a formatting rule. `.editorconfig` sets UTF-8,
LF and a four-space indent — two for the front end, configuration, Markdown and
the NSIS hooks — and `.gitattributes` stores every text file normalised with LF
while checking `.ps1`, `.nsh`, `.bat` and `.cmd` out as CRLF. The two are a pair:
`.editorconfig` decides what an editor writes, `.gitattributes` what git stores,
and neither should be changed without the other.

**`installer`** builds the app and bundles the NSIS installer through
[`tauri-apps/tauri-action`](https://github.com/tauri-apps/tauri-action), then
uploads the executables and bundles as a workflow artifact. It passes no
`tagName` or `releaseId`, so nothing is published and no repository write is
needed. Two things that are deliberately absent:

- **no Node/npm setup** — the front end is static files that `frontendDist`
  points straight at, and there is no `beforeBuildCommand` to run;
- **no NSIS install step** — `tauri-bundler` fetches and hash-verifies its own
  NSIS toolset (`NSIS_URL` → `nsis-3.13.zip`) into its cache, so a preinstalled
  `makensis` is not required.

**`release`** runs only for a `v*` tag and is the one job that writes to the
repository. It checks that the tag names the version in the manifest, builds the
portable executable and the installer, and has `tauri-action` create the GitHub
Release with both attached — body from `scripts/release_notes.py`, with GitHub's
own generated notes appended. The `installer` job skips tag pushes, so a tag is
built once rather than twice.

## Releasing

`src-tauri/Cargo.toml` holds the version. Three other files repeat it —
`tauri.conf.json` (which names the installer), `Cargo.lock` (which cargo checks
under `--locked`) and the artifact table above — and a release is only correct
when all four agree. Neither build script holds a version literal, so a stale
copy cannot silently mislabel an installer.

```bash
python scripts/bump_version.py --check      # do the copies agree?
python scripts/bump_version.py 0.2.0        # write every copy at once
```

`--check` fails on a hardcoded version in either build script, and
`--check --tag v0.2.0` fails when a tag does not name the version in the
manifest — a release tag has to match the build it points at. After a bump,
`cargo metadata --manifest-path src-tauri/Cargo.toml --locked` should succeed;
it fails if the lock was left behind.

Then, in order:

1. **Move the `Unreleased` entries** in [CHANGELOG.md](CHANGELOG.md) into a new
   `## [0.2.0] - YYYY-MM-DD` section.
2. **Read the notes before tagging them**, because they are what the release
   page shows:

   ```bash
   python scripts/release_notes.py          # the version in the manifest
   ```

   That prints the changelog entry followed by every commit since the previous
   tag, straight from `git log`, so the notes cannot drift from the repository.
3. **Commit and tag**:

   ```bash
   git commit -a -m "Release ExcelScry 0.2.0"
   git tag -a v0.2.0 -m "ExcelScry 0.2.0"
   git push origin main --follow-tags
   ```

4. **Let the tag publish it.** Pushing the tag starts the `release` job above,
   which refuses to publish if the tag and the manifest disagree.

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
