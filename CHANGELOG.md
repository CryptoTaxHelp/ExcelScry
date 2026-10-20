# Changelog

All notable changes to ExcelScry are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and this project follows [Semantic Versioning](https://semver.org/spec/v2.0.0.html).
The `Unreleased` section collects work for the next release; a release moves
those entries into a version section and tags the commit that holds it.

## [Unreleased]

### Added

- **A format's zero section and its section colours survive the ODS export.**
  `0.00;[Red]-0.00;"-"` used to lose both the red and the dash: only the
  positive and negative sections were translated, and a section's colour was
  dropped. The third section is now written as a `value()=0` `style:map`, with a
  literal-only zero (`"-"`) borrowing the positive section's number style so a
  percentage or currency zero does not gain a stray `%` or symbol. Each
  section's `[Red]`, `[Blue]`, other named colour or indexed colour becomes
  `fo:color` on that section's ODF number style, and the reader turns it back
  into the matching section token. A literal-only negative section is handled
  the same way; an empty zero section (hide zeros) is still not represented.
- **Export to ODS keeps a sheet's bold and italic text, its colours and its
  column widths.** A styled report used to come back as uniform plain text with
  evenly spaced columns, because the exporter wrote values and number formats
  only. `cell_styles.rs` now reads each cell's font and fill beside its number
  format — `<b>`, `<i>`, `fo:font-weight`, `fo:font-style`, text colour and
  background — resolving Excel's theme colours (with their tint), its 64-entry
  indexed palette and explicit ARGB values, and following an ODF cell style's
  `parent-style-name` chain. The export writes those as ODF text and cell
  properties on the same cell style as the number format, so a bold money total
  stays bold money; a fill on a cell with no value is written as a styled blank,
  and column widths already read for the grid are written as the matching
  physical lengths. The grid itself still renders uniformly, and borders and
  font names/sizes are still not exported.
- **Row heights and merged regions survive the ODS export, and column widths
  cover the whole table.** A report's tall title row, tall total row and merged
  titles used to reopen flattened and unmerged. Row heights, including the
  workbook's default height, are now written as ODF row styles, and each merged
  region is written as a spanning cell with its covered cells marked; a width
  on a blank column at the edge of the report is kept too, rather than being
  dropped because no cell used that column.
- **Frozen rows and columns stay pinned while the sheet scrolls.** A workbook
  that freezes its header row or label column used to open with the frozen band
  scrolling away like any other cell. `sheet_layout.rs` now reads Excel's
  `<pane state="frozen">` and LibreOffice's `HorizontalSplitMode`/
  `VerticalSplitMode` view settings, and the front end draws the four frozen
  panes over the body: the fixed corner, the top band following horizontal
  scrolling, and the left band following vertical scrolling. Frozen row
  numbers and column letters stay with their cells, clicks, the selection and
  scroll-into-view respect the frozen edge, and wheel scrolling over a frozen
  pane still moves the body. A text table's separately chosen header row is
  unchanged.
- **The grid is laid out with the sheet's own column widths, row heights and
  merged cells.** A report built in Excel used to open as a uniform 112 × 24
  grid: a 180-pixel first column collapsed to the default, a deliberately tall
  title row lost its height, and every merged region showed only its anchor in
  one cell with the rest of the region blank. `src-tauri/src/sheet_layout.rs`
  now reads `<cols>`, `<row ht>` and `<mergeCell>` from each worksheet and
  `table:column-width`, `table:row-height` and spanned cells from an ODF
  `content.xml`, converts them to pixels and sends them with the sheet's
  summary. The front end builds prefix sums of the sizes — two typed arrays, so
  a million rows cost no per-cell arithmetic — and lays the grid, the header
  strips, hit-testing, scroll-into-view and Page Up/Down out from them. Each
  merged region is drawn once, from its anchor, at the size of the whole span,
  so it has no stray grid lines inside it, and selecting any cell it covers —
  by click or arrow key — selects the region. Arrow keys leave a merged region
  as a whole. Sheets that specify nothing keep the app's own metrics, and a
  sheet too tall for a browser's canvas is scaled down as a whole so it stays
  scrollable. The UI harness has a *Sized & merged* sheet
  ([docs/sheet-geometry.html](docs/sheet-geometry.html)) for exercising it.
- **Export to ODS keeps the sheet's number formats.** The grid has always shown
  `€ 1,234.50` where the file stored `1234.5`, but the export wrote only the
  value, so the `.ods` opened with `1234.5` under a `dd/mm/yyyy` heading that
  read as a serial. Each distinct format code is now translated into the ODF
  style that says the same thing — money as a `number:currency-style` with its
  symbol, percentages as a `number:percentage-style` that scales by 100, dates
  and times as `number:date-style`/`number:time-style` with the same field
  widths, and a `[h]:mm:ss` duration as a time style that does not wrap at 24
  hours. Excel's second section becomes an ODF `style:map`, so a negative stays
  `($1,234.50)`. Codes ODF cannot express — fractions, scientific notation,
  `mmmmm`, the Buddhist calendar — are written as plain values rather than
  approximated. The new `src-tauri/src/ods_formats.rs` does the translation;
  `cell_styles.rs` learned to read `style:map` back into a two-section code and
  to read a non-overflowing time style as a duration.
- **A delimited file that opens wrong can be re-read with another delimiter or
  encoding.** A `.csv` is bytes plus a convention: the extension asks for a
  comma, the first record can disagree, and a file from a Windows tool is
  Windows-1252 far more often than it is valid UTF-8. Rather than making the
  guess final, a bar above the grid names what the file was read with and
  offers comma, semicolon, tab and pipe, and UTF-8, Windows-1252 and UTF-16 in
  either byte order. Choosing re-reads the file; Reload keeps the choice and
  Auto puts the guess back. When a read leaves undecodable bytes, the bar says
  so and suggests another encoding.
- **CSV encodings are recognised, so the picker is rarely needed.** A `.csv`
  from a Windows tool is usually Windows-1252 or UTF-16 without a byte order
  mark, and both used to need the bar: the first arrived as `caf�`, the second
  as one character per byte. The bytes now settle it before the first record is
  read — a stream that is not UTF-8 is Windows-1252, and a zero byte beside
  every character is UTF-16, little- or big-endian, with the zeros' side
  deciding the byte order and the window decoded to confirm it is text. A file
  whose first 8 KiB look like UTF-8 but whose later bytes do not is read again
  whole as Windows-1252 rather than shown with replacement characters, and a
  multi-byte character cut in half by that window is not mistaken for one. The
  encoding stays on Auto in the bar, which names what the read settled on;
  choosing one still overrides detection, and an explicit UTF-8 read of a
  legacy file still reports the replacement characters and the hint
  ([docs/text-dialect.html](docs/text-dialect.html)).
- **The delimiter and encoding chosen for a file are remembered for it.** An
  export that arrives semicolon-separated in Windows-1252 every week had to be
  fixed every week, because the choice lived only in the window. A choice made
  in the bar is now written to `text-dialects.json` in this build's local data
  directory by `src-tauri/src/text_prefs.rs`, keyed by the file's canonical
  path — made lower-case on Windows, so another spelling of the same file finds
  it. The next open of that file starts from the remembered parts and detects
  only what nobody chose; an explicit *Auto* takes effect immediately and
  forgets the entry rather than being refilled from it. Reload and the first
  open never rewrite the store, so opening a file cannot silently change what
  the next open does. Only what was chosen is stored, never a detection result,
  and a store that cannot be read or written is ignored: the file still opens,
  and the choice applies for that session.
- **A delimited file's first record stays visible while its body scrolls.** In a
  `.csv`/`.tsv` that row is the header, and losing it a few hundred rows down is
  what makes a wide table hard to read. The row is pinned above the grid from
  the first pixel of scroll, follows horizontal scrolling, keeps its row number
  beside it, and carries the selection outline when the selected cell is in it.
  Workbooks are left alone: their first row is data, not necessarily a header.
- **The pinned header row is the user's to choose.** An export that puts a title
  and a generated-on line above its column names was pinned by the wrong row,
  and that title is not what the body needs to keep in sight. A *Header row*
  field in the delimited-text bar names the record to hold, and clicking a row
  number in the strip moves the pin there; clearing the field, or clicking the
  chosen number again, pins nothing. Everything above the chosen row scrolls
  away normally before the strip takes over at that row's own top edge, the
  strip takes the chosen row's height, and a selected header carries its
  outline with it. The choice belongs to the open file, so another delimiter,
  another encoding or Reload keeps it; a workbook still pins nothing
  ([docs/header-row.html](docs/header-row.html)).

### Changed

- **The update manifest names its assets by file, not by asset id.**
  `tauri-action` writes each platform's `url` as the release API's
  `.../releases/assets/<id>`, which serves the asset only to a request that asks
  for `application/octet-stream`; every other tool gets that asset's JSON
  metadata, and `<id>.sig` names the same asset again rather than the signature
  published beside it. `scripts/manifest_urls.py` now rewrites those URLs to the
  release page's `.../releases/download/<tag>/<name>` before the manifest is
  signed, looking the names up in the release being published rather than
  guessing them, so the signature covers the bytes an installed copy downloads.
  The verify job checks the published manifest names its assets that way.
- **A release fails in seconds when the updater signing key is missing or
  wrong.** Every `tauri build` signs its updater artifact, so without
  `TAURI_SIGNING_PRIVATE_KEY` the build failed after compiling the whole app, as
  an exit 1 from `tauri build` that said nothing about a secret; a secret
  holding another key's material built and signed perfectly and was then
  refused by every installed copy. The release and installer jobs now read the
  key before they build anything — the release job before it even installs
  Rust — decoding it the way the Tauri CLI does, checking its own checksum,
  decrypting it with the password secret (empty when unset, as the CLI assumes)
  and holding its key id and public key against the `pubkey` in
  `tauri.conf.json`. The installer job used to have a presence-only shell check
  of its own, which a secret holding the wrong key passed; it now runs
  `verify_signing_key.py`, the same preflight as the release job, so a missing
  or wrong key stops either build before a compile. `scripts/verify_workflow.py`
  holds both jobs to that ordering, and refuses a job whose check is missing,
  moved after the build, or replaced by one of its own.

## [0.3.2] - 2026-10-19

### Fixed

- **Verifying a published release reads the manifest it is handed.** The
  manifest names each asset by its GitHub API URL
  (`.../releases/assets/<id>`), which serves the asset only to a request that
  asks for `application/octet-stream`, and `.sig` appended to an id names that
  same asset again rather than the file published beside it. Verification
  therefore fetched JSON where the installer should be and looked for
  `<id>.sig` instead of the installer's own signature, and failed 0.3.1
  although the release itself was sound. It now asks for the bytes the updater
  asks for, and resolves the asset's own name before looking for the signature
  published beside it.

## [0.3.1] - 2026-10-19

### Fixed

- **The delimited-text samples are the files the generator wrote, on every
  machine.** `.gitattributes` normalised `samples/ExcelScry-sample.csv` and
  `.tsv`, so a checkout with native line endings turned the break inside a
  quoted field into CRLF and the reader - correctly - produced `two\r\nlines`
  where the test expected `two\nlines`. That passed only on a machine that had
  just regenerated the samples, and failed on Windows CI. Both files are marked
  `-text`, so a clone reproduces their bytes exactly.

## [0.3.0] - 2026-10-18

### Added

- **OpenDocument spreadsheets are viewable, and the shell knows it.** An `.ods`
  — from LibreOffice, Google Sheets or ExcelScry's own export — opens in the
  same grid as an Excel workbook, and the format now gets everything the Excel
  formats get from the installer and from *Add to context menu*: the **Scry
  Excel File** entry, a ProgID with its open command and icon, an
  `OpenWithProgids` advert, and a Default Apps capability. The registry
  contract in `associations.rs` and the NSIS hooks register the same four
  extensions, and the Windows integration dialog grows a row for `.ods` by
  reading the registration table rather than a copy of it.
- **A sample `.ods` the app did not write.** `scripts/make_ods_sample.py` builds
  the ODF package by hand, so the viewing test reads XML of the shape a real
  ODF producer emits: a table at C6, a blank row inside the range, a date, a
  boolean, a time of day, markup-looking text and a sheet wider than the
  viewport.
- **Cells are shown with the number format the workbook gives them.** A cell
  formatted as currency, a percentage or `dd/mm/yyyy` now reads `$23,988.00`,
  `35.0%` and `15/01/2026` instead of the raw value and this app's own
  conventions. `calamine` exposes values only, so `cell_styles.rs` reads the
  style tables itself — `xl/styles.xml` with each worksheet's style index for
  OOXML (including the workbook's 1904 date system), and `content.xml`'s
  `number:*` styles with the cell styles pointing at them for ODF — and
  `number_format.rs` renders through `ssfmt`, which implements Excel's
  format-code grammar (sections, bracket currencies, escaped literals, date and
  elapsed-time tokens). ODF number styles are translated into that grammar, so
  both file families go through one renderer. Values stay typed: the grid
  changes, the export does not. Where a format would lose data — a 17-digit
  integer, a boolean, an error — the exact rendering wins.
- **Delimited text files open in the same grid.** A `.csv` or `.tsv` — from a
  database export, a script, or Excel's *Unicode Text* save — is read as
  records and lifted into the model an ODS sheet already uses: one sheet named
  after the file, ragged rows, a blank line kept as a row, and a status bar
  that says what the file is. The reader handles what real files contain: a
  quoted field may hold the delimiter, doubled quotes and line breaks; `CRLF`,
  `LF` and `CR` all end a record; a UTF-8 BOM is skipped and UTF-16 with a BOM
  is decoded; a `.csv` that uses `;` or a tab is recognised from its first
  record; and undecodable bytes become replacement characters instead of
  losing the cell. Every value stays text, because a CSV has no types:
  `000123` and `1E5` are shown as written rather than reparsed, so an export
  cannot quietly change them. The reader shares the workbook's cell budget, so
  a text file too large to hold is truncated and says so. The trust badge
  reports a text file clean — it is not a package, so there is nothing in it
  that could carry macros, links, connections, objects or controls.

### Fixed

- **An `.ods` time cell showed its raw duration instead of the time.** ODF
  stores a bare time as an ISO duration, and the reader passed it through as
  text, so a 09:30 cell displayed `PT09H30M00S`. Time values are now parsed: a
  span shorter than a day becomes a clock time, a longer one stays a duration
  that renders as `36:00:00` rather than wrapping, and fractional seconds
  survive. Anything that is not a duration is still shown verbatim.

## [0.2.0] - 2026-10-16

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
  The states a failed run leaves are in there too, each made by failing the real
  thing rather than by writing the state out: a commit the repository refuses, a
  tag push the remote refuses, a bump killed between its own writes — and for
  each of them, the `--resume` that finishes it, what it does not do again, and
  what it refuses.
  The scripts are copied rather than reimplemented, so the code under test is
  the code in the checkout, and the run fails if this repository's `HEAD` or
  `git status` moved while it was going.
- **A release that fails part-way is finished, not repeated.** `release.py`
  writes the version before it commits anything, so a run that dies after that
  leaves a repository that reads as "not ahead of the current version" while
  everything it wrote is still wanted — a commit that did not happen, a tag the
  remote refused, a machine that went away mid-push. `python scripts/release.py
  --resume` finishes that run where it stopped: it commits what is still
  uncommitted, writes any copy of the version a killed run left behind, makes the
  tag again if there is none, and pushes whatever the remote does not have, both
  the branch and the tag — saying which of those each step is before it does it.
  Steps that are already done are left alone, so running it again is safe, and a
  release that is already out is answered with "nothing to finish" rather than
  pushed at again. The failures that leave this state — the commit that did not
  happen and the push that did not land — now name this command in their own
  message instead of printing a `git push` to be run by hand. It refuses to
  guess: a tag of that version held elsewhere as a different tag object, a change
  the release did not make, and a `HEAD` that is no release commit are refused
  rather than resolved.

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

### Development

- **The release flow's test is checked against a broken release flow.**
  `scripts/test_release.py` is only worth what it catches, and a test that has
  stopped catching something goes on passing - including on the commit that
  breaks the flow. `scripts/mutate_release.py` breaks `release.py` one deliberate
  way at a time and requires the test to fail for the mutation it claims; a
  mutation it passes, or one it fails on somewhere other than that case, is a
  hole, and the run exits 1 on it rather than reporting a green one. Every
  mutation is applied to a repository a worker copies out of the checkout, never
  to the working tree, so a run cannot leave a mutated `release.py` behind. It
  runs as its own job (`mutants`), beside `checks` rather than after it, and is
  skipped for a tag, whose branch push has already been through it.
- **A missing updater signing key is reported as one.** The `installer` job needs
  `TAURI_SIGNING_PRIVATE_KEY`, because `createUpdaterArtifacts` is on and every
  `tauri build` signs its own updater artifact. A secret that was never set
  reaches the bundler as an *empty* variable rather than an absent one - which is
  what GitHub resolves an unset secret to - so the bundler's own "no private key"
  error never fires. The build dies later, decoding the key, with
  `incorrect updater private key password: Missing comment in secret key`, which
  blames the password rather than the key and never says a secret is missing; it
  reads like a broken build rather than a setting nobody made. The key is now
  checked in a step of its own, before anything is built, so the job fails saying
  which secret is missing and where to add it. The build scripts already refuse a
  keyless build the same way, from the environment or `~/.tauri/ExcelScry.key`.

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
