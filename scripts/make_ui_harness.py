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
];

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

function workbookInfo(path) {
  const name = String(path).split(/[\\/]/).pop();
  const trust = trustFor(path);
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
    })),
  };
}

const UPDATE_AVAILABLE = true;

const HANDLERS = {
  initial_file: () => null,
  pick_workbook: () => "/mock/ExcelScry-sample.xlsx",
  open_workbook: ({ path }) => workbookInfo(path),
  close_workbook: () => null,
  get_rows: ({ sheet, rowStart, rowCount, colStart, colCount }) => {
    const s = SHEETS[sheet];
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
