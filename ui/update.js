// Offers a new version of KDownloader when the app finds one.
(() => {
  const { invoke } = window.__TAURI__.core;
  const { listen } = window.__TAURI__.event;
  const $ = (id) => document.getElementById(id);
  const card = $("update-card");
  let offered = null, busy = false;

  // Release notes are Markdown: keep them readable as plain text.
  const plain = (md) => md.replace(/^#+\s*/gm, "").replace(/\*\*(.+?)\*\*/g, "$1").replace(/`([^`]+)`/g, "$1")
    .replace(/\[([^\]]+)\]\([^)]+\)/g, "$1").trim();

  function offer(u) {
    if (!u || busy) return;
    offered = u;
    $("update-title").textContent = `KDownloader ${u.version} is available`;
    $("update-sub").textContent = u.can_install
      ? `You have ${u.current}. Downloads pause during the restart and pick up where they left off.`
      : `You have ${u.current}. Install it from the release page or your package manager.`;
    const notes = plain(u.notes || "");
    $("update-notes").textContent = notes;
    $("update-notes-wrap").hidden = !notes;
    $("update-go").textContent = u.can_install ? "Install and restart" : "Open the release page";
    $("update-go").disabled = false; $("update-later").disabled = false;
    $("update-bar").hidden = true;
    card.hidden = false;
  }
  const later = () => { if (!busy) card.hidden = true; };

  async function install() {
    if (!offered) return;
    if (!offered.can_install) { invoke("dd_open_url", { url: offered.page }).catch(() => {}); later(); return; }
    busy = true;
    $("update-go").disabled = true; $("update-later").disabled = true;
    $("update-go").textContent = "Downloading…";
    $("update-bar").hidden = false;
    try {
      await invoke("update_install"); // restarts the app when it succeeds
    } catch (e) {
      busy = false;
      showError(`The update couldn't be installed: ${e}`);
      offer(offered);
    }
  }

  async function checkNow() {
    try {
      const u = await invoke("update_check");
      if (u) offer(u);
      else showNotice(`KDownloader ${await window.__TAURI__.app.getVersion()} is the latest version.`);
    } catch (e) {
      showError(`Couldn't check for updates: ${e}`);
    }
  }

  $("update-go").addEventListener("click", install);
  $("update-later").addEventListener("click", later);
  $("update-close").addEventListener("click", later);
  listen("update-available", (e) => offer(e.payload));
  listen("update-progress", (e) => {
    if (e.payload == null) return;
    $("update-fill").style.width = `${Math.round(e.payload * 100)}%`;
    if (e.payload >= 1) $("update-go").textContent = "Installing…";
  });
  onMenu("any", { "check-updates": checkNow });
  invoke("update_pending").then(offer).catch(() => {});
})();
