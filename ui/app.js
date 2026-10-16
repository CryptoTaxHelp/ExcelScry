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
  btnOpen: document.getElementById("btn-open"),
  btnOpen2: document.getElementById("btn-open-2"),
  btnExport: document.getElementById("btn-export"),
  btnReload: document.getElementById("btn-reload"),
  btnSettings: document.getElementById("btn-settings"),
  settings: document.getElementById("settings"),
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
const realRows = () => (currentSheet() ? currentSheet().rows : 0);
const realCols = () => (currentSheet() ? currentSheet().cols : 0);
// Always keep one row/column of header geometry, even for an empty sheet.
const gridRows = () => Math.max(realRows(), 1);
const gridCols = () => Math.max(realCols(), 1);

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
  const firstCol = Math.floor(scrollLeft / COL_W);
  const lastCol = Math.ceil((scrollLeft + Math.max(clientWidth, 1)) / COL_W);
  const firstRow = Math.floor(scrollTop / ROW_H);
  const lastRow = Math.ceil((scrollTop + Math.max(clientHeight, 1)) / ROW_H);
  return {
    firstCol: clamp(firstCol, 0, Math.max(gridCols() - 1, 0)),
    lastCol: clamp(lastCol, 1, gridCols()),
    firstRow: clamp(firstRow, 0, Math.max(gridRows() - 1, 0)),
    lastRow: clamp(lastRow, 1, gridRows()),
    scrollLeft,
    scrollTop,
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
      `<div class="colhead${active}" style="left:${col * COL_W}px;width:${COL_W}px">${columnName(col)}</div>`
    );
  }

  const rowParts = [];
  for (let row = view.firstRow; row < view.lastRow; row++) {
    const active = row === state.selection.row ? " is-active" : "";
    rowParts.push(
      `<div class="rowhead${active}" style="top:${row * ROW_H}px;height:${ROW_H}px">${row + 1}</div>`
    );
  }

  els.cols.innerHTML = colParts.join("");
  els.rows.innerHTML = rowParts.join("");

  const cellParts = [];
  for (let row = view.firstRow; row < view.lastRow; row++) {
    const top = row * ROW_H;
    for (let col = view.firstCol; col < view.lastCol; col++) {
      const left = col * COL_W;
      const cell = cellAt(row, col);
      if (cell === undefined) {
        cellParts.push(
          `<div class="cell pending" style="left:${left}px;top:${top}px;width:${COL_W}px;height:${ROW_H}px"></div>`
        );
        continue;
      }
      if (cell === null) continue;
      const text = cell[1];
      if (text === "") continue;
      const title = text.length > 24 ? ` title="${esc(text)}"` : "";
      cellParts.push(
        `<div class="cell k-${cell[0]}"${title} style="left:${left}px;top:${top}px;width:${COL_W}px;height:${ROW_H}px">${esc(text)}</div>`
      );
    }
  }

  cellParts.push(
    `<div class="selection" style="left:${state.selection.col * COL_W}px;top:${
      state.selection.row * ROW_H
    }px;width:${COL_W}px;height:${ROW_H}px"></div>`
  );
  els.cells.innerHTML = cellParts.join("");
}

function layoutSheet() {
  els.cells.style.width = `${gridCols() * COL_W}px`;
  els.cells.style.height = `${gridRows() * ROW_H}px`;
  els.cols.style.width = `${gridCols() * COL_W}px`;
  els.rows.style.height = `${gridRows() * ROW_H}px`;
}

// --------------------------------------------------------------------------
// Selection
// --------------------------------------------------------------------------

function select(row, col, { scrollIntoView = false } = {}) {
  state.selection.row = clamp(row, 0, Math.max(realRows() - 1, 0));
  state.selection.col = clamp(col, 0, Math.max(realCols() - 1, 0));
  updateReadout();

  if (scrollIntoView) {
    const body = els.body;
    const x = state.selection.col * COL_W;
    const y = state.selection.row * ROW_H;
    if (x < body.scrollLeft) body.scrollLeft = x;
    else if (x + COL_W > body.scrollLeft + body.clientWidth)
      body.scrollLeft = x + COL_W - body.clientWidth;
    if (y < body.scrollTop) body.scrollTop = y;
    else if (y + ROW_H > body.scrollTop + body.clientHeight)
      body.scrollTop = y + ROW_H - body.clientHeight;
  }
  scheduleRender();
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

async function openPath(path) {
  if (!path) return;
  setBusy(true, "Reading…");
  try {
    const info = await invoke("open_workbook", { path });
    state.info = info;
    state.sheet = 0;
    resetTiles();
    state.selection.row = 0;
    state.selection.col = 0;

    els.welcome.hidden = true;
    els.viewer.hidden = false;
    els.body.scrollTop = 0;
    els.body.scrollLeft = 0;

    buildTabs();
    layoutSheet();
    updateStatus();
    updateReadout();
    scheduleRender();

    if (info.truncated) {
      flash("Very large workbook: only the first part of it was loaded.", true);
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
  layoutSheet();
  updateStatus();
  updateReadout();
  scheduleRender();
}

function updateStatus() {
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
// Windows integration dialog
// --------------------------------------------------------------------------

function yesNo(value, yes, no) {
  return value ? `<span class="ok">${yes}</span>` : `<span class="no">${no}</span>`;
}

async function openSettings() {
  els.settings.hidden = false;
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
    document.getElementById("fact-xlsx").innerHTML = handlerHtml(
      status.currentXlsxHandler,
      status.isDefaultXlsx
    );
    document.getElementById("fact-xls").innerHTML = handlerHtml(
      status.currentXlsHandler,
      status.isDefaultXls
    );
    document.getElementById("assoc-note").textContent =
      status.userRegistered || status.machineRegistered
        ? status.contextMenuNote
        : "ExcelScry is not registered with the shell yet, so it is not on the context menu and cannot be chosen as the default. Use “Add to context menu”.";
  } catch (err) {
    document.getElementById("assoc-note").textContent = String(err);
  }
}

function handlerHtml(handler, isDefault) {
  if (!handler) return `<span class="no">no handler recorded</span>`;
  const label = esc(handler);
  return isDefault ? `<span class="ok">ExcelScry</span> (${label})` : label;
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
// Wiring
// --------------------------------------------------------------------------

function initEvents() {
  els.btnOpen.addEventListener("click", chooseFile);
  els.btnOpen2.addEventListener("click", chooseFile);
  els.btnExport.addEventListener("click", exportOds);
  els.btnReload.addEventListener("click", () => {
    if (state.info) openPath(state.info.path);
  });
  els.btnSettings.addEventListener("click", openSettings);
  document.getElementById("btn-settings-close").addEventListener("click", closeSettings);
  els.settings.addEventListener("click", (event) => {
    if (event.target.dataset.close) closeSettings();
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

  els.body.addEventListener("scroll", scheduleRender, { passive: true });
  window.addEventListener("resize", () => {
    layoutSheet();
    scheduleRender();
  });

  els.cells.addEventListener("mousedown", (event) => {
    if (!state.info) return;
    const rect = els.cells.getBoundingClientRect();
    const col = Math.floor((event.clientX - rect.left) / COL_W);
    const row = Math.floor((event.clientY - rect.top) / ROW_H);
    select(row, col);
  });

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
  if (event.key === "Escape") {
    closeSettings();
    return;
  }
  if (!state.info || !els.settings.hidden) return;
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
  const pageRows = Math.max(1, Math.floor(els.body.clientHeight / ROW_H) - 1);
  const movements = {
    ArrowUp: [row - 1, col],
    ArrowDown: [row + 1, col],
    ArrowLeft: [row, col - 1],
    ArrowRight: [row, col + 1],
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
}

init();
