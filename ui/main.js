const { invoke } = window.__TAURI__.core;

const $ = (id) => document.getElementById(id);
const val = (id) => $(id).value.trim();
const num = (id) => Math.max(0, parseInt($(id).value, 10) || 0);

const els = {
  form: $("add-form"), magnet: $("magnet"), file: $("file"),
  opts: $("opts"), optsToggle: $("opts-toggle"),
  list: $("list"), empty: $("empty"), error: $("error"),
  count: $("count"), totals: $("totals"), limitsBadge: $("limits-badge"),
  reveal: $("reveal"), catFilter: $("cat-filter"), tpl: $("row-tpl"),
};

let settingsCache = null;
const openRows = new Set();      // ids whose detail panel is expanded
let filterCat = "";

// ---- helpers -------------------------------------------------------------

function showError(msg) {
  els.error.textContent = String(msg);
  els.error.hidden = false;
  clearTimeout(showError._t);
  showError._t = setTimeout(() => (els.error.hidden = true), 6000);
}

function bytes(n) {
  if (n == null || isNaN(n)) return "—";
  n = Number(n);
  const u = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i ? 1 : 0)} ${u[i]}`;
}
function speed(s) {
  if (s == null) return "0 B/s";
  if (typeof s === "number") return `${bytes(s)}/s`;
  if (typeof s === "string") return s;
  if (typeof s === "object") {
    if (typeof s.human_readable === "string") return s.human_readable;
    if (typeof s.mbps === "number") return s.mbps >= 1 ? `${s.mbps.toFixed(1)} MB/s` : `${(s.mbps * 1024).toFixed(0)} KB/s`;
    if (typeof s.bytes_per_second === "number") return `${bytes(s.bytes_per_second)}/s`;
  }
  return "—";
}
function speedBps(s) {
  if (typeof s === "number") return s;
  if (s && typeof s === "object") {
    if (typeof s.bytes_per_second === "number") return s.bytes_per_second;
    if (typeof s.mbps === "number") return s.mbps * 1024 * 1024;
  }
  return 0;
}
function eta(t) {
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
const LABEL = { downloading: "Downloading", seeding: "Seeding", completed: "Completed", paused: "Paused", error: "Error", initializing: "Initializing", idle: "Idle", live: "Downloading" };
const toHHMM = (m) => `${String(Math.floor(m / 60)).padStart(2, "0")}:${String(m % 60).padStart(2, "0")}`;
const fromHHMM = (s) => { const [h, m] = (s || "0:0").split(":").map(Number); return (h || 0) * 60 + (m || 0); };

// ---- add options ---------------------------------------------------------

function addOpts() {
  return {
    category: val("o-category"),
    savePath: val("o-savepath") || null,
    paused: $("o-paused").checked,
    downKbps: num("o-down"),
    upKbps: num("o-up"),
  };
}

// native folder picker → fills the given input
async function browseInto(input) {
  try {
    const p = await invoke("pick_folder");
    if (p) input.value = p;
  } catch (e) { showError(e); }
}
// delegation for static Browse buttons (data-target points at an input id)
document.addEventListener("click", (e) => {
  const b = e.target.closest("button.browse[data-target]");
  if (b) { const inp = $(b.dataset.target); if (inp) browseInto(inp); }
});

els.optsToggle.addEventListener("click", () => {
  els.opts.hidden = !els.opts.hidden;
  els.optsToggle.textContent = els.opts.hidden ? "Options ▾" : "Options ▴";
});

els.form.addEventListener("submit", async (ev) => {
  ev.preventDefault();
  const source = els.magnet.value.trim();
  if (!source) return;
  try {
    await invoke("add_torrent", { source, opts: addOpts() });
    els.magnet.value = "";
    await refresh();
  } catch (e) { showError(e); }
});

els.file.addEventListener("change", async () => {
  const f = els.file.files[0];
  if (!f) return;
  try {
    const buf = new Uint8Array(await f.arrayBuffer());
    await invoke("add_torrent_bytes", { bytes: Array.from(buf), opts: addOpts() });
    await refresh();
  } catch (e) { showError(e); } finally { els.file.value = ""; }
});

// ---- torrent rows --------------------------------------------------------

function buildRow(id) {
  const node = els.tpl.content.firstElementChild.cloneNode(true);
  node.dataset.id = id;
  node.querySelector(".files").addEventListener("click", () => toggleDetail(node));
  node.querySelector(".toggle").addEventListener("click", () => onToggle(node));
  node.querySelector(".remove").addEventListener("click", () => onRemove(node));
  els.list.appendChild(node);
  return node;
}

function render(torrents) {
  // category filter options
  const cats = [...new Set(torrents.map((t) => t.category).filter(Boolean))].sort();
  const cur = els.catFilter.value;
  els.catFilter.innerHTML = '<option value="">All categories</option>' +
    cats.map((c) => `<option value="${c}">${c}</option>`).join("");
  els.catFilter.value = cats.includes(cur) ? cur : "";
  filterCat = els.catFilter.value;

  const shown = torrents.filter((t) => !filterCat || t.category === filterCat);
  const seen = new Set();
  let totalDown = 0, totalUp = 0;

  for (const t of shown) {
    seen.add(String(t.id));
    let row = els.list.querySelector(`.row[data-id="${t.id}"]`);
    if (!row) row = buildRow(t.id);
    row._t = t;

    const st = t.stats || {};
    const total = Number(st.total_bytes || 0);
    const done = Number(st.progress_bytes || 0);
    const pct = total > 0 ? Math.min(100, (done / total) * 100) : st.finished ? 100 : 0;
    const kind = classify(t);

    row.className = `row is-${kind}` + (openRows.has(t.id) ? " open" : "");
    row.dataset.folder = t.output_folder || "";
    const nm = row.querySelector(".name");
    nm.textContent = t.name || `torrent ${t.id}`; nm.title = t.name || "";

    const cat = row.querySelector(".cat");
    cat.hidden = !t.category; cat.textContent = t.category || "";

    const badge = row.querySelector(".state");
    badge.textContent = LABEL[kind] || kind; badge.className = `badge state ${kind}`;

    row.querySelector(".bar-fill").style.width = `${pct}%`;
    row.querySelector(".pct").textContent = `${pct.toFixed(1)}%`;
    row.querySelector(".size").textContent = total > 0 ? `${bytes(done)} / ${bytes(total)}` : bytes(done);

    const live = st.live || null;
    const d = live ? live.download_speed : 0, u = live ? live.upload_speed : 0;
    row.querySelector(".dl").textContent = `↓ ${t.paused ? "0 B/s" : speed(d)}`;
    row.querySelector(".ul").textContent = `↑ ${t.paused ? "0 B/s" : speed(u)}`;
    if (!t.paused) { totalDown += speedBps(d); totalUp += speedBps(u); }

    const e = t.paused ? "" : eta(t);
    row.querySelector(".eta").textContent = e;
    row.querySelector(".eta-sep").classList.toggle("hidden", !e);
    row.querySelector(".toggle").textContent = t.paused ? "▶" : "⏸";
  }

  for (const row of [...els.list.querySelectorAll(".row")]) {
    if (!seen.has(row.dataset.id)) { openRows.delete(Number(row.dataset.id)); row.remove(); }
  }

  const n = shown.length;
  els.empty.style.display = torrents.length ? "none" : "flex";
  els.count.textContent = `${n} torrent${n === 1 ? "" : "s"}` + (filterCat ? ` in “${filterCat}”` : "");
  els.totals.innerHTML = `↓ ${bytes(totalDown)}/s&nbsp;&nbsp;↑ ${bytes(totalUp)}/s`;
}

// ---- row detail (files + category + tags) --------------------------------

async function toggleDetail(row) {
  const id = Number(row.dataset.id);
  const detail = row.querySelector(".row-detail");
  if (openRows.has(id)) {
    openRows.delete(id); detail.hidden = true; detail.innerHTML = ""; row.classList.remove("open");
    return;
  }
  openRows.add(id); row.classList.add("open"); detail.hidden = false;
  detail.innerHTML = '<p class="muted">Loading…</p>';
  const t = row._t || {};

  let files = [];
  try { files = await invoke("list_files", { id }); } catch (e) { showError(e); }

  detail.innerHTML = "";

  // category + tags editor
  const meta = document.createElement("div");
  meta.className = "detail-row";
  meta.innerHTML =
    `<input type="text" class="d-cat" placeholder="category" value="${t.category || ""}" />` +
    `<input type="text" class="d-tags" placeholder="tags (comma-separated)" value="${(t.tags || []).join(", ")}" />` +
    `<button class="ghost d-save">Save</button>`;
  detail.appendChild(meta);
  meta.querySelector(".d-save").addEventListener("click", async () => {
    try {
      await invoke("set_category", { id, category: meta.querySelector(".d-cat").value.trim() });
      const tags = meta.querySelector(".d-tags").value.split(",").map((s) => s.trim()).filter(Boolean);
      await invoke("set_tags", { id, tags });
      await refresh();
    } catch (e) { showError(e); }
  });

  if (!files.length) {
    const p = document.createElement("p");
    p.className = "muted"; p.textContent = "File list not available yet (metadata still resolving).";
    detail.appendChild(p);
    return;
  }

  // file selection
  const listEl = document.createElement("div");
  listEl.className = "files-list";
  for (const f of files) {
    const item = document.createElement("label");
    item.className = "file-item";
    item.innerHTML =
      `<input type="checkbox" data-idx="${f.index}" ${f.selected ? "checked" : ""} />` +
      `<span class="fname" title="${f.name}">${f.name}</span>` +
      `<span class="fsize">${bytes(f.len)}</span>`;
    listEl.appendChild(item);
  }
  detail.appendChild(listEl);

  const apply = document.createElement("div");
  apply.className = "detail-row";
  apply.style.marginTop = "8px";
  apply.innerHTML = `<button class="ghost f-all">Select all</button><button class="ghost f-none">None</button><button class="primary f-apply">Apply selection</button>`;
  detail.appendChild(apply);
  const boxes = () => [...listEl.querySelectorAll('input[type="checkbox"]')];
  apply.querySelector(".f-all").addEventListener("click", () => boxes().forEach((b) => (b.checked = true)));
  apply.querySelector(".f-none").addEventListener("click", () => boxes().forEach((b) => (b.checked = false)));
  apply.querySelector(".f-apply").addEventListener("click", async () => {
    const indices = boxes().filter((b) => b.checked).map((b) => Number(b.dataset.idx));
    try { await invoke("set_files", { id, indices }); } catch (e) { showError(e); }
  });
}

// ---- actions -------------------------------------------------------------

async function onToggle(row) {
  const id = Number(row.dataset.id);
  const paused = row.classList.contains("is-paused");
  try { await invoke(paused ? "resume_torrent" : "pause_torrent", { id }); await refresh(); }
  catch (e) { showError(e); }
}

let rmTarget = null;
function onRemove(row) {
  rmTarget = Number(row.dataset.id);
  $("rm-name").textContent = row.querySelector(".name").textContent;
  $("rm-modal").hidden = false;
}
async function doRemove(deleteFiles) {
  $("rm-modal").hidden = true;
  if (rmTarget == null) return;
  try { await invoke("remove_torrent", { id: rmTarget, deleteFiles }); await refresh(); }
  catch (e) { showError(e); } finally { rmTarget = null; }
}
$("rm-cancel").addEventListener("click", () => ($("rm-modal").hidden = true));
$("rm-keep").addEventListener("click", () => doRemove(false));
$("rm-del").addEventListener("click", () => doRemove(true));

els.catFilter.addEventListener("change", () => refresh());

function updateDirLabel() {
  const dir = settingsCache?.download_dir || "";
  const base = dir.split("/").filter(Boolean).pop() || dir;
  els.reveal.textContent = dir ? `📁 ${base}` : "Download folder";
  els.reveal.title = dir ? `Reveal ${dir} in Finder` : "Reveal in Finder";
}

els.reveal.addEventListener("click", async () => {
  try {
    const dir = settingsCache?.download_dir || (await invoke("download_dir"));
    await invoke("reveal_path", { path: dir });
  } catch (e) { showError(e); }
});

// ---- settings modal ------------------------------------------------------

function feedRow(feed = { url: "", category: "", save_path: "", rules: [] }) {
  const el = document.createElement("div");
  el.className = "feed";
  el.innerHTML =
    `<div class="feed-head"><input type="text" class="f-url" placeholder="Feed URL (RSS)" value="${feed.url || ""}" />` +
    `<button class="ghost f-del" title="Remove feed">✕</button></div>` +
    `<div class="grid2"><input type="text" class="f-cat" placeholder="category" value="${feed.category || ""}" />` +
    `<span class="withbtn"><input type="text" class="f-save" placeholder="save path (optional)" value="${feed.save_path || ""}" />` +
    `<button type="button" class="ghost f-browse">Browse…</button></span></div>` +
    `<textarea class="f-rules" placeholder="title regex rules, one per line (empty = all)">${(feed.rules || []).join("\n")}</textarea>`;
  el.querySelector(".f-del").addEventListener("click", () => el.remove());
  el.querySelector(".f-browse").addEventListener("click", () => browseInto(el.querySelector(".f-save")));
  return el;
}

function openSettings() {
  const s = settingsCache || {};
  $("s-downloaddir").value = s.download_dir || "";
  $("s-gdown").value = s.global_down_kbps ?? 0;
  $("s-gup").value = s.global_up_kbps ?? 0;
  $("s-sched").checked = !!s.scheduler_enabled;
  $("s-from").value = toHHMM(s.sched_from_min ?? 480);
  $("s-to").value = toHHMM(s.sched_to_min ?? 1080);
  $("s-adown").value = s.alt_down_kbps ?? 0;
  $("s-aup").value = s.alt_up_kbps ?? 0;
  $("s-watch").checked = !!s.watch_enabled;
  $("s-watchdir").value = s.watch_dir || "";
  $("s-watchcat").value = s.watch_category || "";
  $("s-rss").checked = !!s.rss_enabled;
  $("s-rssint").value = s.rss_interval_min ?? 15;
  const feeds = $("feeds"); feeds.innerHTML = "";
  (s.feeds || []).forEach((f) => feeds.appendChild(feedRow(f)));
  $("modal").hidden = false;
}

async function saveSettings() {
  const feeds = [...$("feeds").querySelectorAll(".feed")].map((el) => ({
    url: el.querySelector(".f-url").value.trim(),
    category: el.querySelector(".f-cat").value.trim(),
    save_path: el.querySelector(".f-save").value.trim() || null,
    rules: el.querySelector(".f-rules").value.split("\n").map((r) => r.trim()).filter(Boolean),
  })).filter((f) => f.url);

  const settings = {
    download_dir: val("s-downloaddir") || settingsCache?.download_dir || "",
    global_down_kbps: num("s-gdown"), global_up_kbps: num("s-gup"),
    alt_down_kbps: num("s-adown"), alt_up_kbps: num("s-aup"),
    scheduler_enabled: $("s-sched").checked,
    sched_from_min: fromHHMM($("s-from").value), sched_to_min: fromHHMM($("s-to").value),
    watch_enabled: $("s-watch").checked, watch_dir: val("s-watchdir"), watch_category: val("s-watchcat"),
    rss_enabled: $("s-rss").checked, rss_interval_min: Math.max(1, num("s-rssint")), feeds,
  };
  try {
    await invoke("save_settings", { settings });
    settingsCache = settings;
    updateLimitsBadge();
    updateDirLabel();
    $("modal").hidden = true;
  } catch (e) { showError(e); }
}

$("settings-btn").addEventListener("click", openSettings);
$("modal-close").addEventListener("click", () => ($("modal").hidden = true));
$("modal-cancel").addEventListener("click", () => ($("modal").hidden = true));
$("modal-save").addEventListener("click", saveSettings);
$("add-feed").addEventListener("click", () => $("feeds").appendChild(feedRow()));

function updateLimitsBadge() {
  const s = settingsCache;
  if (!s) { els.limitsBadge.textContent = ""; return; }
  const f = (v) => (v > 0 ? v : "∞");
  let txt = `limit ↓${f(s.global_down_kbps)} ↑${f(s.global_up_kbps)} KiB/s`;
  if (s.scheduler_enabled) txt += " ⏱";
  els.limitsBadge.textContent = txt;
}

// ---- poll loop -----------------------------------------------------------

async function refresh() {
  try { render(await invoke("list_torrents")); }
  catch (e) { showError(e); }
}

async function init() {
  try { settingsCache = await invoke("get_settings"); updateLimitsBadge(); updateDirLabel(); } catch (e) { showError(e); }
  await refresh();
  setInterval(refresh, 1000);
}
init();
