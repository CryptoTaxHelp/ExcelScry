/* ExcelScry front-end.
 *
 * The sheet is rendered with a tiled virtual scroller: only the cells inside
 * the viewport exist in the DOM, and cells are pulled from the Rust side in
 * fixed-size tiles as the view moves. That keeps a 500k-row workbook as
 * responsive as a small one, and keeps memory bounded by the tile cache
 * rather than by the size of the sheet.
 */

"use strict";

// --------------------------------------------------------------------------
// Tauri bridge
// --------------------------------------------------------------------------

const T = window.__TAURI__;
const invoke = (cmd, args) => T.core.invoke(cmd, args);

function listen(event, handler) {
  const api = T.event;
  if (api && typeof api.listen === "function") {
    api.listen(event, handler).catch(() => {});
  }
}

// --------------------------------------------------------------------------
// Geometry
// --------------------------------------------------------------------------

// Read the row/column metrics from CSS so the stylesheet stays the single
// source of truth for the grid's dimensions.
function cssNumber(name, fallback) {
  const raw = getComputedStyle(document.documentElement).getPropertyValue(name).trim();
  const value = parseFloat(raw);
  return Number.isFinite(value) ? value : fallback;
}

const ROW_H = cssNumber("--rowh", 24);
const COL_W = cssNumber("--colw", 112);

// How narrow and how wide a dragged column may be. A column narrower than the
// strip's own letter is unreadable and one past this is off the screen; the
// Rust side clamps a remembered width to the same range.
const MIN_COL_W = 24;
const MAX_COL_W = 1600;

// The grid is laid out from the sheet's own geometry when it has one. `xs` and
// `ys` are prefix sums of column widths and row heights: cell i starts at
// xs[i] / ys[i], so a million-row sheet costs two typed arrays rather than a
// lookup per cell. Merges are two maps: anchors to their span, and covered
// cells to the anchor that owns them.
const geom = {
  cols: 0,
  rows: 0,
  xs: new Float64Array(1),
  ys: new Float64Array(1),
  anchors: new Map(),
  covered: new Map(),
  /** Rows and columns the sheet hides, as `[first, last]` runs, in order. */
  hiddenRowRuns: [],
  hiddenColRuns: [],
};
/** Coverage beyond this is not remembered; the anchor still draws its span. */
const MAX_MERGE_COVERAGE = 200_000;
/** Browsers stop scrolling a single element somewhere near 33 million pixels. */
const MAX_CANVAS = 30_000_000;

const mergeAt = (row, col) => geom.anchors.get(`${row}:${col}`) || null;

function mergeOrigin(row, col) {
  const anchor = geom.covered.get(`${row}:${col}`);
  if (!anchor) return null;
  const [r, c] = anchor.split(":").map(Number);
  return { row: r, col: c };
}

const colWidth = (col) => geom.xs[col + 1] - geom.xs[col];
const rowHeight = (row) => geom.ys[row + 1] - geom.ys[row];

/** The widths the reader dragged on one sheet, as a `col -> pixels` map. */
function draggedWidths(sheet = state.sheet) {
  if (!state.widths[sheet]) state.widths[sheet] = {};
  return state.widths[sheet];
}

/**
 * The `[[col, px], ...]` layout for one sheet, with the reader's drags laid
 * over the widths the file itself carries.
 *
 * Both lists are sparse and in column order, so a sheet that sizes a handful of
 * columns stays cheap to lay out, and a column that is neither named keeps the
 * sheet's own default width.
 */
function effectiveWidths(sheet) {
  const fromFile = sheet && sheet.colWidths ? sheet.colWidths : [];
  const dragged = draggedWidths();
  const columns = Object.keys(dragged);
  if (columns.length === 0) return fromFile;
  const merged = new Map(fromFile.map(([col, pixels]) => [col, pixels]));
  for (const column of columns) {
    const col = Number(column);
    if (Number.isInteger(col)) merged.set(col, dragged[column]);
  }
  return [...merged.entries()].sort((a, b) => a[0] - b[0]);
}

/**
 * The dragged widths a remembered view carries, as this file's starting map.
 *
 * The store holds one entry per sheet, `{ sheet, columns: [[col, px], ...] }`,
 * which becomes `widths[sheet][col]` here — the shape the grid lays out from.
 */
function widthsFromView(view) {
  const widths = {};
  if (!view || !Array.isArray(view.widths)) return widths;
  for (const entry of view.widths) {
    if (!entry || !Number.isInteger(entry.sheet) || !Array.isArray(entry.columns)) continue;
    const dragged = {};
    for (const column of entry.columns) {
      if (!Array.isArray(column) || column.length !== 2) continue;
      const [col, pixels] = column;
      if (Number.isInteger(col) && Number.isFinite(pixels) && pixels > 0) {
        dragged[col] = pixels;
      }
    }
    if (Object.keys(dragged).length > 0) widths[entry.sheet] = dragged;
  }
  return widths;
}

/** The widths to write back with the position: sparse, sorted, and per sheet. */
function widthsPayload() {
  const payload = [];
  for (const [sheet, dragged] of Object.entries(state.widths)) {
    const index = Number(sheet);
    const columns = Object.entries(dragged)
      .map(([col, pixels]) => [Number(col), Math.round(pixels)])
      .filter(([col, pixels]) => Number.isInteger(col) && Number.isFinite(pixels))
      .sort((a, b) => a[0] - b[0]);
    if (Number.isInteger(index) && columns.length > 0) {
      payload.push({ sheet: index, columns });
    }
  }
  return payload;
}

/** Largest index whose start is at or before `value`. */
function indexAt(edges, value) {
  let lo = 0;
  let hi = edges.length - 2;
  if (hi < 0) return 0;
  while (lo < hi) {
    const mid = (lo + hi + 1) >> 1;
    if (edges[mid] <= value) lo = mid;
    else hi = mid - 1;
  }
  return lo;
}

const colAtX = (x) => clamp(indexAt(geom.xs, x), 0, Math.max(geom.cols - 1, 0));
const rowAtY = (y) => clamp(indexAt(geom.ys, y), 0, Math.max(geom.rows - 1, 0));

/**
 * Fill an axis with the start of every column or row.
 *
 * `sizes` is the sparse `[[index, pixels], ...]` list the workbook sent, in
 * order; walking it alongside the axis keeps this linear in rows plus
 * overrides, which matters on a sheet with a million rows.
 */
function fillAxis(edges, count, sizes, fallback, collapsed = null) {
  let next = 0;
  let run = 0;
  for (let index = 0; index < count; index += 1) {
    while (next < sizes.length && sizes[next][0] < index) next += 1;
    let size = fallback;
    if (next < sizes.length && sizes[next][0] === index) {
      const value = sizes[next][1];
      if (Number.isFinite(value) && value > 0) size = value;
    }
    // A hidden row or column takes no space, as it does in the sheet, so the
    // ones after it start where it did. Revealing them lays them back out.
    // The runs are sorted, so one walk covers the whole axis.
    if (collapsed) {
      while (run < collapsed.length && collapsed[run][1] < index) run += 1;
      if (run < collapsed.length && index >= collapsed[run][0]) size = 0;
    }
    edges[index + 1] = edges[index] + size;
  }
}

/** Scale a prefix-sum axis down if the sheet would exceed the canvas limit. */
function fitCanvas(edges) {
  const total = edges[edges.length - 1];
  if (total <= MAX_CANVAS) return;
  const scale = MAX_CANVAS / total;
  for (let i = 0; i < edges.length; i += 1) edges[i] *= scale;
}

const TILE_ROWS = 100;
const TILE_COLS = 40;
const TILE_CACHE_MAX = 48;
const PREFETCH_ROWS = 2;
const PREFETCH_COLS = 1;

// --------------------------------------------------------------------------
// State
// --------------------------------------------------------------------------

const state = {
  info: null,
  sheet: 0,
  /** tile key -> { data } where data[row][col] is [kind, display] or null */
  tiles: new Map(),
  inflight: new Set(),
  lru: [],
  /** Bumped on every open/sheet change so stale responses are discarded. */
  generation: 0,
  selection: { row: 0, col: 0 },
  busy: false,
  /** The version this build reports, once the Rust side has been asked. */
  version: "",
  /** The update the release manifest announced, or null. */
  update: null,
  updateChecked: false,
  /** Why the last check failed, if it did - "up to date" would be a lie. */
  updateError: "",
  checkingUpdate: false,
  updating: false,
  /** The delimiter and encoding the open text file was read with, as chosen
   *  in the bar: "" means the file decided. */
  text: { delimiter: "", encoding: "" },
  /** The text table's header, pinned above the grid while the rest scrolls.
   *  Zero-based; `null` pins nothing. */
  headerRow: null,
  /** Whether the rows and columns the sheet hides are shown in place, marked,
   *  instead of collapsed. Reset whenever another file or sheet is opened. */
  showHidden: false,
  /** The column a delimited table is ordered by, if any: `{ col, descending,
   *  label }`. A view of the rows, cleared when another file is opened. */
  sort: null,
  /** Column widths the reader dragged, per sheet, as `widths[sheet][col]` in
   *  pixels. Seeded from the file's remembered view and written back with the
   *  reopen position, so the columns come back as wide as they were left. */
  widths: {},
};

const els = {
  welcome: document.getElementById("welcome"),
  welcomeRecent: document.getElementById("welcome-recent"),
  welcomeRecentList: document.getElementById("welcome-recent-list"),
  viewer: document.getElementById("viewer"),
  dropOverlay: document.getElementById("drop-overlay"),
  cols: document.getElementById("cols"),
  rows: document.getElementById("rows"),
  body: document.getElementById("body"),
  cells: document.getElementById("cells"),
  tabs: document.getElementById("tabs"),
  readoutRef: document.getElementById("readout-ref"),
  readoutKind: document.getElementById("readout-kind"),
  readoutValue: document.getElementById("readout-value"),
  readoutFormula: document.getElementById("readout-formula"),
  statusFile: document.getElementById("status-file"),
  statusSheet: document.getElementById("status-sheet"),
  statusCells: document.getElementById("status-cells"),
  statusFlash: document.getElementById("status-flash"),
  statusNote: document.getElementById("status-note"),
  trustBadge: document.getElementById("trust-badge"),
  trustBadgeText: document.getElementById("trust-badge-text"),
  updatePill: document.getElementById("update-pill"),
  updatePillText: document.getElementById("update-pill-text"),
  updateNote: document.getElementById("update-note"),
  factVersion: document.getElementById("fact-version"),
  factUpdate: document.getElementById("fact-update"),
  releaseNotes: document.getElementById("release-notes"),
  releaseNotesTitle: document.getElementById("release-notes-title"),
  releaseNotesBody: document.getElementById("release-notes-body"),
  updateConfirm: document.getElementById("update-confirm"),
  updateSummarySection: document.getElementById("update-summary-section"),
  updateSummaryTitle: document.getElementById("update-summary-title"),
  updateSummary: document.getElementById("update-summary"),
  updateSummaryMore: document.getElementById("update-summary-more"),
  btnCheckUpdates: document.getElementById("btn-check-updates"),
  btnInstallUpdate: document.getElementById("btn-install-update"),
  trust: document.getElementById("trust"),
  btnOpen: document.getElementById("btn-open"),
  btnOpen2: document.getElementById("btn-open-2"),
  btnExport: document.getElementById("btn-export"),
  btnReload: document.getElementById("btn-reload"),
  btnHidden: document.getElementById("btn-hidden"),
  btnView: document.getElementById("btn-view"),
  viewMenu: document.getElementById("view-menu"),
  menuHidden: document.getElementById("menu-hidden"),
  menuHiddenNote: document.getElementById("menu-hidden-note"),
  menuData: document.getElementById("menu-data"),
  menuScroll: document.getElementById("menu-scroll"),
  btnSettings: document.getElementById("btn-settings"),
  settings: document.getElementById("settings"),
  textbar: document.getElementById("textbar"),
  textDelimiter: document.getElementById("text-delimiter"),
  textEncoding: document.getElementById("text-encoding"),
  textHeader: document.getElementById("text-header"),
  textDetected: document.getElementById("text-detected"),
  textHint: document.getElementById("text-hint"),
  btnFileOrder: document.getElementById("btn-file-order"),
  btnTrace: document.getElementById("btn-trace"),
  deps: document.getElementById("deps"),
  depsTitle: document.getElementById("deps-title"),
  depsSub: document.getElementById("deps-sub"),
  depsBody: document.getElementById("deps-body"),
  btnDepsClose: document.getElementById("btn-deps-close"),
  btnSaveUtf8: document.getElementById("btn-save-utf8"),
  frozenViewport: document.getElementById("frozen-viewport"),
  frozen: document.getElementById("frozen"),
  frozenCorner: document.getElementById("frozen-corner"),
  freezeColsHead: document.getElementById("freeze-cols-head"),
  freezeRowsHead: document.getElementById("freeze-rows-head"),
  freezeTop: document.getElementById("freeze-top"),
  freezeTopCells: document.getElementById("freeze-top-cells"),
  freezeLeft: document.getElementById("freeze-left"),
  freezeLeftCells: document.getElementById("freeze-left-cells"),
  freezeCorner: document.getElementById("freeze-corner-pane"),
  freezeCornerCells: document.getElementById("freeze-corner-cells"),
  assocFacts: document.getElementById("assoc-facts"),
};

// --------------------------------------------------------------------------
// Small helpers
// --------------------------------------------------------------------------

const ESCAPES = { "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" };

/** Cell text comes from the spreadsheet, so it must never be trusted as HTML. */
const esc = (value) => String(value).replace(/[&<>"']/g, (ch) => ESCAPES[ch]);

const clamp = (v, lo, hi) => (v < lo ? lo : v > hi ? hi : v);

/**
 * A formula as it is shown: one `=` at the front, no ODF namespace.
 *
 * The same cell can come from an `.xlsx` (`SUM(B2:B3)`) or an ODS
 * (`of:=SUM([.B2:.B3])`), and the grid should not care which. Only the display
 * is normalised — what the export writes is the reader's own text.
 */
function formulaText(raw) {
  const text = String(raw === null || raw === undefined ? "" : raw).trim();
  if (!text) return "";
  const bare = text.replace(/^(?:of|oooc|msoxl):=/, "");
  return bare.startsWith("=") ? bare : `=${bare}`;
}

/**
 * What a column's head says: the header cell's text, or the letter.
 *
 * A delimited file's columns have names, and they are the row pinned as its
 * header — `Region`, `Product`, `Units` instead of A, B, C. A workbook keeps
 * the letters: its rows are data, not a table with a header. An empty cell, a
 * column past the header's own width and a tile still on its way all fall back
 * to the letter, the last of them until the render that brings the tile.
 */
function columnLabel(col) {
  const header = pinnedRow();
  if (header === null) return columnName(col);
  const cell = cellAt(header, col);
  if (!cell) return columnName(col);
  const text = String(cell[1]).replace(/\s+/g, " ").trim();
  return text || columnName(col);
}

/**
 * One column's head, as markup.
 *
 * A named column is left-aligned and cut with an ellipsis: a header like
 * `Region` is not the three-character letter the strip was built for, and the
 * title still carries the whole name and the letter it stands in for.
 */
function colHeadMarkup(col) {
  // A collapsed column has no width to hold a letter, and drawing one would
  // stack every hidden letter on the seam.
  if (collapsedCol(col)) return "";
  const active = col === state.selection.col ? " is-active" : "";
  const letter = columnName(col);
  const label = columnLabel(col);
  const named = label !== letter;
  // A revealed column is one the file hides, so it is marked as such: dimmed
  // and struck through rather than looking like an ordinary column.
  const hidden = runAt(geom.hiddenColRuns, col) !== null ? " is-hidden-axis" : "";
  // A delimited table's named column is a sort control: its name orders the
  // records by that column, and a second press reverses the order the first
  // one gave. A workbook keeps its letters, and its rows are not a record list.
  const sortable = named && isTextTable() && !hidden;
  const sorted = Boolean(sortable && state.sort && state.sort.col === col);
  const descending = sorted && state.sort.descending;
  const arrow = sorted
    ? `<span class="colhead-sort" aria-hidden="true">${descending ? "▾" : "▴"}</span>`
    : "";
  const title = hidden
    ? ` title="Column ${esc(letter)} is hidden in the file"`
    : sorted
      ? ` title="Sorted by ${esc(label)}, ${descending ? "descending" : "ascending"} — click to reverse"`
      : sortable
        ? ` title="Sort by ${esc(label)}"`
        : named
          ? ` title="${esc(label)} (column ${letter})"`
          : "";
  const sortAttr = sortable ? ` data-sort="${col}"` : "";
  const sortClass = sortable ? " is-sortable" : "";
  const sortedClass = sorted ? (descending ? " is-sorted is-descending" : " is-sorted") : "";
  // The right edge of the head is the column's edge: dragging it sets the
  // width, and the grid remembers it for this file.
  const grip = `<span class="colhead-grip" data-grip="${col}" title="Drag to resize column ${esc(
    letter
  )}; double-click to go back to the file's width"></span>`;
  return `<div class="colhead${active}${named ? " is-named" : ""}${hidden}${sortClass}${sortedClass}"${sortAttr}${title} style="left:${
    geom.xs[col]
  }px;width:${colWidth(col)}px"><span class="colhead-label">${esc(label)}</span>${arrow}${grip}</div>`;
}

/**
 * The collapsed marker for each run of hidden rows or columns in view.
 *
 * A hidden run takes no space, so what is left is the seam between its
 * neighbours: a thin bar in the number or letter strip, with the count and a
 * hint that clicking it brings the rows or columns back. That is the quickest
 * way to reveal one run without showing every hidden thing in the sheet.
 */
function seamMarkup(runs, axis, view) {
  if (state.showHidden || runs.length === 0) return "";
  const parts = [];
  const startOf = axis === "row" ? (index) => geom.ys[index] : (index) => geom.xs[index];
  const scroll = axis === "row" ? view.scrollTop : view.scrollLeft;
  const extent = axis === "row" ? els.body.clientHeight : els.body.clientWidth;
  const inView = runsInView(
    runs,
    startOf,
    (run) => startOf(run[1] + 1) - startOf(run[0]),
    scroll - 1,
    scroll + extent
  );
  for (const [first, last] of inView) {
    const count = last - first + 1;
    const start = startOf(first);
    const what = count === 1 ? (axis === "row" ? "row" : "column") : axis === "row" ? "rows" : "columns";
    const from = axis === "row" ? String(first + 1) : columnName(first);
    const to = axis === "row" ? String(last + 1) : columnName(last);
    const where = count === 1 ? `at ${from}` : `${from}–${to}`;
    const title = `${count} ${what} hidden ${where} — click to show ${count === 1 ? "it" : "them"}`;
    const run = `${first}:${last}`;
    parts.push(
      axis === "row"
        ? `<div class="rowhead is-seam" data-seam="row" data-run="${run}" title="${esc(
            title
          )}" style="top:${Math.max(0, start - 1)}px;height:3px"></div>`
        : `<div class="colhead is-seam" data-seam="col" data-run="${run}" title="${esc(
            title
          )}" style="left:${Math.max(0, start - 1)}px;width:3px"></div>`
    );
  }
  return parts.join("");
}

/** 0 -> A, 25 -> Z, 26 -> AA */
function columnName(index) {
  let n = index + 1;
  let name = "";
  while (n > 0) {
    const rem = (n - 1) % 26;
    name = String.fromCharCode(65 + rem) + name;
    n = Math.floor((n - 1) / 26);
  }
  return name;
}

const KIND_LABEL = {
  i: "number",
  f: "number",
  s: "text",
  b: "boolean",
  d: "date",
  t: "duration",
  e: "error",
};

function formatBytes(bytes) {
  if (!bytes) return "0 B";
  const units = ["B", "KB", "MB", "GB"];
  let i = 0;
  let n = bytes;
  while (n >= 1024 && i < units.length - 1) {
    n /= 1024;
    i++;
  }
  return `${n < 10 && i > 0 ? n.toFixed(1) : Math.round(n)} ${units[i]}`;
}

let flashTimer = 0;
function flash(message, isError = false) {
  els.statusFlash.textContent = message;
  els.statusFlash.classList.toggle("is-error", isError);
  els.statusFlash.classList.add("show");
  window.clearTimeout(flashTimer);
  flashTimer = window.setTimeout(() => els.statusFlash.classList.remove("show"), 5000);
}

function setBusy(busy, message) {
  state.busy = busy;
  els.btnOpen.disabled = busy;
  els.btnOpen2.disabled = busy;
  els.btnExport.disabled = busy || !state.info;
  els.btnReload.disabled = busy || !state.info;
  els.btnSaveUtf8.disabled = busy || !canSaveUtf8();
  // A menu that is open across a read would be acting on a file that is gone.
  if (busy) closeViewMenu();
  updateViewMenu();
  document.body.style.cursor = busy ? "progress" : "";
  if (message) flash(message);
}

// --------------------------------------------------------------------------
// Sheet geometry
// --------------------------------------------------------------------------

const currentSheet = () => (state.info ? state.info.sheets[state.sheet] : null);
/** A delimited text table: a record can be pinned as its header. */
const isTextTable = () => Boolean(state.info && state.info.text);
/** The encoding was not UTF-8, so a UTF-8 copy is worth offering. */
const canSaveUtf8 = () =>
  Boolean(state.info && state.info.text && state.info.text.encoding !== "UTF-8");
const realRows = () => (currentSheet() ? currentSheet().rows : 0);
const realCols = () => (currentSheet() ? currentSheet().cols : 0);
/** The row pinned above the grid: the chosen header, if it still exists. */
function pinnedRow() {
  if (!isTextTable() || state.headerRow === null) return null;
  return state.headerRow >= 0 && state.headerRow < realRows() ? state.headerRow : null;
}
// Always keep one row/column of header geometry, even for an empty sheet.
const gridRows = () => Math.max(realRows(), 1);
const gridCols = () => Math.max(realCols(), 1);

/**
 * The workbook's frozen rows and columns, bounded by the sheet.
 *
 * A text table has no workbook freeze: its pinned header is a separate choice
 * and the two features never apply to the same file.
 */
function frozenCounts() {
  const sheet = currentSheet();
  if (!sheet || isTextTable()) return { rows: 0, cols: 0 };
  const count = (value, limit) =>
    clamp(Number.isInteger(value) ? value : 0, 0, Math.max(limit, 0));
  return {
    rows: count(sheet.frozenRows, realRows()),
    cols: count(sheet.frozenCols, realCols()),
  };
}

/** Width and height of the frozen bands, in the grid's pixel metrics. */
function frozenSize(freeze = frozenCounts()) {
  return {
    width: freeze.cols > 0 ? geom.xs[freeze.cols] : 0,
    height: freeze.rows > 0 ? geom.ys[freeze.rows] : 0,
  };
}

/**
 * The bands pinned over the part of the grid that scrolls.
 *
 * A workbook pins with a freeze; a delimited file pins its header row, which is
 * a band of its own above the records. Neither belongs to the scrolling area,
 * so both have to be cleared by anything that puts a cell at its top.
 */
function pinnedBands() {
  const freeze = frozenCounts();
  const header = pinnedRow();
  const size = frozenSize(freeze);
  return {
    width: size.width,
    height: size.height + (header === null ? 0 : rowHeight(header)),
  };
}

/**
 * The cell a jump to the data should land on.
 *
 * A report often begins below a title and right of an empty margin, so A1 is
 * not the data; the file says where its first used row and column are. A
 * delimited file's first used row is the record pinned as its header, so the
 * data starts at the first record below that rather than on the names.
 */
function dataStart() {
  const sheet = currentSheet();
  if (!sheet || realRows() === 0) return null;
  const header = pinnedRow();
  const firstRow = Number.isInteger(sheet.firstUsedRow) ? sheet.firstUsedRow : null;
  const firstCol = Number.isInteger(sheet.firstUsedCol) ? sheet.firstUsedCol : null;
  if (firstRow === null && firstCol === null) return null;
  let row = firstRow === null ? 0 : firstRow;
  if (header !== null && row <= header) row = header + 1;
  if (row >= realRows()) return null;
  const col = firstCol === null ? 0 : firstCol;
  return { row, col: clamp(col, 0, Math.max(realCols() - 1, 0)) };
}

/**
 * Put a cell at the top-left of the part of the grid that scrolls.
 *
 * Scrolling a cell *into view* moves the body only as far as it must, which
 * leaves a jump landing at the bottom edge of the pane. A jump means the cell
 * should start the view — and with rows or columns pinned over the body, that
 * start is one band down and across from the body's own origin, or the cell
 * would sit underneath the band it was scrolled under.
 */
function placeAtScrollingOrigin(row, col) {
  const bands = pinnedBands();
  els.body.scrollTop = Math.max(0, geom.ys[row] - bands.height);
  els.body.scrollLeft = Math.max(0, geom.xs[col] - bands.width);
  scheduleViewSave();
  scheduleRender();
}

/**
 * The rows and columns the sheet hides, and the runs they come in.
 *
 * A sheet can hide a column of workings or a block of rows, and the grid
 * collapses them as the sheet's author left them: a report reads as it does in
 * Excel rather than as a wall of detail. The runs are kept because a seam is
 * drawn at each one and because the selection has to step over them.
 */
const hiddenRowRunsOf = () =>
  currentSheet() && Array.isArray(currentSheet().hiddenRowRuns)
    ? currentSheet().hiddenRowRuns
    : [];
const hiddenColRunsOf = () =>
  currentSheet() && Array.isArray(currentSheet().hiddenColRuns)
    ? currentSheet().hiddenColRuns
    : [];

/** How many rows or columns a list of runs stands for. */
const runCount = (runs) => runs.reduce((sum, [first, last]) => sum + last - first + 1, 0);

/** The runs, clamped to the sheet the grid is drawing. */
function boundedRuns(runs, limit) {
  const out = [];
  for (const run of runs) {
    if (!Array.isArray(run) || run.length !== 2) continue;
    const first = Number(run[0]);
    const last = Number(run[1]);
    if (!Number.isInteger(first) || !Number.isInteger(last)) continue;
    if (last < 0 || first >= limit) continue;
    const start = Math.max(0, first);
    const end = Math.min(limit - 1, last);
    if (end >= start) out.push([start, end]);
  }
  return out;
}

/** True when the sheet hides this row or column and it is not being shown. */
const collapsedRow = (row) => !state.showHidden && runAt(geom.hiddenRowRuns, row) !== null;
const collapsedCol = (col) => !state.showHidden && runAt(geom.hiddenColRuns, col) !== null;

/** The run `index` belongs to, or null when that row or column is visible. */
function runAt(runs, index) {
  let lo = 0;
  let hi = runs.length - 1;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    const [first, last] = runs[mid];
    if (index < first) hi = mid - 1;
    else if (index > last) lo = mid + 1;
    else return runs[mid];
  }
  return null;
}

/**
 * The runs that can touch a band of one axis, from `from` to `to`.
 *
 * A binary search finds where the band starts, then the walk stops once the
 * runs begin past its end: a sheet that hides most of its rows costs one pair
 * per render, not one per hidden row.
 */
function runsInView(runs, startOf, sizeOf, from, to) {
  let lo = 0;
  let hi = runs.length - 1;
  let index = 0;
  while (lo <= hi) {
    const mid = (lo + hi) >> 1;
    if (startOf(runs[mid][0]) <= from) {
      index = mid + 1;
      lo = mid + 1;
    } else {
      hi = mid - 1;
    }
  }
  // One run can start above the band and reach into it.
  while (index > 0 && startOf(runs[index - 1][0]) + sizeOf(runs[index - 1]) >= from) index -= 1;
  const found = [];
  for (; index < runs.length; index += 1) {
    if (startOf(runs[index][0]) > to) break;
    found.push(runs[index]);
  }
  return found;
}

/**
 * The nearest visible position, looking in `direction` first.
 *
 * A pointer and the clamping of a selection look forward — the next visible
 * cell is the one the pointer would have hit — while an arrow key looks the
 * way it is travelling, so moving up into a collapsed run lands above it
 * rather than jumping over it and back down.
 */
function visibleIndex(runs, index, limit, direction = 1) {
  const run = runAt(runs, index);
  if (!run) return index;
  if (direction < 0 && run[0] > 0) return run[0] - 1;
  if (run[1] + 1 < limit) return run[1] + 1;
  return Math.max(0, run[0] - 1);
}

/**
 * Lay the sheet out from its own column widths, row heights and merges.
 *
 * A sheet that says nothing about its geometry keeps this app's uniform
 * metrics; a workbook-default width still applies to every column it does not
 * name by hand, so a wide sheet does not collapse to the app's own default.
 */
function rebuildGeometry() {
  const sheet = currentSheet();
  const cols = gridCols();
  const rows = gridRows();
  const widths = effectiveWidths(sheet);
  const heights = sheet && sheet.rowHeights ? sheet.rowHeights : [];
  const defaultCol = Number.isFinite(sheet && sheet.defaultColWidth)
    ? sheet.defaultColWidth
    : COL_W;
  const defaultRow = Number.isFinite(sheet && sheet.defaultRowHeight)
    ? sheet.defaultRowHeight
    : ROW_H;

  // A hidden row or column is collapsed unless the grid is showing what the
  // sheet hides; revealed, it keeps the size the sheet gave it.
  const hiddenColRuns = boundedRuns(hiddenColRunsOf(), cols);
  const hiddenRowRuns = boundedRuns(hiddenRowRunsOf(), rows);

  const xs = new Float64Array(cols + 1);
  const ys = new Float64Array(rows + 1);
  fillAxis(xs, cols, widths, defaultCol, state.showHidden ? null : hiddenColRuns);
  fillAxis(ys, rows, heights, defaultRow, state.showHidden ? null : hiddenRowRuns);
  // A browser cannot scroll past ~33 million pixels: beyond that the
  // scrollbar lies about where it is. A sheet that tall is scaled down as a
  // whole (keeping every cell's share) rather than becoming unreachable.
  fitCanvas(xs);
  fitCanvas(ys);

  geom.cols = cols;
  geom.rows = rows;
  geom.xs = xs;
  geom.ys = ys;
  geom.anchors.clear();
  geom.covered.clear();
  geom.hiddenColRuns = hiddenColRuns;
  geom.hiddenRowRuns = hiddenRowRuns;

  let covered = 0;
  for (const merge of sheet && sheet.merges ? sheet.merges : []) {
    if (!merge || merge.rows < 1 || merge.cols < 1) continue;
    if (merge.row >= rows || merge.col >= cols) continue;
    const bottom = Math.min(rows, merge.row + merge.rows);
    const right = Math.min(cols, merge.col + merge.cols);
    const anchor = `${merge.row}:${merge.col}`;
    geom.anchors.set(anchor, { rows: bottom - merge.row, cols: right - merge.col });
    for (let row = merge.row; row < bottom; row += 1) {
      for (let col = merge.col; col < right; col += 1) {
        if (row === merge.row && col === merge.col) continue;
        if (covered >= MAX_MERGE_COVERAGE) break;
        geom.covered.set(`${row}:${col}`, anchor);
        covered += 1;
      }
    }
  }
}

/**
 * Show what the sheet hides, or collapse it again.
 *
 * The sheet's own choice is the default — a hidden column of workings stays
 * hidden when the file opens — and revealing is only a view: nothing is
 * written back, and what is shown is still marked as hidden.
 */
function setShowHidden(on) {
  if (state.showHidden === on) return;
  state.showHidden = on;
  rebuildGeometry();
  layoutSheet();
  updateHiddenButton();
  // The selection may have been sitting on a row that is now collapsed away.
  const { row, col } = state.selection;
  const nextRow = visibleIndex(geom.hiddenRowRuns, row, realRows());
  const nextCol = visibleIndex(geom.hiddenColRuns, col, realCols());
  if (nextRow !== row || nextCol !== col) select(nextRow, nextCol);
  else scheduleRender();
  flash(
    on
      ? "Showing the rows and columns this sheet hides."
      : "Hidden rows and columns are collapsed again."
  );
}

/**
 * The order a delimited file was left in, as the grid tracks it.
 *
 * The Rust side puts the rows back in that order while it reads the file and
 * sends the choice along with the text info, so the strip's arrow and the
 * status bar describe the table from the first paint — this is not a second
 * sort, and the label is the one the strip showed when it was chosen.
 */
function sortFromText(text) {
  const sort = text && text.sort;
  if (!sort || !Number.isInteger(sort.column)) return null;
  return {
    col: sort.column,
    descending: Boolean(sort.descending),
    label: typeof sort.label === "string" ? sort.label : "",
  };
}

/**
 * Order a delimited table's records by one of its named columns.
 *
 * The Rust side sorts the rows in memory, below the pinned header, so every
 * other field travels with the record it belongs to and a title or note above
 * the header keeps its place. The file on disk is untouched: this is the order
 * of the view, and a reload reads the rows back as the file has them.
 *
 * Pressing the same column again reverses what the first press gave. Pressing a
 * different one starts over, ascending, rather than carrying the previous
 * direction to a column nobody asked to reverse.
 */
async function sortByColumn(col) {
  if (!isTextTable() || state.busy) return;
  const header = pinnedRow();
  if (header === null) return;
  const same = Boolean(state.sort && state.sort.col === col);
  const descending = same && !state.sort.descending;
  const label = columnLabel(col);
  try {
    await invoke("sort_text_rows", {
      sheet: state.sheet,
      headerRow: header,
      column: col,
      descending,
      // The name the strip shows is what the status bar repeats next session.
      label,
    });
  } catch (err) {
    flash(String(err), true);
    return;
  }
  state.sort = { col, descending, label };
  // The rows themselves moved, so every cached tile is stale and the grid has
  // to ask for them again. The geometry stays: a row keeps its height, and the
  // header is still the row the header was pinned to.
  resetTiles();
  scheduleRender();
  updateStatus();
  flash(`Sorted by ${label}, ${descending ? "descending" : "ascending"}.`);
}

/**
 * The way back to the file's own order, offered only while the records are
 * sorted: a table already in the order its file has needs no such control.
 */
function updateFileOrderButton() {
  const sorted = Boolean(state.info && state.info.text && state.sort);
  els.btnFileOrder.hidden = !sorted;
  els.btnFileOrder.disabled = !sorted || state.busy;
}

/**
 * Put the records back in the order the file has them.
 *
 * The Rust side moves the rows back in memory, so nothing is parsed again and
 * the grid keeps its scroll, its selection and the tiles it has loaded. The
 * chosen order is forgotten there too, so the next open is the file's own.
 */
async function restoreFileOrder() {
  if (!isTextTable() || state.busy || !state.sort) return;
  try {
    await invoke("restore_text_order", { sheet: state.sheet });
  } catch (err) {
    flash(String(err), true);
    return;
  }
  state.sort = null;
  // The rows themselves moved, so every cached tile is stale; the geometry, the
  // header and the hidden rows are unchanged.
  resetTiles();
  scheduleRender();
  updateStatus();
  flash("Back in the file's own order.");
}

const plural = (count, word) =>
  `${count.toLocaleString()} ${word}${count === 1 ? "" : "s"}`;

/** The toolbar control that offers the hidden rows and columns, when there are
 *  any; a sheet that hides nothing has no button to wonder about. */
function updateHiddenButton() {
  const rows = runCount(hiddenRowRunsOf());
  const cols = runCount(hiddenColRunsOf());
  els.btnHidden.hidden = rows === 0 && cols === 0;
  if (rows === 0 && cols === 0) {
    updateViewMenu();
    return;
  }
  els.btnHidden.classList.toggle("is-on", state.showHidden);
  const count = countText(rows, cols);
  els.btnHidden.textContent = state.showHidden ? "Hide them again" : `Show ${count}`;
  els.btnHidden.title = state.showHidden
    ? "Collapse the rows and columns this sheet hides"
    : `This sheet hides ${count}; show them in place`;
  updateViewMenu();
}

/** `3 rows and 1 column`, for the two places that say how much is hidden. */
function countText(rows, cols) {
  return rows && cols
    ? `${plural(rows, "row")} and ${plural(cols, "column")}`
    : rows
      ? plural(rows, "row")
      : plural(cols, "column");
}

// --------------------------------------------------------------------------
// View menu
// --------------------------------------------------------------------------

/**
 * The menu the grid's view switches live in.
 *
 * Each one is a switch on the view rather than on the file — what the sheet
 * hides, where its data begins and where the part that scrolls starts — so they
 * belong together instead of spread along the toolbar, where a button that
 * appears only for some files reads as one more thing to work out.
 */
const viewMenuItems = () => [...els.viewMenu.querySelectorAll(".menu-item")];
const firstEnabledMenuItem = () =>
  viewMenuItems().find((item) => !item.disabled) || null;

/**
 * What the menu says about this sheet, and which of its actions can be taken.
 *
 * The notes are the facts a reader would otherwise have to scroll to find: how
 * much is hidden, and whether there is a freeze at all.
 */
function updateViewMenu() {
  const open = Boolean(state.info) && !state.busy;
  els.btnView.disabled = !open;

  const rows = runCount(hiddenRowRunsOf());
  const cols = runCount(hiddenColRunsOf());
  const hides = rows > 0 || cols > 0;
  const count = hides ? countText(rows, cols) : "none";
  els.menuHidden.disabled = !open || !hides;
  els.menuHidden.setAttribute("aria-checked", String(hides && state.showHidden));
  els.menuHiddenNote.textContent = count;
  els.menuHidden.title = !hides
    ? "This sheet hides nothing."
    : state.showHidden
      ? "Collapse the rows and columns this sheet hides"
      : `This sheet hides ${count}; show them in place`;

  const start = open ? dataStart() : null;
  els.menuData.disabled = !start;
  els.menuData.title = start
    ? `Select ${columnName(start.col)}${start.row + 1}, where the data begins`
    : "This sheet has no data to jump to.";

  const bands = open ? pinnedBands() : { width: 0, height: 0 };
  const pinned = bands.width > 0 || bands.height > 0;
  els.menuScroll.disabled = !open || !pinned;
  els.menuScroll.title = pinned
    ? "Select the first cell below and right of what is pinned"
    : "Nothing is pinned, so the whole grid scrolls.";
}

function openViewMenu() {
  if (els.btnView.disabled) return;
  updateViewMenu();
  els.viewMenu.hidden = false;
  els.btnView.setAttribute("aria-expanded", "true");
  // A menu is a keyboard surface as well as a mouse one: opening it moves the
  // focus to its first action rather than leaving it behind on the button.
  const first = firstEnabledMenuItem();
  if (first) first.focus();
}

function closeViewMenu({ focusButton = false } = {}) {
  if (els.viewMenu.hidden) return;
  els.viewMenu.hidden = true;
  els.btnView.setAttribute("aria-expanded", "false");
  if (focusButton) els.btnView.focus();
}

function toggleViewMenu() {
  if (els.viewMenu.hidden) openViewMenu();
  else closeViewMenu();
}

/** Move the focus down or up the menu, skipping the actions that are off. */
function stepViewMenu(delta) {
  const items = viewMenuItems().filter((item) => !item.disabled);
  if (items.length === 0) return;
  const at = items.indexOf(document.activeElement);
  const next = at === -1 ? (delta > 0 ? 0 : items.length - 1) : (at + delta + items.length) % items.length;
  items[next].focus();
}

/**
 * Jump to where the data begins.
 *
 * The target is placed at the top-left of the scrolling area, not merely
 * scrolled into view, so the view starts at the data rather than ending at it.
 */
function jumpToData() {
  const start = dataStart();
  if (!start) {
    flash("This sheet has no data to jump to.", true);
    return;
  }
  closeViewMenu();
  select(start.row, start.col);
  const { row, col } = state.selection;
  placeAtScrollingOrigin(row, col);
  flash(`Jumped to ${columnName(col)}${row + 1}, where the data starts.`);
}

/** What the pinned bands are, for the message that reports jumping past them. */
function pinnedBandText() {
  const freeze = frozenCounts();
  const header = pinnedRow();
  const frozen =
    freeze.rows && freeze.cols
      ? "frozen rows and columns"
      : freeze.rows
        ? "frozen rows"
        : freeze.cols
          ? "frozen columns"
          : "";
  if (header !== null) return frozen ? `pinned header and ${frozen}` : "pinned header";
  return frozen;
}

/**
 * Jump to the first cell that is neither frozen nor behind a pinned header.
 *
 * On a sheet that pins its header, A1 is the header: the cell where the
 * scrolling area begins is the one worth going to when the reading is about to
 * start, and it stays at the top-left rather than somewhere under the band.
 */
function jumpToScrollingCell() {
  const bands = pinnedBands();
  if (bands.width === 0 && bands.height === 0) {
    flash("Nothing is pinned, so the whole grid scrolls.", true);
    return;
  }
  const freeze = frozenCounts();
  const header = pinnedRow();
  const row = freeze.rows > 0 ? freeze.rows : header !== null ? header + 1 : 0;
  const col = clamp(freeze.cols, 0, Math.max(realCols() - 1, 0));
  closeViewMenu();
  select(clamp(row, 0, Math.max(realRows() - 1, 0)), col);
  const target = state.selection;
  placeAtScrollingOrigin(target.row, target.col);
  flash(
    `Jumped past the ${pinnedBandText()} to ${columnName(target.col)}${target.row + 1}.`
  );
}

const tileKey = (tr, tc) => `${tr}:${tc}`;
const maxTileRow = () => Math.max(0, Math.ceil(realRows() / TILE_ROWS) - 1);
const maxTileCol = () => Math.max(0, Math.ceil(realCols() / TILE_COLS) - 1);

function resetTiles() {
  state.tiles.clear();
  state.inflight.clear();
  state.lru.length = 0;
  state.generation += 1;
}

/**
 * Returns [kind, display, look, hidden, formula], null for a known-empty cell,
 * undefined if unknown. `hidden` is the value the cell holds but its number
 * format shows nothing for, which the grid marks so it is not read as empty;
 * `formula` is what the file calculates the cell from, when it calculates it.
 */
function cellAt(row, col) {
  const tr = Math.floor(row / TILE_ROWS);
  const tc = Math.floor(col / TILE_COLS);
  const tile = state.tiles.get(tileKey(tr, tc));
  if (!tile) return undefined;
  const localRow = tile.data[row - tr * TILE_ROWS];
  if (!localRow) return null; // past the end of the sheet
  const cell = localRow[col - tc * TILE_COLS];
  return cell === undefined ? null : cell;
}

function requestTiles(keys) {
  for (const key of keys) {
    if (state.tiles.has(key) || state.inflight.has(key)) continue;
    const [tr, tc] = key.split(":").map(Number);
    // Never ask for tiles outside the sheet: they would come back empty and
    // the cells would shimmer as "loading" forever.
    if (tr > maxTileRow() || tc > maxTileCol()) continue;

    state.inflight.add(key);
    const generation = state.generation;
    invoke("get_rows", {
      sheet: state.sheet,
      rowStart: tr * TILE_ROWS,
      rowCount: TILE_ROWS,
      colStart: tc * TILE_COLS,
      colCount: TILE_COLS,
    })
      .then((page) => {
        state.inflight.delete(key);
        if (generation !== state.generation) return; // sheet changed underneath us
        state.tiles.set(key, { data: page.rows });
        touchTile(key);
        // The readout may have been showing "…" for a cell in this tile.
        updateReadout();
        scheduleRender();
      })
      .catch((err) => {
        state.inflight.delete(key);
        if (generation !== state.generation) return;
        flash(String(err), true);
      });
  }
}

function touchTile(key) {
  const at = state.lru.indexOf(key);
  if (at >= 0) state.lru.splice(at, 1);
  state.lru.push(key);
  while (state.lru.length > TILE_CACHE_MAX) {
    const oldest = state.lru.shift();
    if (!state.inflight.has(oldest)) state.tiles.delete(oldest);
  }
}

// --------------------------------------------------------------------------
// Rendering
// --------------------------------------------------------------------------

let renderQueued = false;

function scheduleRender() {
  if (renderQueued) return;
  renderQueued = true;
  window.requestAnimationFrame(() => {
    renderQueued = false;
    render();
  });
}

function visibleWindow() {
  const { scrollLeft, scrollTop, clientWidth, clientHeight } = els.body;
  const width = Math.max(clientWidth, 1);
  const height = Math.max(clientHeight, 1);
  // Binary search rather than division: columns and rows are no longer one
  // width, and the offset of cell 900,000 cannot be computed by multiplying.
  const firstCol = colAtX(scrollLeft);
  const firstRow = rowAtY(scrollTop);
  const lastCol = Math.min(gridCols(), colAtX(scrollLeft + width) + 1);
  const lastRow = Math.min(gridRows(), rowAtY(scrollTop + height) + 1);
  return {
    firstCol: clamp(firstCol, 0, Math.max(gridCols() - 1, 0)),
    lastCol: clamp(lastCol, 1, gridCols()),
    firstRow: clamp(firstRow, 0, Math.max(gridRows() - 1, 0)),
    lastRow: clamp(lastRow, 1, gridRows()),
    scrollLeft,
    scrollTop,
  };
}

/** Where the selected cell sits, grown to its whole merged region. */
function selectionBox() {
  const { row, col } = state.selection;
  const span = mergeAt(row, col) || { rows: 1, cols: 1 };
  const left = geom.xs[col];
  const top = geom.ys[row];
  return {
    left,
    top,
    width: geom.xs[Math.min(col + span.cols, geom.cols)] - left,
    height: geom.ys[Math.min(row + span.rows, geom.rows)] - top,
  };
}

function ensureTiles(view) {
  const lastRow = Math.max(view.lastRow - 1, 0);
  const lastCol = Math.max(view.lastCol - 1, 0);
  const fromRow = Math.max(0, Math.floor(view.firstRow / TILE_ROWS) - PREFETCH_ROWS);
  const toRow = Math.min(maxTileRow(), Math.floor(lastRow / TILE_ROWS) + PREFETCH_ROWS);
  const fromCol = Math.max(0, Math.floor(view.firstCol / TILE_COLS) - PREFETCH_COLS);
  const toCol = Math.min(maxTileCol(), Math.floor(lastCol / TILE_COLS) + PREFETCH_COLS);

  const wanted = [];
  for (let tr = fromRow; tr <= toRow; tr++) {
    for (let tc = fromCol; tc <= toCol; tc++) wanted.push(tileKey(tr, tc));
  }
  // A text table's header is pinned above the viewport while scrolling, so the
  // tile holding it is needed even when that row is a long way above the
  // visible ones.
  const header = pinnedRow();
  if (header !== null) {
    const tr = Math.floor(header / TILE_ROWS);
    for (let tc = fromCol; tc <= toCol; tc++) wanted.push(tileKey(tr, tc));
  }

  // Frozen rows and columns stay visible while the body scrolls, so their
  // tiles are wanted even when they are outside the visible window.
  const freeze = frozenCounts();
  const frozenRows = Math.ceil(freeze.rows / TILE_ROWS);
  const frozenCols = Math.ceil(freeze.cols / TILE_COLS);
  for (let tr = 0; tr < frozenRows; tr++) {
    for (let tc = 0; tc < frozenCols; tc++) wanted.push(tileKey(tr, tc));
    for (let tc = fromCol; tc <= toCol; tc++) wanted.push(tileKey(tr, tc));
  }
  for (let tc = 0; tc < frozenCols; tc++) {
    for (let tr = fromRow; tr <= toRow; tr++) wanted.push(tileKey(tr, tc));
  }
  requestTiles(wanted);
}

function render() {
  if (!state.info) return;
  const view = visibleWindow();
  ensureTiles(view);

  // The header strips are separate clipped panes that ride along with the
  // body's scroll offset.
  els.cols.style.transform = `translateX(${-view.scrollLeft}px)`;
  els.rows.style.transform = `translateY(${-view.scrollTop}px)`;

  const colParts = [];
  for (let col = view.firstCol; col < view.lastCol; col++) {
    colParts.push(colHeadMarkup(col));
  }
  colParts.push(seamMarkup(geom.hiddenColRuns, "col", view));

  // The header row is named in the strip, so "which row is pinned" can be
  // answered without scrolling to it, and a click on any number moves the pin.
  const header = pinnedRow();
  const pinHint = isTextTable() ? ' title="Click to pin this row as the header"' : "";
  const rowParts = [];
  for (let row = view.firstRow; row < view.lastRow; row++) {
    // A collapsed row has no height to hold a number; the seam below stands
    // in for the run it belongs to.
    if (collapsedRow(row)) continue;
    const active = row === state.selection.row ? " is-active" : "";
    const pinned = row === header;
    const revealed = runAt(geom.hiddenRowRuns, row) !== null ? " is-hidden-axis" : "";
    const title = revealed
      ? ` title="Row ${row + 1} is hidden in the file"`
      : pinned
        ? ' title="Header row — click to unpin it"'
        : isTextTable()
          ? pinHint
          : "";
    rowParts.push(
      `<div class="rowhead${active}${pinned ? " is-pinned" : ""}${revealed}"${title} data-row="${row}" style="top:${
        geom.ys[row]
      }px;height:${rowHeight(row)}px">${row + 1}</div>`
    );
  }
  rowParts.push(seamMarkup(geom.hiddenRowRuns, "row", view));

  els.cols.innerHTML = colParts.join("");
  els.rows.innerHTML = rowParts.join("");

  const cellParts = [];
  for (let row = view.firstRow; row < view.lastRow; row++) {
    for (let col = view.firstCol; col < view.lastCol; col++) {
      cellParts.push(cellMarkup(cellAt(row, col), row, col));
    }
  }

  cellParts.push(hiddenBandMarkup(view));

  const box = selectionBox();
  cellParts.push(
    `<div class="selection" style="left:${box.left}px;top:${box.top}px;width:${box.width}px;height:${box.height}px"></div>`
  );
  els.cells.innerHTML = cellParts.join("");

  renderFrozenRow(view);
  renderFreezePanes(view);
}

/**
 * The inline CSS for a cell's workbook style.
 *
 * A look only carries what the file sets, so a fill-only cell keeps the grid's
 * semantic text colour and a bold header keeps its weight when it has some
 * other styling. Colours are checked again here even though the Rust side
 * normalises them: nothing from a workbook is interpolated into markup raw.
 */
const LOOK_COLOUR = /^#[0-9a-f]{6}$/i;
const lookColour = (value) =>
  typeof value === "string" && LOOK_COLOUR.test(value) ? value : "";

function lookStyle(look) {
  if (!look) return "";
  const css = [];
  if (look.bold) css.push("font-weight:700");
  if (look.italic) css.push("font-style:italic");
  const color = lookColour(look.color);
  if (color) css.push(`color:${color}`);
  const background = lookColour(look.background);
  if (background) css.push(`background:${background}`);
  return css.length ? `;${css.join(";")}` : "";
}

/**
 * One cell as markup: a pending block, nothing for an empty, or the value.
 *
 * A merged region is drawn once, from its anchor, at the size of the whole
 * span; every other cell it covers is left empty so the anchor's border is the
 * only grid line inside it. The line height is set inline so text still sits
 * centred in a row the sheet made taller.
 */
function cellMarkup(cell, row, col, maxBottom = Infinity, topOffset = 0) {
  // A collapsed row or column has no space of its own; its cells would be a
  // zero-width pile at the seam.
  if (collapsedRow(row) || collapsedCol(col)) return "";
  const merge = mergeAt(row, col);
  if (!merge && mergeOrigin(row, col)) return "";

  const right = merge ? Math.min(col + merge.cols, geom.cols) : col + 1;
  const bottom = merge ? Math.min(row + merge.rows, geom.rows, maxBottom) : row + 1;
  const left = geom.xs[col];
  // `topOffset` puts a row at the top of its own layer: the pinned strip is
  // one row tall, so a header deeper in the sheet would otherwise be clipped
  // out of it.
  const top = geom.ys[row] - topOffset;
  const width = geom.xs[right] - left;
  const height = geom.ys[bottom] - geom.ys[row];
  const box = `left:${left}px;top:${top}px;width:${width}px;height:${height}px;line-height:${Math.max(
    11,
    height - 1
  )}px`;

  if (cell === undefined) {
    return `<div class="cell pending" style="${box}"></div>`;
  }
  if (cell === null) return "";
  const text = cell[1];
  const look = cell[2];
  // A format can render a value as nothing (`#,##0;-#,##0;` for a zero). Such
  // a cell has no text to draw, so it keeps its box and gets a corner mark.
  const hidden = cell[3] || "";
  const formula = formulaText(cell[4]);
  if (text === "" && !look && !hidden && !formula) return "";
  // The tooltip carries what the cell does not show: the formula behind it,
  // the value its format hides, or the tail of text the column cut off.
  const tips = [];
  if (formula) tips.push(formula);
  if (hidden) tips.push(`Hidden by its number format: ${hidden}`);
  else if (text.length > 24) tips.push(text);
  const title = tips.length ? ` title="${esc(tips.join("  ·  "))}"` : "";
  const style = `${box}${lookStyle(look)}`;
  const kind = `cell k-${cell[0]}${hidden ? " is-hidden" : ""}${formula ? " has-formula" : ""}`;
  return `<div class="${kind}"${title} style="${style}">${esc(text)}</div>`;
}

/**
 * The wash over a revealed hidden row or column.
 *
 * Showing what the sheet hides is not the same as unhiding it: the rows and
 * columns are still hidden in the file, so they are laid out where they belong
 * under a translucent wash, with their number or letter struck through. The
 * frozen panes are left alone — their headers still carry the mark, and a
 * frozen band is not where a hidden row usually lives.
 */
function hiddenBandMarkup(view) {
  if (!state.showHidden) return "";
  const parts = [];
  const width = geom.xs[geom.cols];
  const height = geom.ys[geom.rows];
  const rowsInView = runsInView(
    geom.hiddenRowRuns,
    (row) => geom.ys[row],
    (run) => geom.ys[run[1] + 1] - geom.ys[run[0]],
    view.scrollTop,
    view.scrollTop + els.body.clientHeight
  );
  for (const [first, last] of rowsInView) {
    const top = geom.ys[first];
    const size = geom.ys[last + 1] - top;
    parts.push(
      `<div class="hidden-band is-row-band" style="top:${top}px;height:${size}px;width:${width}px"></div>`
    );
  }
  const colsInView = runsInView(
    geom.hiddenColRuns,
    (col) => geom.xs[col],
    (run) => geom.xs[run[1] + 1] - geom.xs[run[0]],
    view.scrollLeft,
    view.scrollLeft + els.body.clientWidth
  );
  for (const [first, last] of colsInView) {
    const left = geom.xs[first];
    const size = geom.xs[last + 1] - left;
    parts.push(
      `<div class="hidden-band is-col-band" style="left:${left}px;width:${size}px;height:${height}px"></div>`
    );
  }
  return parts.join("");
}

/**
 * Paint the chosen header above the scrolled body, or hide it.
 *
 * It takes over from the first pixel past the header's own top edge: the real
 * row is still in place underneath at that point, so the swap does not jump,
 * and anything above the header - a title, a note about the export - scrolls
 * away before it pins. A workbook is left alone; its rows are data, not
 * necessarily a table.
 */
function renderFrozenRow(view) {
  const header = pinnedRow();
  const show =
    header !== null && realRows() > header + 1 && view.scrollTop > geom.ys[header];
  els.frozenViewport.hidden = !show;
  els.frozenCorner.hidden = !show;
  if (!show) return;

  els.frozenCorner.textContent = String(header + 1);
  els.frozen.style.transform = `translateX(${-view.scrollLeft}px)`;
  const parts = [];
  // A header cell merged into the rows below it is drawn to the bottom of the
  // strip rather than to the bottom of its whole region, which the strip clips.
  for (let col = view.firstCol; col < view.lastCol; col++) {
    parts.push(cellMarkup(cellAt(header, col), header, col, header + 1, geom.ys[header]));
  }
  // The selection is in the body layer, so it scrolls away with the row it
  // surrounds. When that row is the pinned one, the highlight has to be pinned
  // too or the grid would look like nothing is selected.
  if (state.selection.row === header) {
    const box = selectionBox();
    parts.push(
      `<div class="selection" style="left:${box.left}px;top:0;width:${box.width}px;height:${rowHeight(
        header
      )}px"></div>`
    );
  }
  els.frozen.innerHTML = parts.join("");
  els.frozenCorner.classList.toggle("is-active", state.selection.row === header);
}

/** Cells for a rectangle of the sheet, as the pane that owns them sees it. */
function paneCells(rowStart, rowEnd, colStart, colEnd) {
  const parts = [];
  for (let row = rowStart; row < rowEnd; row++) {
    for (let col = colStart; col < colEnd; col++) {
      parts.push(cellMarkup(cellAt(row, col), row, col));
    }
  }
  return parts.join("");
}

/** The selection outline, in sheet coordinates, for a pane to clip. */
function paneSelection() {
  const box = selectionBox();
  return `<div class="selection" style="left:${box.left}px;top:${box.top}px;width:${box.width}px;height:${box.height}px"></div>`;
}

/** Column letters and row numbers for the frozen bands. */
function renderFreezeHeaders(freeze) {
  const activeRow = state.selection.row;

  const colParts = [];
  const lastCol = Math.min(freeze.cols, colAtX(els.body.clientWidth) + 1);
  for (let col = 0; col < lastCol; col++) {
    colParts.push(colHeadMarkup(col));
  }
  els.freezeColsHead.innerHTML = colParts.join("");

  const rowParts = [];
  const lastRow = Math.min(freeze.rows, rowAtY(els.body.clientHeight) + 1);
  for (let row = 0; row < lastRow; row++) {
    const active = row === activeRow ? " is-active" : "";
    rowParts.push(
      `<div class="rowhead${active}" style="top:${geom.ys[row]}px;height:${rowHeight(
        row
      )}px">${row + 1}</div>`
    );
  }
  els.freezeRowsHead.innerHTML = rowParts.join("");
}

/**
 * Draw the four frozen panes over the body.
 *
 * The corner is fixed; the top pane follows horizontal scrolling and the left
 * pane follows vertical scrolling. Each is clipped to its band, so the cells
 * that scroll underneath it disappear exactly where the freeze ends.
 */
function renderFreezePanes(view) {
  const freeze = frozenCounts();
  const showRows = freeze.rows > 0;
  const showCols = freeze.cols > 0;
  els.freezeTop.hidden = !showRows;
  els.freezeLeft.hidden = !showCols;
  els.freezeCorner.hidden = !showRows || !showCols;
  els.freezeColsHead.hidden = !showCols;
  els.freezeRowsHead.hidden = !showRows;
  if (!showRows && !showCols) return;

  renderFreezeHeaders(freeze);
  const selection = paneSelection();
  const size = frozenSize(freeze);

  // Only the part of a frozen band that fits on screen needs cells; a band
  // taller than the window is clipped at the same place either way.
  const visibleRows = Math.min(freeze.rows, rowAtY(els.body.clientHeight) + 1);
  const visibleCols = Math.min(freeze.cols, colAtX(els.body.clientWidth) + 1);

  if (showRows) {
    const fromCol = Math.max(freeze.cols, view.firstCol);
    els.freezeTopCells.innerHTML =
      paneCells(0, visibleRows, fromCol, view.lastCol) + selection;
    // The pane starts after the frozen columns, so its content has to move by
    // the frozen width as well as by the body's scroll offset.
    els.freezeTopCells.style.transform = `translateX(${-(view.scrollLeft + size.width)}px)`;
  } else {
    els.freezeTopCells.innerHTML = "";
  }

  if (showCols) {
    const fromRow = Math.max(freeze.rows, view.firstRow);
    els.freezeLeftCells.innerHTML =
      paneCells(fromRow, view.lastRow, 0, visibleCols) + selection;
    els.freezeLeftCells.style.transform = `translateY(${-(view.scrollTop + size.height)}px)`;
  } else {
    els.freezeLeftCells.innerHTML = "";
  }

  if (showRows && showCols) {
    els.freezeCornerCells.innerHTML =
      paneCells(0, visibleRows, 0, visibleCols) + selection;
  } else {
    els.freezeCornerCells.innerHTML = "";
  }
}

function layoutSheet() {
  els.cells.style.width = `${geom.xs[geom.cols]}px`;
  els.cells.style.height = `${geom.ys[geom.rows]}px`;
  els.cols.style.width = `${geom.xs[geom.cols]}px`;
  els.rows.style.height = `${geom.ys[geom.rows]}px`;
  // The pinned strip is a copy of the header row, so it follows that row's
  // height when the sheet sizes it.
  const header = pinnedRow();
  const pinned = header === null ? ROW_H : rowHeight(header);
  els.frozenViewport.style.height = `${pinned}px`;
  els.frozenCorner.style.height = `${pinned}px`;
  els.frozen.style.height = `${pinned}px`;
  // The pinned row is painted over the body, so stop it at the body's vertical
  // scrollbar rather than covering the top of it.
  els.frozenViewport.style.right = `${els.body.offsetWidth - els.body.clientWidth}px`;

  // The frozen panes sit exactly over the bands of the body they replace. The
  // body's own offsets are the row-header and column-header sizes.
  const freeze = frozenCounts();
  const size = frozenSize(freeze);
  const scrollX = els.body.offsetWidth - els.body.clientWidth;
  const scrollY = els.body.offsetHeight - els.body.clientHeight;
  const left = els.body.offsetLeft;
  const top = els.body.offsetTop;

  els.freezeTop.hidden = freeze.rows === 0;
  els.freezeLeft.hidden = freeze.cols === 0;
  els.freezeCorner.hidden = freeze.rows === 0 || freeze.cols === 0;
  els.freezeColsHead.hidden = freeze.cols === 0;
  els.freezeRowsHead.hidden = freeze.rows === 0;
  if (freeze.rows === 0 && freeze.cols === 0) return;

  els.freezeTop.style.left = `${left + size.width}px`;
  els.freezeTop.style.top = `${top}px`;
  els.freezeTop.style.right = `${scrollX}px`;
  els.freezeTop.style.height = `${size.height}px`;
  els.freezeLeft.style.left = `${left}px`;
  els.freezeLeft.style.top = `${top + size.height}px`;
  els.freezeLeft.style.width = `${size.width}px`;
  els.freezeLeft.style.bottom = `${scrollY}px`;
  els.freezeCorner.style.left = `${left}px`;
  els.freezeCorner.style.top = `${top}px`;
  els.freezeCorner.style.width = `${size.width}px`;
  els.freezeCorner.style.height = `${size.height}px`;
  els.freezeColsHead.style.left = `${left}px`;
  els.freezeColsHead.style.top = "0px";
  els.freezeColsHead.style.width = `${size.width}px`;
  els.freezeColsHead.style.height = `${top}px`;
  els.freezeRowsHead.style.left = "0px";
  els.freezeRowsHead.style.top = `${top}px`;
  els.freezeRowsHead.style.width = `${left}px`;
  els.freezeRowsHead.style.height = `${size.height}px`;
}

// --------------------------------------------------------------------------
// Column widths
// --------------------------------------------------------------------------

/** The drag in progress, if the pointer is on a column's edge. */
let columnResize = null;
/** Set while the click that ends a drag is still on its way. */
let swallowHeadClick = false;

/**
 * Start dragging a column's edge.
 *
 * The listeners go on `window` rather than on the grip: the strip is redrawn on
 * every frame of the drag, and the element the pointer started on does not
 * outlive the first of those frames.
 */
function startColumnResize(event) {
  if (!state.info || event.button !== 0) return;
  const grip = event.target.closest("[data-grip]");
  if (!grip) return;
  const col = Number(grip.dataset.grip);
  if (!Number.isInteger(col)) return;
  event.preventDefault();
  columnResize = { col, from: event.clientX, width: colWidth(col), moved: false };
  document.body.classList.add("is-resizing");
  window.addEventListener("pointermove", moveColumnResize);
  window.addEventListener("pointerup", endColumnResize);
  window.addEventListener("pointercancel", endColumnResize);
}

function moveColumnResize(event) {
  if (!columnResize) return;
  const delta = event.clientX - columnResize.from;
  // A press with no travel is a click, not a width: it should not change one.
  if (!columnResize.moved && Math.abs(delta) < 2) return;
  columnResize.moved = true;
  applyColumnWidth(columnResize.col, columnResize.width + delta);
}

/**
 * Draw one column wider or narrower without laying out the whole sheet again.
 *
 * `geom.xs` is a prefix sum of widths, so a change to one column moves every
 * start after it by the same amount and leaves the ones before it alone. Merges
 * and hidden runs are still where they were; the sheet is not re-measured on
 * every frame of a drag.
 */
function applyColumnWidth(col, width) {
  const next = clamp(Math.round(width), MIN_COL_W, MAX_COL_W);
  const delta = next - colWidth(col);
  if (delta === 0) return;
  for (let index = col + 1; index < geom.xs.length; index += 1) geom.xs[index] += delta;
  draggedWidths()[col] = next;
  layoutSheet();
  scheduleRender();
}

function endColumnResize() {
  window.removeEventListener("pointermove", moveColumnResize);
  window.removeEventListener("pointerup", endColumnResize);
  window.removeEventListener("pointercancel", endColumnResize);
  document.body.classList.remove("is-resizing");
  const resized = columnResize;
  columnResize = null;
  if (!resized || !resized.moved) return;
  // The click that follows belongs to the drag, not to the head: widening the
  // column that sorts the table should not also sort it.
  swallowHeadClick = true;
  window.setTimeout(() => {
    swallowHeadClick = false;
  }, 0);
  // A drag back to where it started is not a remembered width at all, so this
  // file keeps opening with the width it itself gives the column.
  if (Math.round(colWidth(resized.col)) === Math.round(resized.width)) {
    delete draggedWidths()[resized.col];
  }
  // The drag is over, so the sheet is laid out from its geometry again rather
  // than from a running adjustment to it.
  rebuildGeometry();
  layoutSheet();
  scheduleRender();
  scheduleViewSave();
  flash(
    `Column ${columnName(resized.col)} is ${Math.round(
      colWidth(resized.col)
    )} px wide; this file reopens with it that way.`
  );
}

/** Put a column back to the width the file gives it. */
function resetColumnWidth(event) {
  const grip = event.target.closest("[data-grip]");
  if (!grip || !state.info) return;
  const col = Number(grip.dataset.grip);
  const dragged = draggedWidths();
  if (!Number.isInteger(col) || !(col in dragged)) return;
  event.preventDefault();
  delete dragged[col];
  rebuildGeometry();
  layoutSheet();
  scheduleRender();
  scheduleViewSave();
  flash(`Column ${columnName(col)} is back at the width the file gives it.`);
}

// --------------------------------------------------------------------------
// Selection
// --------------------------------------------------------------------------

/**
 * Where the grid is, to be put back when the file is opened again.
 *
 * The store is written when the moving stops: a write per arrow key would be a
 * write per keypress. Opening, switching sheets and scrolling all end up here,
 * and the Rust side stores it against the file that is open, so what is saved
 * is always this file's position. A failure is not worth a message — the file
 * is still open, and the next move tries again.
 */
let viewSaveTimer = 0;

function scheduleViewSave() {
  if (!state.info || state.busy) return;
  window.clearTimeout(viewSaveTimer);
  viewSaveTimer = window.setTimeout(() => {
    viewSaveTimer = 0;
    saveView();
  }, 250);
}

/**
 * Write a position that is still waiting, before the file it belongs to stops
 * being the one that is open.
 */
function flushViewSave() {
  if (!viewSaveTimer) return;
  window.clearTimeout(viewSaveTimer);
  viewSaveTimer = 0;
  saveView();
}

function saveView() {
  if (!state.info) return;
  invoke("remember_view", {
    view: {
      sheet: state.sheet,
      row: state.selection.row,
      col: state.selection.col,
      top: els.body.scrollTop,
      left: els.body.scrollLeft,
      widths: widthsPayload(),
    },
  }).catch(() => {});
}

function select(row, col, { scrollIntoView = false } = {}) {
  // Every cell of a merged region is the same cell: selecting a covered one
  // selects the region's anchor, so the readout and the outline agree.
  const origin = mergeOrigin(row, col);
  if (origin) {
    row = origin.row;
    col = origin.col;
  }
  // A collapsed row or column cannot hold the selection: nothing would be
  // outlined and the readout would describe a cell that is not on screen. The
  // nearest visible one is taken instead, which is what the pointer and the
  // arrow keys would have hit anyway.
  let targetRow = clamp(row, 0, Math.max(realRows() - 1, 0));
  let targetCol = clamp(col, 0, Math.max(realCols() - 1, 0));
  if (!state.showHidden) {
    targetRow = visibleIndex(geom.hiddenRowRuns, targetRow, realRows());
    targetCol = visibleIndex(geom.hiddenColRuns, targetCol, realCols());
  }
  state.selection.row = targetRow;
  state.selection.col = targetCol;
  updateReadout();
  // An open panel follows the selection, so the chain on screen is always the
  // chain around the cell the readout names.
  refreshDependencies();

  if (scrollIntoView) {
    const body = els.body;
    const box = selectionBox();
    const freeze = frozenCounts();
    const size = frozenSize(freeze);

    // A frozen row or column is already on screen; scrolling to it would move
    // the body for no reason. Otherwise the visible band starts after the
    // frozen one, and that is the edge the cell has to clear.
    if (state.selection.col >= freeze.cols) {
      const left = body.scrollLeft + size.width;
      if (box.left < left) body.scrollLeft = box.left - size.width;
      else if (box.left + box.width > body.scrollLeft + body.clientWidth)
        body.scrollLeft = box.left + box.width - body.clientWidth;
    }
    if (state.selection.row >= freeze.rows) {
      const top = body.scrollTop + size.height;
      if (box.top < top) body.scrollTop = box.top - size.height;
      else if (box.top + box.height > body.scrollTop + body.clientHeight)
        body.scrollTop = box.top + box.height - body.clientHeight;
    }
  }
  scheduleViewSave();
  scheduleRender();
}

/**
 * Turn a pointer position into a cell, honouring the frozen bands.
 *
 * Inside the frozen width or height the sheet coordinate is the viewport
 * coordinate; outside it, the body's scroll offset applies.
 */
function selectAtPoint(event) {
  if (!state.info) return;
  const rect = els.body.getBoundingClientRect();
  const x = clamp(event.clientX - rect.left, 0, Math.max(rect.width - 1, 0));
  const y = clamp(event.clientY - rect.top, 0, Math.max(rect.height - 1, 0));
  const freeze = frozenCounts();
  const size = frozenSize(freeze);
  const col = colAtX(x < size.width ? x : x + els.body.scrollLeft);
  const row = rowAtY(y < size.height ? y : y + els.body.scrollTop);
  select(row, col);
}

/** A wheel event over a frozen pane still scrolls the body behind it. */
function scrollFromPane(event) {
  const body = els.body;
  const unit =
    event.deltaMode === 1 ? 16 : event.deltaMode === 2 ? body.clientHeight : 1;
  if (event.deltaX) body.scrollLeft += event.deltaX * unit;
  if (event.deltaY) body.scrollTop += event.deltaY * unit;
  if (event.deltaX || event.deltaY) event.preventDefault();
}

/** The formula the readout shows, or nothing for a cell that is not calculated. */
function showReadoutFormula(formula) {
  els.readoutFormula.textContent = formula;
  els.readoutFormula.title = formula
    ? `Formula: ${formula} — Ctrl+Shift+C copies it`
    : "";
  els.readoutFormula.hidden = !formula;
  // Following the chain is only offered where there is a recipe to start from.
  els.btnTrace.hidden = !formula;
}

// ---------------------------------------------------------------------------
// Dependencies
// ---------------------------------------------------------------------------

/**
 * The chain around the selected cell.
 *
 * A report is a chain of calculations, and a figure that looks wrong gives no
 * hint of where to look next. The panel names the cells the selected formula
 * reads and the cells that read it, each with its text; every name is a button,
 * so the chain can be walked from either end while the place it started from
 * stays one keypress away.
 */
let depsRequest = 0;

function closeDependencies() {
  // An answer still on its way belongs to the panel being closed.
  depsRequest += 1;
  els.deps.hidden = true;
}

async function openDependencies() {
  els.deps.hidden = false;
  refreshDependencies();
}

/**
 * Ask for the chain around the selection, and draw it when it arrives.
 *
 * The answer belongs to the cell that was selected when it was asked for; a
 * later selection makes an earlier answer stale, so the panel never shows the
 * chain of a cell the reader has already left.
 */
async function refreshDependencies() {
  if (els.deps.hidden || !state.info || state.busy) return;
  const { row, col } = state.selection;
  const request = ++depsRequest;
  try {
    const data = await invoke("cell_dependencies", {
      sheet: state.sheet,
      row,
      col,
    });
    if (request !== depsRequest) return;
    renderDependencies(row, col, data);
  } catch (err) {
    if (request !== depsRequest) return;
    els.depsTitle.textContent = `${columnName(col)}${row + 1}`;
    els.depsSub.textContent = "";
    els.depsBody.innerHTML = `<p class="deps-empty">${esc(String(err))}</p>`;
  }
}

/**
 * The formula the panel's heading repeats, once the cell in front has loaded.
 *
 * A chain can be drawn before the tile holding its cell has arrived, so this is
 * also called as tiles land: the heading says what the cell is calculated from
 * as soon as the grid itself knows.
 */
function updateDepsSub(row, col) {
  const cell = cellAt(row, col);
  const formula = cell && cell[4] ? formulaText(cell[4]) : "";
  els.depsSub.textContent = formula;
  return formula;
}

function renderDependencies(row, col, data) {
  const cell = cellAt(row, col);
  const formula = updateDepsSub(row, col);
  els.depsTitle.textContent = `${columnName(col)}${row + 1}`;

  const section = (heading, cells, empty) => {
    const count = cells.length
      ? ` <span class="deps-count">${cells.length}</span>`
      : "";
    const list = cells.length
      ? `<ul class="deps-list">${cells
          .map(
            (item) =>
              `<li><button type="button" class="deps-cell${
                item.formula ? " is-calc" : ""
              }" data-row="${item.row}" data-col="${item.col}" title="${
                item.formula
                  ? "Calculated here — press to follow it"
                  : "Press to select this cell"
              }"><span class="deps-ref">${columnName(item.col)}${
                item.row + 1
              }</span><span class="deps-text">${esc(
                item.text || "·"
              )}</span></button></li>`
          )
          .join("")}</ul>`
      : `<p class="deps-empty">${esc(empty)}</p>`;
    return `<section class="deps-section"><h3>${heading}${count}</h3>${list}</section>`;
  };

  const parts = [
    section(
      "Feeds this cell",
      data.precedents,
      // A cell that has not arrived yet cannot say which of the two it is.
      cell === undefined
        ? "Nothing feeds it."
        : formula
        ? "Its formula names no cells on this sheet."
        : "Nothing feeds it: it is typed in, not calculated."
    ),
    section("Reads this cell", data.dependents, "Nothing is calculated from it."),
  ];
  if (data.unlisted && data.unlisted.length) {
    parts.push(
      `<p class="deps-note">It also reads ${data.unlisted
        .map((name) => `<code>${esc(name)}</code>`)
        .join(", ")}, a whole column or row: each one covers too many cells to list here.</p>`
    );
  }
  if (data.truncated) {
    parts.push(
      `<p class="deps-note">The list stops before the end: there are more cells than it lists.</p>`
    );
  }
  els.depsBody.innerHTML = parts.join("");
}

function updateReadout() {
  const { row, col } = state.selection;
  els.readoutRef.textContent = `${columnName(col)}${row + 1}`;
  // The panel's heading carries the same formula, and the cell may have
  // arrived after the chain around it was drawn.
  if (!els.deps.hidden) updateDepsSub(row, col);
  els.readoutValue.classList.remove("is-hidden-value");
  els.readoutValue.title = "";
  const cell = cellAt(row, col);
  if (cell === undefined) {
    els.readoutKind.textContent = "";
    els.readoutValue.textContent = "…";
    showReadoutFormula("");
    return;
  }
  // A calculated cell says what it is calculated from; a typed-in one does not.
  showReadoutFormula(cell === null ? "" : formulaText(cell[4]));
  if (cell === null || cell[0] === "z") {
    els.readoutKind.textContent = "empty";
    els.readoutValue.textContent = "";
    return;
  }
  const hidden = cell[3];
  els.readoutKind.textContent = hidden
    ? `${KIND_LABEL[cell[0]] || "value"} · hidden`
    : KIND_LABEL[cell[0]] || "";
  // The grid shows what the cell's format shows; when that is nothing, the
  // readout is the one place the value behind the blank can be read.
  els.readoutValue.textContent = hidden || cell[1];
  if (hidden) {
    els.readoutValue.classList.add("is-hidden-value");
    els.readoutValue.title = "The number format shows nothing for this value";
  }
}

/**
 * Copy the selected cell.
 *
 * `formula` asks for the recipe rather than the result: the same text the
 * readout shows, with its leading `=`, so it can be pasted into a formula bar
 * or another sheet instead of arriving as the number it produced. The formula
 * is copied exactly as the file writes it — a reference is not shifted the way
 * a spreadsheet would shift it for the destination.
 */
function copySelection({ formula = false } = {}) {
  const { row, col } = state.selection;
  const ref = `${columnName(col)}${row + 1}`;
  const cell = cellAt(row, col);
  if (!cell) return;
  // A hidden value is what copying the cell gives in Excel too, and the
  // readout has already shown it: copying the blank would lose the number.
  const text = formula ? formulaText(cell[4]) : cell[3] || cell[1];
  // A cell that holds no formula has no recipe to give; saying so is better
  // than a copy that looks like it failed.
  if (formula && !text) {
    flash(`${ref} is not calculated, so it has no formula to copy.`, true);
    return;
  }
  const done = () =>
    flash(formula ? `Copied the formula in ${ref}` : `Copied ${ref}`);

  if (navigator.clipboard && navigator.clipboard.writeText) {
    navigator.clipboard.writeText(text).then(done).catch(() => legacyCopy(text, done));
  } else {
    legacyCopy(text, done);
  }
}

/** Clipboard API needs a secure context; fall back to a hidden textarea. */
function legacyCopy(text, done) {
  const ta = document.createElement("textarea");
  ta.value = text;
  ta.style.position = "fixed";
  ta.style.opacity = "0";
  document.body.appendChild(ta);
  ta.select();
  try {
    document.execCommand("copy");
    done();
  } catch {
    flash("Could not copy to the clipboard.", true);
  } finally {
    document.body.removeChild(ta);
  }
}

// --------------------------------------------------------------------------
// Opening and closing files
// --------------------------------------------------------------------------

/**
 * The header row to pin after opening.
 *
 * A text table opened for the first time pins its first record, which is the
 * convention the bar is built on. Re-reading the same file with another
 * delimiter keeps whatever was chosen, including "nothing": the choice is the
 * user's, and a re-read is not a new file.
 */
function headerAfterOpen(info, previous) {
  if (!info.text) return null;
  if (previous === null) return null;
  const rows = info.sheets.length ? info.sheets[0].rows : 0;
  // A file opened on its own uses the row chosen for it before: absent means
  // nobody chose one, so the first record pins; null that the file was left
  // unpinned; a number a row. A re-read keeps whatever is pinned now, because
  // the choice belongs to the open file and a re-read is not a new one.
  const remembered = "headerRow" in info.text ? info.text.headerRow : 0;
  const candidate = Number.isInteger(previous) ? previous : remembered;
  if (candidate === null) return null;
  return Number.isInteger(candidate) && candidate < rows ? candidate : 0;
}

/**
 * Put the files the Rust side remembers on the welcome screen.
 *
 * The path is data from disk, so both the visible text and the attribute the
 * click reads go through `esc`. The backend has already dropped entries whose
 * files are gone; a failure here simply leaves the screen without the list.
 */
async function refreshRecent() {
  try {
    const files = await invoke("recent_files");
    const items = Array.isArray(files)
      ? files.filter((file) => file && typeof file.path === "string" && file.path)
      : [];
    els.welcomeRecent.hidden = !items.length;
    els.welcomeRecentList.innerHTML = items
      .map(
        (file) =>
          `<li><button class="recent-file" type="button" data-path="${esc(file.path)}">` +
          `<span class="recent-name">${esc(file.name || file.path)}</span>` +
          `<span class="recent-path">${esc(file.path)}</span></button></li>`
      )
      .join("");
  } catch {
    els.welcomeRecent.hidden = true;
    els.welcomeRecentList.innerHTML = "";
  }
}

/**
 * Open a file, optionally reading a delimited one the way the user chose.
 *
 * A text table is re-read from disk when the delimiter or encoding changes:
 * the parse is the thing that was wrong, so there is nothing to patch in the
 * cells already in memory. Any other format ignores the dialect.
 */
async function openPath(path, dialect = { delimiter: "", encoding: "" }, { explicit = false } = {}) {
  if (!path) return;
  // A move that has not been written yet still belongs to the file that is
  // open now, not to the one about to replace it.
  flushViewSave();
  // The panel and the menu are about the file being replaced.
  closeDependencies();
  closeViewMenu();
  setBusy(true, "Reading…");
  // Captured before the await: a second open of the same file must not read
  // the state the first one is about to replace.
  const previousHeader = state.info && state.info.path === path ? state.headerRow : undefined;
  try {
    const info = await invoke("open_workbook", {
      path,
      delimiter: dialect.delimiter || null,
      encoding: dialect.encoding || null,
      dialectExplicit: explicit,
    });
    state.info = info;
    state.text = { delimiter: dialect.delimiter, encoding: dialect.encoding };
    // A choice remembered from an earlier open comes back from the Rust side
    // rather than from this call, so the selects have to be told about it: a
    // re-read must go on using what the bar says.
    if (info.text) {
      if (!state.text.delimiter && info.text.delimiterChosen) {
        state.text.delimiter = info.text.delimiter;
      }
      if (!state.text.encoding && info.text.encodingChosen) {
        state.text.encoding = encodingValue(info.text.encoding);
      }
    }
    state.headerRow = headerAfterOpen(info, previousHeader);
    // A file reopens on the sheet it was left on, when the workbook still has
    // that sheet: a tab removed since would otherwise reopen nothing.
    const viewed = info.view || null;
    // The columns it was left with, by sheet — drags this reader made earlier,
    // laid over the widths the file itself carries.
    state.widths = widthsFromView(viewed);
    state.sheet =
      viewed &&
      Number.isInteger(viewed.sheet) &&
      viewed.sheet >= 0 &&
      viewed.sheet < info.sheets.length
        ? viewed.sheet
        : 0;
    // Another file, another set of hidden rows: the view starts collapsed. The
    // order a delimited file's records were left in comes back with the file —
    // the Rust side has already put the rows that way, so this is the strip and
    // the status bar catching up with the table, not a second sort.
    state.showHidden = false;
    state.sort = sortFromText(info.text);
    resetTiles();
    state.selection.row = 0;
    state.selection.col = 0;

    els.welcome.hidden = true;
    els.viewer.hidden = false;
    els.body.scrollTop = 0;
    els.body.scrollLeft = 0;

    buildTabs();
    rebuildGeometry();
    layoutSheet();
    // The offsets only mean something once the sheet has a height, and the
    // remembered cell is selected without scrolling to it: the offset is where
    // the reader was looking, not a place to move them away from.
    if (viewed) {
      els.body.scrollTop = Number.isFinite(viewed.top) ? viewed.top : 0;
      els.body.scrollLeft = Number.isFinite(viewed.left) ? viewed.left : 0;
      select(viewed.row || 0, viewed.col || 0);
    }
    updateStatus();
    updateTextBar();
    updateHiddenButton();
    updateReadout();
    scheduleRender();

    if (info.truncated) {
      flash("Very large workbook: only the first part of it was loaded.", true);
    } else if (explicit && info.text) {
      // A choice made in the bar is also the one the next open starts from, so
      // say which way it went: remembered, or back to detection.
      const auto = !state.text.delimiter && !state.text.encoding;
      flash(
        auto
          ? `Read as ${info.text.delimiter} · ${info.text.encoding}; detection is back.`
          : `Read as ${info.text.delimiter} · ${info.text.encoding}; remembered for this file.`
      );
    } else {
      flash(`Opened ${info.fileName}`);
    }
  } catch (err) {
    flash(String(err), true);
    // A recent path can point at a file that has gone since the list was made.
    if (!els.welcome.hidden) refreshRecent();
  } finally {
    setBusy(false);
  }
}

async function chooseFile() {
  setBusy(true);
  try {
    const path = await invoke("pick_workbook");
    if (path) {
      setBusy(false);
      await openPath(path);
      return;
    }
  } catch (err) {
    flash(String(err), true);
  }
  setBusy(false);
}

function closeWorkbook() {
  // A position still waiting to be written belongs to the file being closed.
  flushViewSave();
  closeDependencies();
  closeViewMenu();
  state.info = null;
  state.text = { delimiter: "", encoding: "" };
  state.headerRow = null;
  // The widths belonged to the file that is being closed; the next open brings
  // its own from the store.
  state.widths = {};
  els.frozenViewport.hidden = true;
  els.frozenCorner.hidden = true;
  els.frozen.innerHTML = "";
  for (const pane of [els.freezeTop, els.freezeLeft, els.freezeCorner]) pane.hidden = true;
  els.freezeColsHead.hidden = true;
  els.freezeRowsHead.hidden = true;
  for (const cells of [
    els.freezeTopCells,
    els.freezeLeftCells,
    els.freezeCornerCells,
  ]) {
    cells.innerHTML = "";
  }
  updateTextBar();
  closeTrust();
  resetTiles();
  els.viewer.hidden = true;
  els.welcome.hidden = false;
  els.cells.innerHTML = "";
  updateStatus();
  refreshRecent();
  setBusy(false);
}

async function exportOds() {
  if (!state.info) return;
  setBusy(true, "Exporting…");
  try {
    const summary = await invoke("export_ods", { destination: null });
    flash(
      `Wrote ${summary.sheets} sheet${summary.sheets === 1 ? "" : "s"} (${formatBytes(
        summary.bytes
      )}) to ${summary.path}`
    );
  } catch (err) {
    const message = String(err);
    if (message.includes("cancel")) flash("Export cancelled.");
    else flash(message, true);
  } finally {
    setBusy(false);
  }
}

function buildTabs() {
  const sheets = state.info ? state.info.sheets : [];
  if (sheets.length <= 1) {
    // A single sheet needs no tab strip; note it instead so the layout does
    // not jump when a workbook does have several sheets.
    els.tabs.innerHTML = sheets.length
      ? `<span class="tab-empty">1 sheet</span>`
      : `<span class="tab-empty">No sheets</span>`;
    return;
  }
  els.tabs.innerHTML = sheets
    .map(
      (sheet, index) =>
        `<button type="button" class="tab${
          index === state.sheet ? " is-active" : ""
        }" data-sheet="${index}">${esc(sheet.name)}</button>`
    )
    .join("");
}

function setSheet(index) {
  if (!state.info || index === state.sheet) return;
  state.sheet = index;
  // Each sheet decides for itself what it hides; the view starts collapsed and
  // in the file's own order.
  state.showHidden = false;
  state.sort = null;
  // Cells are addressed per sheet; a chain from the sheet behind would be
  // naming the wrong cells, and the menu's notes describe the sheet in front.
  closeDependencies();
  closeViewMenu();
  resetTiles();
  state.selection.row = 0;
  state.selection.col = 0;
  els.body.scrollTop = 0;
  els.body.scrollLeft = 0;
  buildTabs();
  rebuildGeometry();
  layoutSheet();
  updateStatus();
  updateHiddenButton();
  updateReadout();
  // Another sheet is another position for this file: it is remembered too, so
  // closing here reopens on the sheet that was in front.
  scheduleViewSave();
  scheduleRender();
}

function updateStatus() {
  updateTrustBadge();
  updateStatusNote();
  updateFileOrderButton();
  const info = state.info;
  if (!info) {
    els.statusFile.textContent = "No file open";
    els.statusSheet.textContent = "—";
    els.statusCells.textContent = "";
    return;
  }
  els.statusFile.textContent = info.fileName;
  const sheet = currentSheet();
  els.statusSheet.textContent = sheet
    ? `${sheet.name} · ${sheet.rows.toLocaleString()} rows × ${sheet.cols.toLocaleString()} cols`
    : "—";
  const bits = [
    info.format,
    formatBytes(info.fileSize),
    `${info.usedCells.toLocaleString()} filled cells`,
  ];
  if (sheet && sheet.firstUsedRow) {
    bits.push(`data starts at row ${(sheet.firstUsedRow + 1).toLocaleString()}`);
  }
  if (state.sort && isTextTable()) {
    const label = state.sort.label || columnLabel(state.sort.col);
    bits.push(`sorted by ${label}, ${state.sort.descending ? "descending" : "ascending"}`);
  }
  els.statusCells.textContent = bits.join("  ·  ");
}

// --------------------------------------------------------------------------
// Delimited text: the delimiter and the encoding, which a file cannot say
// --------------------------------------------------------------------------

/**
 * The select value for an encoding name the Rust side reports.
 *
 * The bar reports the name a person reads — `UTF-16 LE`, `Shift_JIS`, `GBK` —
 * and the select holds the name the backend accepts (`utf-16le`, `shift_jis`,
 * `gbk`). Comparing letters and digits alone matches the two spellings without
 * a table that would fall out of step with the options; a name the picker does
 * not list leaves the select on Auto, which is where a re-read starts from.
 */
function encodingValue(label) {
  const key = String(label || "")
    .toLowerCase()
    .replace(/[^a-z0-9]/g, "");
  if (!key) return "";
  const option = [...els.textEncoding.options].find(
    (candidate) =>
      candidate.value &&
      candidate.value.toLowerCase().replace(/[^a-z0-9]/g, "") === key
  );
  return option ? option.value : "";
}

/**
 * Show or hide the text bar, and say what the open file was read with.
 *
 * The selects hold the user's choice rather than the detected value: an
 * `<option>` cannot say "Auto (comma)", and the line beside them is where the
 * answer belongs. Choosing "Auto" again puts the guess back.
 */
function updateTextBar() {
  const text = state.info && state.info.text;
  els.textbar.hidden = !text;
  // Only a text table has a row to pin, so only its row numbers invite a click.
  els.viewer.classList.toggle("is-text", Boolean(text));
  if (!text) {
    els.textDetected.textContent = "";
    els.textHint.hidden = true;
    els.textHeader.value = "";
    els.textHeader.disabled = true;
    els.btnSaveUtf8.hidden = true;
    els.btnFileOrder.hidden = true;
    els.btnFileOrder.disabled = true;
    els.btnSaveUtf8.disabled = true;
    return;
  }
  els.btnSaveUtf8.hidden = text.encoding === "UTF-8";
  els.btnSaveUtf8.disabled = state.busy || !canSaveUtf8();
  els.textDelimiter.value = state.text.delimiter;
  els.textEncoding.value = state.text.encoding;
  const row = pinnedRow();
  els.textHeader.disabled = realRows() === 0;
  els.textHeader.max = String(Math.max(realRows(), 1));
  els.textHeader.value = row === null ? "" : String(row + 1);
  els.textDetected.textContent = `Read as ${text.delimiter} · ${text.encoding}`;
  els.textHint.hidden = !text.undecodable;
  els.textHint.textContent = text.undecodable
    ? "Some characters did not decode — try another encoding."
    : "";
}

/**
 * Save a UTF-8 copy of the open text file, decoded with the encoding it was
 * read with. The original is left alone; the backend asks for a new name.
 */
async function saveUtf8Copy() {
  if (!canSaveUtf8() || state.busy) return;
  setBusy(true, "Saving a UTF-8 copy…");
  try {
    const summary = await invoke("save_utf8_copy", { destination: null });
    flash(`Saved a UTF-8 copy (${formatBytes(summary.bytes)}) to ${summary.path}`);
  } catch (err) {
    const message = String(err);
    if (message.includes("cancel")) flash("Save cancelled.");
    else flash(message, true);
  } finally {
    setBusy(false);
  }
}

/** Re-read the open text file with the delimiter and encoding now selected. */
async function applyTextDialect() {
  if (!state.info || !state.info.text) return;
  await openPath(
    state.info.path,
    {
      delimiter: els.textDelimiter.value,
      encoding: els.textEncoding.value,
    },
    // Only a choice made here is remembered for the file; Reload and the first
    // open leave the store alone.
    { explicit: true }
  );
}

/**
 * Pin `row` as the header, or nothing.
 *
 * The pinned strip is a copy of one row drawn over the body, so moving it is a
 * layout change rather than a re-read: nothing touches the file and the tiles
 * already in memory stay valid.
 */
function setHeaderRow(row) {
  if (!isTextTable()) return;
  const rows = realRows();
  const next = Number.isInteger(row) && row >= 0 && row < rows ? row : null;
  state.headerRow = next;
  // The records start after the header, so an order sorted below the old one
  // no longer describes what follows: the marker is dropped rather than lying,
  // and the status bar stops repeating an order the table no longer keeps.
  if (state.sort) {
    state.sort = null;
    updateStatus();
  }
  rememberHeader(next);
  updateTextBar();
  layoutSheet();
  scheduleRender();
  if (next === null) flash("Header unpinned.");
  else flash(`Row ${(next + 1).toLocaleString()} is the header, pinned while the rest scrolls.`);
}

/**
 * Tell the backend which row is now the header, so the file reopens with it.
 *
 * Moving the pin is a view change, not a re-read, so this is its own call. The
 * store is a convenience: a failure leaves this session with the same header
 * and only loses it next time.
 */
function rememberHeader(row) {
  if (!isTextTable() || !state.info || !state.info.path) return;
  invoke("remember_header", { row }).catch(() => {});
}

/** Take the header row from the bar's field: empty unpins it. */
function applyHeaderInput() {
  if (!isTextTable()) return;
  const raw = els.textHeader.value.trim();
  if (raw === "") {
    setHeaderRow(null);
    return;
  }
  const rows = realRows();
  const row = Number(raw);
  if (!Number.isInteger(row) || row < 1 || row > rows) {
    flash(`Header row must be a whole number from 1 to ${rows.toLocaleString()}.`, true);
    updateTextBar();
    return;
  }
  setHeaderRow(row - 1);
}

// --------------------------------------------------------------------------
// What the workbook carries: macros, links and other active content
// --------------------------------------------------------------------------

/** `["a", "b", "c"]` -> `"a, b and c"`. */
function joinList(items) {
  if (!items.length) return "";
  if (items.length === 1) return items[0];
  return `${items.slice(0, -1).join(", ")} and ${items[items.length - 1]}`;
}

/**
 * A standing line for a macro-enabled workbook, on top of the badge. The badge
 * says macros are there; this says what ExcelScry does about them, which is the
 * part that stops someone assuming the viewer would have offered to run them.
 */
function updateStatusNote() {
  const trust = state.info && state.info.trust;
  const macros = Boolean(trust && trust.detail.macros === true);
  els.statusNote.hidden = !macros;
  els.statusNote.textContent = macros ? "Macros present — ExcelScry will not run them" : "";
}

function updateTrustBadge() {
  const trust = state.info && state.info.trust;
  if (!trust) {
    els.trustBadge.hidden = true;
    return;
  }
  els.trustBadge.hidden = false;
  els.trustBadge.dataset.level = trust.level;
  els.trustBadgeText.textContent = trust.badge;

  // The badge is the only place the unexamined signals are visible without a
  // click, so spell them out in the tooltip rather than letting "No macros"
  // read as a clean bill of health.
  // When nothing was found, repeat what *was* checked (the badge) rather than
  // claiming a bare "nothing found" for signals that never ran.
  const lines = [
    trust.hazards.length ? `Present: ${trust.hazards.join(", ")}.` : `${trust.badge}.`,
  ];
  if (trust.unchecked.length) {
    lines.push(`This format gives no way to check ${joinList(trust.unchecked)}.`);
  }
  lines.push("Click for details.");
  els.trustBadge.title = lines.join(" ");
}

/** The five signals, in the order they are reported. */
const TRUST_ROWS = [
  ["macros", "Macros"],
  ["externalLinks", "External links"],
  ["connections", "Data connections"],
  ["embeddedObjects", "Embedded objects"],
  ["activex", "ActiveX controls"],
];

/** `null` means the format gave no way to look, which is not the same as "none". */
function trustValueHtml(value) {
  if (value === null || value === undefined) {
    return '<span class="trust-skip">Not checked</span>';
  }
  if (typeof value === "number") {
    if (value === 0) return '<span class="trust-none">None found</span>';
    return `<span class="trust-hit">${value.toLocaleString()} found</span>`;
  }
  return value
    ? '<span class="trust-hit">Present</span>'
    : '<span class="trust-none">None found</span>';
}

function openTrust() {
  const trust = state.info && state.info.trust;
  if (!trust) return;
  els.trust.hidden = false;

  document.getElementById("trust-facts").innerHTML = TRUST_ROWS.map(
    ([key, label]) =>
      `<div><dt>${esc(label)}</dt><dd>${trustValueHtml(trust.detail[key])}</dd></div>`
  ).join("");

  // `badge` is capped for the status bar; the dialog has room for the lot.
  const tone = trust.hazards.length ? "trust-hit" : "trust-none";
  const summary = trust.hazards.length ? trust.hazards.join(", ") : trust.badge;
  document.getElementById("trust-summary").innerHTML =
    `<span class="${tone}">${esc(summary)}</span>`;

  const notes = [];
  if (trust.hazards.length) {
    notes.push(
      "ExcelScry only reads the cell values, so none of this has been run. Opening the workbook in Excel or LibreOffice is what would activate it."
    );
  } else if (!trust.unchecked.length) {
    notes.push(
      "ExcelScry checked every part of this package and found no macros, links or other active content."
    );
  }
  if (trust.unchecked.length) {
    const caveat = trust.hazards.length ? "" : ", so an empty result is not a clean bill of health";
    notes.push(
      `This format gives ExcelScry no way to check ${joinList(trust.unchecked)}${caveat}.`
    );
  }
  document.getElementById("trust-note").textContent = notes.join(" ");
}

function closeTrust() {
  els.trust.hidden = true;
}

// --------------------------------------------------------------------------
// Windows integration dialog
// --------------------------------------------------------------------------

function yesNo(value, yes, no) {
  return value ? `<span class="ok">${yes}</span>` : `<span class="no">${no}</span>`;
}

async function openSettings() {
  els.settings.hidden = false;
  renderUpdateFacts();
  try {
    const status = await invoke("association_status");
    document.getElementById("fact-exe").textContent = status.exePath || "unknown";
    document.getElementById("fact-user").innerHTML = yesNo(
      status.userRegistered,
      "Yes",
      "Not registered"
    );
    document.getElementById("fact-machine").innerHTML = yesNo(
      status.machineRegistered,
      "Yes (installer)",
      "No"
    );
    renderExtensionFacts(status.extensions);
    document.getElementById("assoc-note").textContent =
      status.userRegistered || status.machineRegistered
        ? status.contextMenuNote
        : "ExcelScry is not registered with the shell yet, so it is not on the context menu and cannot be chosen as the default. Use “Add to context menu”.";
  } catch (err) {
    document.getElementById("assoc-note").textContent = String(err);
  }
  await refreshVersion();
  renderUpdateFacts();
}

function handlerHtml(handler, isDefault) {
  if (!handler) return `<span class="no">no handler recorded</span>`;
  const label = esc(handler);
  return isDefault ? `<span class="ok">ExcelScry</span> (${label})` : label;
}

/**
 * One row per extension ExcelScry registers with the shell.
 *
 * The rows come from the status rather than from a fixed list in the markup, so
 * a format added to the Rust side's registration table shows up here without a
 * front-end change. Macro-enabled workbooks are the case that made this matter:
 * `.xlsm` is registered exactly like `.xlsx` and `.xls`, and a list that named
 * only two of them quietly hid a format the app does handle — the rows it fed
 * were also reading fields the backend has never sent, so they said
 * "no handler recorded" while the registry said otherwise.
 */
function renderExtensionFacts(extensions) {
  for (const row of els.assocFacts.querySelectorAll("[data-extension]")) {
    row.remove();
  }
  for (const ext of extensions || []) {
    const row = document.createElement("div");
    row.dataset.extension = ext.ext;
    const term = document.createElement("dt");
    term.textContent = `Currently opens ${ext.ext}`;
    const value = document.createElement("dd");
    value.innerHTML = handlerHtml(ext.currentHandler, ext.isDefault);
    row.append(term, value);
    els.assocFacts.append(row);
  }
}

function closeSettings() {
  els.settings.hidden = true;
}

async function withSettingsRefresh(action) {
  try {
    const message = await invoke(action);
    if (message) flash(String(message));
  } catch (err) {
    flash(String(err), true);
  }
  await openSettings();
}

// --------------------------------------------------------------------------
// Updates
// --------------------------------------------------------------------------

/** The version this build reports, so the dialog can name it without a network. */
async function refreshVersion() {
  if (state.version) return state.version;
  try {
    state.version = await invoke("app_version");
  } catch {
    state.version = "";
  }
  return state.version;
}

function renderUpdateFacts() {
  if (els.factVersion) els.factVersion.textContent = state.version || "—";
  if (els.factUpdate) {
    if (state.update) {
      els.factUpdate.innerHTML = `<span class="ok">${esc(state.update.version)}</span>`;
    } else if (state.updateError) {
      els.factUpdate.innerHTML = `<span class="no">check failed</span>`;
    } else {
      els.factUpdate.textContent = state.updateChecked ? "up to date" : "not checked yet";
    }
  }
  if (els.btnInstallUpdate) els.btnInstallUpdate.hidden = !state.update;
  renderReleaseNotes(state.update);
  // A check that came back with nothing must not leave a confirmation standing.
  if (!state.update) closeUpdateConfirm();
}

/** How much of the notes to render. The rest is on the release page. */
const NOTES_LIMIT = 8000;

/**
 * Render the manifest's notes, which are Markdown.
 *
 * The notes are the release body: they arrive with the manifest and are covered
 * by the signature over it, like every other field. That says who wrote them,
 * not that they are HTML, so they are still treated as text: every node here is
 * created with `createElement` or `createTextNode`, never `innerHTML`, which
 * means a note containing markup renders as the characters someone typed. Only
 * the shapes this project's own release notes use are understood - headings,
 * bullets, `**bold**`, `` `code` ``, links - and anything else stays literal.
 * Headings are text throughout: a heading is a label, not a place to land.
 */
function renderReleaseNotes(update) {
  if (!els.releaseNotes) return;
  const notes = update && update.notes ? String(update.notes) : "";
  els.releaseNotesBody.replaceChildren();
  els.releaseNotes.hidden = !notes.trim();
  if (els.releaseNotes.hidden) return;

  els.releaseNotesTitle.textContent = `What's new in ${update.version}`;
  const lines = (notes.length > NOTES_LIMIT ? `${notes.slice(0, NOTES_LIMIT)}\n…` : notes)
    .split(/\r?\n/);
  // The body starts with the changelog's own version heading, which the title
  // above already says.
  const first = lines.findIndex((line) => line.trim());
  if (first >= 0 && /^#{1,3}\s+\[?\d+\.\d+/.test(lines[first].trim())) lines.splice(first, 1);

  let list = null;
  for (const raw of lines) {
    const line = raw.trimEnd();
    const heading = /^#{1,6}\s+(.*)$/.exec(line.trim());
    const bullet = /^[-*]\s+(.*)$/.exec(line.trim());
    if (heading) {
      list = null;
      const headingEl = document.createElement("h5");
      headingEl.textContent = heading[1].replace(/\*\*/g, "");
      els.releaseNotesBody.append(headingEl);
    } else if (bullet) {
      if (!list) {
        list = document.createElement("ul");
        els.releaseNotesBody.append(list);
      }
      const item = document.createElement("li");
      appendNoteText(bullet[1], item);
      list.append(item);
    } else if (line.trim()) {
      list = null;
      const paragraph = document.createElement("p");
      appendNoteText(line.trim(), paragraph);
      els.releaseNotesBody.append(paragraph);
    } else {
      list = null;
    }
  }
}

/**
 * The Markdown shapes a line of notes may carry: a link, an autolink, `**bold**`
 * and `` `code` ``. Everything else is text, which is the point.
 */
const NOTE_MARKUP = /(\[[^\]\n]+\]\([^()\s]+\)|<https?:\/\/[^\s>]+>|\*\*[^*\n]+\*\*|`[^`\n]+`)/g;

/** Add one line of note text, marking up the shapes above and nothing else. */
function appendNoteText(text, parent) {
  for (const part of text.split(NOTE_MARKUP)) {
    if (!part) continue;
    const link = /^\[([^\]\n]+)\]\(([^()\s]+)\)$/.exec(part) ||
      /^<(https?:\/\/[^\s>]+)>$/.exec(part);
    if (link) {
      appendNoteLink(link[1], link[2] || link[1], part, parent);
    } else if (part.startsWith("**") && part.endsWith("**") && part.length > 4) {
      const strong = document.createElement("strong");
      strong.textContent = part.slice(2, -2);
      parent.append(strong);
    } else if (part.startsWith("`") && part.endsWith("`") && part.length > 2) {
      const code = document.createElement("code");
      code.textContent = part.slice(1, -1);
      parent.append(code);
    } else {
      parent.append(document.createTextNode(part));
    }
  }
}

/**
 * The address, if it is one this app will open; otherwise nothing.
 *
 * A release body is not this project's own text, so a link in one is a claim
 * rather than an instruction. Only the two schemes a browser is for are links:
 * `javascript:`, `data:` and `file:` stay the characters someone typed. The URL
 * parser settles the rest - what it produces is an absolute address, or nothing.
 */
function webLink(url) {
  const text = String(url || "").trim();
  try {
    const parsed = new URL(text);
    return parsed.protocol === "http:" || parsed.protocol === "https:" ? parsed.href : "";
  } catch {
    return "";
  }
}

/**
 * One link from the notes, opened in the user's own browser.
 *
 * Clicking is handled here rather than left to the browser the webview is: an
 * unhandled link would replace ExcelScry with a web page. The address is handed
 * to the Rust side, which checks it again and opens it with the shell; the
 * `href` is what the status bar shows and what makes this a link to the
 * keyboard, not a route out of the app.
 */
function appendNoteLink(label, url, source, parent) {
  const target = webLink(url);
  if (!target) {
    // Refused links are exactly the characters someone wrote, brackets and all.
    parent.append(document.createTextNode(source));
    return;
  }
  const anchor = document.createElement("a");
  anchor.className = "note-link";
  anchor.href = target;
  anchor.title = target;
  anchor.rel = "noreferrer";
  anchor.textContent = label === url ? target : label;
  const open = (event) => {
    event.preventDefault();
    invoke("open_link", { url: target }).catch((err) => flash(String(err), true));
  };
  anchor.addEventListener("click", open);
  // Middle-click asks for a second window by default; there is no second window
  // in this app, so it opens in the browser like any other click.
  anchor.addEventListener("auxclick", open);
  parent.append(anchor);
}

/** How many of the release's own points the confirmation shows. */
const SUMMARY_POINTS = 3;

/** The longest a single summary line may run before it is cut. */
const SUMMARY_WIDTH = 160;

/**
 * The release's notes, cut down to the few lines the confirmation shows.
 *
 * The notes are the changelog entry followed by the commit list, and the commit
 * list says nothing about what an update is for, so the summary stops where the
 * notes start listing commits. Each point is the bullet's own opening - a
 * changelog entry leads with its conclusion in bold - so what the confirmation
 * shows is the release's words rather than anyone's paraphrase of them.
 */
function noteSummary(notes) {
  const points = [];
  let remainder = 0;
  let listing = false;
  for (const raw of String(notes || "").split(/\r?\n/)) {
    const line = raw.trim();
    if (/^#{1,6}\s/.test(line)) {
      listing = /^#{1,6}\s+Commits\b/i.test(line);
      continue;
    }
    const bullet = /^[-*]\s+(.*)$/.exec(line);
    if (!bullet || listing) continue;
    if (points.length < SUMMARY_POINTS) points.push(summarisePoint(bullet[1]));
    else remainder += 1;
  }
  return { points, remainder };
}

/**
 * One bullet's opening: its bold conclusion, or its first sentence.
 *
 * A summary line is cut at [`SUMMARY_WIDTH`], which would land in the middle of
 * a URL, so a link here is its label: the full notes, with links that work, are
 * one click away in **Settings › Updates**.
 */
function summarisePoint(text) {
  const plain = text
    .replace(/\[([^\]\n]+)\]\([^()\s]+\)/g, "$1")
    .replace(/<(https?:\/\/[^\s>]+)>/g, "$1");
  const lead = /^\*\*([^*]+)\*\*/.exec(plain);
  if (lead && /[.!?]$/.test(lead[1].trim())) return `**${lead[1].trim()}**`;
  const sentence = /^(.+?[.!?])(\s|$)/.exec(plain);
  const line = (sentence ? sentence[1] : plain).trim();
  return line.length > SUMMARY_WIDTH ? `${line.slice(0, SUMMARY_WIDTH - 1).trimEnd()}…` : line;
}

/** The status-bar pill, which exists only while there is something to install. */
function renderUpdatePill() {
  const info = state.update;
  els.updatePill.hidden = !info;
  if (!info) return;
  els.updatePillText.textContent = `Update to ${info.version}`;
  els.updatePill.title = state.version
    ? `ExcelScry ${state.version} → ${info.version} — click for what's new, then install`
    : `ExcelScry ${info.version} is available — click for what's new, then install`;
}

/**
 * Ask the release manifest what the latest version is.
 *
 * `silent` is for the check that runs at start-up: a viewer opening a
 * spreadsheet should not report that it could not reach GitHub.
 */
async function checkForUpdate({ silent = false } = {}) {
  if (state.checkingUpdate) return;
  state.checkingUpdate = true;
  if (els.btnCheckUpdates) els.btnCheckUpdates.disabled = true;
  try {
    await refreshVersion();
    const info = await invoke("check_for_update");
    state.update = info || null;
    state.updateChecked = true;
    state.updateError = "";
    if (els.updateNote) {
      els.updateNote.textContent = info
        ? `ExcelScry ${info.version} is available. It will be downloaded, checked against the release signature, and installed for you.`
        : "This build is the latest release.";
    }
    if (!silent) flash(info ? `ExcelScry ${info.version} is available` : "ExcelScry is up to date");
  } catch (err) {
    state.update = null;
    state.updateError = String(err);
    if (els.updateNote) els.updateNote.textContent = `Could not check for updates: ${String(err)}`;
    if (!silent) flash(`Could not check for updates: ${String(err)}`, true);
  } finally {
    state.checkingUpdate = false;
    if (els.btnCheckUpdates) els.btnCheckUpdates.disabled = false;
    renderUpdatePill();
    renderUpdateFacts();
  }
}

/**
 * Ask before installing, which is all the pill and the Settings button do now.
 *
 * An update closes the app, replaces it and starts it again, and the release's
 * own notes are the thing worth reading before that happens - so the click asks
 * first, showing the release's opening points rather than a paraphrase of them.
 * Nothing is downloaded until the question is answered.
 */
function requestUpdate() {
  if (!state.update || state.updating || !els.updateConfirm.hidden) return;
  const version = state.update.version;
  const { points, remainder } = noteSummary(state.update.notes);

  els.updateSummaryTitle.textContent = `What's new in ${version}`;
  els.updateSummary.replaceChildren();
  for (const point of points) {
    const item = document.createElement("li");
    appendNoteText(point, item);
    els.updateSummary.append(item);
  }
  els.updateSummarySection.hidden = !points.length;
  els.updateSummaryMore.textContent = remainder
    ? `…and ${remainder} more in Settings › Updates.`
    : "The full notes are in Settings › Updates.";

  document.getElementById("update-confirm-title").textContent = `Install ExcelScry ${version}?`;
  els.updateConfirm.hidden = false;
  // Focus lands on the button that changes nothing: installing takes a
  // deliberate second click, not the Enter key that opened this.
  document.getElementById("btn-update-cancel").focus();
}

function closeUpdateConfirm() {
  els.updateConfirm.hidden = true;
}

/**
 * Download, verify and install, then let the installer take over: on Windows
 * it closes this app and starts the new version itself.
 */
async function installUpdate() {
  if (!state.update || state.updating) return;
  state.updating = true;
  els.updatePill.disabled = true;
  if (els.btnInstallUpdate) els.btnInstallUpdate.disabled = true;
  flash(`Downloading ExcelScry ${state.update.version}…`);
  try {
    await invoke("install_update");
    // Only reached if the installer did not take the app down with it.
    flash("The installer is running — ExcelScry restarts when it finishes.");
  } catch (err) {
    state.updating = false;
    els.updatePill.disabled = false;
    if (els.btnInstallUpdate) els.btnInstallUpdate.disabled = false;
    flash(String(err), true);
  }
}

// --------------------------------------------------------------------------
// Wiring
// --------------------------------------------------------------------------

function initEvents() {
  els.btnOpen.addEventListener("click", chooseFile);
  els.btnOpen2.addEventListener("click", chooseFile);
  els.welcomeRecentList.addEventListener("click", (event) => {
    const button = event.target.closest("[data-path]");
    if (!button || state.busy) return;
    openPath(button.dataset.path);
  });
  els.btnExport.addEventListener("click", exportOds);

  // The view menu: a click toggles it, a click anywhere else dismisses it, and
  // the arrow keys walk it once it is open.
  els.btnView.addEventListener("click", toggleViewMenu);
  els.btnView.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown" && els.viewMenu.hidden) {
      event.preventDefault();
      openViewMenu();
    }
  });
  els.viewMenu.addEventListener("keydown", (event) => {
    if (event.key === "ArrowDown") {
      event.preventDefault();
      stepViewMenu(1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      stepViewMenu(-1);
    } else if (event.key === "Home" || event.key === "End") {
      event.preventDefault();
      const items = viewMenuItems().filter((item) => !item.disabled);
      const target = event.key === "Home" ? items[0] : items[items.length - 1];
      if (target) target.focus();
    }
  });
  document.addEventListener("click", (event) => {
    if (els.viewMenu.hidden) return;
    if (event.target instanceof Element && event.target.closest(".menu-anchor")) return;
    closeViewMenu();
  });
  els.menuHidden.addEventListener("click", () => {
    closeViewMenu();
    setShowHidden(!state.showHidden);
  });
  els.menuData.addEventListener("click", jumpToData);
  els.menuScroll.addEventListener("click", jumpToScrollingCell);

  // The readout's chain button opens the panel; while it is open, pressing it
  // again re-reads the chain around the cell in front.
  els.btnTrace.addEventListener("click", () => {
    if (els.deps.hidden) openDependencies();
    else refreshDependencies();
  });
  els.btnDepsClose.addEventListener("click", closeDependencies);
  els.depsBody.addEventListener("click", (event) => {
    const button = event.target.closest("[data-row]");
    if (!button || state.busy) return;
    select(Number(button.dataset.row), Number(button.dataset.col), true);
  });
  // Reload keeps the text dialect: re-reading the file should not silently
  // undo the delimiter and encoding the user picked for it.
  els.btnReload.addEventListener("click", () => {
    if (state.info) openPath(state.info.path, state.text);
  });
  els.btnSettings.addEventListener("click", openSettings);

  els.updatePill.addEventListener("click", requestUpdate);
  els.btnCheckUpdates.addEventListener("click", () => checkForUpdate());
  els.btnInstallUpdate.addEventListener("click", requestUpdate);
  document.getElementById("btn-update-cancel").addEventListener("click", closeUpdateConfirm);
  document.getElementById("btn-update-cancel-x").addEventListener("click", closeUpdateConfirm);
  document.getElementById("btn-update-go").addEventListener("click", () => {
    closeUpdateConfirm();
    installUpdate();
  });
  els.updateConfirm.addEventListener("click", (event) => {
    if (event.target.dataset.close) closeUpdateConfirm();
  });
  listen("update-progress", (event) => {
    const progress = event.payload || {};
    const version = state.update ? state.update.version : "the update";
    const size = progress.total ? ` (${formatBytes(progress.total)})` : "";
    const share = progress.total
      ? ` ${Math.round((progress.downloaded / progress.total) * 100)}%`
      : ` ${formatBytes(progress.downloaded)}`;
    flash(`Downloading ExcelScry ${version}${size}…${share}`);
  });
  document.getElementById("btn-settings-close").addEventListener("click", closeSettings);
  els.settings.addEventListener("click", (event) => {
    if (event.target.dataset.close) closeSettings();
  });

  els.trustBadge.addEventListener("click", openTrust);
  document.getElementById("btn-trust-close").addEventListener("click", closeTrust);
  els.trust.addEventListener("click", (event) => {
    if (event.target.dataset.close) closeTrust();
  });

  document
    .getElementById("btn-assoc-install")
    .addEventListener("click", () => withSettingsRefresh("install_associations"));
  document
    .getElementById("btn-assoc-remove")
    .addEventListener("click", () => withSettingsRefresh("remove_associations"));
  document
    .getElementById("btn-assoc-default")
    .addEventListener("click", () => withSettingsRefresh("open_default_apps_settings"));

  els.tabs.addEventListener("click", (event) => {
    const button = event.target.closest("[data-sheet]");
    if (button) setSheet(Number(button.dataset.sheet));
  });

  els.textDelimiter.addEventListener("change", applyTextDialect);
  els.textEncoding.addEventListener("change", applyTextDialect);
  els.textHeader.addEventListener("change", applyHeaderInput);
  els.btnSaveUtf8.addEventListener("click", saveUtf8Copy);
  els.btnFileOrder.addEventListener("click", restoreFileOrder);
  // Any row number can be the header: pointing at the row is how a table with
  // a title above its column names is fixed. Clicking the pin again releases it.
  // A seam stands where a hidden run was collapsed away: clicking it shows the
  // rows or columns the sheet hides, which is quicker than hunting for them.
  const revealSeam = (seam, axis) => {
    const first = Number(String(seam.dataset.run).split(":")[0]);
    setShowHidden(true);
    // Land on the run that was clicked rather than wherever the view was.
    if (!Number.isInteger(first)) return;
    if (axis === "row") select(first, state.selection.col, { scrollIntoView: true });
    else select(state.selection.row, first, { scrollIntoView: true });
  };
  els.rows.addEventListener("click", (event) => {
    const seam = event.target.closest("[data-seam]");
    if (seam) {
      revealSeam(seam, "row");
      return;
    }
    const head = event.target.closest(".rowhead");
    if (!head || !isTextTable()) return;
    const row = Number(head.dataset.row);
    setHeaderRow(row === state.headerRow ? null : row);
  });
  els.cols.addEventListener("click", (event) => {
    // The press that ended a drag is not a press on the head, and a press on a
    // column's edge is about its width rather than its order.
    if (swallowHeadClick) {
      swallowHeadClick = false;
      return;
    }
    if (event.target.closest("[data-grip]")) return;
    const seam = event.target.closest("[data-seam]");
    if (seam) {
      revealSeam(seam, "col");
      return;
    }
    const head = event.target.closest("[data-sort]");
    if (head) sortByColumn(Number(head.dataset.sort));
  });
  // A column's width is dragged from the right edge of its head. The frozen
  // columns keep their own head strip, and it drags the same way.
  for (const strip of [els.cols, els.freezeColsHead]) {
    strip.addEventListener("pointerdown", startColumnResize);
    strip.addEventListener("dblclick", resetColumnWidth);
  }
  els.btnHidden.addEventListener("click", () => setShowHidden(!state.showHidden));

  els.body.addEventListener(
    "scroll",
    () => {
      scheduleRender();
      scheduleViewSave();
    },
    { passive: true }
  );
  window.addEventListener("resize", () => {
    layoutSheet();
    scheduleRender();
  });

  // Hit-test through the prefix sums: a click lands in the column whose width
  // it falls in, not at a fixed 112-pixel stride. The frozen panes share the
  // same mapping, with their own fixed edge for the frozen band.
  els.cells.addEventListener("mousedown", selectAtPoint);
  for (const pane of [els.freezeTop, els.freezeLeft, els.freezeCorner]) {
    pane.addEventListener("mousedown", selectAtPoint);
    pane.addEventListener("wheel", scrollFromPane, { passive: false });
  }

  document.addEventListener("keydown", onKeyDown);

  listen("open-file", (event) => {
    if (event.payload) openPath(String(event.payload));
  });

  // Native file drop. Tauri intercepts these at the window level, so the
  // ordinary HTML drag events never fire.
  const webview = T.webview && typeof T.webview.getCurrentWebview === "function"
    ? T.webview.getCurrentWebview()
    : null;
  if (webview && typeof webview.onDragDropEvent === "function") {
    webview.onDragDropEvent((event) => {
      const payload = event.payload || {};
      if (payload.type === "over") {
        els.dropOverlay.hidden = false;
      } else if (payload.type === "drop") {
        els.dropOverlay.hidden = true;
        const first = (payload.paths || [])[0];
        if (first) openPath(first);
      } else {
        els.dropOverlay.hidden = true;
      }
    });
  }
}

function onKeyDown(event) {
  // A field in the chrome owns its keys: typing a row number must not also
  // move the selection, and Ctrl+C in one must copy its text.
  const field = event.target instanceof HTMLElement && event.target.closest("input, select, textarea");
  if (event.key === "Escape") {
    // The menu hands the focus back to its button only when it had it: Escape
    // pressed while reading the grid should not pull the focus off it.
    closeViewMenu({ focusButton: els.viewMenu.contains(document.activeElement) });
    closeUpdateConfirm();
    closeSettings();
    closeTrust();
    closeDependencies();
    // Escape leaves a field without committing it: the header goes back to the
    // row that is actually pinned, and the blur that follows has nothing to
    // change.
    if (field instanceof HTMLElement) {
      event.preventDefault();
      if (field === els.textHeader) {
        const row = pinnedRow();
        els.textHeader.value = row === null ? "" : String(row + 1);
      }
      field.blur();
    }
    return;
  }
  if (field) return;
  if (!state.info || !els.settings.hidden || !els.trust.hidden) return;
  if (event.ctrlKey && event.key.toLowerCase() === "o") {
    event.preventDefault();
    chooseFile();
    return;
  }
  // Ctrl+Shift+C gives what the cell is calculated from rather than what it
  // shows, which is what a formula bar wants pasted into it.
  if (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === "c") {
    event.preventDefault();
    copySelection({ formula: true });
    return;
  }
  // The two jumps the view menu offers, reachable without opening it: where the
  // data begins, and where the part of the sheet that scrolls begins.
  if (event.ctrlKey && event.shiftKey && event.key.toLowerCase() === "j") {
    event.preventDefault();
    jumpToData();
    return;
  }
  if (event.ctrlKey && event.shiftKey && event.key === "Home") {
    event.preventDefault();
    jumpToScrollingCell();
    return;
  }
  if (event.ctrlKey && event.key.toLowerCase() === "c") {
    copySelection();
    return;
  }
  if (event.ctrlKey && event.key === "Home") {
    event.preventDefault();
    select(0, 0, { scrollIntoView: true });
    return;
  }

  const { row, col } = state.selection;
  // A page is however many rows fit between the top of the body and its
  // bottom now, which varies with the heights the sheet gave them.
  const frozenHeight = frozenSize().height;
  const pageRows = Math.max(
    1,
    rowAtY(els.body.scrollTop + els.body.clientHeight) -
      rowAtY(els.body.scrollTop + frozenHeight) -
      1
  );
  // An arrow key leaves a merged region as a whole, rather than landing on a
  // cell the region hides and being snapped back to its anchor.
  const step = (dr, dc) => {
    const span = mergeAt(row, col) || { rows: 1, cols: 1 };
    return [
      dr > 0 ? row + span.rows : dr < 0 ? row - 1 : row,
      dc > 0 ? col + span.cols : dc < 0 ? col - 1 : col,
    ];
  };
  const movements = {
    ArrowUp: step(-1, 0),
    ArrowDown: step(1, 0),
    ArrowLeft: step(0, -1),
    ArrowRight: step(0, 1),
    PageUp: [row - pageRows, col],
    PageDown: [row + pageRows, col],
    Home: [row, 0],
    End: [row, Math.max(realCols() - 1, 0)],
  };
  const move = movements[event.key];
  if (!move) return;
  event.preventDefault();
  // A collapsed run is stepped over in the direction of travel, so an arrow
  // key never lands on a row that is not on screen.
  if (!state.showHidden) {
    move[0] = visibleIndex(geom.hiddenRowRuns, move[0], realRows(), Math.sign(move[0] - row) || 1);
    move[1] = visibleIndex(geom.hiddenColRuns, move[1], realCols(), Math.sign(move[1] - col) || 1);
  }
  select(move[0], move[1], { scrollIntoView: true });
}

async function init() {
  initEvents();
  await refreshRecent();
  try {
    const path = await invoke("initial_file");
    if (path) await openPath(path);
  } catch (err) {
    flash(String(err), true);
  }
  // One quiet check at start-up. If a newer release exists the status bar
  // offers it; nothing is downloaded until that is clicked, and being offline
  // is not worth a message over someone's spreadsheet.
  await refreshVersion();
  checkForUpdate({ silent: true });
}

init();
