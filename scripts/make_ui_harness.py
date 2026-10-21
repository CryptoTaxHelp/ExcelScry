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
    // A bold header with a fill, the way most reports style one.
    return h
      ? ["s", h, { bold: true, color: "#1F4E79", background: "#DCE6F1" }]
      : null;
  }
  // A fill on a cell with no value, which the grid has to draw anyway.
  if (r === 3 && c === 1) return ["z", "", { background: "#FDE68A" }];
  // A zero its number format hides (`#,##0;-#,##0;`): the cell is blank, so
  // the grid marks it and the readout says what is behind the blank.
  if (r === 7 && c === 6) return ["f", "", null, "0"];
  if (r === 8 && c === 8) return ["s", "", null, "anything"];
  // The total under the revenue column: a block of cells summed, so a table
  // has a second step and the dependency view has a dependent to name.
  if (r === 27) {
    if (c === 2) return ["s", "Total", { bold: true, background: "#E2E8F0" }];
    if (c === 6)
      return ["f", "$848,975.30", { bold: true, background: "#E2E8F0" }, null, "SUM(G7:G26)"];
    return null;
  }
  if (r >= 6 && r <= 25) {
    const regions = ["North", "South", "East", "West"];
    switch (c) {
      case 2: return ["s", regions[r % 4]];
      case 3: return ["s", r % 2 ? "Widget" : "Gadget"];
      case 4: return ["i", group(r * 137)];
      case 5: return ["f", "$19.99"];
      case 6:
        // A calculated column: units times unit price, the shape a report's
        // own formula takes, so the grid can mark it and the readout show it.
        return ["f", "$" + group((r * 137 * 19.99).toFixed(2)), null, null, `E${r + 1}*F${r + 1}`];
      case 7: return ["d", `1${r % 10}/0${(r % 9) + 1}/2026`];
      case 8:
        return r === 9
          ? ["e", "#N/A upstream"]
          : ["s", "note " + r, { italic: true, color: "#6B7280" }];
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
  // The frozen row and column are styled too, so the pinned panes can be seen
  // carrying the workbook's look rather than only their positions.
  if (r === 0)
    return ["s", `col ${c + 1}`, { bold: true, color: "#1F4E79", background: "#E2E8F0" }];
  if (c === 0)
    return ["i", String(r), { bold: true, background: "#F1F5F9" }];
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
    return c === 0
      ? [
          "s",
          "Quarterly totals — merged across four columns",
          { bold: true, color: "#FFFFFF", background: "#1F4E79" },
        ]
      : null;
  }
  if (r === 1) {
    const headers = ["Region", "Units", "Notes over two columns"];
    return c <= 2 && headers[c]
      ? ["s", headers[c], { bold: true, background: "#E2E8F0" }]
      : null;
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
  // The sales report keeps its first two rows and its first two columns empty
  // for a title and a margin, so its data corner is C6 rather than A1: that is
  // what "jump to data" has to find.
  { name: "Sales report", rows: 200, cols: 11, firstUsed: 5, firstUsedCol: 2, cell: salesCell },
  { name: "Wide sheet", rows: 60, cols: 140, firstUsed: 0, firstUsedCol: 0, cell: wideCell },
  { name: "Tall sheet", rows: 50000, cols: 4, firstUsed: 0, firstUsedCol: 0, cell: tallCell },
  { name: "Unicode & escaping", rows: 12, cols: 3, firstUsed: 0, firstUsedCol: 0, cell: markupCell },
  { name: "Sized & merged", rows: 80, cols: 8, firstUsed: 0, firstUsedCol: 0, cell: layoutCell },
];

// What the backend reports for that sheet: two workbook defaults, four column
// widths, three row heights and four merged regions. The rest keep the
// workbook's own 64 x 20.
const LAYOUTS = {
  "Sales report": {
    // A prepared report hides the column of order dates and three rows of
    // detail, so the grid can be seen collapsing them and offering them back.
    defaultColWidth: null,
    defaultRowHeight: null,
    colWidths: [],
    rowHeights: [],
    merges: [],
    hiddenColRuns: [[7, 7]],
    hiddenRowRuns: [[15, 17]],
  },
  "Wide sheet": {
    // The header row and the index column are frozen: scrolling right keeps
    // the row labels, scrolling down keeps the column names. It also hides one
    // row and one column, so the view menu has all three of its switches live
    // on the same sheet.
    defaultColWidth: null,
    defaultRowHeight: null,
    colWidths: [],
    rowHeights: [],
    merges: [],
    hiddenColRuns: [[3, 3]],
    hiddenRowRuns: [[2, 2]],
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

// ---- The chain around a cell ---------------------------------------------

// `cell_dependencies` reads the same formula text the grid shows, so a
// reference names the cell the strip names: `E7*F7` feeds G7 with the units and
// the unit price it multiplies.
function colIndex(letters) {
  let index = 0;
  for (const letter of letters) index = index * 26 + (letter.charCodeAt(0) - 64);
  return index - 1;
}

// Every reference a formula makes: one cell, or a rectangle named `C7:C9`.
// Function names carry no digits, so they are never mistaken for references.
function referencesOf(formula) {
  const found = [];
  const cell = /\$?([A-Z]{1,3})\$?([0-9]+)(?::\$?([A-Z]{1,3})\$?([0-9]+))?/g;
  for (const hit of formula.matchAll(cell)) {
    const top = Number(hit[2]) - 1;
    const bottom = hit[4] ? Number(hit[4]) - 1 : top;
    const left = colIndex(hit[1]);
    const right = hit[3] ? colIndex(hit[3]) : left;
    found.push({
      top: Math.min(top, bottom),
      bottom: Math.max(top, bottom),
      left: Math.min(left, right),
      right: Math.max(left, right),
    });
  }
  return found;
}

function formulaOf(sheet, row, col) {
  const cell = sheet.cell(row, col);
  return cell && cell[4] ? "=" + cell[4] : "";
}

function describeCell(sheet, row, col) {
  const cell = sheet.cell(row, col);
  return {
    row,
    col,
    text: cell ? cell[1] : "",
    formula: Boolean(cell && cell[4]),
  };
}

// What feeds a cell and what it feeds. The mock's formulas live in the Revenue
// column, so the reverse walk is over the cells themselves rather than over a
// sparse table of formulas the way the Rust side can.
function depsOf(sheetIndex, row, col) {
  const sheet = OPENED[sheetIndex];
  const seen = new Map();
  let truncated = false;

  for (const reference of referencesOf(formulaOf(sheet, row, col))) {
    for (let r = reference.top; r <= reference.bottom && !truncated; r++) {
      for (let c = reference.left; c <= reference.right && !truncated; c++) {
        // A cell is not fed by itself, whatever its formula says.
        if ((r === row && c === col) || seen.has(r + ":" + c)) continue;
        if (seen.size >= 200) truncated = true;
        else seen.set(r + ":" + c, describeCell(sheet, r, c));
      }
    }
    if (truncated) break;
  }

  const dependents = [];
  for (let r = 0; r < sheet.rows; r++) {
    for (let c = 0; c < sheet.cols; c++) {
      if (r === row && c === col) continue;
      const formula = formulaOf(sheet, r, c);
      if (!formula) continue;
      const points = referencesOf(formula).some(
        (reference) =>
          row >= reference.top &&
          row <= reference.bottom &&
          col >= reference.left &&
          col <= reference.right
      );
      if (points) dependents.push(describeCell(sheet, r, c));
    }
  }
  dependents.sort((a, b) => a.row - b.row || a.col - b.col);

  return {
    precedents: [...seen.values()],
    dependents: dependents.slice(0, 200),
    unlisted: [],
    truncated: truncated || dependents.length > 200,
  };
}

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

// A delimited file at `#legacy-jp`: a Japanese export, which the Rust side now
// recognises as Shift_JIS from the bytes. The rows arrive decoded here, so the
// bar is what this mock is for: the line naming Shift_JIS with nobody having
// chosen it, the picker still on Auto, and Shift_JIS offered in the list.
const JP_TEXT =
  [
    "商品コード,商品名,単価,数量,備考",
    ...Array.from(
      { length: 40 },
      (_, i) =>
        `A-${1001 + i},商品${i + 1},${150 + i},${12 + i},` +
        `青森県産の新鮮な果物を東京の倉庫へ`
    ),
  ].join("\n") + "\n";

// A fourth delimited file, at `#legacy`: an export from a Windows tool, with
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
// starts from it. The header row is a separate command, so it lives in the
// same entry but does not depend on a dialect change.
const REMEMBERED = new Map();

// The position remembered for each file, mirroring views.json: the Rust side
// keeps one per file and hands it back with the workbook, so reopening a file
// can be seen landing where it was left.
const VIEWS = new Map();

// The text file that is open, so `remember_header` — which the real app answers
// from its own state — knows which file the pin belongs to.
let OPEN_PATH = "";

// The welcome screen's recent list, mirroring recent-files.json. Newest first,
// capped the same way, and every successful open goes to the front.
let RECENT = ["/mock/ExcelScry-sample.xlsx", "/mock/ExcelScry-sample.csv"];

function rememberRecent(path) {
  const key = String(path);
  RECENT = [
    key,
    ...RECENT.filter((entry) => entry.toLowerCase() !== key.toLowerCase()),
  ].slice(0, 8);
}

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

// The bar's select is the list of names the Rust side can report, so the mock
// reads them from it rather than keeping a copy that would go stale as
// encodings are added.
const ENCODING_LABELS = Object.fromEntries(
  [...document.querySelectorAll("#text-encoding option")]
    .filter((option) => option.value)
    .map((option) => [option.value, option.textContent.trim()])
);

function textWorkbook(path, delimiter, encoding) {
  const name = String(path);
  const japanese = name.includes("japanese");
  const legacy = name.includes("legacy");
  const source = japanese
    ? JP_TEXT
    : legacy
      ? LEGACY_TEXT
      : name.includes("report")
        ? REPORT_TEXT
        : CSV_TEXT;
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
    // The parsed records, so `sort_text_rows` can move them the way the Rust
    // side moves its own: a record is a row, and its other fields travel with
    // it.
    data: rows,
    // The file's own order, kept so `restore_text_order` can put the records
    // back without re-reading the file — the mock's stand-in for the
    // permutation `SheetData` keeps.
    original: rows.slice(),
    cell: (r, c) =>
      sheet.data[r] && sheet.data[r][c] !== undefined ? ["s", sheet.data[r][c]] : null,
  };
  OPENED = [sheet];
  // The order the file was left in. The Rust side applies it while reading, so
  // the mock hands back sorted rows on the next open too, with the choice in
  // the text info for the strip and the status bar.
  const storedSort = mockStoredSort(path);
  if (storedSort) {
    const header = mockStoredHeader(path);
    const first = header === undefined ? 1 : header === null ? 0 : header + 1;
    if (first <= sheet.data.length) {
      mockSortData(sheet, first, storedSort.column, storedSort.descending);
    }
  }
  return {
    path,
    fileName: String(path).split(/[\\/]/).pop(),
    format: "Comma-Separated Values (csv)",
    fileSize: source.length,
    truncated: false,
    totalCells: 0,
    usedCells: rows.reduce((count, row) => count + row.length, 0),
    trust: TRUST_REPORTS.xlsx,
    view: VIEWS.get(path),
    sheets: [
      {
        name: sheet.name,
        rows: sheet.rows,
        cols: sheet.cols,
        usedCells: 0,
        firstUsedRow: 0,
        firstUsedCol: 0,
      },
    ],
    text: {
      delimiter: delimiter || "comma",
      // What the read settled on: the chosen encoding, or the one the bytes
      // were recognised as.
      encoding: encoding
        ? ENCODING_LABELS[encoding] || "UTF-8"
        : japanese
          ? "Shift_JIS"
          : legacy
            ? "Windows-1252"
            : "UTF-8",
      delimiterChosen: Boolean(delimiter),
      encodingChosen: Boolean(encoding),
      // A legacy file read as UTF-8 on purpose is the case that still needs
      // the hint; every automatic read of it succeeds.
      undecodable: (legacy || japanese) && encoding === "utf-8",
      // The row pinned the last time this file was open. Absent when nobody
      // chose one, null when it was left unpinned, a number for a row — the
      // three states the UI tells apart.
      ...(mockStoredHeader(path) === undefined ? {} : { headerRow: mockStoredHeader(path) }),
      ...(storedSort ? { sort: storedSort } : {}),
    },
  };
}

// The mock's own helpers are prefixed: the app's script shares this global
// scope, and a name it also defines would be shadowed by it.

/** A field as a number, when the whole of it is one; `1,50` stays text. */
function mockNumber(text) {
  const trimmed = String(text).trim();
  if (!trimmed) return null;
  const value = Number(trimmed);
  return Number.isFinite(value) ? value : null;
}

/** Two fields compared the way a person reading the table would. */
function mockCompareFields(a, b) {
  const left = mockNumber(a);
  const right = mockNumber(b);
  if (left !== null && right !== null) return left < right ? -1 : left > right ? 1 : 0;
  const foldedA = a.toLowerCase();
  const foldedB = b.toLowerCase();
  if (foldedA !== foldedB) return foldedA < foldedB ? -1 : 1;
  return a === b ? 0 : a < b ? -1 : 1;
}

/** Two records ordered, with blanks last whichever direction is asked for. */
function mockOrderRecords(a, b, descending) {
  if (a.blank && b.blank) return 0;
  if (a.blank) return 1;
  if (b.blank) return -1;
  const order = mockCompareFields(a.key, b.key);
  return descending ? -order : order;
}

/** The header row remembered for a path, if the store has an entry for it. */
function mockStoredHeader(path) {
  const entry = REMEMBERED.get(path);
  return entry && "headerRow" in entry ? entry.headerRow : undefined;
}

/** The order remembered for a path, if the store has one. */
function mockStoredSort(path) {
  const entry = REMEMBERED.get(path);
  return entry && entry.sort ? entry.sort : null;
}

/**
 * Move a text sheet's records into an order, leaving the rows above the header
 * where they are.
 *
 * Shared by the sort control and the read that puts a remembered order back, so
 * the mock cannot show one order and reapply another.
 */
function mockSortData(sheet, first, column, descending) {
  const records = sheet.data.slice(first).map((row) => {
    const text = row[column] === undefined ? "" : String(row[column]);
    return { row, key: text, blank: !text.trim() };
  });
  records.sort((a, b) => mockOrderRecords(a, b, Boolean(descending)));
  sheet.data = sheet.data.slice(0, first).concat(records.map((record) => record.row));
}

/** Remember a header row, keeping the dialect stored beside it. */
function mockRememberHeader(path, row) {
  if (!path) return;
  REMEMBERED.set(path, {
    ...(REMEMBERED.get(path) || {}),
    headerRow: row === undefined ? null : row,
  });
}

function workbookInfo(path) {
  const name = String(path).split(/[\\/]/).pop();
  const trust = trustFor(path);
  OPENED = SHEETS;
  OPEN_PATH = path;
  return {
    path,
    fileName: name,
    format: "Excel Workbook (xlsx)",
    fileSize: 43341,
    truncated: false,
    totalCells: 0,
    usedCells: 6423,
    trust,
    view: VIEWS.get(path),
    sheets: SHEETS.map((s) => ({
      name: s.name,
      rows: s.rows,
      cols: s.cols,
      usedCells: 0,
      firstUsedRow: s.firstUsed,
      firstUsedCol: s.firstUsedCol,
      colWidths: LAYOUTS[s.name] ? LAYOUTS[s.name].colWidths : [],
      rowHeights: LAYOUTS[s.name] ? LAYOUTS[s.name].rowHeights : [],
      defaultColWidth: LAYOUTS[s.name] ? LAYOUTS[s.name].defaultColWidth : null,
      defaultRowHeight: LAYOUTS[s.name] ? LAYOUTS[s.name].defaultRowHeight : null,
      merges: LAYOUTS[s.name] ? LAYOUTS[s.name].merges : [],
      hiddenColRuns: LAYOUTS[s.name] ? LAYOUTS[s.name].hiddenColRuns || [] : [],
      hiddenRowRuns: LAYOUTS[s.name] ? LAYOUTS[s.name].hiddenRowRuns || [] : [],
      frozenCols: LAYOUTS[s.name] ? LAYOUTS[s.name].frozenCols || 0 : 0,
      frozenRows: LAYOUTS[s.name] ? LAYOUTS[s.name].frozenRows || 0 : 0,
    })),
  };
}

const UPDATE_AVAILABLE = true;

const HANDLERS = {
  initial_file: () => null,
  recent_files: () =>
    RECENT.map((path) => ({
      path,
      name: String(path).split(/[\\/]/).pop(),
    })),
  // `#csv` opens the delimited sample instead, so the text bar can be seen;
  // `#preamble` opens one whose header is not its first row; `#legacy` and
  // `#legacy-jp` open files in encodings the bytes have to be read for.
  pick_workbook: () =>
    location.hash === "#preamble"
      ? "/mock/Exported-report.csv"
      : location.hash === "#legacy-jp"
        ? "/mock/japanese-export.csv"
        : location.hash === "#legacy"
          ? "/mock/legacy-export.csv"
          : location.hash === "#csv"
            ? "/mock/ExcelScry-sample.csv"
            : "/mock/ExcelScry-sample.xlsx",
  open_workbook: ({ path, delimiter, encoding, dialectExplicit }) => {
    rememberRecent(path);
    const name = String(path).toLowerCase();
    if (!name.endsWith(".csv") && !name.endsWith(".tsv")) return workbookInfo(path);
    const remembered = REMEMBERED.get(path) || {};
    const explicit = Boolean(dialectExplicit);
    // A choice in the bar is complete on its own: Auto means auto now, not the
    // remembered value again. Anything filled in from the store is listed as
    // chosen, so the bar's selects show it.
    const effectiveDelimiter = explicit ? delimiter || "" : delimiter || remembered.delimiter || "";
    const effectiveEncoding = explicit ? encoding || "" : encoding || remembered.encoding || "";
    // The header row is chosen separately, so changing the dialect keeps it.
    OPEN_PATH = path;
    if (explicit) {
      const entry = { ...remembered, delimiter: delimiter || "", encoding: encoding || "" };
      if (!entry.delimiter && !entry.encoding && !("headerRow" in entry)) REMEMBERED.delete(path);
      else REMEMBERED.set(path, entry);
    }
    return textWorkbook(path, effectiveDelimiter, effectiveEncoding);
  },
  remember_header: ({ row }) => {
    mockRememberHeader(OPEN_PATH, row === undefined ? null : row);
    return null;
  },
  // Mirrors `view_prefs::remember`: the position and the column widths the
  // reader dragged belong to the file that is open, and the next open of that
  // file is handed both back.
  remember_view: ({ view }) => {
    if (OPEN_PATH) VIEWS.set(OPEN_PATH, view);
    return null;
  },
  // Mirrors `LoadedWorkbook::sort_text`: only a delimited file sorts, the
  // records below the pinned header move as whole rows, and the preamble above
  // it is left alone. Mirrors `text_prefs::remember_sort` too: the order is
  // kept for the file, so reopening it comes back sorted.
  sort_text_rows: ({ sheet, headerRow, column, descending, label }) => {
    const s = OPENED[sheet];
    if (!s || !s.data) throw new Error("Only a delimited text file can be sorted.");
    const first = headerRow === null || headerRow === undefined ? 0 : headerRow + 1;
    if (first > s.data.length) throw new Error("That header row is past the end of the file.");
    mockSortData(s, first, column, descending);
    if (OPEN_PATH) {
      REMEMBERED.set(OPEN_PATH, {
        ...(REMEMBERED.get(OPEN_PATH) || {}),
        sort: { column, descending: Boolean(descending), label: label || "" },
      });
    }
    return null;
  },
  // Mirrors `SheetData::restore_file_order`: the records go back where the file
  // had them, and the chosen order is forgotten so the next open is the file's.
  restore_text_order: ({ sheet }) => {
    const s = OPENED[sheet];
    if (!s || !s.original) throw new Error("Only a delimited text file has a file order.");
    s.data = s.original.slice();
    if (OPEN_PATH) {
      const entry = { ...(REMEMBERED.get(OPEN_PATH) || {}) };
      delete entry.sort;
      if (entry.delimiter || entry.encoding || "headerRow" in entry) {
        REMEMBERED.set(OPEN_PATH, entry);
      } else {
        REMEMBERED.delete(OPEN_PATH);
      }
    }
    return true;
  },
  close_workbook: () => null,
  save_utf8_copy: ({ destination }) => ({
    path: destination || "/mock/legacy-export-utf8.csv",
    bytes: 18432,
  }),
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
  cell_dependencies: ({ sheet, row, col }) => depsOf(sheet, row, col),
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
