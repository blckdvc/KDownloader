const { invoke } = window.__TAURI__.core;
const $ = (id) => document.getElementById(id);
const val = (id) => $(id).value.trim();
const num = (id) => Math.max(0, parseInt($(id).value, 10) || 0);

let torrents = [];
let settingsCache = null;
let selectedId = null;
let selectedIds = new Set(); // every selected row; selectedId is the one the detail pane shows
let anchorId = null;        // where a shift-click range starts
let statusFilter = "all";
let catFilter = "";     // "" = all; "\u0000none" = uncategorized
let tagFilter = "";
let sort = { col: "name", dir: 1 };
let detailTab = "general";
let contentFor = null;  // selection id whose Content tab is loaded

// ---- helpers -------------------------------------------------------------
function showError(m) {
  const e = $("error");
  e.textContent = String(m); e.hidden = false; e.classList.remove("info");
  clearTimeout(showError._t); showError._t = setTimeout(() => (e.hidden = true), 6000);
}
function showNotice(m) {
  const e = $("error");
  e.textContent = String(m); e.hidden = false; e.classList.add("info");
  clearTimeout(showError._t); showError._t = setTimeout(() => { e.hidden = true; e.classList.remove("info"); }, 7000);
}
function bytes(n) {
  if (n == null || isNaN(n)) return "—";
  n = Number(n); const u = ["B", "KB", "MB", "GB", "TB"]; let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
}
function speedBps(s) {
  if (typeof s === "number") return s;
  if (s && typeof s === "object") {
    if (typeof s.bytes_per_second === "number") return s.bytes_per_second;
    if (typeof s.mbps === "number") return s.mbps * 1024 * 1024;
  }
  return 0;
}
function etaText(t) {
  const tr = t?.stats?.live?.time_remaining;
  if (!tr) return "";
  if (typeof tr === "string") return tr;
  if (typeof tr.human_readable === "string") return tr.human_readable;
  return "";
}
function classify(t) {
  const st = t.stats || {};
  if (t.paused) return "paused";
  if (st.error) return "error";
  if (st.finished) return st.live ? "seeding" : "completed";
  if (st.live) return "downloading";
  const s = st.state;
  if (typeof s === "object" && s && "Initializing" in s) return "initializing";
  if (typeof s === "string") return s.toLowerCase();
  return "idle";
}
const LABEL = { downloading: "Downloading", seeding: "Seeding", completed: "Completed", paused: "Paused", error: "Errored", stalled: "Stalled", initializing: "Initializing", idle: "Idle" };
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const toHHMM = (m) => `${String(Math.floor(m / 60)).padStart(2, "0")}:${String(m % 60).padStart(2, "0")}`;
const fromHHMM = (s) => { const [h, m] = (s || "0:0").split(":").map(Number); return (h || 0) * 60 + (m || 0); };

// Each refresh brings new objects, so a result lasts exactly one refresh.
const metricsCache = new WeakMap();
function metrics(t) {
  let m = metricsCache.get(t);
  if (!m) { m = computeMetrics(t); metricsCache.set(t, m); }
  return m;
}
function computeMetrics(t) {
  const st = t.stats || {}, live = st.live || null, snap = live ? live.snapshot : null, ps = snap ? snap.peer_stats : null;
  const total = Number(st.total_bytes || 0), done = Number(st.progress_bytes || 0), up = Number(st.uploaded_bytes || 0);
  const dl = t.paused || !live ? 0 : speedBps(live.download_speed);
  const ul = t.paused || !live ? 0 : speedBps(live.upload_speed);
  const pct = total > 0 ? Math.min(100, (done / total) * 100) : st.finished ? 100 : 0;
  const peers = ps ? ps.live || 0 : 0, swarm = ps ? ps.seen || 0 : 0;
  const seeds = snap ? snap.connected_seeds || 0 : 0, leechers = Math.max(0, peers - seeds);
  const ratio = done > 0 ? up / done : up > 0 ? Infinity : 0;
  const etaSec = !t.paused && dl > 0 && total > done ? (total - done) / dl : st.finished ? -1 : Infinity;
  let kind = classify(t);
  // "Stalled": incomplete, not paused, but going nowhere (no peers, no speed).
  // On a dead/private swarm this is the tell-tale of missing/corrupt data to
  // clean up — flagged red alongside hard errors.
  if ((kind === "downloading" || kind === "idle") && !t.paused && !st.finished && pct < 100 && peers === 0 && dl === 0) {
    kind = "stalled";
  }
  return { st, live, snap, ps, total, done, up, dl, ul, pct, peers, swarm, seeds, leechers, ratio, etaSec, kind };
}

// ---- add / options -------------------------------------------------------
function addOpts() {
  return { category: val("o-category"), savePath: val("o-savepath") || null, paused: $("o-paused").checked, downKbps: num("o-down"), upKbps: num("o-up") };
}
$("add-form").addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const source = $("magnet").value.trim(); if (!source) return;
  try { await invoke("add_torrent", { source, opts: addOpts() }); $("magnet").value = ""; await refresh(); }
  catch (e) { showError(e); }
});
$("file").addEventListener("change", async () => {
  const f = $("file").files[0]; if (!f) return;
  try { const buf = new Uint8Array(await f.arrayBuffer()); await invoke("add_torrent_bytes", { bytes: Array.from(buf), opts: addOpts() }); await refresh(); }
  catch (e) { showError(e); } finally { $("file").value = ""; }
});
async function browseInto(input) { try { const p = await invoke("pick_folder"); if (p) input.value = p; } catch (e) { showError(e); } }
document.addEventListener("click", (e) => {
  const b = e.target.closest("button.browse[data-target]");
  if (b) { const i = $(b.dataset.target); if (i) browseInto(i); }
});

// ---- sidebar -------------------------------------------------------------
const STATUSES = [
  ["all", "All"], ["downloading", "Downloading"], ["seeding", "Seeding"],
  ["completed", "Completed"], ["paused", "Paused"], ["active", "Active"],
  ["inactive", "Inactive"], ["stalled", "Stalled"], ["errored", "Errored"],
];
function matchStatus(t, f) {
  const m = metrics(t), k = m.kind;
  switch (f) {
    case "all": return true;
    case "downloading": return k === "downloading" || k === "initializing";
    case "seeding": return k === "seeding";
    case "completed": return k === "completed" || k === "seeding";
    case "paused": return k === "paused";
    case "active": return !t.paused && (m.dl > 0 || m.ul > 0);
    case "inactive": return t.paused || (m.dl === 0 && m.ul === 0);
    case "stalled": return k === "stalled";
    case "errored": return k === "error";
    default: return true;
  }
}
function renderSidebar() {
  setHtml($("status-filters"), STATUSES.map(([k, label]) => {
    const n = torrents.filter((t) => matchStatus(t, k)).length;
    return `<li data-status="${k}" tabindex="0" class="${statusFilter === k ? "active" : ""}"><span>${label}</span><span class="cnt">${n}</span></li>`;
  }).join(""));

  const cats = {};
  for (const t of torrents) { const c = t.category || ""; cats[c] = (cats[c] || 0) + 1; }
  const catKeys = Object.keys(cats).filter((c) => c).sort();
  let catHtml = `<li data-cat="" class="${catFilter === "" ? "active" : ""}"><span>All</span><span class="cnt">${torrents.length}</span></li>`;
  if (cats[""]) catHtml += `<li data-cat="\u0000none" class="${catFilter === "\u0000none" ? "active" : ""}"><span>Uncategorized</span><span class="cnt">${cats[""]}</span></li>`;
  catHtml += catKeys.map((c) => `<li data-cat="${esc(c)}" class="${catFilter === c ? "active" : ""}"><span>${esc(c)}</span><span class="cnt">${cats[c]}</span></li>`).join("");
  setHtml($("cat-filters"), catHtml);

  const tags = {};
  for (const t of torrents) for (const tg of t.tags || []) tags[tg] = (tags[tg] || 0) + 1;
  const tagKeys = Object.keys(tags).sort();
  let tagHtml = `<li data-tag="" class="${tagFilter === "" ? "active" : ""}"><span>All</span><span class="cnt">${torrents.length}</span></li>`;
  tagHtml += tagKeys.length ? tagKeys.map((tg) => `<li data-tag="${esc(tg)}" class="${tagFilter === tg ? "active" : ""}"><span>${esc(tg)}</span><span class="cnt">${tags[tg]}</span></li>`).join("")
    : `<li class="empty-hint">no tags</li>`;
  setHtml($("tag-filters"), tagHtml);
}
$("status-filters").addEventListener("click", (e) => { const li = e.target.closest("li[data-status]"); if (li) { statusFilter = li.dataset.status; refresh(); } });
$("cat-filters").addEventListener("click", (e) => { const li = e.target.closest("li[data-cat]"); if (li) { catFilter = li.dataset.cat; refresh(); } });
$("tag-filters").addEventListener("click", (e) => { const li = e.target.closest("li[data-tag]"); if (li) { tagFilter = li.dataset.tag; refresh(); } });

// ---- table ---------------------------------------------------------------
function visibleTorrents() {
  return torrents.filter((t) =>
    matchStatus(t, statusFilter) &&
    (catFilter === "" || (catFilter === "\u0000none" ? !t.category : t.category === catFilter)) &&
    (tagFilter === "" || (t.tags || []).includes(tagFilter))
  );
}
function sortVal(t, col) {
  const m = metrics(t);
  switch (col) {
    case "name": return (t.name || "").toLowerCase();
    case "size": return m.total;
    case "progress": return m.pct;
    case "status": return m.kind;
    case "seeds": return t.tracker_seeds ?? m.seeds;
    case "peers": return t.tracker_peers ?? m.leechers;
    case "down": return m.dl;
    case "up": return m.ul;
    case "eta": return m.etaSec;
    case "ratio": return m.ratio === Infinity ? 1e18 : m.ratio;
    default: return 0;
  }
}
// "connected (in the swarm)", as in qBittorrent; the total comes from trackers.
const swarmCell = (connected, total) => (total == null ? `${connected}` : `${connected} (${total})`);
function renderTable() {
  const rows = visibleTorrents().sort((a, b) => {
    const x = sortVal(a, sort.col), y = sortVal(b, sort.col);
    if (x < y) return -1 * sort.dir; if (x > y) return 1 * sort.dir; return 0;
  });
  for (const id of selectedIds) if (!torrents.some((t) => t.id === id)) selectedIds.delete(id);
  if (selectedId != null && !selectedIds.has(selectedId)) selectedId = [...selectedIds].pop() ?? null;
  patchRows($("tbody"), rows.map((t) => {
    const m = metrics(t);
    const dl = m.dl > 0 ? `${bytes(m.dl)}/s` : "", ul = m.ul > 0 ? `${bytes(m.ul)}/s` : "";
    const eta = t.paused ? "" : etaText(t);
    const ratio = m.ratio === Infinity ? "∞" : m.ratio.toFixed(2);
    const cat = t.category ? `<span class="cat-tag">${esc(t.category)}</span>` : "";
    const cls = selectedIds.has(t.id) ? `s-${m.kind} selected` : `s-${m.kind}`;
    return { id: t.id, cls, html: `
      <td class="col-name" title="${esc(t.name)}">${esc(t.name)}${cat}</td>
      <td class="num">${bytes(m.total)}</td>
      <td><span class="pbar"><span style="width:${m.pct}%"></span><em>${m.pct.toFixed(1)}%</em></span></td>
      <td><span class="st"${m.st.error ? ` title="${esc(m.st.error)}"` : ""}>${LABEL[m.kind] || m.kind}</span></td>
      <td class="num">${swarmCell(m.seeds, t.tracker_seeds)}</td>
      <td class="num">${swarmCell(m.leechers, t.tracker_peers)}</td>
      <td class="num">${dl}</td>
      <td class="num">${ul}</td>
      <td class="num">${eta}</td>
      <td class="num">${ratio}</td>` };
  }));
  $("empty").style.display = torrents.length ? "none" : "flex";
  const n = rows.length;
  setText($("count"), `${n} torrent${n === 1 ? "" : "s"}${torrents.length !== n ? ` / ${torrents.length}` : ""}`);

  // sort indicators
  document.querySelectorAll("thead th[data-sort]").forEach((th) => {
    th.classList.toggle("sorted", th.dataset.sort === sort.col);
    th.classList.toggle("asc", th.dataset.sort === sort.col && sort.dir === 1);
  });
}
$("tbody").addEventListener("click", (e) => { const tr = e.target.closest("tr[data-id]"); if (tr) pickRow(Number(tr.dataset.id), e); });
$("tbody").addEventListener("dblclick", async (e) => {
  const tr = e.target.closest("tr[data-id]"); if (!tr) return;
  const t = torrents.find((x) => x.id === Number(tr.dataset.id));
  if (t?.output_folder) { try { await invoke("reveal_path", { path: t.output_folder }); } catch (err) { showError(err); } }
});
document.querySelectorAll("thead th[data-sort]").forEach((th) => {
  th.addEventListener("click", () => {
    const c = th.dataset.sort;
    if (sort.col === c) sort.dir *= -1; else { sort.col = c; sort.dir = 1; }
    renderTable();
  });
});

// ---- fixed + resizable columns -------------------------------------------
resizableColumns({
  storageKey: "kdownloader-colw",
  defaults: { size: 80, progress: 150, status: 115, seeds: 84, peers: 84, down: 92, up: 92, eta: 80, ratio: 72 },
  table: document.querySelector("#view-torrent .ttable"),
  headers: document.querySelectorAll("#view-torrent thead th[data-sort]"),
  headerKey: (th) => th.dataset.sort,
  column: (key) => document.querySelector(`col[data-col="${key}"]`),
  minName: 240,
});

// ---- toolbar actions -----------------------------------------------------
function syncToolbar() {
  const picked = torrents.filter((x) => selectedIds.has(x.id));
  $("tb-resume").disabled = !picked.some((t) => t.paused);
  $("tb-pause").disabled = !picked.some((t) => !t.paused);
  $("tb-remove").disabled = !picked.length;
}
async function forSelected(command, ids = [...selectedIds]) {
  try { await Promise.all(ids.map((id) => invoke(command, { id }))); await refresh(); } catch (e) { showError(e); }
}
$("tb-resume").addEventListener("click", () => forSelected("resume_torrent", torrents.filter((t) => selectedIds.has(t.id) && t.paused).map((t) => t.id)));
$("tb-pause").addEventListener("click", () => forSelected("pause_torrent", torrents.filter((t) => selectedIds.has(t.id) && !t.paused).map((t) => t.id)));

let rmTarget = null;
function openRemoveModal(ids = [...selectedIds]) {
  const picked = torrents.filter((x) => ids.includes(x.id)); if (!picked.length) return;
  const saved = rememberedRemoval();
  if (saved) { removeNow(picked.map((t) => t.id), saved === "trash"); return; }
  setTimeout(() => $("rm-cancel").focus());
  rmTarget = picked.map((t) => t.id);
  $("rm-name").textContent = picked.length === 1 ? picked[0].name || `torrent ${picked[0].id}` : `${picked.length} torrents selected`;
  $("rm-modal").hidden = false;
}
async function removeNow(ids, del) {
  try {
    await Promise.all(ids.map((id) => invoke("remove_torrent", { id, deleteFiles: del })));
    for (const id of ids) selectedIds.delete(id);
    if (!selectedIds.size) { selectedId = null; loadTab(); }
    await refresh();
  } catch (e) { showError(e); }
}
$("tb-remove").addEventListener("click", () => openRemoveModal());
// The menu entry already says what happens, so only a first-time user is asked.
function confirmTrash(ids) {
  if (rememberedRemoval()) removeNow(ids, true);
  else openRemoveModal(ids);
}
async function doRemove(del) {
  $("rm-modal").hidden = true; if (rmTarget == null) return;
  if ($("rm-remember").checked) setRememberedRemoval(del ? "trash" : "keep");
  $("rm-remember").checked = false;
  const ids = rmTarget; rmTarget = null;
  await removeNow(ids, del);
}
$("rm-cancel").addEventListener("click", () => ($("rm-modal").hidden = true));
$("rm-keep").addEventListener("click", () => doRemove(false));
$("rm-del").addEventListener("click", () => doRemove(true));

// ---- right-click context menu -------------------------------------------
$("tbody").addEventListener("scroll", () => hideCtx(true), true);

$("tbody").addEventListener("contextmenu", (e) => {
  const tr = e.target.closest("tr[data-id]"); if (!tr) return;
  e.preventDefault();
  const id = Number(tr.dataset.id);
  if (!selectedIds.has(id)) selectTorrent(id);
  const t = torrents.find((x) => x.id === id); if (!t) return;
  const ids = [...selectedIds];
  const items = ids.length > 1 ? [
    { label: `▶ Resume ${ids.length} torrents`, act: () => forSelected("resume_torrent", ids) },
    { label: `⏸ Pause ${ids.length} torrents`, act: () => forSelected("pause_torrent", ids) },
    { label: `📡 Force re-announce ${ids.length} torrents`, act: () => forSelected("reannounce", ids) },
    { sep: true },
    { label: `✕ Remove ${ids.length} torrents (keep files)`, act: () => removeNow(ids, false) },
    { label: `🗑 Remove ${ids.length} torrents + move files to Trash`, danger: true, act: () => confirmTrash(ids) },
  ] : [
    t.paused || metrics(t).st.error
      ? { label: metrics(t).st.error ? "▶ Resume (downloads what's missing)" : "▶ Resume", act: () => invoke("resume_torrent", { id }).then(refresh).catch(showError) }
      : { label: "⏸ Pause", act: () => invoke("pause_torrent", { id }).then(refresh).catch(showError) },
    { label: "📡 Force re-announce (reconnect peers)", act: () => invoke("reannounce", { id }).then(refresh).catch(showError) },
    { label: "📂 Open destination folder", act: () => t.output_folder && invoke("reveal_path", { path: t.output_folder }).catch(showError) },
    { label: "ⓘ Details / category…", act: () => { selectTorrent(id); showDetails(true); } },
    { sep: true },
    { label: "📋 Copy info hash", act: () => { try { navigator.clipboard.writeText(t.info_hash || ""); } catch (_) {} } },
    { sep: true },
    { label: "✕ Remove (keep files)", act: () => removeNow([id], false) },
    { label: "🗑 Remove + move files to Trash", danger: true, act: () => confirmTrash([id]) },
  ];
  showContextMenu(e, items);
});

// ---- keyboard ------------------------------------------------------------
function rowOrder() { return [...document.querySelectorAll("#tbody tr[data-id]")].map((tr) => Number(tr.dataset.id)); }

document.addEventListener("keydown", (e) => {
  // Remove dialog open: Esc cancels, Enter removes (⇧/⌘ also deletes files).
  if (!$("rm-modal").hidden) {
    if (e.key === "Escape") { e.preventDefault(); $("rm-modal").hidden = true; rmTarget = null; }
    else if (e.key === "Enter") { e.preventDefault(); doRemove(e.shiftKey || e.metaKey); }
    return;
  }
  const tag = (e.target.tagName || "").toLowerCase();
  if (tag === "input" || tag === "textarea" || e.target.isContentEditable) return;
  if (!$("modal").hidden) { if (e.key === "Escape") $("modal").hidden = true; return; } // settings open
  if ($("view-torrent").hidden) return; // keys belong to the visible tab

  if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "a") {
    e.preventDefault();
    selectRows(rowOrder());
  } else if (e.key === "Delete" || e.key === "Backspace") {
    if (!selectedIds.size) return;
    e.preventDefault();
    openRemoveModal();
  } else if (e.key === "Escape") {
    hideCtx(); showDetails(false);
  } else if (e.metaKey && e.key.toLowerCase() === "i") {
    e.preventDefault();
    showDetails($("detail").hidden);
  } else if (e.key === "ArrowDown" || e.key === "ArrowUp") {
    const order = rowOrder(); if (!order.length) return;
    e.preventDefault();
    let i = order.indexOf(selectedId);
    i = i === -1 ? 0 : (e.key === "ArrowDown" ? Math.min(order.length - 1, i + 1) : Math.max(0, i - 1));
    selectTorrent(order[i]);
    document.querySelector(`#tbody tr[data-id="${order[i]}"]`)?.scrollIntoView({ block: "nearest" });
  }
});

// ---- detail pane ---------------------------------------------------------
function paintSelection() {
  document.querySelectorAll("#tbody tr").forEach((tr) => tr.classList.toggle("selected", selectedIds.has(Number(tr.dataset.id))));
  syncToolbar();
}
function selectTorrent(id) {
  selectedId = id; anchorId = id; selectedIds = new Set([id]); contentFor = null;
  paintSelection(); loadTab();
}
function selectRows(ids) {
  selectedIds = new Set(ids); selectedId = ids[ids.length - 1] ?? null; contentFor = null;
  paintSelection();
  loadTab();
}
function pickRow(id, e) {
  const picked = nextSelection(selectedIds, rowOrder(), anchorId, id, e);
  if (!picked) { selectTorrent(id); return; }
  if (e.metaKey || e.ctrlKey) anchorId = id;
  selectRows(picked);
}
// The pane is the user's to open: clicking a row only selects it.
const DETAILS_KEY = "kdownloader-details-open";
function showDetails(open) {
  $("detail").hidden = !open;
  $("details-toggle").setAttribute("aria-pressed", String(open));
  try { localStorage.setItem(DETAILS_KEY, open ? "1" : ""); } catch (e) {}
  contentFor = null;
  loadTab();
}
$("detail-close").addEventListener("click", () => showDetails(false));
$("details-toggle").addEventListener("click", () => showDetails($("detail").hidden));
try { showDetails(localStorage.getItem(DETAILS_KEY) === "1"); } catch (e) {}
document.querySelectorAll(".dtab").forEach((b) => b.addEventListener("click", () => {
  detailTab = b.dataset.tab; contentFor = null;
  document.querySelectorAll(".dtab").forEach((x) => x.classList.toggle("active", x === b));
  loadTab();
}));
function curTorrent() { return torrents.find((t) => t.id === selectedId) || null; }

function loadTab() {
  if ($("detail").hidden) return;
  if (selectedId == null) { $("detail-body").innerHTML = '<p class="muted">Select a torrent to see its details.</p>'; return; }
  if (detailTab === "general") renderGeneral();
  else if (detailTab === "trackers") fetchTrackers();
  else if (detailTab === "peers") fetchPeers();
  else if (detailTab === "content") fetchContent();
}
function renderGeneral() {
  const t = curTorrent(); if (!t) return;
  const m = metrics(t);
  const ratio = m.ratio === Infinity ? "∞" : m.ratio.toFixed(3);
  const pieces = m.snap ? m.snap.downloaded_and_checked_pieces : 0;
  patchList($("detail-body"), `<dl class="kv">
    <dt>Name</dt><dd>${esc(t.name)}</dd>
    <dt>Status</dt><dd>${LABEL[m.kind] || m.kind}</dd>
    ${m.st.error ? `<dt>Error</dt><dd class="err-text">${esc(m.st.error)}</dd>` : ""}
    <dt>Size</dt><dd>${bytes(m.done)} / ${bytes(m.total)} (${m.pct.toFixed(1)}%)</dd>
    <dt>Download / Upload</dt><dd>↓ ${m.dl > 0 ? bytes(m.dl) + "/s" : "—"} · ↑ ${m.ul > 0 ? bytes(m.ul) + "/s" : "—"}</dd>
    <dt>Downloaded / Uploaded</dt><dd>${bytes(m.done)} / ${bytes(m.up)}</dd>
    <dt>Ratio</dt><dd>${ratio}</dd>
    <dt>Peers / Swarm</dt><dd>${m.peers} connected · ${m.swarm} seen</dd>
    <dt>Pieces done</dt><dd>${pieces}</dd>
    <dt>ETA</dt><dd>${etaText(t) || "—"}</dd>
    <dt>Category</dt><dd>${esc(t.category) || "—"}</dd>
    <dt>Tags</dt><dd>${(t.tags || []).map(esc).join(", ") || "—"}</dd>
    <dt>Save path</dt><dd>${esc(t.output_folder)}</dd>
    <dt>Info hash</dt><dd>${esc(t.info_hash)}</dd>
  </dl>`);
}
async function fetchTrackers() {
  const id = selectedId; $("detail-body").innerHTML = '<p class="muted">Loading…</p>';
  try {
    const tr = await invoke("list_trackers", { id });
    if (selectedId !== id || detailTab !== "trackers") return;
    $("detail-body").innerHTML = tr.length
      ? `<table class="dtable"><thead><tr><th>Tracker URL</th></tr></thead><tbody>${tr.map((u) => `<tr><td>${esc(u)}</td></tr>`).join("")}</tbody></table>`
      : '<p class="muted">No trackers (DHT / magnet-only, or none announced).</p>';
  } catch (e) { showError(e); }
}
async function fetchPeers() {
  const id = selectedId;
  try {
    const peers = await invoke("list_peers", { id });
    if (selectedId !== id || detailTab !== "peers") return;
    $("detail-body").innerHTML = peers.length
      ? `<table class="dtable"><thead><tr><th>Address</th><th>Client</th><th>Conn</th><th>State</th><th class="num">↓ total</th><th class="num">↑ total</th></tr></thead><tbody>${peers.map((p) =>
          `<tr><td>${esc(p.addr)}</td><td>${esc(p.client || "—")}</td><td>${esc(p.kind || "—")}</td><td>${esc(p.state || "")}</td><td class="num">${bytes(p.down)}</td><td class="num">${bytes(p.up)}</td></tr>`).join("")}</tbody></table>`
      : '<p class="muted">No connected peers.</p>';
  } catch (e) { showError(e); }
}
async function fetchContent() {
  const id = selectedId;
  if (contentFor === id) return; // already loaded for this torrent; don't clobber edits
  contentFor = id;
  $("detail-body").innerHTML = '<p class="muted">Loading…</p>';
  const t = curTorrent();
  let files = [];
  try { files = await invoke("list_files", { id }); } catch (e) { showError(e); }
  if (selectedId !== id || detailTab !== "content") return;
  const body = $("detail-body"); body.innerHTML = "";
  const meta = document.createElement("div");
  meta.className = "detail-row";
  meta.innerHTML = `<input type="text" class="d-cat" placeholder="category" value="${esc(t?.category || "")}" />
    <input type="text" class="d-tags" placeholder="tags (comma-separated)" value="${esc((t?.tags || []).join(", "))}" />
    <button class="ghost d-save">Save</button>`;
  body.appendChild(meta);
  meta.querySelector(".d-save").addEventListener("click", async () => {
    try {
      await invoke("set_category", { id, category: meta.querySelector(".d-cat").value.trim() });
      await invoke("set_tags", { id, tags: meta.querySelector(".d-tags").value.split(",").map((s) => s.trim()).filter(Boolean) });
      await refresh();
    } catch (e) { showError(e); }
  });
  if (!files.length) { const p = document.createElement("p"); p.className = "muted"; p.textContent = "File list not available yet (metadata resolving)."; body.appendChild(p); return; }
  const list = document.createElement("div"); list.className = "files-list";
  list.innerHTML = files.map((f) => `<label class="file-item"><input type="checkbox" data-idx="${f.index}" ${f.selected ? "checked" : ""} /><span class="fname" title="${esc(f.name)}">${esc(f.name)}</span><span class="fsize">${bytes(f.len)}</span></label>`).join("");
  body.appendChild(list);
  const bar = document.createElement("div"); bar.className = "detail-row";
  bar.innerHTML = `<button class="ghost f-all">Select all</button><button class="ghost f-none">None</button><button class="primary f-apply">Apply selection</button>`;
  body.appendChild(bar);
  const boxes = () => [...list.querySelectorAll('input[type="checkbox"]')];
  bar.querySelector(".f-all").addEventListener("click", () => boxes().forEach((b) => (b.checked = true)));
  bar.querySelector(".f-none").addEventListener("click", () => boxes().forEach((b) => (b.checked = false)));
  bar.querySelector(".f-apply").addEventListener("click", async () => {
    try { await invoke("set_files", { id, indices: boxes().filter((b) => b.checked).map((b) => Number(b.dataset.idx)) }); }
    catch (e) { showError(e); }
  });
}

// ---- settings (unchanged logic) -----------------------------------------
function feedRow(feed = { url: "", category: "", save_path: "", rules: [] }) {
  const el = document.createElement("div"); el.className = "feed";
  el.innerHTML = `<div class="feed-head"><input type="text" class="f-url" placeholder="Feed URL (RSS)" value="${esc(feed.url)}" /><button class="ghost f-del" title="Remove feed">✕</button></div>
    <div class="grid2"><input type="text" class="f-cat" placeholder="category" value="${esc(feed.category)}" /><span class="withbtn"><input type="text" class="f-save" placeholder="save path (optional)" value="${esc(feed.save_path || "")}" /><button type="button" class="ghost f-browse">Browse…</button></span></div>
    <textarea class="f-rules" placeholder="title regex rules, one per line (empty = all)">${esc((feed.rules || []).join("\n"))}</textarea>`;
  el.querySelector(".f-del").addEventListener("click", () => el.remove());
  el.querySelector(".f-browse").addEventListener("click", () => browseInto(el.querySelector(".f-save")));
  return el;
}
async function syncDefaultButton() {
  const isDefault = await invoke("is_default_handler").catch(() => false);
  $("make-default").disabled = isDefault;
  $("make-default").textContent = isDefault ? "KDownloader opens magnet links and .torrent files" : "Open magnet links and .torrent files with KDownloader";
}
$("make-default").addEventListener("click", () => invoke("make_default_handler").then(syncDefaultButton).catch(showError));

function openSettings() {
  const s = settingsCache || {};
  syncDefaultButton();
  $("s-downloaddir").value = s.download_dir || "";
  $("s-gdown").value = s.global_down_kbps ?? 0; $("s-gup").value = s.global_up_kbps ?? 0;
  $("s-sched").checked = !!s.scheduler_enabled; $("s-from").value = toHHMM(s.sched_from_min ?? 480); $("s-to").value = toHHMM(s.sched_to_min ?? 1080);
  $("s-adown").value = s.alt_down_kbps ?? 0; $("s-aup").value = s.alt_up_kbps ?? 0;
  $("s-watch").checked = !!s.watch_enabled; $("s-watchdir").value = s.watch_dir || ""; $("s-watchcat").value = s.watch_category || "";
  $("s-rss").checked = !!s.rss_enabled; $("s-rssint").value = s.rss_interval_min ?? 15;
  $("s-port").value = s.listen_port ?? 51413; $("s-upnp").checked = s.upnp_enabled ?? true; $("s-utp").checked = !!s.utp_enabled; $("s-bind").value = s.bind_device || "";
  const fe = $("feeds"); fe.innerHTML = ""; (s.feeds || []).forEach((f) => fe.appendChild(feedRow(f)));
  $("modal").hidden = false;
}
async function saveSettings() {
  const feeds = [...$("feeds").querySelectorAll(".feed")].map((el) => ({
    url: el.querySelector(".f-url").value.trim(), category: el.querySelector(".f-cat").value.trim(),
    save_path: el.querySelector(".f-save").value.trim() || null,
    rules: el.querySelector(".f-rules").value.split("\n").map((r) => r.trim()).filter(Boolean),
  })).filter((f) => f.url);
  const settings = {
    download_dir: val("s-downloaddir") || settingsCache?.download_dir || "",
    global_down_kbps: num("s-gdown"), global_up_kbps: num("s-gup"), alt_down_kbps: num("s-adown"), alt_up_kbps: num("s-aup"),
    scheduler_enabled: $("s-sched").checked, sched_from_min: fromHHMM($("s-from").value), sched_to_min: fromHHMM($("s-to").value),
    watch_enabled: $("s-watch").checked, watch_dir: val("s-watchdir"), watch_category: val("s-watchcat"),
    rss_enabled: $("s-rss").checked, rss_interval_min: Math.max(1, num("s-rssint")), feeds,
    listen_port: Math.min(65535, Math.max(1, num("s-port") || 51413)), upnp_enabled: $("s-upnp").checked, utp_enabled: $("s-utp").checked, bind_device: val("s-bind"),
  };
  const portChanged = settingsCache && (settings.listen_port !== settingsCache.listen_port || settings.upnp_enabled !== settingsCache.upnp_enabled || settings.utp_enabled !== settingsCache.utp_enabled || settings.bind_device !== settingsCache.bind_device);
  try {
    await invoke("save_settings", { settings }); settingsCache = settings; updateLimitsBadge(); updateDirLabel(); $("modal").hidden = true;
    if (portChanged) showNotice("Network settings saved — restart KDownloader for the new port/UPnP to take effect.");
  }
  catch (e) { showError(e); }
}
$("settings-btn").addEventListener("click", openSettings);
$("limits-badge").addEventListener("click", () => { openSettings(); $("s-gdown").select(); });
$("modal-close").addEventListener("click", () => ($("modal").hidden = true));
$("modal-cancel").addEventListener("click", () => ($("modal").hidden = true));
$("modal-save").addEventListener("click", saveSettings);
$("add-feed").addEventListener("click", () => $("feeds").appendChild(feedRow()));

// ---- status bar ----------------------------------------------------------
function updateLimitsBadge() {
  const s = settingsCache; if (!s) { $("limits-badge").textContent = ""; return; }
  const f = (v) => (v > 0 ? v : "∞");
  $("limits-badge").textContent = `limit ↓${f(s.global_down_kbps)} ↑${f(s.global_up_kbps)} KiB/s${s.scheduler_enabled ? " ⏱" : ""}`;
}
function updateDirLabel() {
  const dir = settingsCache?.download_dir || "";
  const base = dir.split("/").filter(Boolean).pop() || dir;
  $("reveal").textContent = dir ? `📁 ${base}` : "Download folder";
  $("reveal").title = dir ? `Reveal ${dir} in Finder` : "Reveal in Finder";
}
$("reveal").addEventListener("click", async () => {
  try { const dir = settingsCache?.download_dir || (await invoke("download_dir")); await invoke("reveal_path", { path: dir }); }
  catch (e) { showError(e); }
});

// ---- poll loop -----------------------------------------------------------
let refreshing = false;
async function refresh() {
  if (refreshing) return;
  refreshing = true;
  try { torrents = await invoke("list_torrents"); } catch (e) { showError(e); return; } finally { refreshing = false; }
  // The Bandwidth tab reads `torrents`, so only the drawing is skipped when hidden.
  if ($("view-torrent").hidden) return;
  if (selectedId != null && !torrents.some((t) => t.id === selectedId)) { selectedId = null; loadTab(); }
  renderSidebar(); renderTable(); syncToolbar();
  let td = 0, tu = 0;
  for (const t of torrents) { const m = metrics(t); td += m.dl; tu += m.ul; }
  setText($("totals"), `↓ ${bytes(td)}/s\u00a0\u00a0↑ ${bytes(tu)}/s`);
  // live detail refresh
  if (!$("detail").hidden && selectedId != null) {
    if (detailTab === "general") renderGeneral();
    else if (detailTab === "peers") fetchPeers();
  }
}
window.addEventListener("kdownloader-tab", refresh);

// ---- about ---------------------------------------------------------------
$("about-logo").append(document.querySelector("#about-open .logo").cloneNode(true));
$("about-open").addEventListener("click", async () => {
  try { $("about-title").textContent = `KDownloader ${await window.__TAURI__.app.getVersion()}`; } catch (e) {}
  $("about-modal").hidden = false;
  $("about-close").focus();
});
$("about-close").addEventListener("click", () => ($("about-modal").hidden = true));
$("about-donate").addEventListener("click", () => invoke("open_donation_page").catch(showError));
document.addEventListener("keydown", (e) => { if (e.key === "Escape") $("about-modal").hidden = true; });

// Sidebar entries work from the keyboard, and icon buttons say what they do.
document.addEventListener("keydown", (e) => {
  if (e.key === "Enter" && e.target.matches?.("li[tabindex]")) e.target.click();
});
document.querySelectorAll("button[title]").forEach((b) => b.setAttribute("aria-label", b.title));
async function init() {
  try { settingsCache = await invoke("get_settings"); updateLimitsBadge(); updateDirLabel(); } catch (e) { showError(e); }
  await refresh();
  setInterval(refresh, 1000);
}
init();
