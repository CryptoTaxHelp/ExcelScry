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
function fillAxis(edges, count, sizes, fallback) {
  let next = 0;
  for (let index = 0; index < count; index += 1) {
    while (next < sizes.length && sizes[next][0] < index) next += 1;
    let size = fallback;
    if (next < sizes.length && sizes[next][0] === index) {
      const value = sizes[next][1];
      if (Number.isFinite(value) && value > 0) size = value;
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
};

const els = {
  welcome: document.getElementById("welcome"),
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
  btnSettings: document.getElementById("btn-settings"),
  settings: document.getElementById("settings"),
  textbar: document.getElementById("textbar"),
  textDelimiter: document.getElementById("text-delimiter"),
  textEncoding: document.getElementById("text-encoding"),
  textHeader: document.getElementById("text-header"),
  textDetected: document.getElementById("text-detected"),
  textHint: document.getElementById("text-hint"),
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
  document.body.style.cursor = busy ? "progress" : "";
  if (message) flash(message);
}

// --------------------------------------------------------------------------
// Sheet geometry
// --------------------------------------------------------------------------

const currentSheet = () => (state.info ? state.info.sheets[state.sheet] : null);
/** A delimited text table: a record can be pinned as its header. */
const isTextTable = () => Boolean(state.info && state.info.text);
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
  const widths = sheet && sheet.colWidths ? sheet.colWidths : [];
  const heights = sheet && sheet.rowHeights ? sheet.rowHeights : [];
  const defaultCol = Number.isFinite(sheet && sheet.defaultColWidth)
    ? sheet.defaultColWidth
    : COL_W;
  const defaultRow = Number.isFinite(sheet && sheet.defaultRowHeight)
    ? sheet.defaultRowHeight
    : ROW_H;

  const xs = new Float64Array(cols + 1);
  const ys = new Float64Array(rows + 1);
  fillAxis(xs, cols, widths, defaultCol);
  fillAxis(ys, rows, heights, defaultRow);
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

const tileKey = (tr, tc) => `${tr}:${tc}`;
const maxTileRow = () => Math.max(0, Math.ceil(realRows() / TILE_ROWS) - 1);
const maxTileCol = () => Math.max(0, Math.ceil(realCols() / TILE_COLS) - 1);

function resetTiles() {
  state.tiles.clear();
  state.inflight.clear();
  state.lru.length = 0;
  state.generation += 1;
}

/** Returns [kind, display], null for a known-empty cell, undefined if unknown. */
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
    const active = col === state.selection.col ? " is-active" : "";
    colParts.push(
      `<div class="colhead${active}" style="left:${geom.xs[col]}px;width:${colWidth(
        col
      )}px">${columnName(col)}</div>`
    );
  }

  // The header row is named in the strip, so "which row is pinned" can be
  // answered without scrolling to it, and a click on any number moves the pin.
  const header = pinnedRow();
  const pinHint = isTextTable() ? ' title="Click to pin this row as the header"' : "";
  const rowParts = [];
  for (let row = view.firstRow; row < view.lastRow; row++) {
    const active = row === state.selection.row ? " is-active" : "";
    const pinned = row === header;
    const title = pinned
      ? ' title="Header row — click to unpin it"'
      : isTextTable()
        ? pinHint
        : "";
    rowParts.push(
      `<div class="rowhead${active}${pinned ? " is-pinned" : ""}"${title} data-row="${row}" style="top:${
        geom.ys[row]
      }px;height:${rowHeight(row)}px">${row + 1}</div>`
    );
  }

  els.cols.innerHTML = colParts.join("");
  els.rows.innerHTML = rowParts.join("");

  const cellParts = [];
  for (let row = view.firstRow; row < view.lastRow; row++) {
    for (let col = view.firstCol; col < view.lastCol; col++) {
      cellParts.push(cellMarkup(cellAt(row, col), row, col));
    }
  }

  const box = selectionBox();
  cellParts.push(
    `<div class="selection" style="left:${box.left}px;top:${box.top}px;width:${box.width}px;height:${box.height}px"></div>`
  );
  els.cells.innerHTML = cellParts.join("");

  renderFrozenRow(view);
  renderFreezePanes(view);
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
  if (text === "") return "";
  const title = text.length > 24 ? ` title="${esc(text)}"` : "";
  return `<div class="cell k-${cell[0]}"${title} style="${box}">${esc(text)}</div>`;
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
  const activeCol = state.selection.col;

  const colParts = [];
  const lastCol = Math.min(freeze.cols, colAtX(els.body.clientWidth) + 1);
  for (let col = 0; col < lastCol; col++) {
    const active = col === activeCol ? " is-active" : "";
    colParts.push(
      `<div class="colhead${active}" style="left:${geom.xs[col]}px;width:${colWidth(
        col
      )}px">${columnName(col)}</div>`
    );
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
// Selection
// --------------------------------------------------------------------------

function select(row, col, { scrollIntoView = false } = {}) {
  // Every cell of a merged region is the same cell: selecting a covered one
  // selects the region's anchor, so the readout and the outline agree.
  const origin = mergeOrigin(row, col);
  if (origin) {
    row = origin.row;
    col = origin.col;
  }
  state.selection.row = clamp(row, 0, Math.max(realRows() - 1, 0));
  state.selection.col = clamp(col, 0, Math.max(realCols() - 1, 0));
  updateReadout();

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

function updateReadout() {
  const { row, col } = state.selection;
  els.readoutRef.textContent = `${columnName(col)}${row + 1}`;
  const cell = cellAt(row, col);
  if (cell === undefined) {
    els.readoutKind.textContent = "";
    els.readoutValue.textContent = "…";
    return;
  }
  if (cell === null) {
    els.readoutKind.textContent = "empty";
    els.readoutValue.textContent = "";
    return;
  }
  els.readoutKind.textContent = KIND_LABEL[cell[0]] || "";
  els.readoutValue.textContent = cell[1];
}

function copySelection() {
  const { row, col } = state.selection;
  const cell = cellAt(row, col);
  if (!cell) return;
  const text = cell[1];
  const done = () => flash(`Copied ${columnName(col)}${row + 1}`);

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
  return Number.isInteger(previous) && previous < rows ? previous : 0;
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
        state.text.encoding = ENCODING_VALUES[info.text.encoding] || "";
      }
    }
    state.headerRow = headerAfterOpen(info, previousHeader);
    state.sheet = 0;
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
    updateStatus();
    updateTextBar();
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
  state.info = null;
  state.text = { delimiter: "", encoding: "" };
  state.headerRow = null;
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
  resetTiles();
  state.selection.row = 0;
  state.selection.col = 0;
  els.body.scrollTop = 0;
  els.body.scrollLeft = 0;
  buildTabs();
  rebuildGeometry();
  layoutSheet();
  updateStatus();
  updateReadout();
  scheduleRender();
}

function updateStatus() {
  updateTrustBadge();
  updateStatusNote();
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
  els.statusCells.textContent = bits.join("  ·  ");
}

// --------------------------------------------------------------------------
// Delimited text: the delimiter and the encoding, which a file cannot say
// --------------------------------------------------------------------------

/** The select value for an encoding name the Rust side reports. */
const ENCODING_VALUES = {
  "UTF-8": "utf-8",
  "Windows-1252": "windows-1252",
  "UTF-16 LE": "utf-16le",
  "UTF-16 BE": "utf-16be",
};

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
    return;
  }
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
  updateTextBar();
  layoutSheet();
  scheduleRender();
  if (next === null) flash("Header unpinned.");
  else flash(`Row ${(next + 1).toLocaleString()} is the header, pinned while the rest scrolls.`);
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
  els.btnExport.addEventListener("click", exportOds);
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
  // Any row number can be the header: pointing at the row is how a table with
  // a title above its column names is fixed. Clicking the pin again releases it.
  els.rows.addEventListener("click", (event) => {
    const head = event.target.closest(".rowhead");
    if (!head || !isTextTable()) return;
    const row = Number(head.dataset.row);
    setHeaderRow(row === state.headerRow ? null : row);
  });

  els.body.addEventListener("scroll", scheduleRender, { passive: true });
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
    closeUpdateConfirm();
    closeSettings();
    closeTrust();
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
  select(move[0], move[1], { scrollIntoView: true });
}

async function init() {
  initEvents();
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
