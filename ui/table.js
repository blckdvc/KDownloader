// macOS draws the app's own title bar over the window and labels shortcuts
// with ⌘; Windows and Linux have the system's title bar and Ctrl.
const IS_MAC = /Mac/.test(navigator.userAgent);
document.documentElement.classList.add(IS_MAC ? "mac" : "not-mac");
const keyLabel = (k) => (IS_MAC ? k : k.replace(/⇧/g, "Shift+").replace(/⌥/g, "Alt+").replace(/⌘/g, "Ctrl+").replace(/⌫/g, "Del"));
if (!IS_MAC) document.querySelectorAll("kbd").forEach((k) => (k.textContent = keyLabel(k.textContent)));

// One-time move of saved preferences from the app's former name.
try {
  for (const key of Object.keys(localStorage)) {
    if (!key.startsWith("pulsar-")) continue;
    localStorage.setItem(key.replace("pulsar-", "kdownloader-"), localStorage.getItem(key));
    localStorage.removeItem(key);
  }
} catch {}

// What the Torrent and Direct Download tabs share: efficient table updates,
// column resizing, Finder-style selection and the context menu.
// Touching only what changed keeps selections, tooltips and clicks intact across
// the once-a-second refresh.
const lastHtml = new WeakMap();
function setHtml(el, html) {
  if (lastHtml.get(el) === html) return;
  lastHtml.set(el, html);
  el.innerHTML = html;
}
// While the window is hidden, polls run one tick in five: the Dock speed comes
// from Rust, and the lists catch up as soon as the window shows again.
function idleTick(counter) {
  if (!document.hidden) return false;
  counter.n = (counter.n + 1) % 5;
  return counter.n !== 0;
}
function setText(el, text) {
  if (el.textContent !== text) el.textContent = text;
}
// Replaces only the entries that changed, so text can be selected and copied.
function patchList(container, html) {
  const holder = document.createElement("div");
  holder.innerHTML = html;
  const next = holder.firstElementChild, current = container.firstElementChild;
  if (current?.tagName !== "DL" || current.children.length !== next.children.length) {
    lastHtml.delete(container);
    container.replaceChildren(next);
    return;
  }
  [...next.children].forEach((entry, i) => {
    if (current.children[i].outerHTML !== entry.outerHTML) current.children[i].replaceWith(entry);
  });
}
// rows: [{ id, cls, html }] in display order.
function patchRows(tbody, rows) {
  const existing = new Map([...tbody.children].map((tr) => [tr.dataset.id, tr]));
  rows.forEach((row, i) => {
    const key = String(row.id);
    let tr = existing.get(key);
    if (tr) existing.delete(key);
    else { tr = document.createElement("tr"); tr.dataset.id = key; }
    if (tr.className !== row.cls) tr.className = row.cls;
    setHtml(tr, row.html);
    if (tbody.children[i] !== tr) tbody.insertBefore(tr, tbody.children[i] || null);
  });
  existing.forEach((tr) => tr.remove());
}

// Fixed columns with drag handles; the Name column takes the rest, down to minName.
function resizableColumns({ storageKey, defaults, table, headers, headerKey, column, minName }) {
  let widths = { ...defaults };
  try { widths = { ...defaults, ...JSON.parse(localStorage.getItem(storageKey) || "{}") }; } catch (e) {}
  const save = () => { try { localStorage.setItem(storageKey, JSON.stringify(widths)); } catch (e) {} };
  const fit = () => {
    const fixed = Object.keys(defaults).reduce((sum, key) => sum + widths[key], 0);
    table.style.minWidth = `${fixed + minName}px`;
  };
  const setWidth = (key, width) => { widths[key] = width; column(key).style.width = `${width}px`; fit(); };
  for (const key in defaults) setWidth(key, widths[key]);

  headers.forEach((th) => {
    const key = headerKey(th);
    if (!(key in defaults)) return;
    const handle = document.createElement("div");
    handle.className = "col-resize";
    handle.title = "Drag to resize";
    th.appendChild(handle);
    handle.addEventListener("click", (e) => e.stopPropagation()); // don't trigger sort
    handle.addEventListener("pointerdown", (e) => {
      e.preventDefault();
      e.stopPropagation();
      const startX = e.clientX, startW = column(key).getBoundingClientRect().width;
      handle.classList.add("drag");
      const move = (ev) => setWidth(key, Math.max(44, Math.round(startW + ev.clientX - startX)));
      const up = () => {
        handle.classList.remove("drag");
        document.removeEventListener("pointermove", move);
        document.removeEventListener("pointerup", up);
        save();
      };
      document.addEventListener("pointermove", move);
      document.addEventListener("pointerup", up);
    });
    handle.addEventListener("dblclick", (e) => {
      e.stopPropagation();
      setWidth(key, defaults[key]);
      save();
    });
  });
}

// Finder-style selection: returns the new set of ids, or null for a plain click.
function nextSelection(selected, order, anchor, id, e) {
  if (e.metaKey || e.ctrlKey) {
    const next = new Set(selected);
    if (!next.delete(id)) next.add(id);
    return [...next];
  }
  if (e.shiftKey && anchor != null) {
    const a = order.indexOf(anchor), b = order.indexOf(id);
    if (a !== -1 && b !== -1) return order.slice(Math.min(a, b), Math.max(a, b) + 1);
  }
  return null;
}

// What to do with files when removing, once the user has said: "keep", "trash", or
// nothing to keep asking. Shared by both tabs.
const REMOVAL_KEY = "kdownloader-removal";
function rememberedRemoval() {
  try { return localStorage.getItem(REMOVAL_KEY) || ""; } catch (e) { return ""; }
}
function setRememberedRemoval(choice) {
  try {
    if (choice) localStorage.setItem(REMOVAL_KEY, choice);
    else localStorage.removeItem(REMOVAL_KEY);
  } catch (e) {}
}

// The Options panel is one state across both tabs, and is kept between launches.
const OPTIONS_KEY = "kdownloader-options-open";
function showOptions(open) {
  for (const prefix of ["", "dd-"]) {
    document.getElementById(`${prefix}opts`).hidden = !open;
    document.getElementById(`${prefix}opts-toggle`).textContent = open ? "Options ▴" : "Options ▾";
  }
  try { localStorage.setItem(OPTIONS_KEY, open ? "1" : ""); } catch (e) {}
}
for (const id of ["opts-toggle", "dd-opts-toggle"]) {
  document.getElementById(id).addEventListener("click", () => showOptions(document.getElementById("opts").hidden));
}
try { showOptions(localStorage.getItem(OPTIONS_KEY) === "1"); } catch (e) {}

// ---- context menu ----------------------------------------------------------
const contextMenu = document.getElementById("ctxmenu");
let contextShownAt = 0;
function hideCtx(force) {
  // ignore the trailing click of the very gesture that opened the menu
  if (!force && performance.now() - contextShownAt < 250) return;
  if (!contextMenu.hidden) { contextMenu.hidden = true; contextMenu.innerHTML = ""; }
}
document.addEventListener("click", () => hideCtx());
window.addEventListener("blur", () => hideCtx(true));

// entries: [{ label, act, danger, key }] or { sep: true }; key is the shortcut shown.
function showContextMenu(e, entries) {
  contextMenu.innerHTML = entries
    .map((it, i) => (it.sep ? '<div class="ctx-sep"></div>' : `<div class="ctx-item${it.danger ? " danger" : ""}" data-i="${i}"><span>${it.label}</span>${it.key ? `<kbd>${keyLabel(it.key)}</kbd>` : ""}</div>`))
    .join("");
  contextMenu.hidden = false;
  contextShownAt = performance.now();
  contextMenu.style.left = `${Math.min(e.clientX, window.innerWidth - contextMenu.offsetWidth - 6)}px`;
  contextMenu.style.top = `${Math.min(e.clientY, window.innerHeight - contextMenu.offsetHeight - 6)}px`;
  contextMenu.querySelectorAll(".ctx-item").forEach((el) => el.addEventListener("click", (ev) => {
    ev.stopPropagation();
    hideCtx(true);
    entries[Number(el.dataset.i)].act?.();
  }));
}

// ---- menu bar and keyboard ---------------------------------------------------
// Menu bar commands arrive as "menu" events and run on the tab in front;
// "any" handlers work from every tab.
const menuHandlers = { torrent: {}, direct: {}, bandwidth: {}, any: {} };
function onMenu(scope, handlers) { Object.assign(menuHandlers[scope], handlers); }
const frontTab = () => ["torrent", "direct", "bandwidth"].find((t) => !document.getElementById(`view-${t}`).hidden);
const dialogOpen = () => [...document.querySelectorAll(".overlay")].some((o) => !o.hidden);
function runMenu(id) {
  // A dialog takes the keyboard: commands behind it would act out of sight.
  if (dialogOpen() && id !== "about" && id !== "shortcuts") return;
  (menuHandlers[frontTab()][id] || menuHandlers.any[id])?.();
}
window.__TAURI__.event.listen("menu", (event) => runMenu(event.payload));

// The row a key moves to from `cursor`, or undefined for keys that don't move.
function listTarget(e, order, cursor) {
  if (!order.length) return undefined;
  const page = 10, last = order.length - 1;
  let i = order.indexOf(cursor);
  switch (e.key) {
    case "ArrowDown": i = i === -1 ? 0 : Math.min(last, i + 1); break;
    case "ArrowUp": i = i === -1 ? 0 : Math.max(0, i - 1); break;
    case "PageDown": i = i === -1 ? 0 : Math.min(last, i + page); break;
    case "PageUp": i = i === -1 ? 0 : Math.max(0, i - page); break;
    case "Home": i = 0; break;
    case "End": i = last; break;
    default: return undefined;
  }
  return order[i];
}
// Rows from the anchor to `to`, for ⇧ with the arrow keys.
function rangeTo(order, anchor, to) {
  const a = order.indexOf(anchor), b = order.indexOf(to);
  return a === -1 ? [to] : order.slice(Math.min(a, b), Math.max(a, b) + 1);
}
const typingIn = (target) => {
  const tag = (target.tagName || "").toLowerCase();
  return tag === "input" || tag === "textarea" || tag === "select" || target.isContentEditable;
};

// "?" shows every shortcut, as does Help ▸ Keyboard Shortcuts.
function showShortcuts() {
  document.getElementById("keys-modal").hidden = false;
  document.getElementById("keys-close").focus();
}
document.getElementById("keys-close").addEventListener("click", () => (document.getElementById("keys-modal").hidden = true));
document.addEventListener("keydown", (e) => {
  const sheet = document.getElementById("keys-modal");
  if (!sheet.hidden && (e.key === "Escape" || e.key === "Enter")) { e.preventDefault(); sheet.hidden = true; return; }
  if (e.key === "?" && !typingIn(e.target) && !dialogOpen()) { e.preventDefault(); showShortcuts(); }
});
onMenu("any", {
  shortcuts: showShortcuts,
  options: () => showOptions(document.getElementById("opts").hidden),
});
