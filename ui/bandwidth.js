// The Bandwidth tab: a rolling history of every transfer's speed, in one scale.
// Samples start with the app and stay in memory, so a restart clears them.
(() => {
const $ = (id) => document.getElementById(`bw-${id}`);

const SERIES = [
  { key: "torrentDown", name: "Torrent ↓", cls: "bw-torrent-down" },
  { key: "torrentUp", name: "Torrent ↑", cls: "bw-torrent-up" },
  { key: "direct", name: "Direct ↓", cls: "bw-direct" },
  { key: "total", name: "Total", cls: "bw-total" },
];
const MAX_SECONDS = 30 * 60;
const MARGIN = { top: 14, right: 92, bottom: 26, left: 76 };
const LABEL_GAP = 15;
const MINI_SECONDS = 60;
const MINI_W = 124, MINI_H = 28;

let samples = [];
let range = 60;
let hover = null;

const rate = (n) => `${bytes(n)}/s`;
const visible = () => samples.slice(-range);

function record() {
  let torrentDown = 0, torrentUp = 0;
  for (const t of torrents) {
    const m = metrics(t);
    torrentDown += m.dl;
    torrentUp += m.ul;
  }
  const direct = window.pulsarDirectSpeed || 0;
  samples.push({ at: Date.now(), torrentDown, torrentUp, direct, total: torrentDown + torrentUp + direct });
  if (samples.length > MAX_SECONDS) samples.shift();
  renderMini();
  if (!document.getElementById("view-bandwidth").hidden) render();
}

// The tab bar's sparkline: total speed over the last minute.
function renderMini() {
  const recent = samples.slice(-MINI_SECONDS);
  const last = recent[recent.length - 1];
  $("mini-text").textContent = `↓ ${rate(last.torrentDown + last.direct)}  ↑ ${rate(last.torrentUp)}`;
  const peak = Math.max(1024, ...recent.map((p) => p.total));
  const x = (i) => MINI_W * ((MINI_SECONDS - recent.length + i) / (MINI_SECONDS - 1));
  const y = (v) => MINI_H - 2 - (MINI_H - 4) * (v / peak);
  const points = recent.map((p, i) => `${x(i).toFixed(1)},${y(p.total).toFixed(1)}`).join(" ");
  $("mini-chart").innerHTML = `<line class="bw-grid" x1="0" x2="${MINI_W}" y1="${MINI_H - 1}" y2="${MINI_H - 1}"/><polyline class="bw-line bw-mini-line" points="${points}"/>`;
}

// The smallest 1, 2 or 5 step in binary units that fits the peak in four rows.
function axisStep(peak) {
  const unit = 1024 ** Math.max(0, Math.floor(Math.log(Math.max(peak, 1)) / Math.log(1024)));
  const scaled = peak / 4 / unit;
  const decade = 10 ** Math.floor(Math.log10(Math.max(scaled, 1e-9)));
  const step = [1, 2, 5, 10].map((m) => m * decade).find((s) => s >= scaled);
  return step * unit;
}

function clock(at) {
  return new Date(at).toLocaleTimeString("en-GB");
}

function renderSummary(shown) {
  $("summary").innerHTML = SERIES.map((s) => {
    const values = shown.map((p) => p[s.key]);
    const now = values[values.length - 1] ?? 0;
    const peak = Math.max(0, ...values);
    const average = values.length ? values.reduce((a, b) => a + b, 0) / values.length : 0;
    return `<tr><td><svg class="bw-key" width="22" height="8" aria-hidden="true"><line class="${s.cls}" x1="1" x2="21" y1="4" y2="4"/></svg>${s.name}</td>
      <td class="num">${rate(now)}</td><td class="num">${rate(peak)}</td><td class="num">${rate(average)}</td></tr>`;
  }).join("");
}

function render() {
  const shown = visible();
  renderSummary(shown);
  const chart = $("chart");
  const width = chart.clientWidth, height = chart.clientHeight;
  $("waiting").hidden = shown.length >= 2;
  if (shown.length < 2 || !width || !height) { chart.innerHTML = ""; return; }

  const plotW = width - MARGIN.left - MARGIN.right, plotH = height - MARGIN.top - MARGIN.bottom;
  const peak = Math.max(1024, ...shown.flatMap((p) => SERIES.map((s) => p[s.key])));
  const step = axisStep(peak);
  const top = Math.ceil(peak / step) * step;
  const x = (i) => MARGIN.left + plotW * ((range - shown.length + i) / (range - 1));
  const y = (v) => MARGIN.top + plotH * (1 - v / top);

  let svg = "";
  for (let v = 0; v <= top + step / 2; v += step) {
    svg += `<line class="bw-grid" x1="${MARGIN.left}" x2="${MARGIN.left + plotW}" y1="${y(v)}" y2="${y(v)}"/>`;
    svg += `<text class="bw-axis" x="${MARGIN.left - 8}" y="${y(v) + 4}" text-anchor="end">${rate(v)}</text>`;
  }
  const minutes = range / 60;
  svg += `<text class="bw-axis" x="${MARGIN.left}" y="${height - 7}">${minutes} min ago</text>`;
  svg += `<text class="bw-axis" x="${MARGIN.left + plotW}" y="${height - 7}" text-anchor="end">now</text>`;

  for (const s of SERIES) {
    const points = shown.map((p, i) => `${x(i).toFixed(1)},${y(p[s.key]).toFixed(1)}`).join(" ");
    svg += `<polyline class="bw-line ${s.cls}" points="${points}"/>`;
  }

  // End labels, pushed apart where the lines converge.
  const last = shown[shown.length - 1];
  const ends = SERIES.map((s) => ({ s, at: y(last[s.key]) })).sort((a, b) => a.at - b.at);
  ends.forEach((end, i) => { if (i && end.at - ends[i - 1].at < LABEL_GAP) end.at = ends[i - 1].at + LABEL_GAP; });
  const overflow = ends[ends.length - 1].at - (MARGIN.top + plotH);
  if (overflow > 0) ends.forEach((end) => { end.at -= overflow; });
  for (const end of ends) {
    svg += `<text class="bw-end" x="${MARGIN.left + plotW + 8}" y="${end.at + 4}">${end.s.name}</text>`;
  }

  const tip = $("tip");
  if (hover != null && shown[hover]) {
    const p = shown[hover], cx = x(hover);
    svg += `<line class="bw-cross" x1="${cx}" x2="${cx}" y1="${MARGIN.top}" y2="${MARGIN.top + plotH}"/>`;
    for (const s of SERIES) svg += `<circle class="bw-dot ${s.cls}" cx="${cx}" cy="${y(p[s.key])}" r="4"/>`;
    tip.innerHTML = `<div class="bw-tip-time">${clock(p.at)}</div>` + SERIES
      .map((s) => `<div class="bw-tip-row"><span>${s.name}</span><strong>${rate(p[s.key])}</strong></div>`).join("");
    tip.hidden = false;
    const flip = cx + tip.offsetWidth + 16 > width;
    tip.style.left = `${flip ? cx - tip.offsetWidth - 12 : cx + 12}px`;
    tip.style.top = `${MARGIN.top}px`;
  } else {
    tip.hidden = true;
  }
  chart.innerHTML = svg;
}

$("chart").addEventListener("mousemove", (e) => {
  const shown = visible();
  const plotW = $("chart").clientWidth - MARGIN.left - MARGIN.right;
  const slot = Math.round(((e.offsetX - MARGIN.left) / plotW) * (range - 1)) - (range - shown.length);
  hover = shown.length ? Math.min(shown.length - 1, Math.max(0, slot)) : null;
  render();
});
$("chart").addEventListener("mouseleave", () => { hover = null; render(); });

document.querySelectorAll("#view-bandwidth [data-range]").forEach((button) => {
  button.addEventListener("click", () => {
    range = Number(button.dataset.range);
    hover = null;
    document.querySelectorAll("#view-bandwidth [data-range]").forEach((b) => b.classList.toggle("active", b === button));
    render();
  });
});

new ResizeObserver(render).observe($("chart"));
setInterval(record, 1000);
})();
