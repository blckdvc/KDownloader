// The Bandwidth tab's map: where the torrent peers and Direct Download servers
// we exchange data with are. Addresses are placed by the app, offline.
(() => {
const $ = (id) => document.getElementById(id);
const NS = "http://www.w3.org/2000/svg";
const svg = $("map-svg"), tip = $("map-tip");
const VIEW_KEY = "kdownloader-bandwidth-view";
const POLL_MS = 2000;
const CLUSTER_PX = 14;
const IDLE = 1024; // below 1 KB/s a place is drawn without an arc

let world = null, lands = null;
let snap = null;
let zoom = 1, pan = [0, 0];
const shown = { down: true, up: true, dd: true };
const countryOf = new Map(); // "lon,lat" → atlas feature

const rate = (n) => `${bytes(n)}/s`;
const el = (tag, attrs, parent) => {
  const e = document.createElementNS(NS, tag);
  for (const k in attrs) e.setAttribute(k, attrs[k]);
  parent?.appendChild(e);
  return e;
};
const esc = (s) => String(s ?? "").replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]));
const placeName = (p) => [p.city, p.country].filter(Boolean).join(", ") || "Unknown place";
const mapShown = () => !$("view-bandwidth").hidden && !$("map-view").hidden && !document.hidden;

// ---- chart / map switch -----------------------------------------------------
function setView(view) {
  const map = view === "map";
  $("map-view").hidden = !map;
  $("map-chips").hidden = !map;
  $("bw-chart-view").hidden = map;
  document.querySelector("#view-bandwidth .bw-ranges").hidden = map;
  document.querySelectorAll("[data-bw-view]").forEach((b) => b.classList.toggle("active", b.dataset.bwView === view));
  try { localStorage.setItem(VIEW_KEY, view); } catch (_) {}
  if (map) refresh();
  else window.dispatchEvent(new Event("resize"));
}
document.querySelectorAll("[data-bw-view]").forEach((b) => b.addEventListener("click", () => setView(b.dataset.bwView)));

// ---- data -------------------------------------------------------------------
async function loadWorld() {
  if (world) return;
  const topo = await (await fetch("vendor/countries-110m.json")).json();
  world = topojson.feature(topo, topo.objects.countries);
  // Antarctica holds no peers: the map is fitted to the other lands.
  lands = { type: "FeatureCollection", features: world.features.filter((f) => f.properties.name !== "Antarctica") };
}

async function refresh() {
  if (!mapShown()) return;
  try {
    await loadWorld();
    snap = await invoke("map_snapshot");
  } catch (e) {
    console.error("map", e);
    return;
  }
  setup();
  draw();
  table();
}

function setup() {
  const db = snap.db;
  const panel = $("map-setup");
  panel.hidden = db.state === "ready";
  const button = $("map-get-db");
  button.disabled = db.state === "downloading";
  const mb = (n) => (n / 1048576).toFixed(0);
  $("map-db-state").textContent =
    db.state === "downloading" ? (db.total ? `Downloading… ${mb(db.done)} of ${mb(db.total)} MB` : `Downloading… ${mb(db.done)} MB`)
    : db.state === "failed" ? `The download failed: ${db.error}`
    : "";
  button.textContent = db.state === "failed" ? "Try again" : "Download the database (60 MB)";
}
$("map-get-db").addEventListener("click", async () => {
  await invoke("map_download_db");
  refresh();
});
$("map-credit").addEventListener("click", (e) => {
  e.preventDefault();
  invoke("dd_open_url", { url: "https://db-ip.com" });
});

// ---- projection, zoom and pan -------------------------------------------------
function projection() {
  const W = svg.clientWidth, H = svg.clientHeight;
  const base = d3.geoNaturalEarth1().fitExtent([[16, 16], [W - 16, H - 30]], lands);
  const [tx, ty] = base.translate();
  // Zoom around the middle of the card, then pan.
  return base.scale(base.scale() * zoom).translate([W / 2 + (tx - W / 2) * zoom + pan[0], H / 2 + (ty - H / 2) * zoom + pan[1]]);
}

function zoomAt(factor, cx, cy) {
  const next = Math.min(16, Math.max(1, zoom * factor));
  if (next === zoom) return;
  const W = svg.clientWidth, H = svg.clientHeight;
  const f = next / zoom;
  // Keep the point under the cursor in place.
  pan = [(cx - W / 2) * (1 - f) + pan[0] * f, (cy - H / 2) * (1 - f) + pan[1] * f];
  zoom = next;
  if (zoom === 1) pan = [0, 0];
  queueDraw();
}
svg.addEventListener("wheel", (e) => {
  e.preventDefault();
  const box = svg.getBoundingClientRect();
  zoomAt(Math.exp(-e.deltaY * 0.0025), e.clientX - box.left, e.clientY - box.top);
}, { passive: false });
$("map-zoom-in").addEventListener("click", () => zoomAt(1.6, svg.clientWidth / 2, svg.clientHeight / 2));
$("map-zoom-out").addEventListener("click", () => zoomAt(1 / 1.6, svg.clientWidth / 2, svg.clientHeight / 2));
svg.addEventListener("dblclick", (e) => {
  const box = svg.getBoundingClientRect();
  zoomAt(e.altKey ? 1 / 2 : 2, e.clientX - box.left, e.clientY - box.top);
});
let drag = null;
svg.addEventListener("mousedown", (e) => {
  if (e.button !== 0 || zoom === 1) return;
  drag = { x: e.clientX, y: e.clientY, pan: [...pan] };
  svg.classList.add("dragging");
});
window.addEventListener("mousemove", (e) => {
  if (!drag) return;
  pan = [drag.pan[0] + e.clientX - drag.x, drag.pan[1] + e.clientY - drag.y];
  queueDraw();
});
window.addEventListener("mouseup", () => { drag = null; svg.classList.remove("dragging"); });

let queued = false;
function queueDraw() {
  if (queued) return;
  queued = true;
  requestAnimationFrame(() => { queued = false; draw(); });
}

// ---- drawing ------------------------------------------------------------------
// Places closer than a few pixels at this zoom are drawn as one.
function clusters(spots, proj) {
  const out = [];
  for (const s of [...spots].sort((a, b) => (b.down + b.up) - (a.down + a.up))) {
    const xy = proj([s.lon, s.lat]);
    if (!xy) continue;
    const near = out.find((c) => Math.hypot(c.xy[0] - xy[0], c.xy[1] - xy[1]) < CLUSTER_PX);
    if (near) {
      near.spots.push(s);
      near.peers += s.peers; near.down += s.down; near.up += s.up;
    } else {
      out.push({ xy, lon: s.lon, lat: s.lat, spots: [s], peers: s.peers, down: s.down, up: s.up });
    }
  }
  return out;
}

function landAt(lon, lat) {
  const key = `${lon},${lat}`;
  if (!countryOf.has(key)) countryOf.set(key, lands.features.find((f) => d3.geoContains(f, [lon, lat])) || null);
  return countryOf.get(key);
}

function draw() {
  if (!snap || !lands || !mapShown()) return;
  const W = svg.clientWidth, H = svg.clientHeight;
  if (!W || !H) return;
  svg.setAttribute("viewBox", `0 0 ${W} ${H}`);
  svg.replaceChildren();
  const proj = projection();
  const path = d3.geoPath(proj);
  el("path", { class: "map-grat", d: path(d3.geoGraticule10()) }, svg);

  const busy = new Set(snap.spots.map((s) => landAt(s.lon, s.lat)).filter(Boolean));
  const landLayer = el("g", {}, svg);
  for (const f of lands.features) {
    const p = el("path", { class: "map-land" + (busy.has(f) ? " active" : ""), d: path(f) }, landLayer);
    p.__feature = f;
  }
  const arcs = el("g", {}, svg), nodes = el("g", {}, svg), labels = el("g", {}, svg);
  const home = snap.home ? [snap.home.lon, snap.home.lat] : null;
  const arc = (from, cls, color) => {
    if (!home) return;
    const line = { type: "LineString", coordinates: d3.range(0, 1.0001, 1 / 48).map(d3.geoInterpolate(from, home)) };
    el("path", { class: "map-arc " + cls, d: path(line), stroke: color }, arcs);
  };

  let totalDown = 0, totalUp = 0, totalDd = 0;
  for (const c of clusters(snap.spots, proj)) {
    totalDown += c.down; totalUp += c.up;
    const down = shown.down && c.down >= IDLE, up = shown.up && c.up >= IDLE;
    const idle = c.down < IDLE && c.up < IDLE;
    if (!idle && !down && !up) continue;
    if (down) arc([c.lon, c.lat], "", "var(--bw-torrent-down)");
    if (up) arc([c.lon, c.lat], "up", "var(--bw-torrent-up)");
    const color = c.down >= c.up ? "var(--bw-torrent-down)" : "var(--bw-torrent-up)";
    const r = idle ? 3 : Math.min(18, 3.5 + Math.sqrt((c.down + c.up) / 1024) / 6);
    const dot = el("circle", { class: "map-node" + (idle ? " idle" : ""), cx: c.xy[0], cy: c.xy[1], r, fill: idle ? "" : color }, nodes);
    dot.__countries = new Set(c.spots.map((s) => s.country));
    const names = c.spots.map(placeName);
    hover(dot, () => `<h4>${esc(names.slice(0, 3).join(" · "))}${names.length > 3 ? ` +${names.length - 3}` : ""}</h4>
      <div class="row">Peers <b>${c.peers}</b></div>
      <div class="row">Download <b>${c.down >= IDLE ? rate(c.down) : "—"}</b></div>
      <div class="row">Upload <b>${c.up >= IDLE ? rate(c.up) : "—"}</b></div>`);
  }
  for (const s of snap.servers) {
    totalDd += s.speed;
    if (!shown.dd) continue;
    arc([s.lon, s.lat], "dd", "var(--bw-direct)");
    const xy = proj([s.lon, s.lat]);
    if (!xy) continue;
    const r = Math.min(14, 4 + Math.sqrt(s.speed / 1024) / 18);
    const sq = el("rect", { class: "map-node", x: xy[0] - r, y: xy[1] - r, width: 2 * r, height: 2 * r, rx: 3, fill: "var(--bw-direct)" }, nodes);
    hover(sq, () => `<h4>${esc(s.name)}</h4><div class="row">Server <b>${esc(placeName(s))}</b></div><div class="row">Download <b>${rate(s.speed)}</b></div>`);
  }
  if (home) {
    const [hx, hy] = proj(home);
    el("circle", { class: "map-home-halo", cx: hx, cy: hy, r: 12 }, nodes);
    el("circle", { class: "map-home", cx: hx, cy: hy, r: 6 }, nodes);
    el("text", { class: "map-label home", x: hx - 12, y: hy - 14, "text-anchor": "end" }, labels).textContent = `You · ${snap.home.city || ""}`;
  }
  $("map-t-down").textContent = rate(totalDown);
  $("map-t-up").textContent = rate(totalUp);
  $("map-t-dd").textContent = rate(totalDd);
}

function hover(node, html) {
  node.addEventListener("mouseenter", () => { tip.innerHTML = html(); tip.hidden = false; });
  node.addEventListener("mousemove", (e) => {
    const box = svg.getBoundingClientRect();
    let x = e.clientX - box.left + 14, y = e.clientY - box.top + 14;
    if (x + tip.offsetWidth > box.width) x -= tip.offsetWidth + 28;
    if (y + tip.offsetHeight > box.height) y -= tip.offsetHeight + 28;
    tip.style.left = `${x}px`; tip.style.top = `${y}px`;
  });
  node.addEventListener("mouseleave", () => { tip.hidden = true; });
}

// ---- side panel ---------------------------------------------------------------
function table() {
  const by = new Map();
  for (const s of snap.spots) {
    const name = s.country || "Unknown";
    const c = by.get(name) || { name, peers: 0, down: 0, up: 0, lands: new Set() };
    c.peers += s.peers; c.down += s.down; c.up += s.up;
    const land = landAt(s.lon, s.lat);
    if (land) c.lands.add(land);
    by.set(name, c);
  }
  const rows = [...by.values()].sort((a, b) => (b.down + b.up) - (a.down + a.up) || b.peers - a.peers);
  const max = Math.max(1, ...rows.map((r) => r.down + r.up));
  const peers = rows.reduce((a, r) => a + r.peers, 0) + (snap.unplaced || 0);
  $("map-count").textContent = peers ? `${peers} peers · ${rows.length} ${rows.length === 1 ? "country" : "countries"}` : "";
  $("map-empty").hidden = rows.length > 0;
  const body = $("map-countries");
  body.innerHTML = rows.map((r, i) => `<tr data-i="${i}">
    <td><div class="name">${esc(r.name)}<div class="map-bar" style="width:${Math.max(6, ((r.down + r.up) / max) * 100)}%">
      ${r.down >= IDLE ? `<span style="flex:${r.down};background:var(--bw-torrent-down)"></span>` : ""}
      ${r.up >= IDLE ? `<span style="flex:${r.up};background:var(--bw-torrent-up)"></span>` : ""}</div></div></td>
    <td class="num">${r.peers}</td><td class="num">${r.down >= IDLE ? rate(r.down) : "—"}</td><td class="num">${r.up >= IDLE ? rate(r.up) : "—"}</td></tr>`).join("");
  for (const tr of body.rows) {
    const row = rows[tr.dataset.i];
    tr.addEventListener("mouseenter", () => {
      svg.querySelectorAll(".map-land").forEach((p) => p.classList.toggle("hl", row.lands.has(p.__feature)));
      svg.querySelectorAll(".map-node").forEach((n) => n.classList.toggle("hl", !!n.__countries?.has(row.name)));
    });
    tr.addEventListener("mouseleave", () => svg.querySelectorAll(".hl").forEach((n) => n.classList.remove("hl")));
  }
  $("map-server-list").innerHTML = snap.servers.length
    ? snap.servers.map((s) => `<div class="map-srv"><i></i>${esc(s.name)} <span class="where">${esc(placeName(s))}</span><span class="spd">${rate(s.speed)}</span></div>`).join("")
    : `<div class="map-srv none">No download running.</div>`;
}

$("map-chips").addEventListener("click", (e) => {
  const chip = e.target.closest(".chip");
  if (!chip) return;
  shown[chip.dataset.kind] = !shown[chip.dataset.kind];
  chip.classList.toggle("off", !shown[chip.dataset.kind]);
  draw();
});

new ResizeObserver(queueDraw).observe(svg);
document.addEventListener("visibilitychange", refresh);
window.addEventListener("kdownloader-tab", refresh);
setInterval(refresh, POLL_MS);
let saved = "chart";
try { saved = localStorage.getItem(VIEW_KEY) || "chart"; } catch (_) {}
setView(saved);
})();
