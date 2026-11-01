// The Direct Download tab. Scoped so that it shares no globals with main.js.
(() => {
const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (id) => document.getElementById(`dd-${id}`);

const STATUS = {
  queued: "Queued", active: "Downloading", paused: "Paused",
  extracting: "Extracting…", done: "Finished", failed: "Failed",
};
const PROVIDERS = {
  "real-debrid": "Real-Debrid", "all-debrid": "AllDebrid", premiumize: "Premiumize", "debrid-link": "Debrid-Link",
  "uploaded-premium-link": "Uploadedpremiumlink", torbox: "TorBox", "mega-debrid": "Mega-Debrid",
  cocoleech: "Cocoleech", fakirdebrid: "Fakirdebrid",
};

// A .torrent URL or magnet belongs to the Torrent tab, everything else here.
const TORRENT_LINKS = /magnet:\?[^\s"'<>]+|https?:\/\/[^\s"'<>]+\.torrent(?:\?[^\s"'<>]*)?/gi;

let grabbed = [];
let grabbing = 0;
let toastTimer;

function showError(e) {
  const toast = $("toast");
  toast.textContent = String(e);
  toast.hidden = false;
  clearTimeout(toastTimer);
  toastTimer = setTimeout(() => (toast.hidden = true), 5000);
}

async function call(cmd, args) {
  try { return await invoke(cmd, args); } catch (e) { showError(e); }
}

function el(tag, className, text) {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function button(label, onClick) {
  const b = el("button", "", label);
  b.addEventListener("click", onClick);
  return b;
}

function bytes(n) {
  if (n == null) return "?";
  const units = ["B", "KB", "MB", "GB", "TB"];
  let i = 0;
  while (n >= 1024 && i < units.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(i && n < 100 ? 1 : 0)} ${units[i]}`;
}

function duration(s) {
  if (!isFinite(s)) return "";
  if (s < 60) return `${Math.ceil(s)}s`;
  if (s < 3600) return `${Math.floor(s / 60)}m ${Math.ceil(s % 60)}s`;
  return `${Math.floor(s / 3600)}h ${Math.floor((s % 3600) / 60)}m`;
}

function showApp(name) {
  for (const t of ["torrent", "direct"]) {
    document.getElementById(`app-tab-${t}`).classList.toggle("active", t === name);
    document.getElementById(`view-${t}`).hidden = t !== name;
  }
}

function routeClipboard(text) {
  const torrents = text.match(TORRENT_LINKS) || [];
  if (torrents.length) {
    // Only prefilled: adding a torrent starts sharing, so it stays a user action.
    document.getElementById("magnet").value = torrents[0];
    showApp("torrent");
  }
  const rest = text.replace(TORRENT_LINKS, " ");
  if (/https?:\/\//i.test(rest)) analyze(rest, true);
}

function showTab(name) {
  for (const t of ["downloads", "grabber"]) {
    $(`tab-${t}`).classList.toggle("active", t === name);
    $(`view-${t}`).hidden = t !== name;
  }
}

function itemRow(item) {
  const row = el("div", `item ${item.status}`);
  const top = el("div", "item-top");
  top.append(el("div", "item-name", item.name));
  top.lastChild.title = item.url;

  const actions = el("div", "item-actions");
  if (item.status === "active" || item.status === "queued") {
    actions.append(button("Pause", () => call("dd_pause", { id: item.id }).then(refresh)));
  }
  if (item.status === "paused" || item.status === "failed") {
    actions.append(button(item.status === "failed" ? "Retry" : "Resume", () => call("dd_resume", { id: item.id }).then(refresh)));
  }
  if (item.output) {
    actions.append(button("Show", () => call("dd_reveal", { path: item.output })));
  }
  actions.append(button("Remove", () => call("dd_remove", { id: item.id, deleteFile: false }).then(refresh)));
  top.append(actions);
  row.append(top);

  const finished = item.status === "done" || item.status === "extracting";
  const ratio = finished ? 1 : item.size ? item.done / item.size : 0;
  const bar = el("div", "bar");
  const fill = el("div");
  fill.style.width = `${Math.min(100, ratio * 100)}%`;
  bar.append(fill);
  row.append(bar);

  const meta = el("div", "item-meta");
  meta.append(el("span", "", STATUS[item.status]));
  meta.append(el("span", "", `${bytes(finished ? item.size : item.done)} / ${bytes(item.size)}`));
  if (item.provider) meta.append(el("span", "", PROVIDERS[item.provider]));
  if (item.status === "active") {
    meta.append(el("span", "", `${bytes(item.speed)}/s`));
    meta.append(el("span", "", `${item.segments.length} connection${item.segments.length > 1 ? "s" : ""}`));
    if (item.size && item.speed) meta.append(el("span", "", duration((item.size - item.done) / item.speed)));
  }
  if (item.error) meta.append(el("span", "error", item.error));
  row.append(meta);
  return row;
}

async function refresh() {
  const items = await invoke("dd_list").catch(() => null);
  if (!items) return;
  const packages = new Map();
  for (const item of items) {
    if (!packages.has(item.package)) packages.set(item.package, []);
    packages.get(item.package).push(item);
  }
  const root = $("packages");
  const nodes = [];
  for (const [name, members] of packages) {
    const box = el("section", "package");
    const head = el("div", "package-head", name);
    const left = members.filter((m) => m.status !== "done").length;
    head.append(el("span", "muted", left ? `${members.length - left}/${members.length} finished` : "Finished"));
    box.append(head, ...members.map(itemRow));
    nodes.push(box);
  }
  // Rebuilding under a held mouse button would swallow the click.
  if (!root.matches(":active")) root.replaceChildren(...nodes);
  $("empty").hidden = items.length > 0;
  const speed = items.reduce((sum, i) => sum + (i.speed || 0), 0);
  $("total-speed").textContent = speed ? `${bytes(speed)}/s` : "";
}

function renderGrabbed() {
  const body = $("grabbed");
  body.replaceChildren(...grabbed.map((p) => {
    const tr = el("tr");
    const check = el("input");
    check.type = "checkbox";
    check.checked = p.selected;
    check.disabled = !p.online;
    check.addEventListener("change", () => { p.selected = check.checked; updateGrabButtons(); });
    const td = el("td");
    td.append(check);
    let state = p.online ? (p.resumable ? "Online" : "Online (no resume)") : p.error || "Offline";
    if (p.online && p.provider) state = PROVIDERS[p.provider];
    if (p.duplicate) state = "Already in queue";
    const name = el("td", "", p.name);
    name.title = p.url;
    tr.append(td, name, el("td", "", p.package), el("td", "", bytes(p.size)), el("td", p.online ? "" : "error", state));
    return tr;
  }));
  updateGrabButtons();
}

function updateGrabButtons() {
  const count = grabbed.filter((p) => p.selected).length;
  $("add-selected").disabled = count === 0;
  $("grab-count").hidden = grabbed.length === 0;
  $("grab-count").textContent = grabbed.length;
  const appCount = document.getElementById("app-grab-count");
  appCount.hidden = grabbed.length === 0;
  appCount.textContent = grabbed.length;
}

async function analyze(text, fromClipboard = false) {
  grabbing++;
  $("grab-status").textContent = "Analyzing…";
  const found = await call("dd_grab", { text, filesOnly: fromClipboard });
  grabbing--;
  if (!grabbing) $("grab-status").textContent = "";
  if (!found) return;
  if (!found.length) {
    if (!fromClipboard) showError("No links found");
    return;
  }
  for (const probe of found) {
    if (!grabbed.some((p) => p.url === probe.url)) {
      grabbed.push({ ...probe, selected: probe.online && !probe.duplicate });
    }
  }
  renderGrabbed();
  if (fromClipboard) {
    showApp("direct");
    showTab("grabber");
  }
}

$("tab-downloads").addEventListener("click", () => showTab("downloads"));
$("tab-grabber").addEventListener("click", () => showTab("grabber"));
$("resume-all").addEventListener("click", () => call("dd_resume_all").then(refresh));
$("pause-all").addEventListener("click", () => call("dd_pause_all").then(refresh));
$("clear-done").addEventListener("click", () => call("dd_clear_done").then(refresh));

$("analyze").addEventListener("click", async () => {
  const text = $("links").value;
  if (!text.trim()) return;
  await analyze(text);
  $("links").value = "";
});

$("add-selected").addEventListener("click", async () => {
  const chosen = grabbed.filter((p) => p.selected);
  await call("dd_add", { probes: chosen });
  grabbed = grabbed.filter((p) => !p.selected);
  renderGrabbed();
  showTab("downloads");
  refresh();
});

$("clear-grabbed").addEventListener("click", () => { grabbed = []; renderGrabbed(); });

function accountStatus(view) {
  if (view.error) return ["error", view.error];
  if (!view.connected) return ["muted", "Not connected"];
  const { username, premium, expires } = view.account;
  if (!premium) return ["error", `${username}: no active premium`];
  const until = expires ? ` until ${new Date(expires * 1000).toLocaleDateString("en-GB")}` : "";
  return ["muted", `${username}: premium${until}`];
}

async function renderAccounts() {
  const views = await call("dd_list_accounts");
  if (!views) return;
  $("accounts").replaceChildren(...views.map((view) => {
    const box = el("div", "account");
    const head = el("div", "account-head");
    const [cls, text] = accountStatus(view);
    head.append(el("strong", "", view.name), el("span", cls, text));
    box.append(head);
    const row = el("div", "row");
    if (view.connected) {
      row.append(button("Disconnect", async (e) => {
        e.preventDefault();
        await call("dd_disconnect_account", { provider: view.id });
        renderAccounts();
      }));
    } else {
      const input = el("input");
      input.type = "password";
      input.placeholder = "API key";
      input.autocomplete = "off";
      row.append(input, button("Connect", async (e) => {
        e.preventDefault();
        const key = input.value.trim();
        if (!key) return;
        input.value = "";
        if (await call("dd_connect_account", { provider: view.id, key })) renderAccounts();
      }), button("Get key", (e) => {
        e.preventDefault();
        call("dd_open_key_page", { provider: view.id });
      }));
    }
    box.append(row);
    return box;
  }));
}

$("open-settings").addEventListener("click", async () => {
  const s = await call("dd_get_settings");
  if (!s) return;
  $("s-dir").value = s.download_dir;
  $("s-active").value = s.max_active;
  $("s-chunks").value = s.chunks;
  $("s-limit").value = s.speed_limit_kib;
  $("s-extract").checked = s.auto_extract;
  $("s-delete").checked = s.delete_archive;
  $("s-clipboard").checked = s.watch_clipboard;
  $("settings").showModal();
  renderAccounts();
});

$("s-browse").addEventListener("click", async () => {
  const path = await call("pick_folder");
  if (path) $("s-dir").value = path;
});

$("s-save").addEventListener("click", () => {
  call("dd_set_settings", {
    settings: {
      download_dir: $("s-dir").value,
      max_active: Number($("s-active").value) || 3,
      chunks: Number($("s-chunks").value) || 4,
      speed_limit_kib: Number($("s-limit").value) || 0,
      auto_extract: $("s-extract").checked,
      delete_archive: $("s-delete").checked,
      watch_clipboard: $("s-clipboard").checked,
    },
  });
});

document.getElementById("app-tab-torrent").addEventListener("click", () => showApp("torrent"));
document.getElementById("app-tab-direct").addEventListener("click", () => showApp("direct"));
listen("clipboard", (event) => routeClipboard(event.payload));
listen("clipboard-error", (event) => showError(`Clipboard unavailable: ${event.payload}`));

refresh();
setInterval(refresh, 1000);
})();
