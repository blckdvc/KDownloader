// The Direct Download tab. It follows the Torrent tab's model (toolbar acting on
// the selection, sidebar filters, sortable table) and reuses main.js's helpers.
(() => {
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(`dd-${id}`);

const PROVIDERS = {
  "real-debrid": "Real-Debrid", "all-debrid": "AllDebrid", premiumize: "Premiumize", "debrid-link": "Debrid-Link",
  "uploaded-premium-link": "Uploadedpremiumlink", torbox: "TorBox", "mega-debrid": "Mega-Debrid",
  cocoleech: "Cocoleech", fakirdebrid: "Fakirdebrid",
};
const STATUSES = [
  ["all", "All"], ["downloading", "Downloading"], ["queued", "Queued"],
  ["paused", "Paused"], ["completed", "Completed"], ["errored", "Errored"],
];
const KIND = { queued: "queued", active: "downloading", paused: "paused", waiting: "queued", extracting: "downloading", done: "completed", failed: "error" };
const LABELS = { queued: "Queued", active: "Downloading", paused: "Paused", waiting: "Waiting to retry", extracting: "Extracting…", done: "Completed", failed: "Errored" };

// A .torrent URL or magnet belongs to the Torrent tab, everything else here.
const TORRENT_LINKS = /magnet:\?[^\s"'<>]+|https?:\/\/[^\s"'<>]+\.torrent(?:\?[^\s"'<>]*)?/gi;

let items = [];
const GRABBED_KEY = "pulsar-dd-grabbed";
let grabbed = [];
try { grabbed = JSON.parse(localStorage.getItem(GRABBED_KEY) || "[]"); } catch (e) {}
let settings = null;
let selectedIds = new Set();
let anchorId = null;
let statusFilter = "all";
let packageFilter = "";
let grabberOpen = false;
let sort = { col: "queue", dir: 1 }; // queue order until a column is clicked
let rmTarget = null;
let errorTimer;

function showMessage(message, notice) {
  const box = $("error");
  box.textContent = String(message);
  box.classList.toggle("info", !!notice);
  box.hidden = false;
  clearTimeout(errorTimer);
  errorTimer = setTimeout(() => (box.hidden = true), 6000);
}
const showFailure = (e) => showMessage(e);
const needsExtraction = (i) => i.status === "done" && /^extraction/.test(i.error || "");
const kindOf = (i) => (needsExtraction(i) ? "error" : KIND[i.status]);

async function call(cmd, args) {
  try { return await invoke(cmd, args); } catch (e) { showFailure(e); }
}

const sizeText = (n) => (n == null ? "—" : bytes(n));

function duration(s) {
  if (!isFinite(s)) return "";
  if (s < 60) return `${Math.ceil(s)}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${Math.ceil(s % 60)}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

function percent(item) {
  if (item.status === "extracting" && item.extract_total) return Math.min(100, (item.extracted / item.extract_total) * 100);
  if (item.status === "done" || item.status === "extracting") return 100;
  return item.size ? Math.min(100, (item.done / item.size) * 100) : 0;
}

function etaSeconds(item) {
  if (item.status === "extracting") {
    return item.extract_total && item.extract_speed ? Math.max(0, item.extract_total - item.extracted) / item.extract_speed : Infinity;
  }
  return item.status === "active" && item.size && item.speed ? (item.size - item.done) / item.speed : Infinity;
}

function matchStatus(item, filter) {
  const kind = kindOf(item);
  return filter === "all" || kind === filter || (filter === "errored" && kind === "error");
}

// ---- app tabs and clipboard ------------------------------------------------
function showApp(name) {
  for (const t of ["torrent", "direct", "bandwidth"]) {
    document.getElementById(`app-tab-${t}`).classList.toggle("active", t === name);
    document.getElementById(`view-${t}`).hidden = t !== name;
  }
  // Hidden tabs aren't drawn: bring the new one up to date at once.
  window.dispatchEvent(new Event("pulsar-tab"));
}

function routeClipboard(text) {
  const torrents = text.match(TORRENT_LINKS) || [];
  if (torrents.length) {
    // Only prefilled: adding a torrent starts sharing, so it stays a user action.
    const field = document.getElementById("magnet");
    if (!field.value.trim()) field.value = torrents[0];
    showApp("torrent");
  }
  const rest = text.replace(TORRENT_LINKS, " ");
  if (/https?:\/\//i.test(rest)) collect(rest);
}

// Copied links start by themselves, or wait in the grabber if the user prefers.
async function collect(text, review = false) {
  // Links from an unknown page aren't contacted until the user accepts them.
  const found = await call(review ? "dd_list_unchecked" : "dd_grab", review ? { text } : { text, filesOnly: true });
  if (!found?.length) return;
  openCaptchas(found);
  const automatic = settings?.auto_start_clipboard && !review;
  const blocked = automatic ? await start(found) : found;
  if (!automatic) grab(found);
  // Only a captcha calls for attention; anything else waits behind the badge.
  if (found.some((p) => p.protected)) {
    showApp("direct");
    openGrabber(true);
  } else if (blocked.length && !document.getElementById("view-direct").hidden) {
    openGrabber(true);
  }
  refresh();
}

// The user solves the captcha in a popup; the links it reveals come back here.
function openCaptchas(found) {
  for (const p of found) {
    if (p.protected) call("dd_open_captcha", { url: p.url });
  }
}

function addOptions() {
  return {
    save_to: $("o-savepath").value.trim() || null,
    paused: $("o-paused").checked,
    skip_extraction: $("o-noextract").checked,
  };
}

// These two are the app's settings, shown here for quick changes.
async function saveQuickSetting(field, input) {
  const value = Number(input.value);
  if (!settings || !value) { loadSettings(); return; }
  await call("dd_set_settings", { settings: { ...settings, [field]: value } });
  loadSettings();
}
$("o-connections").addEventListener("change", (e) => saveQuickSetting("chunks", e.target));
$("o-active").addEventListener("change", (e) => saveQuickSetting("max_active", e.target));

// Starts what can start, and returns the links left in the grabber.
async function start(found) {
  const ready = found.filter((p) => p.online && !p.duplicate);
  const blocked = found.filter((p) => !ready.includes(p));
  if (ready.length) await call("dd_add", { probes: ready, options: addOptions() });
  if (blocked.length) {
    grab(blocked);
    const added = ready.length ? `Added ${ready.length}. ` : "";
    showMessage(`${added}${blocked.length} link${blocked.length > 1 ? "s" : ""} couldn't start: see Link Grabber.`, true);
  }
  return blocked;
}

function grab(found) {
  for (const probe of found) {
    if (!grabbed.some((p) => p.url === probe.url)) {
      grabbed.push({ ...probe, selected: probe.online && !probe.duplicate });
    }
  }
  renderGrabbed();
}

// ---- toolbar ---------------------------------------------------------------
// The toolbar's Add is an explicit request, like Add on the Torrent tab: what
// can start does, and only what can't is left in the grabber to look at.
$("add-form").addEventListener("submit", async (event) => {
  event.preventDefault();
  const text = $("links").value.trim();
  if (!text) return;
  const button = $("add");
  button.disabled = true;
  const found = await call("dd_grab", { text, filesOnly: false });
  button.disabled = false;
  if (!found) return;
  if (!found.length) { showFailure("No links found"); return; }
  $("links").value = "";
  openCaptchas(found);
  const blocked = await start(found);
  openGrabber(blocked.length > 0);
  refresh();
});

// A text field would squash pasted lines together into one long URL.
$("links").addEventListener("paste", (event) => {
  const text = event.clipboardData?.getData("text");
  if (!text || !/\n/.test(text)) return;
  event.preventDefault();
  $("links").setRangeText(text.trim().replace(/\s+/g, " "), $("links").selectionStart, $("links").selectionEnd, "end");
});

const selectedItems = () => (grabberOpen ? [] : items.filter((i) => selectedIds.has(i.id)));
const resumable = (i) => ["paused", "failed", "waiting"].includes(i.status);
const pausable = (i) => ["active", "queued", "waiting"].includes(i.status);

function syncToolbar() {
  const picked = selectedItems();
  $("tb-resume").disabled = !picked.some(resumable);
  $("tb-pause").disabled = !picked.some(pausable);
  $("tb-remove").disabled = !picked.length;
}

async function forEach(command, list) {
  await Promise.all(list.map((item) => call(command, { id: item.id })));
  refresh();
}

$("tb-resume").addEventListener("click", () => forEach("dd_resume", selectedItems().filter(resumable)));
$("tb-pause").addEventListener("click", () => forEach("dd_pause", selectedItems().filter(pausable)));
$("tb-remove").addEventListener("click", () => openRemoveModal());
$("start-all").addEventListener("click", () => call("dd_resume_all").then(refresh));
$("pause-all").addEventListener("click", () => call("dd_pause_all").then(refresh));
$("clear-done").addEventListener("click", () => call("dd_clear_done").then(refresh));

// ---- sidebar ---------------------------------------------------------------
function renderSidebar() {
  setHtml($("status-filters"), STATUSES.map(([key, label]) => {
    const n = items.filter((i) => matchStatus(i, key)).length;
    const active = !grabberOpen && statusFilter === key ? "active" : "";
    return `<li data-status="${key}" tabindex="0" class="${active}"><span>${label}</span><span class="cnt">${n}</span></li>`;
  }).join(""));

  setHtml($("grab-filter"), `<li data-grabber="1" tabindex="0" class="${grabberOpen ? "active" : ""}"><span>Link Grabber</span><span class="cnt">${grabbed.length}</span></li>`);

  const packages = {};
  for (const item of items) packages[item.package] = (packages[item.package] || 0) + 1;
  const names = Object.keys(packages).sort();
  const all = `<li data-package="" tabindex="0" class="${packageFilter === "" ? "active" : ""}"><span>All</span><span class="cnt">${items.length}</span></li>`;
  setHtml($("package-filters"), all + names
    .map((name) => `<li data-package="${esc(name)}" tabindex="0" class="${packageFilter === name ? "active" : ""}" title="${esc(name)}"><span>${esc(name)}</span><span class="cnt">${packages[name]}</span></li>`)
    .join(""));

  const count = document.getElementById("app-grab-count");
  count.hidden = grabbed.length === 0;
  setText(count, String(grabbed.length));
}

function openGrabber(open) {
  grabberOpen = open;
  $("queue-view").hidden = open;
  $("grabber-view").hidden = !open;
  renderSidebar();
  syncToolbar();
}

$("status-filters").addEventListener("click", (e) => {
  const li = e.target.closest("li[data-status]");
  if (li) { statusFilter = li.dataset.status; openGrabber(false); refresh(); }
});
$("grab-filter").addEventListener("click", () => openGrabber(true));
$("package-filters").addEventListener("click", (e) => {
  const li = e.target.closest("li[data-package]");
  if (li) { packageFilter = li.dataset.package; openGrabber(false); refresh(); }
});

// ---- queue table -----------------------------------------------------------
function sortValue(item, col) {
  switch (col) {
    case "name": return item.name.toLowerCase();
    case "size": return item.size ?? -1;
    case "progress": return percent(item);
    case "status": return item.status;
    case "speed": return item.speed;
    case "eta": return etaSeconds(item);
    case "conns": return item.segments.length;
    case "service": return PROVIDERS[item.provider] || "";
    case "package": return item.package.toLowerCase();
    default: return 0;
  }
}

function renderTable() {
  const rows = items
    .filter((i) => matchStatus(i, statusFilter) && (packageFilter === "" || i.package === packageFilter))
    .sort((a, b) => {
      const x = sortValue(a, sort.col), y = sortValue(b, sort.col);
      return x < y ? -sort.dir : x > y ? sort.dir : 0;
    });
  patchRows($("tbody"), rows.map((item) => {
    const pct = percent(item);
    const active = item.status === "active";
    const eta = etaSeconds(item);
    const problem = item.error ? ` title="${esc(item.error)}"` : "";
    const extracting = item.status === "extracting";
    const label = extracting && item.extract_total ? `Extracting… ${pct.toFixed(0)}%` : LABELS[item.status];
    const text = item.error ? `${label}: ${esc(item.error)}` : label;
    const kind = `s-${kindOf(item)}`;
    return { id: item.id, cls: selectedIds.has(item.id) ? `${kind} selected` : kind, html: `
      <td class="col-name" title="${esc(item.url)}">${esc(item.name)}</td>
      <td class="num">${sizeText(item.size)}</td>
      <td><span class="pbar"><span style="width:${pct}%"></span><em>${pct.toFixed(1)}%</em></span></td>
      <td class="dd-status"><span class="st"${problem}>${text}</span></td>
      <td class="num">${active && item.speed ? `${bytes(item.speed)}/s` : extracting && item.extract_speed ? `${bytes(item.extract_speed)}/s` : ""}</td>
      <td class="num">${isFinite(eta) ? duration(eta) : ""}</td>
      <td class="num">${active ? item.segments.length : ""}</td>
      <td>${esc(PROVIDERS[item.provider] || "")}</td>
      <td class="dd-clip" title="${esc(item.package)}">${esc(item.package)}</td>` };
  }));
  $("empty").style.display = items.length ? "none" : "flex";
  const n = rows.length;
  setText($("count"), `${n} download${n === 1 ? "" : "s"}${items.length !== n ? ` / ${items.length}` : ""}`);
  document.querySelectorAll("#dd-queue-view th[data-dd-sort]").forEach((th) => {
    th.classList.toggle("sorted", th.dataset.ddSort === sort.col);
    th.classList.toggle("asc", th.dataset.ddSort === sort.col && sort.dir === 1);
  });
}

function selectRows(ids) {
  selectedIds = new Set(ids);
  document.querySelectorAll("#dd-tbody tr").forEach((tr) => tr.classList.toggle("selected", selectedIds.has(Number(tr.dataset.id))));
  syncToolbar();
}

function select(id) {
  anchorId = id;
  selectRows([id]);
}

const rowOrder = () => [...document.querySelectorAll("#dd-tbody tr[data-id]")].map((tr) => Number(tr.dataset.id));

function pickRow(id, e) {
  const picked = nextSelection(selectedIds, rowOrder(), anchorId, id, e);
  if (!picked) { select(id); return; }
  if (e.metaKey || e.ctrlKey) anchorId = id;
  selectRows(picked);
}

function reveal(item) {
  if (item.output) call("dd_reveal", { path: item.output });
  else call("reveal_path", { path: item.dir });
}

$("tbody").addEventListener("click", (e) => {
  const tr = e.target.closest("tr[data-id]");
  if (tr) pickRow(Number(tr.dataset.id), e);
});
$("tbody").addEventListener("dblclick", (e) => {
  const tr = e.target.closest("tr[data-id]");
  const item = tr && items.find((i) => i.id === Number(tr.dataset.id));
  if (item) reveal(item);
});
document.querySelectorAll("#dd-queue-view th[data-dd-sort]").forEach((th) => {
  th.addEventListener("click", () => {
    const col = th.dataset.ddSort;
    if (sort.col === col) sort.dir *= -1; else { sort.col = col; sort.dir = 1; }
    renderTable();
  });
});

// ---- resizable columns -----------------------------------------------------
resizableColumns({
  storageKey: "pulsar-dd-colw",
  defaults: { size: 80, progress: 150, status: 150, speed: 92, eta: 72, conns: 56, service: 110, package: 130 },
  table: document.querySelector("#dd-queue-view .ttable"),
  headers: document.querySelectorAll("#dd-queue-view th[data-dd-sort]"),
  headerKey: (th) => th.dataset.ddSort,
  column: (key) => document.querySelector(`col[data-dd-col="${key}"]`),
  minName: 220,
});

// ---- context menu ----------------------------------------------------------
$("tbody").addEventListener("contextmenu", (e) => {
  const tr = e.target.closest("tr[data-id]");
  if (!tr) return;
  e.preventDefault();
  const id = Number(tr.dataset.id);
  if (!selectedIds.has(id)) select(id);
  const picked = selectedItems();
  const item = picked.find((i) => i.id === id);
  if (!item) return;
  const ids = picked.map((i) => i.id);
  const entries = [];
  if (picked.length > 1) {
    const n = picked.length;
    if (picked.some(resumable)) entries.push({ label: `▶ Resume ${n} downloads`, act: () => forEach("dd_resume", picked.filter(resumable)) });
    if (picked.some(pausable)) entries.push({ label: `⏸ Pause ${n} downloads`, act: () => forEach("dd_pause", picked.filter(pausable)) });
    entries.push(
      { label: `⤒ Move ${n} downloads to top`, act: () => call("dd_move", { ids, toTop: true }).then(refresh) },
      { label: `⤓ Move ${n} downloads to bottom`, act: () => call("dd_move", { ids, toTop: false }).then(refresh) },
      { label: "📋 Copy links", act: () => { try { navigator.clipboard.writeText(picked.map((i) => i.url).join("\n")); } catch (_) {} } },
      { sep: true },
      { label: `✕ Remove ${n} downloads (keep files)`, act: () => removeNow(ids, false) },
      { label: `🗑 Remove ${n} downloads + move files to Trash`, danger: true, act: () => confirmTrash(ids) },
    );
  } else {
  if (needsExtraction(item)) {
    entries.push({ label: "↻ Retry extraction", act: () => call("dd_retry_extraction", { id }).then(refresh) });
  }
  if (resumable(item)) {
    entries.push({ label: item.status === "failed" ? "▶ Retry" : "▶ Resume", act: () => call("dd_resume", { id }).then(refresh) });
  }
  if (pausable(item)) {
    entries.push({ label: "⏸ Pause", act: () => call("dd_pause", { id }).then(refresh) });
  }
  entries.push(
    { label: "⤒ Move to top of queue", act: () => call("dd_move", { ids: [id], toTop: true }).then(refresh) },
    { label: "⤓ Move to bottom of queue", act: () => call("dd_move", { ids: [id], toTop: false }).then(refresh) },
    { label: "📂 Show in Finder", act: () => reveal(item) },
    { label: "📋 Copy link", act: () => { try { navigator.clipboard.writeText(item.url); } catch (_) {} } },
    { sep: true },
    { label: "✕ Remove (keep files)", act: () => removeNow([id], false) },
    { label: "🗑 Remove + move files to Trash", danger: true, act: () => confirmTrash([id]) },
  );
  }
  showContextMenu(e, entries);
});

// ---- removal ---------------------------------------------------------------
function openRemoveModal(ids = [...selectedIds]) {
  const picked = items.filter((i) => ids.includes(i.id));
  if (!picked.length) return;
  const saved = rememberedRemoval();
  if (saved) { removeNow(picked.map((i) => i.id), saved === "trash"); return; }
  setTimeout(() => $("rm-cancel").focus());
  rmTarget = picked.map((i) => i.id);
  $("rm-name").textContent = picked.length === 1 ? picked[0].name : `${picked.length} downloads selected`;
  $("rm-modal").hidden = false;
}

async function removeNow(ids, deleteFile) {
  await Promise.all(ids.map((id) => call("dd_remove", { id, deleteFile })));
  for (const id of ids) selectedIds.delete(id);
  refresh();
}

// The menu entry already says what happens, so only a first-time user is asked.
function confirmTrash(ids) {
  if (rememberedRemoval()) removeNow(ids, true);
  else openRemoveModal(ids);
}

function confirmRemove(deleteFile) {
  $("rm-modal").hidden = true;
  if ($("rm-remember").checked) setRememberedRemoval(deleteFile ? "trash" : "keep");
  $("rm-remember").checked = false;
  if (rmTarget != null) removeNow(rmTarget, deleteFile);
  rmTarget = null;
}

$("rm-cancel").addEventListener("click", () => { $("rm-modal").hidden = true; rmTarget = null; });
$("rm-keep").addEventListener("click", () => confirmRemove(false));
$("rm-del").addEventListener("click", () => confirmRemove(true));

// ---- keyboard --------------------------------------------------------------
document.addEventListener("keydown", (e) => {
  if (document.getElementById("view-direct").hidden) return;
  if (!$("rm-modal").hidden) {
    if (e.key === "Escape") { e.preventDefault(); $("rm-modal").hidden = true; rmTarget = null; }
    else if (e.key === "Enter") { e.preventDefault(); confirmRemove(e.shiftKey || e.metaKey); }
    return;
  }
  const tag = (e.target.tagName || "").toLowerCase();
  if (tag === "input" || tag === "textarea" || e.target.isContentEditable) return;
  if (!$("modal").hidden) {
    if (e.key === "Escape") $("modal").hidden = true;
    return;
  }
  if (grabberOpen) return;

  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "a") {
    e.preventDefault();
    selectRows(rowOrder());
  } else if (e.key === "Delete" || e.key === "Backspace") {
    if (!selectedIds.size) return;
    e.preventDefault();
    openRemoveModal();
  } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    const order = rowOrder();
    if (!order.length) return;
    e.preventDefault();
    let i = order.indexOf(anchorId);
    i = i === -1 ? 0 : e.key === "ArrowDown" ? Math.min(order.length - 1, i + 1) : Math.max(0, i - 1);
    select(order[i]);
    document.querySelector(`#dd-tbody tr[data-id="${order[i]}"]`)?.scrollIntoView({ block: "nearest" });
  }
});

// ---- link grabber ----------------------------------------------------------
function grabbedStatus(p) {
  if (p.unchecked) return "Not checked yet";
  if (p.duplicate) return "Already in queue";
  if (!p.online) return p.error || "Offline";
  if (p.provider) return PROVIDERS[p.provider];
  return p.resumable ? "Online" : "Online (no resume)";
}

function renderGrabbed() {
  $("grabbed").replaceChildren(...grabbed.map((p) => {
    const tr = document.createElement("tr");
    tr.className = p.online ? "" : "s-error";
    tr.innerHTML = `<td><input type="checkbox"></td><td class="col-name" title="${esc(p.url)}">${esc(p.name)}</td>
      <td class="num">${sizeText(p.size)}</td><td class="dd-clip">${esc(p.package)}</td>
      <td class="dd-status"><span class="st" title="${esc(grabbedStatus(p))}">${esc(grabbedStatus(p))}</span></td>`;
    if (p.page) {
      const open = document.createElement("button");
      open.className = "link dd-open";
      open.textContent = p.protected ? "Solve captcha" : "Open in browser";
      open.addEventListener("click", () => call(p.protected ? "dd_open_captcha" : "dd_open_url", { url: p.url }));
      tr.lastElementChild.append(open);
    }
    const check = tr.querySelector("input");
    check.checked = p.selected;
    check.disabled = !p.online;
    check.addEventListener("change", () => { p.selected = check.checked; updateGrabButtons(); });
    return tr;
  }));
  $("grab-empty").style.display = grabbed.length ? "none" : "flex";
  try { localStorage.setItem(GRABBED_KEY, JSON.stringify(grabbed)); } catch (e) {}
  updateGrabButtons();
  renderSidebar();
}

function updateGrabButtons() {
  $("add-selected").disabled = !grabbed.some((p) => p.selected);
  $("clear-grabbed").disabled = grabbed.length === 0;
}

$("add-selected").addEventListener("click", async () => {
  const chosen = grabbed.filter((p) => p.selected);
  grabbed = grabbed.filter((p) => !p.selected);
  const unchecked = chosen.filter((p) => p.unchecked);
  await call("dd_add", { probes: chosen.filter((p) => !p.unchecked), options: addOptions() });
  // Accepted links are only now sent to a debrid service.
  if (unchecked.length) {
    const found = await call("dd_grab", { text: unchecked.map((p) => p.url).join(" "), filesOnly: false });
    if (found) await start(found);
  }
  renderGrabbed();
  openGrabber(false);
  refresh();
});
$("clear-grabbed").addEventListener("click", () => { grabbed = []; renderGrabbed(); });

// ---- settings --------------------------------------------------------------
function accountStatus(view) {
  if (view.error) return ["acct-bad", view.error];
  if (!view.connected) return ["muted", "Not connected"];
  const { username, premium, expires } = view.account;
  if (!premium) return ["acct-bad", `${username}: no active premium`];
  const until = expires ? ` until ${new Date(expires * 1000).toLocaleDateString("en-GB")}` : "";
  return ["muted", `${username}: premium${until}`];
}

function plainButton(label, onClick) {
  const b = document.createElement("button");
  b.type = "button";
  b.className = "ghost";
  b.textContent = label;
  b.addEventListener("click", onClick);
  return b;
}

async function renderAccounts() {
  const views = await call("dd_list_accounts");
  if (!views) return;
  $("accounts").replaceChildren(...views.map((view) => {
    const box = document.createElement("div");
    box.className = "dd-account";
    const [cls, text] = accountStatus(view);
    box.innerHTML = `<div class="dd-account-head"><strong>${esc(view.name)}</strong><span class="${cls}">${esc(text)}</span></div>`;
    const row = document.createElement("div");
    row.className = "withbtn";
    if (view.connected) {
      row.append(plainButton("Disconnect", async () => {
        await call("dd_disconnect_account", { provider: view.id });
        renderAccounts();
      }));
    } else {
      const input = document.createElement("input");
      input.type = "password";
      input.placeholder = "API key";
      input.autocomplete = "off";
      row.append(input, plainButton("Connect", async () => {
        const key = input.value.trim();
        if (!key) return;
        input.value = "";
        if (await call("dd_connect_account", { provider: view.id, key })) renderAccounts();
      }), plainButton("Get key", () => call("dd_open_key_page", { provider: view.id })));
    }
    box.append(row);
    return box;
  }));
}

$("settings-btn").addEventListener("click", async () => {
  const s = await call("dd_get_settings");
  if (!s) return;
  $("s-dir").value = s.download_dir;
  $("s-active").value = s.max_active;
  $("s-chunks").value = s.chunks;
  $("s-limit").value = s.speed_limit_kib;
  $("s-extract").checked = s.auto_extract;
  $("s-delete").checked = s.delete_archive;
  $("s-clipboard").checked = s.watch_clipboard;
  $("s-autostart").checked = s.auto_start_clipboard;
  $("s-removal").value = rememberedRemoval() || "";
  $("modal").hidden = false;
  renderAccounts();
});
$("s-removal").addEventListener("change", (e) => setRememberedRemoval(e.target.value));
$("modal-close").addEventListener("click", () => ($("modal").hidden = true));
$("modal-cancel").addEventListener("click", () => ($("modal").hidden = true));
$("modal-save").addEventListener("click", async () => {
  await call("dd_set_settings", {
    settings: {
      download_dir: $("s-dir").value.trim(),
      max_active: Number($("s-active").value) || 3,
      chunks: Number($("s-chunks").value) || 4,
      speed_limit_kib: Number($("s-limit").value) || 0,
      auto_extract: $("s-extract").checked,
      delete_archive: $("s-delete").checked,
      watch_clipboard: $("s-clipboard").checked,
      auto_start_clipboard: $("s-autostart").checked,
    },
  });
  $("modal").hidden = true;
  loadSettings();
});

async function loadSettings() {
  settings = await call("dd_get_settings");
  if (!settings) return;
  $("o-connections").value = settings.chunks;
  $("o-active").value = settings.max_active;
  const dir = settings.download_dir;
  $("reveal").textContent = `📁 ${dir.split("/").filter(Boolean).pop() || dir}`;
  $("reveal").title = `Reveal ${dir} in Finder`;
  $("limits-badge").textContent = settings.speed_limit_kib ? `↓ limit ${bytes(settings.speed_limit_kib * 1024)}/s` : "";
}
$("reveal").addEventListener("click", () => settings && call("reveal_path", { path: settings.download_dir }));

// ---- poll loop -------------------------------------------------------------
// A package is done when its last file is: one notification, not twenty-eight.
function notifyFinished(before, after) {
  if (!before.length) return;
  const wasDone = new Set(before.filter((i) => i.status === "done").map((i) => i.id));
  const fresh = new Set(after.filter((i) => i.status === "done" && !wasDone.has(i.id)).map((i) => i.package));
  for (const name of fresh) {
    if (after.every((i) => i.package !== name || i.status === "done")) {
      call("dd_notify", { title: "Download finished", body: name });
    }
  }
}

let refreshing = false;
async function refresh() {
  if (refreshing) return;
  refreshing = true;
  const next = await invoke("dd_list").catch(() => null);
  refreshing = false;
  if (!next) return;
  notifyFinished(items, next);
  items = next;
  const speed = items.reduce((sum, i) => sum + (i.speed || 0), 0);
  window.pulsarDirectSpeed = speed; // read by the Bandwidth tab
  if (document.getElementById("view-direct").hidden) return;
  for (const id of selectedIds) if (!items.some((i) => i.id === id)) selectedIds.delete(id);
  renderSidebar();
  renderTable();
  syncToolbar();
  setText($("totals"), `↓ ${bytes(speed)}/s`);
}
window.addEventListener("pulsar-tab", refresh);

document.getElementById("app-tab-torrent").addEventListener("click", () => showApp("torrent"));
document.getElementById("app-tab-direct").addEventListener("click", () => showApp("direct"));
document.getElementById("app-tab-bandwidth").addEventListener("click", () => showApp("bandwidth"));
listen("clipboard", (event) => routeClipboard(event.payload));
listen("protected-links", (event) => {
  grabbed = grabbed.filter((p) => p.url !== event.payload.source);
  renderGrabbed();
  collect(event.payload.links, event.payload.review);
});
listen("clipboard-error", (event) => showFailure(`Clipboard unavailable: ${event.payload}`));

renderGrabbed();
loadSettings();
refresh();
setInterval(refresh, 1000);
})();
