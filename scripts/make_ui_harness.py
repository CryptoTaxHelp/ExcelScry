"""Build a self-contained harness for the ExcelScry UI.

The real app embeds its front end inside the executable, and its window cannot
be driven by ordinary browser tooling. This generator inlines `ui/styles.css`
and `ui/app.js` into a single HTML file together with a mock of the Tauri
bridge, so the grid, tab strip, virtual scroller and dialogs can be exercised
and screenshotted in a normal browser.

The mock reproduces the IPC contract exactly (`get_rows` returns the same
camelCase page shape as the Rust `RowsPage`, clamped to the sheet's extent), so
the front end sees no difference between this and the real backend.

Run from the repository root:  python scripts/make_ui_harness.py
"""

from __future__ import annotations

import pathlib
import re

ROOT = pathlib.Path(__file__).resolve().parent.parent
UI = ROOT / "ui"
OUT = ROOT / "test" / "ui-harness.html"

MOCK = r"""
// ---- Mocked Tauri bridge -------------------------------------------------

const LOREM = "a very long value that should be clipped in the grid but shown in full in the readout";

// The backend renders a cell through its number format, so the strings the
// grid receives are already display-ready: `$23,988.00`, `15/01/2026`, `35.0%`.
function group(value) {
  const [whole, fraction] = String(value).split(".");
  const grouped = whole.replace(/\B(?=(\d{3})+(?!\d))/g, ",");
  return fraction ? `${grouped}.${fraction}` : grouped;
}

function salesCell(r, c) {
  if (r === 5) {
    const h = ["Region", "Product", "Units", "Unit price", "Revenue", "Ordered", "Notes", "Margin", "Start time"][c - 2];
    return h ? ["s", h] : null;
  }
  if (r >= 6 && r <= 25) {
    const regions = ["North", "South", "East", "West"];
    switch (c) {
      case 2: return ["s", regions[r % 4]];
      case 3: return ["s", r % 2 ? "Widget" : "Gadget"];
      case 4: return ["i", group(r * 137)];
      case 5: return ["f", "$19.99"];
      case 6: return ["f", "$" + group((r * 137 * 19.99).toFixed(2))];
      case 7: return ["d", `1${r % 10}/0${(r % 9) + 1}/2026`];
      case 8: return r === 9 ? ["e", "#N/A upstream"] : ["s", "note " + r];
      case 9: return ["f", `${(20 + (r % 30)).toFixed(1)}%`];
      case 10: return ["t", `0${r % 9 + 1}:3${r % 6}`];
      default: return null;
    }
  }
  if (r === 30) {
    // A value far wider than any column: it must truncate from the tail, never
    // from the front, or the grid would show a different number.
    return c === 2 ? ["i", "9223372036854775807"] : null;
  }
  return null;
}

function wideCell(r, c) {
  if (r === 0) return ["s", `col ${c + 1}`];
  if (c === 0) return ["i", String(r)];
  return ["i", String((r * c) % 997)];
}

function tallCell(r, c) {
  if (r === 0) return c === 0 ? ["s", "index"] : ["s", "value"];
  if (c === 0) return ["i", String(r)];
  if (c === 1) return ["f", String(((r * 37) % 1000) / 4)];
  if (c === 2) return ["b", r % 2 ? "TRUE" : "FALSE"];
  return null;
}

function layoutCell(r, c) {
  if (r === 0) {
    // The banner is merged across A:D, so only its anchor holds anything.
    return c === 0 ? ["s", "Quarterly totals — merged across four columns"] : null;
  }
  if (r === 1) {
    const headers = ["Region", "Units", "Notes over two columns"];
    return c <= 2 && headers[c] ? ["s", headers[c]] : null;
  }
  if (r === 3 && c === 0) return ["s", "tall merged cell"];
  if (r === 10 && c === 1) return ["s", "block of six cells"];
  if (r >= 2 && r <= 30) {
    switch (c) {
      case 0: return r === 4 ? null : ["s", `Region ${r % 4}`];
      case 1: return ["i", String(r * 17)];
      case 2: return ["s", `note ${r}`];
      case 3: return ["f", `${(r % 10) * 1.5}%`];
      default: return null;
    }
  }
  return null;
}

function markupCell(r, c) {
  if (c !== 0) return null;
  const values = [
    "Markup-looking text must not be interpreted:",
    // Assembled piecewise so no literal tag sequence appears inside this
    // harness's own <script> block, which would close it early.
    "<" + "script>alert('xss')<" + "/script>",
    'a "quoted" & <angled> value',
    "emoji \u2705 and accents \u00e9\u00f6\u00fc",
    "=1+1 stays text when exported",
    "000123",
    LOREM,
    "<img src=x onerror=alert(1)>",
    "</td></tr><h1>injected</h1>",
    "0.1 + 0.2",
  ];
  return r < values.length ? ["s", values[r]] : null;
}

const SHEETS = [
  { name: "Sales report", rows: 200, cols: 11, firstUsed: 5, cell: salesCell },
  { name: "Wide sheet", rows: 60, cols: 140, firstUsed: 0, cell: wideCell },
  { name: "Tall sheet", rows: 50000, cols: 4, firstUsed: 0, cell: tallCell },
  { name: "Unicode & escaping", rows: 12, cols: 3, firstUsed: 0, cell: markupCell },
  { name: "Sized & merged", rows: 80, cols: 8, firstUsed: 0, cell: layoutCell },
];

// What the backend reports for that sheet: two workbook defaults, four column
// widths, three row heights and four merged regions. The rest keep the
// workbook's own 64 x 20.
const LAYOUTS = {
  "Wide sheet": {
    // The header row and the index column are frozen: scrolling right keeps
    // the row labels, scrolling down keeps the column names.
    defaultColWidth: null,
    defaultRowHeight: null,
    colWidths: [],
    rowHeights: [],
    merges: [],
    frozenCols: 1,
    frozenRows: 1,
  },
  "Sized & merged": {
    defaultColWidth: 64,
    defaultRowHeight: 20,
    colWidths: [
      [0, 180],
      [1, 70],
      [2, 150],
      [3, 60],
    ],
    rowHeights: [
      [0, 44],
      [3, 32],
      [10, 30],
    ],
    merges: [
      { row: 0, col: 0, rows: 1, cols: 4 },
      { row: 1, col: 2, rows: 1, cols: 2 },
      { row: 3, col: 0, rows: 2, cols: 1 },
      { row: 10, col: 1, rows: 3, cols: 2 },
    ],
  },
};

// What `get_rows` serves: the sheets of the workbook that was opened last.
let OPENED = SHEETS;

// A delimited file for the text bar, at `#csv`. It is semicolon-separated with
// decimal commas, so the comma a `.csv` starts with leaves it in one column -
// which is the state the bar exists to fix. The encoding is reported as
// undecodable until one is chosen, so the hint can be seen too.
const CSV_TEXT =
  [
    "Region;Product;Units;Price;Ordered;Margin;Owner;Notes;Status;Warehouse;Carrier;Reference",
    ...Array.from(
      { length: 200 },
      (_, i) =>
        `Region ${i + 1};Widget ${i + 1};${12 + i};1,${String(50 + i).slice(-2)};` +
        `2026-01-${String((i % 28) + 1).padStart(2, "0")};${5 + (i % 9)}%;` +
        `Buyer ${i + 1};Reorder at the end of the quarter;Open;WH-${(i % 4) + 1};` +
        `Carrier ${(i % 3) + 1};REF-${1000 + i}`
    ),
  ].join("\n") + "\n";
const DELIMITERS = { comma: ",", semicolon: ";", tab: "\t", pipe: "|" };

// A second delimited file, at `#preamble`: a title and a note above the column
// names, the shape that makes the header row a choice rather than a fact.
const REPORT_TEXT =
  [
    "Quarterly export, all regions",
    "Generated 2026-01-05 by the warehouse system",
    "Region,Product,Units,Price,Ordered,Margin,Owner,Notes,Status,Warehouse,Carrier,Reference",
    ...Array.from(
      { length: 40 },
      (_, i) =>
        `Region ${i + 1},Widget ${i + 1},${12 + i},1.${String(50 + i).slice(-2)},` +
        `2026-01-${String((i % 28) + 1).padStart(2, "0")},${5 + (i % 9)}%,` +
        `Buyer ${i + 1},Reorder at the end of the quarter,Open,WH-${(i % 4) + 1},` +
        `Carrier ${(i % 3) + 1},REF-${1000 + i}`
    ),
  ].join("\n") + "\n";

// A third delimited file, at `#legacy`: an export from a Windows tool, with
// accented text a UTF-8 reader would not decode. The mock reports what the Rust
// side now works out for such a file: Windows-1252, with nobody choosing it.
const LEGACY_TEXT =
  [
    "Region,Product,Units,Price,Notes",
    ...Array.from(
      { length: 40 },
      (_, i) =>
        `Region ${i + 1},Widget ${i + 1},${12 + i},1,${String(50 + i).slice(-2)},` +
        `caf\u00e9 na\u00efve r\u00e9sum\u00e9; deliver to Z\u00fcrich`
    ),
  ].join("\n") + "\n";

// The per-file choices, mirroring the Rust side's text-dialects.json: only
// what a bar change made explicit is stored, and a first open or a reload
// starts from it.
const REMEMBERED = new Map();

// Opening a mock path ending in these names exercises each badge state.
const TRUST_REPORTS = {
  xlsx: {
    hazards: [],
    unchecked: [],
    badge: "No macros or links",
    level: "clean",
    detail: {
      macros: false,
      externalLinks: 0,
      connections: false,
      embeddedObjects: 0,
      activex: false,
    },
  },
  xlsm: {
    hazards: ["Macros"],
    unchecked: [],
    badge: "Macros",
    level: "warn",
    detail: {
      macros: true,
      externalLinks: 0,
      connections: false,
      embeddedObjects: 0,
      activex: false,
    },
  },
  risky: {
    hazards: [
      "Macros",
      "2 external links",
      "Data connections",
      "1 embedded object",
      "ActiveX controls",
    ],
    unchecked: [],
    // Capped by the Rust side to fit the status bar; `hazards` keeps them all.
    badge: "Macros, 2 external links, +3 more",
    level: "warn",
    detail: {
      macros: true,
      externalLinks: 2,
      connections: true,
      embeddedObjects: 1,
      activex: true,
    },
  },
  // A legacy .xls: macros can be checked, nothing else can.
  xls: {
    hazards: [],
    unchecked: ["external links", "data connections", "embedded objects", "ActiveX controls"],
    badge: "No macros",
    level: "partial",
    detail: {
      macros: false,
      externalLinks: null,
      connections: null,
      embeddedObjects: null,
      activex: null,
    },
  },
};

function trustFor(path) {
  const name = String(path).split(/[\\/]/).pop().toLowerCase();
  if (name.includes("risky")) return TRUST_REPORTS.risky;
  if (name.endsWith(".xls")) return TRUST_REPORTS.xls;
  if (name.endsWith(".xlsm")) return TRUST_REPORTS.xlsm;
  return TRUST_REPORTS.xlsx;
}

const ENCODING_LABELS = {
  "windows-1252": "Windows-1252",
  "utf-16le": "UTF-16 LE",
  "utf-16be": "UTF-16 BE",
};

function textWorkbook(path, delimiter, encoding) {
  const name = String(path);
  const legacy = name.includes("legacy");
  const source = legacy ? LEGACY_TEXT : name.includes("report") ? REPORT_TEXT : CSV_TEXT;
  const separator = DELIMITERS[delimiter] || ",";
  const rows = source.split("\n")
    .filter((line) => line.length > 0)
    .map((line) => line.split(separator));
  const sheet = {
    name: String(path)
      .split(/[\\/]/)
      .pop()
      .replace(/\.[^.]+$/, ""),
    rows: rows.length,
    cols: Math.max(...rows.map((row) => row.length)),
    firstUsed: 0,
    cell: (r, c) => (rows[r] && rows[r][c] !== undefined ? ["s", rows[r][c]] : null),
  };
  OPENED = [sheet];
  return {
    path,
    fileName: String(path).split(/[\\/]/).pop(),
    format: "Comma-Separated Values (csv)",
    fileSize: source.length,
    truncated: false,
    totalCells: 0,
    usedCells: rows.reduce((count, row) => count + row.length, 0),
    trust: TRUST_REPORTS.xlsx,
    sheets: [
      { name: sheet.name, rows: sheet.rows, cols: sheet.cols, usedCells: 0, firstUsedRow: 0 },
    ],
    text: {
      delimiter: delimiter || "comma",
      // What the read settled on: the chosen encoding, or the one the bytes
      // were recognised as.
      encoding: encoding ? ENCODING_LABELS[encoding] || "UTF-8" : legacy ? "Windows-1252" : "UTF-8",
      delimiterChosen: Boolean(delimiter),
      encodingChosen: Boolean(encoding),
      // A legacy file read as UTF-8 on purpose is the case that still needs
      // the hint; every automatic read of it succeeds.
      undecodable: legacy && encoding === "utf-8",
    },
  };
}

function workbookInfo(path) {
  const name = String(path).split(/[\\/]/).pop();
  const trust = trustFor(path);
  OPENED = SHEETS;
  return {
    path,
    fileName: name,
    format: "Excel Workbook (xlsx)",
    fileSize: 43341,
    truncated: false,
    totalCells: 0,
    usedCells: 6423,
    trust,
    sheets: SHEETS.map((s) => ({
      name: s.name,
      rows: s.rows,
      cols: s.cols,
      usedCells: 0,
      firstUsedRow: s.firstUsed,
      colWidths: LAYOUTS[s.name] ? LAYOUTS[s.name].colWidths : [],
      rowHeights: LAYOUTS[s.name] ? LAYOUTS[s.name].rowHeights : [],
      defaultColWidth: LAYOUTS[s.name] ? LAYOUTS[s.name].defaultColWidth : null,
      defaultRowHeight: LAYOUTS[s.name] ? LAYOUTS[s.name].defaultRowHeight : null,
      merges: LAYOUTS[s.name] ? LAYOUTS[s.name].merges : [],
      frozenCols: LAYOUTS[s.name] ? LAYOUTS[s.name].frozenCols || 0 : 0,
      frozenRows: LAYOUTS[s.name] ? LAYOUTS[s.name].frozenRows || 0 : 0,
    })),
  };
}

const UPDATE_AVAILABLE = true;

const HANDLERS = {
  initial_file: () => null,
  // `#csv` opens the delimited sample instead, so the text bar can be seen;
  // `#preamble` opens one whose header is not its first row.
  pick_workbook: () =>
    location.hash === "#preamble"
      ? "/mock/Exported-report.csv"
      : location.hash === "#legacy"
        ? "/mock/legacy-export.csv"
        : location.hash === "#csv"
          ? "/mock/ExcelScry-sample.csv"
          : "/mock/ExcelScry-sample.xlsx",
  open_workbook: ({ path, delimiter, encoding, dialectExplicit }) => {
    const name = String(path).toLowerCase();
    if (!name.endsWith(".csv") && !name.endsWith(".tsv")) return workbookInfo(path);
    const remembered = REMEMBERED.get(path) || {};
    const explicit = Boolean(dialectExplicit);
    // A choice in the bar is complete on its own: Auto means auto now, not the
    // remembered value again. Anything filled in from the store is listed as
    // chosen, so the bar's selects show it.
    const effectiveDelimiter = explicit ? delimiter || "" : delimiter || remembered.delimiter || "";
    const effectiveEncoding = explicit ? encoding || "" : encoding || remembered.encoding || "";
    if (explicit) {
      if (!delimiter && !encoding) REMEMBERED.delete(path);
      else REMEMBERED.set(path, { delimiter: delimiter || "", encoding: encoding || "" });
    }
    return textWorkbook(path, effectiveDelimiter, effectiveEncoding);
  },
  close_workbook: () => null,
  get_rows: ({ sheet, rowStart, rowCount, colStart, colCount }) => {
    const s = OPENED[sheet];
    const rEnd = Math.min(rowStart + rowCount, s.rows);
    const cEnd = Math.min(colStart + colCount, s.cols);
    const rows = [];
    for (let r = rowStart; r < rEnd; r++) {
      const row = [];
      for (let c = colStart; c < cEnd; c++) row.push(s.cell(r, c));
      rows.push(row);
    }
    return {
      sheet,
      rowStart,
      rowCount: rows.length,
      colStart,
      colCount: Math.max(cEnd - colStart, 0),
      totalRows: s.rows,
      totalCols: s.cols,
      rows,
    };
  },
  export_ods: () => ({
    path: "C:\\Users\\you\\Documents\\ExcelScry-sample.ods",
    sheets: 4,
    cells: 6423,
    bytes: 28417,
    warnings: [],
  }),
  association_status: () => ({
    exePath: "C:\\Program Files\\ExcelScry\\ExcelScry.exe",
    userRegistered: false,
    machineRegistered: true,
    // One entry per extension the app registers, in registration order. The
    // dialog renders whatever is here, so a format added to the Rust side
    // needs no change on the front end.
    extensions: [
      {
        ext: ".xlsx",
        progid: "ExcelScry.xlsx",
        currentHandler: "Excel.Sheet.12",
        isDefault: false,
        registeredForUser: false,
        registeredMachine: true,
      },
      {
        ext: ".xlsm",
        progid: "ExcelScry.xlsm",
        currentHandler: "Excel.SheetMacroEnabled.12",
        isDefault: false,
        registeredForUser: false,
        registeredMachine: true,
      },
      {
        ext: ".xls",
        progid: "ExcelScry.xls",
        currentHandler: "ExcelScry.xls",
        isDefault: true,
        registeredForUser: false,
        registeredMachine: true,
      },
      {
        ext: ".ods",
        progid: "ExcelScry.ods",
        currentHandler: "OpenDocument.Spreadsheet.1",
        isDefault: false,
        registeredForUser: false,
        registeredMachine: true,
      },
    ],
    settingsUri: "ms-settings:defaultapps?registeredAppMachine=ExcelScry",
    contextMenuNote: 'On Windows 11 the entry appears under "Show more options".',
  }),
  // The updater bridge. Set UPDATE_AVAILABLE to false to see the "up to date"
  // state; a real install downloads a verified installer, closes the app and
  // lets the installer start the new version, which a harness cannot show.
  app_version: () => "0.1.0",
  check_for_update: () =>
    UPDATE_AVAILABLE
      ? {
          version: "0.1.1",
          current_version: "0.1.0",
          // A realistic release body: the changelog section plus the commit
          // list, which is what the dialog has to render.
          notes: [
            "## [0.1.1] - 2026-10-16",
            "",
            "### Added",
            "",
            "- **Self-updating** from the project's GitHub releases; `Settings › Updates` says what changed, and the [release page](https://github.com/CryptoTaxHelp/ExcelScry/releases) has the rest.",
            "",
            "### Fixed",
            "",
            "- The Windows integration dialog lists every registered extension, `.xlsm` included.",
            "",
            "### Commits since v0.1.0",
            "",
            "- Add self-updating from GitHub releases (a7b0375)",
          ].join("\n"),
        }
      : null,
  install_update: () => null,
  // Opening a link is the one command that leaves the app, so the harness
  // records the address instead of opening it: the point of the check is which
  // link a click asked for, and that nothing else was asked for.
  open_link: (args = {}) => {
    window.__opened = window.__opened || [];
    window.__opened.push(args.url);
    return null;
  },
  install_associations: () => "Registered for the current user.",
  remove_associations: () => "Removed the ExcelScry context menu and file associations.",
  open_default_apps_settings: () => null,
  reveal_in_explorer: () => null,
};

window.__TAURI__ = {
  core: {
    invoke: async (cmd, args = {}) => {
      const handler = HANDLERS[cmd];
      if (!handler) throw new Error("unmocked command: " + cmd);
      return handler(args);
    },
  },
  event: { listen: async () => () => {} },
  webview: { getCurrentWebview: () => ({ onDragDropEvent: () => {} }) },
};
"""


def body_without_scripts(html: str) -> str:
    body = re.search(r"<body[^>]*>(.*?)</body>", html, re.S)
    if not body:
        raise SystemExit("could not find <body> in ui/index.html")
    content = body.group(1)
    # The app script is inlined separately, after the mock bridge exists.
    content = re.sub(r"<script[^>]*src=[^>]*>\s*</script>", "", content)
    return content.strip()


def main() -> None:
    index = (UI / "index.html").read_text(encoding="utf-8")
    styles = (UI / "styles.css").read_text(encoding="utf-8")
    app = (UI / "app.js").read_text(encoding="utf-8")

    html = f"""<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8" />
    <title>ExcelScry UI harness</title>
    <style>
{styles}
    </style>
  </head>
  <body>
{body_without_scripts(index)}
    <script>
{MOCK}
    </script>
    <script>
{app}
    </script>
  </body>
</html>
"""
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(html, encoding="utf-8")
    print(f"wrote {OUT.relative_to(ROOT)} ({OUT.stat().st_size} bytes)")


if __name__ == "__main__":
    main()
