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
- **Never runs macros.** An `.xlsm` is opened as plain data: the VBA project
  inside `xl/vbaProject.bin` is not executed, extracted or written anywhere.
  That makes ExcelScry a safe way to look inside a macro-enabled workbook from
  a source you do not trust, and `.xlsm` gets the same shell registration as
  the other Excel formats.
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
  associations.rs       Windows registry contract for the shell integration
  windows/hooks.nsh     NSIS installer hooks (context menu, associations)
scripts/
  make_icons.py         generates src-tauri/icons/* from code
  make_sample.py        generates samples/ExcelScry-sample.xlsx
  make_macro_sample.py  generates samples/ExcelScry-macro-sample.xlsm
  make_ui_harness.py    generates test/ui-harness.html
  build.sh / build.ps1  build and collect artifacts
  capture-window.ps1    screenshot a running app window (verification aid)
samples/                sample workbooks covering the awkward cases, including
                        a genuine macro-enabled .xlsm
```

## Tests

```bash
cd src-tauri && cargo test        # 28 unit tests
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
  numbers, times and durations.

`python scripts/make_macro_sample.py` builds a **genuine macro-enabled** `.xlsm`
to test against: it rewrites the content type to
`application/vnd.ms-excel.sheet.macroEnabled.main+xml` and adds a real OLE2
`xl/vbaProject.bin` part, so the viewer is exercised against a macro-enabled
package rather than a renamed `.xlsx`. The VBA streams in the fixture are
placeholders, not runnable macros — deliberately, since nothing reads them.
[docs/xlsm-verification.html](docs/xlsm-verification.html) shows it opening.

### UI harness

The app's front end lives inside the executable, so the UI cannot be driven by
ordinary browser tooling. `python scripts/make_ui_harness.py` inlines
`ui/styles.css` and `ui/app.js` into `test/ui-harness.html` together with a mock
of the Tauri bridge that reproduces the IPC contract exactly (including
`get_rows` returning the same clamped camelCase page shape as the Rust side).
Open that file in a browser to exercise the grid, tabs, virtual scrolling,
dialogs and escaping without a build. It is a development tool and is not
shipped.

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
