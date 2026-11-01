mod direct;

use std::collections::{HashMap, HashSet};
use std::net::Ipv6Addr;
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ListenerMode, ListenerOptions, MseMode,
    Session, SessionOptions, SessionPersistenceConfig,
};

/// Fixed incoming port for peer connections (also UPnP-forwarded). A high
/// ephemeral port avoids the 6881-6889 range some ISPs throttle.
const LISTEN_PORT: u16 = 51413;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tauri::{Manager, State};
use tauri_plugin_dialog::DialogExt;

// ---------------------------------------------------------------------------
// Persisted configuration
// ---------------------------------------------------------------------------

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Feed {
    url: String,
    category: String,
    save_path: Option<String>,
    /// Title regexes; ANY match adds the item. Empty = match everything.
    rules: Vec<String>,
}
impl Default for Feed {
    fn default() -> Self {
        Self { url: String::new(), category: String::new(), save_path: None, rules: Vec::new() }
    }
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
struct Settings {
    download_dir: String,
    // Global limits in KiB/s; 0 = unlimited.
    global_down_kbps: u32,
    global_up_kbps: u32,
    // Alternate limits applied during the scheduler window.
    alt_down_kbps: u32,
    alt_up_kbps: u32,
    scheduler_enabled: bool,
    // Minutes from midnight (local time).
    sched_from_min: u32,
    sched_to_min: u32,
    // Watch folder
    watch_enabled: bool,
    watch_dir: String,
    watch_category: String,
    // RSS
    rss_enabled: bool,
    rss_interval_min: u32,
    feeds: Vec<Feed>,
    // Network / connectivity (applied at session start — needs a restart)
    listen_port: u16,
    upnp_enabled: bool,
    utp_enabled: bool,
    /// Bind all networking (incl. UPnP/SSDP discovery) to this interface, e.g.
    /// "en0". Fixes UPnP when VPN/tunnel (utun) interfaces hijack multicast.
    /// Empty = let the OS choose (default).
    bind_device: String,
}
impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: String::new(),
            global_down_kbps: 0,
            global_up_kbps: 0,
            alt_down_kbps: 500,
            alt_up_kbps: 100,
            scheduler_enabled: false,
            sched_from_min: 8 * 60,
            sched_to_min: 18 * 60,
            watch_enabled: false,
            watch_dir: String::new(),
            watch_category: String::new(),
            rss_enabled: false,
            rss_interval_min: 15,
            feeds: Vec::new(),
            listen_port: LISTEN_PORT,
            upnp_enabled: true,
            utp_enabled: false,
            bind_device: String::new(),
        }
    }
}

/// Per-torrent app-side metadata, keyed by info-hash so it survives restarts.
#[derive(Clone, Default, Serialize, Deserialize)]
struct TMeta {
    category: String,
    tags: Vec<String>,
    /// Cumulative bytes uploaded across *all* runs. librqbit only counts the
    /// current session, so we accumulate and persist it ourselves — otherwise
    /// the ratio resets to 0 on every restart.
    #[serde(default)]
    uploaded_total: u64,
}

// ---------------------------------------------------------------------------
// Shared state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct Shared {
    session: Arc<Session>,
    cfg_dir: PathBuf,
    settings: Arc<Mutex<Settings>>,
    meta: Arc<Mutex<HashMap<String, TMeta>>>,
    rss_seen: Arc<Mutex<HashSet<String>>>,
    http: reqwest::Client,
    /// Last-seen per-session uploaded bytes per info-hash, to fold deltas into
    /// the persisted `uploaded_total`. Runtime only (not persisted).
    last_up: Arc<Mutex<HashMap<String, u64>>>,
}

/// Options shared by every "add" path.
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AddOpts {
    #[serde(default)]
    category: String,
    #[serde(default)]
    save_path: Option<String>,
    #[serde(default)]
    paused: bool,
    #[serde(default)]
    only_files: Option<Vec<usize>>,
    #[serde(default)]
    down_kbps: u32,
    #[serde(default)]
    up_kbps: u32,
}

fn e2s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

fn kbps_to_bps(kbps: u32) -> Option<NonZeroU32> {
    NonZeroU32::new(kbps.saturating_mul(1024))
}

fn hash_hex(h: &Arc<librqbit::ManagedTorrent>) -> String {
    format!("{:?}", h.info_hash())
}

/// Turn a torrent name into a single safe path component for use as the
/// per-torrent sub-folder: no separators, no traversal, no empties.
fn sanitize_component(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '\0' => '_',
            c if (c as u32) < 0x20 => '_',
            c => c,
        })
        .collect();
    // Strip leading/trailing dots & spaces (".", "..", trailing-dot dirs on
    // Windows/macOS edge cases) and fall back to a constant if nothing's left.
    let trimmed = cleaned.trim().trim_matches('.').trim();
    if trimmed.is_empty() {
        "download".to_string()
    } else {
        trimmed.to_string()
    }
}

impl Shared {
    fn settings_path(&self) -> PathBuf {
        self.cfg_dir.join("settings.json")
    }
    fn meta_path(&self) -> PathBuf {
        self.cfg_dir.join("torrents.json")
    }
    fn rss_seen_path(&self) -> PathBuf {
        self.cfg_dir.join("rss_seen.json")
    }

    fn save_settings(&self) {
        if let Ok(s) = self.settings.lock() {
            let _ = std::fs::write(
                self.settings_path(),
                serde_json::to_vec_pretty(&*s).unwrap_or_default(),
            );
        }
    }
    fn save_meta(&self) {
        if let Ok(m) = self.meta.lock() {
            let _ = std::fs::write(
                self.meta_path(),
                serde_json::to_vec_pretty(&*m).unwrap_or_default(),
            );
        }
    }
    fn save_rss_seen(&self) {
        if let Ok(s) = self.rss_seen.lock() {
            let v: Vec<&String> = s.iter().collect();
            let _ = std::fs::write(self.rss_seen_path(), serde_json::to_vec(&v).unwrap_or_default());
        }
    }

    fn build_opts(&self, o: &AddOpts) -> AddTorrentOptions {
        // Per-torrent save path wins; otherwise default to the *current*
        // download dir so a change takes effect immediately for new adds
        // (the session's start-up folder is only the fallback).
        let output_folder = o.save_path.clone().filter(|s| !s.is_empty()).or_else(|| {
            self.settings
                .lock()
                .ok()
                .map(|s| s.download_dir.clone())
                .filter(|s| !s.is_empty())
        });
        AddTorrentOptions {
            paused: o.paused,
            only_files: o.only_files.clone(),
            overwrite: true,
            output_folder,
            ratelimits: LimitsConfig {
                download_bps: kbps_to_bps(o.down_kbps),
                upload_bps: kbps_to_bps(o.up_kbps),
            },
            ..Default::default()
        }
    }

    async fn finish_add(&self, resp: AddTorrentResponse, o: &AddOpts) -> anyhow::Result<u64> {
        let handle = resp
            .into_handle()
            .ok_or_else(|| anyhow::anyhow!("torrent could not be added"))?;
        let hash = hash_hex(&handle);
        if let Ok(mut m) = self.meta.lock() {
            let e = m.entry(hash).or_default();
            if !o.category.is_empty() {
                e.category = o.category.clone();
            }
        }
        self.save_meta();
        Ok(handle.id() as u64)
    }

    /// Add a torrent so that its content always lands in its *own* folder
    /// under the save path, named after the torrent.
    ///
    /// Because we always pass an explicit `output_folder` (to honour a live
    /// change of the download dir), librqbit skips the top-level folder it
    /// would otherwise create — so every torrent, single- or multi-file,
    /// would dump straight into the download dir and mix together. We fix that
    /// by resolving the metadata first (list-only) to learn the name, then
    /// re-adding into `<save path>/<name>`.
    async fn add_prepared(&self, add: AddTorrent<'_>, o: AddOpts) -> anyhow::Result<u64> {
        let mut probe = self.build_opts(&o);
        probe.list_only = true;
        let resp = self.session.add_torrent(add, Some(probe)).await?;
        let lo = match resp {
            AddTorrentResponse::ListOnly(lo) => lo,
            // Already managed (duplicate) — nothing to place, just return it.
            other => return self.finish_add(other, &o).await,
        };

        let mut opts = self.build_opts(&o);
        let folder = sanitize_component(&lo.info.name_or_else(|| "download"));
        opts.output_folder = Some(lo.output_folder.join(folder).to_string_lossy().into_owned());

        let resp = self
            .session
            .add_torrent(AddTorrent::from_bytes(lo.torrent_bytes), Some(opts))
            .await?;
        self.finish_add(resp, &o).await
    }

    async fn add_source(&self, source: String, o: AddOpts) -> anyhow::Result<u64> {
        let add = AddTorrent::from_cli_argument(&source)?;
        self.add_prepared(add, o).await
    }

    async fn add_bytes(&self, bytes: Vec<u8>, o: AddOpts) -> anyhow::Result<u64> {
        let add = AddTorrent::from_bytes(bytes);
        self.add_prepared(add, o).await
    }

    /// Effective (down, up) KiB/s right now, taking the scheduler into account.
    fn effective_limits(&self) -> (u32, u32) {
        let s = match self.settings.lock() {
            Ok(s) => s,
            Err(_) => return (0, 0),
        };
        let (mut d, mut u) = (s.global_down_kbps, s.global_up_kbps);
        if s.scheduler_enabled {
            let now = local_now_minutes();
            let inwin = if s.sched_from_min <= s.sched_to_min {
                now >= s.sched_from_min && now < s.sched_to_min
            } else {
                now >= s.sched_from_min || now < s.sched_to_min
            };
            if inwin {
                d = s.alt_down_kbps;
                u = s.alt_up_kbps;
            }
        }
        (d, u)
    }

    fn apply_limits(&self) {
        let (d, u) = self.effective_limits();
        self.session.ratelimits.set_download_bps(kbps_to_bps(d));
        self.session.ratelimits.set_upload_bps(kbps_to_bps(u));
    }
}

/// Local wall-clock minutes-from-midnight. Shells out to `date` to avoid a
/// timezone-database dependency; good enough for a once-per-30s scheduler tick.
fn local_now_minutes() -> u32 {
    if let Ok(out) = std::process::Command::new("date").arg("+%H:%M").output() {
        if let Ok(s) = String::from_utf8(out.stdout) {
            if let Some((h, m)) = s.trim().split_once(':') {
                if let (Ok(h), Ok(m)) = (h.parse::<u32>(), m.parse::<u32>()) {
                    return h * 60 + m;
                }
            }
        }
    }
    0
}

// ---------------------------------------------------------------------------
// Tauri commands
// ---------------------------------------------------------------------------

#[tauri::command]
async fn add_torrent(state: State<'_, Shared>, source: String, opts: Option<AddOpts>) -> Result<u64, String> {
    let source = source.trim().to_string();
    if source.is_empty() {
        return Err("empty source".into());
    }
    state.add_source(source, opts.unwrap_or_default()).await.map_err(e2s)
}

#[tauri::command]
async fn add_torrent_bytes(state: State<'_, Shared>, bytes: Vec<u8>, opts: Option<AddOpts>) -> Result<u64, String> {
    state.add_bytes(bytes, opts.unwrap_or_default()).await.map_err(e2s)
}

#[tauri::command]
fn list_torrents(state: State<'_, Shared>) -> Result<Vec<Value>, String> {
    // Lock order: last_up then meta (matches the accounting loop).
    let last_up = state.last_up.lock().map_err(|_| "lock".to_string())?;
    let meta = state.meta.lock().map_err(|_| "lock".to_string())?;
    let out = state.session.with_torrents(|it| {
        let mut v = Vec::new();
        for (id, handle) in it {
            let stats = handle.stats();
            let mut stats_json = serde_json::to_value(&stats).unwrap_or(Value::Null);
            let swarm = state.session.tracker_swarm(&handle.info_hash());
            let name = handle.name().unwrap_or_else(|| format!("torrent {id}"));
            let hash = format!("{:?}", handle.info_hash());
            let folder = handle.output_folder().to_string_lossy().to_string();
            let m = meta.get(&hash).cloned().unwrap_or_default();
            // Show cumulative upload (persisted base + this session's delta).
            let total_up = m.uploaded_total
                + stats.uploaded_bytes.saturating_sub(last_up.get(&hash).copied().unwrap_or(0));
            if let Some(obj) = stats_json.as_object_mut() {
                obj.insert("uploaded_bytes".to_string(), json!(total_up));
            }
            v.push(json!({
                "id": id,
                "name": name,
                "info_hash": hash,
                "output_folder": folder,
                "paused": handle.is_paused(),
                "category": m.category,
                "tags": m.tags,
                "stats": stats_json,
                "tracker_seeds": swarm.map(|s| s.0),
                "tracker_peers": swarm.map(|s| s.1),
            }));
        }
        v
    });
    Ok(out)
}

#[tauri::command]
fn list_files(state: State<'_, Shared>, id: u64) -> Result<Vec<Value>, String> {
    let h = state
        .session
        .get((id as usize).into())
        .ok_or_else(|| "torrent not found".to_string())?;
    let selected: Option<Vec<usize>> = h.only_files();
    let sel_set: Option<HashSet<usize>> = selected.map(|v| v.into_iter().collect());
    let files = h
        .with_metadata(|m| {
            m.file_infos
                .iter()
                .enumerate()
                .map(|(i, fi)| {
                    let checked = sel_set.as_ref().map(|s| s.contains(&i)).unwrap_or(true);
                    json!({
                        "index": i,
                        "name": fi.relative_filename.to_string_lossy(),
                        "len": fi.len,
                        "selected": checked,
                    })
                })
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Ok(files)
}

#[tauri::command]
async fn set_files(state: State<'_, Shared>, id: u64, indices: Vec<usize>) -> Result<(), String> {
    let session = state.session.clone();
    let h = session
        .get((id as usize).into())
        .ok_or_else(|| "torrent not found".to_string())?;
    let set: HashSet<usize> = indices.into_iter().collect();
    session.update_only_files(&h, &set).await.map_err(e2s)
}

#[tauri::command]
fn set_category(state: State<'_, Shared>, id: u64, category: String) -> Result<(), String> {
    let h = state
        .session
        .get((id as usize).into())
        .ok_or_else(|| "torrent not found".to_string())?;
    let hash = hash_hex(&h);
    {
        let mut m = state.meta.lock().map_err(|_| "lock".to_string())?;
        m.entry(hash).or_default().category = category;
    }
    state.save_meta();
    Ok(())
}

#[tauri::command]
fn set_tags(state: State<'_, Shared>, id: u64, tags: Vec<String>) -> Result<(), String> {
    let h = state
        .session
        .get((id as usize).into())
        .ok_or_else(|| "torrent not found".to_string())?;
    let hash = hash_hex(&h);
    {
        let mut m = state.meta.lock().map_err(|_| "lock".to_string())?;
        m.entry(hash).or_default().tags = tags;
    }
    state.save_meta();
    Ok(())
}

#[tauri::command]
async fn pause_torrent(state: State<'_, Shared>, id: u64) -> Result<(), String> {
    let session = state.session.clone();
    let handle = session
        .get((id as usize).into())
        .ok_or_else(|| "torrent not found".to_string())?;
    session.pause(&handle).await.map_err(e2s)
}

#[tauri::command]
async fn resume_torrent(state: State<'_, Shared>, id: u64) -> Result<(), String> {
    let session = state.session.clone();
    let handle = session
        .get((id as usize).into())
        .ok_or_else(|| "torrent not found".to_string())?;
    session.unpause(&handle).await.map_err(e2s)
}

/// Force a fresh tracker announce + peer re-discovery. `unpause` rebuilds the
/// peer stream with announce=true, so pausing (if live) then unpausing makes
/// the torrent re-announce — the fix when a private-tracker torrent comes back
/// from a restart with 0 peers (DHT is disabled for private torrents).
#[tauri::command]
async fn reannounce(state: State<'_, Shared>, id: u64) -> Result<(), String> {
    let session = state.session.clone();
    let handle = session
        .get((id as usize).into())
        .ok_or_else(|| "torrent not found".to_string())?;
    if !handle.is_paused() {
        session.pause(&handle).await.map_err(e2s)?;
    }
    session.unpause(&handle).await.map_err(e2s)
}

#[tauri::command]
async fn remove_torrent(state: State<'_, Shared>, id: u64, delete_files: bool) -> Result<(), String> {
    // Capture the hash first so we can drop app-side metadata too.
    let hash = state.session.get((id as usize).into()).map(|h| hash_hex(&h));
    state
        .session
        .delete((id as usize).into(), delete_files)
        .await
        .map_err(e2s)?;
    if let Some(hash) = hash {
        if let Ok(mut m) = state.meta.lock() {
            m.remove(&hash);
        }
        state.save_meta();
    }
    Ok(())
}

/// Live per-peer list for the selected torrent (Peers tab).
#[tauri::command]
fn list_peers(state: State<'_, Shared>, id: u64) -> Result<Vec<Value>, String> {
    let h = state
        .session
        .get((id as usize).into())
        .ok_or_else(|| "torrent not found".to_string())?;
    let live = match h.live() {
        Some(l) => l,
        None => return Ok(vec![]),
    };
    let snap = live.per_peer_stats_snapshot(Default::default());
    let mut peers: Vec<Value> = snap
        .peers
        .into_iter()
        .map(|(addr, p)| {
            json!({
                "addr": addr,
                "state": p.state,
                "kind": p.conn_kind.map(|k| format!("{k:?}")),
                "client": p.client_name,
                "down": p.counters.fetched_bytes,
                "up": p.counters.uploaded_bytes,
            })
        })
        .collect();
    peers.sort_by(|a, b| a["addr"].as_str().cmp(&b["addr"].as_str()));
    Ok(peers)
}

/// Tracker URLs configured for the selected torrent (Trackers tab).
#[tauri::command]
fn list_trackers(state: State<'_, Shared>, id: u64) -> Result<Vec<String>, String> {
    let h = state
        .session
        .get((id as usize).into())
        .ok_or_else(|| "torrent not found".to_string())?;
    let mut t: Vec<String> = h.shared().trackers.iter().map(|u| u.to_string()).collect();
    t.sort();
    Ok(t)
}

#[tauri::command]
fn get_settings(state: State<'_, Shared>) -> Result<Settings, String> {
    state.settings.lock().map(|s| s.clone()).map_err(|_| "lock".into())
}

#[tauri::command]
fn save_settings(state: State<'_, Shared>, settings: Settings) -> Result<(), String> {
    // Ensure the download folder exists so new torrents can be saved there.
    if !settings.download_dir.is_empty() {
        std::fs::create_dir_all(&settings.download_dir).map_err(e2s)?;
    }
    {
        let mut s = state.settings.lock().map_err(|_| "lock".to_string())?;
        *s = settings;
    }
    state.save_settings();
    state.apply_limits();
    Ok(())
}

/// Change the default download folder. Takes effect immediately for new
/// torrents (existing ones keep their folder; the session default is unchanged
/// until next launch, but new adds now default to this path).
#[tauri::command]
fn set_download_dir(state: State<'_, Shared>, path: String) -> Result<(), String> {
    let path = path.trim().to_string();
    if path.is_empty() {
        return Err("empty path".into());
    }
    std::fs::create_dir_all(&path).map_err(e2s)?;
    {
        let mut s = state.settings.lock().map_err(|_| "lock".to_string())?;
        s.download_dir = path;
    }
    state.save_settings();
    Ok(())
}

#[tauri::command]
fn download_dir(state: State<'_, Shared>) -> String {
    state
        .settings
        .lock()
        .map(|s| s.download_dir.clone())
        .unwrap_or_default()
}

/// Open a native folder picker and return the chosen path (None if cancelled).
#[tauri::command]
async fn pick_folder(app: tauri::AppHandle) -> Option<String> {
    let (tx, rx) = tokio::sync::oneshot::channel();
    app.dialog().file().pick_folder(move |picked| {
        let _ = tx.send(picked.map(|p| p.to_string()));
    });
    rx.await.ok().flatten()
}

#[tauri::command]
fn reveal_path(path: String) -> Result<(), String> {
    #[cfg(target_os = "macos")]
    std::process::Command::new("open").arg(&path).spawn().map(|_| ()).map_err(e2s)?;
    #[cfg(target_os = "windows")]
    std::process::Command::new("explorer").arg(&path).spawn().map(|_| ()).map_err(e2s)?;
    #[cfg(all(unix, not(target_os = "macos")))]
    std::process::Command::new("xdg-open").arg(&path).spawn().map(|_| ()).map_err(e2s)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Background workers
// ---------------------------------------------------------------------------

/// Re-applies effective speed limits every 30s so the scheduler window turns
/// alternate limits on/off automatically.
async fn scheduler_loop(shared: Shared) {
    loop {
        shared.apply_limits();
        tokio::time::sleep(Duration::from_secs(30)).await;
    }
}

/// Accumulates and persists each torrent's uploaded bytes so the ratio
/// survives restarts (librqbit only tracks the current session).
async fn upload_accounting_loop(shared: Shared) {
    let mut ticks: u32 = 0;
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        // Snapshot (hash -> current session uploaded) without holding locks.
        let live: Vec<(String, u64)> = shared.session.with_torrents(|it| {
            it.map(|(_, h)| (format!("{:?}", h.info_hash()), h.stats().uploaded_bytes))
                .collect()
        });
        {
            let mut last = match shared.last_up.lock() { Ok(l) => l, Err(_) => continue };
            let mut meta = match shared.meta.lock() { Ok(m) => m, Err(_) => continue };
            for (hash, up) in live {
                let prev = last.get(&hash).copied().unwrap_or(0);
                let delta = up.saturating_sub(prev); // resets to 0 on restart -> delta = up
                if delta > 0 {
                    meta.entry(hash.clone()).or_default().uploaded_total += delta;
                }
                last.insert(hash, up);
            }
        }
        ticks += 1;
        if ticks % 5 == 0 {
            shared.save_meta(); // persist roughly every 15s
        }
    }
}

/// Watches a folder for *.torrent files, adds them, and moves them aside.
async fn watch_loop(shared: Shared) {
    loop {
        tokio::time::sleep(Duration::from_secs(5)).await;
        let (enabled, dir, category) = {
            match shared.settings.lock() {
                Ok(s) => (s.watch_enabled, s.watch_dir.clone(), s.watch_category.clone()),
                Err(_) => continue,
            }
        };
        if !enabled || dir.is_empty() {
            continue;
        }
        let dir = PathBuf::from(dir);
        let added = dir.join(".pulsar-added");
        let failed = dir.join(".pulsar-failed");
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().and_then(|e| e.to_str()).map(|e| e.eq_ignore_ascii_case("torrent"))
                != Some(true)
            {
                continue;
            }
            let bytes = match std::fs::read(&path) {
                Ok(b) => b,
                Err(_) => continue,
            };
            let opts = AddOpts { category: category.clone(), ..Default::default() };
            let ok = shared.add_bytes(bytes, opts).await.is_ok();
            let dest_dir = if ok { &added } else { &failed };
            let _ = std::fs::create_dir_all(dest_dir);
            if let Some(name) = path.file_name() {
                let _ = std::fs::rename(&path, dest_dir.join(name));
            }
        }
    }
}

fn child_text(node: roxmltree::Node, tag: &str) -> Option<String> {
    node.children()
        .find(|c| c.has_tag_name(tag))
        .and_then(|c| c.text())
        .map(|s| s.trim().to_string())
}

/// Picks the torrent/magnet URL out of an RSS <item>.
fn item_torrent_url(item: roxmltree::Node) -> Option<String> {
    // Preferred: <enclosure url="..." type="application/x-bittorrent"/>
    if let Some(enc) = item.children().find(|c| c.has_tag_name("enclosure")) {
        if let Some(url) = enc.attribute("url") {
            return Some(url.to_string());
        }
    }
    // Fallbacks: a <link> or <guid> that is a magnet or .torrent.
    for tag in ["link", "guid"] {
        if let Some(t) = child_text(item, tag) {
            if t.starts_with("magnet:") || t.to_lowercase().ends_with(".torrent") {
                return Some(t);
            }
        }
    }
    None
}

async fn rss_poll_once(shared: &Shared) {
    let (enabled, feeds) = match shared.settings.lock() {
        Ok(s) => (s.rss_enabled, s.feeds.clone()),
        Err(_) => return,
    };
    if !enabled {
        return;
    }
    for feed in feeds {
        if feed.url.is_empty() {
            continue;
        }
        let body = match shared.http.get(&feed.url).send().await {
            Ok(r) => r.text().await.ok(),
            Err(_) => None,
        };
        let Some(body) = body else { continue };
        let doc = match roxmltree::Document::parse(&body) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let regexes: Vec<regex::Regex> = feed
            .rules
            .iter()
            .filter_map(|r| regex::Regex::new(r).ok())
            .collect();
        for item in doc.descendants().filter(|n| n.has_tag_name("item")) {
            let title = child_text(item, "title").unwrap_or_default();
            let Some(url) = item_torrent_url(item) else { continue };
            let guid = child_text(item, "guid").unwrap_or_else(|| url.clone());

            if let Ok(seen) = shared.rss_seen.lock() {
                if seen.contains(&guid) {
                    continue;
                }
            }
            if !feed.rules.is_empty() && !regexes.iter().any(|re| re.is_match(&title)) {
                continue;
            }
            let opts = AddOpts {
                category: feed.category.clone(),
                save_path: feed.save_path.clone(),
                ..Default::default()
            };
            if shared.add_source(url, opts).await.is_ok() {
                if let Ok(mut seen) = shared.rss_seen.lock() {
                    seen.insert(guid);
                }
                shared.save_rss_seen();
            }
        }
    }
}

async fn rss_loop(shared: Shared) {
    // small initial delay so the UI is up first
    tokio::time::sleep(Duration::from_secs(10)).await;
    let mut mins: u32 = 0;
    loop {
        let interval = shared
            .settings
            .lock()
            .map(|s| s.rss_interval_min.max(1))
            .unwrap_or(15);
        if mins % interval == 0 {
            rss_poll_once(&shared).await;
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
        mins = mins.wrapping_add(1);
    }
}

// ---------------------------------------------------------------------------
// macOS dock-icon bandwidth meter
// ---------------------------------------------------------------------------

/// Polls the session's aggregate speeds once a second and repaints the dock
/// tile on the main thread. Redraws only when the displayed values change.
#[cfg(target_os = "macos")]
async fn dock_loop(shared: Shared, app: tauri::AppHandle) {
    let mut last: (String, String) = (String::new(), String::new());
    loop {
        let snap = shared.session.stats_snapshot();
        let d = dock::fmt(snap.download_speed.as_bytes());
        let u = dock::fmt(snap.upload_speed.as_bytes());
        if (d.clone(), u.clone()) != last {
            last = (d.clone(), u.clone());
            let idle = snap.download_speed.as_bytes() == 0 && snap.upload_speed.as_bytes() == 0;
            let _ = app.run_on_main_thread(move || dock::update(idle, &d, &u));
        }
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(target_os = "macos")]
mod dock {
    use objc2::rc::Retained;
    use objc2::runtime::AnyObject;
    use objc2::{AllocAnyThread, MainThreadMarker};
    use objc2_app_kit::{
        NSApplication, NSBezierPath, NSColor, NSCompositingOperation, NSFont, NSFontAttributeName,
        NSForegroundColorAttributeName, NSImage, NSImageNameApplicationIcon, NSImageView,
        NSStringDrawing,
    };
    use objc2_foundation::{NSDictionary, NSPoint, NSRect, NSSize, NSString};

    /// Compact bytes/sec formatter, e.g. "0", "850K", "1.2M".
    pub fn fmt(bps: u64) -> String {
        if bps == 0 {
            return "0".into();
        }
        let b = bps as f64;
        if b < 1024.0 {
            format!("{b:.0}B")
        } else if b < 1024.0 * 1024.0 {
            format!("{:.0}K", b / 1024.0)
        } else if b < 1024.0 * 1024.0 * 1024.0 {
            let m = b / (1024.0 * 1024.0);
            if m < 10.0 {
                format!("{m:.1}M")
            } else {
                format!("{m:.0}M")
            }
        } else {
            format!("{:.1}G", b / (1024.0 * 1024.0 * 1024.0))
        }
    }

    pub fn update(idle: bool, down: &str, up: &str) {
        let Some(mtm) = MainThreadMarker::new() else {
            return;
        };
        let app = NSApplication::sharedApplication(mtm);
        let tile = app.dockTile();

        // Idle → restore the plain app icon.
        if idle {
            unsafe { tile.setContentView(None) };
            tile.display();
            return;
        }

        let size = NSSize::new(128.0, 128.0);
        let image = NSImage::initWithSize(NSImage::alloc(), size);
        let full = NSRect::new(NSPoint::new(0.0, 0.0), size);

        image.lockFocus();
        unsafe {
            // Base app icon.
            if let Some(icon) = NSImage::imageNamed(NSImageNameApplicationIcon) {
                icon.drawInRect_fromRect_operation_fraction(
                    full,
                    NSRect::new(NSPoint::new(0.0, 0.0), NSSize::new(0.0, 0.0)),
                    NSCompositingOperation::SourceOver,
                    1.0,
                );
            }

            // Shared font; white numbers with qBittorrent-style colored arrows.
            let font = NSFont::boldSystemFontOfSize(20.0);
            let kf: &NSString = NSFontAttributeName;
            let kc: &NSString = NSForegroundColorAttributeName;
            let vf: &AnyObject = &font;

            let white = NSColor::whiteColor();
            let green = NSColor::colorWithCalibratedRed_green_blue_alpha(0.36, 0.84, 0.44, 1.0);
            let blue = NSColor::colorWithCalibratedRed_green_blue_alpha(0.33, 0.66, 1.0, 1.0);
            let vw: &AnyObject = &white;
            let vg: &AnyObject = &green;
            let vb: &AnyObject = &blue;
            let white_attrs: Retained<NSDictionary<NSString, AnyObject>> =
                NSDictionary::from_slices(&[kf, kc], &[vf, vw]);
            let green_attrs: Retained<NSDictionary<NSString, AnyObject>> =
                NSDictionary::from_slices(&[kf, kc], &[vf, vg]);
            let blue_attrs: Retained<NSDictionary<NSString, AnyObject>> =
                NSDictionary::from_slices(&[kf, kc], &[vf, vb]);

            let down_arrow = NSString::from_str("\u{2193}");
            let up_arrow = NSString::from_str("\u{2191}");
            let down_num = NSString::from_str(down);
            let up_num = NSString::from_str(up);

            let da = down_arrow.sizeWithAttributes(Some(&green_attrs));
            let dn = down_num.sizeWithAttributes(Some(&white_attrs));
            let ua = up_arrow.sizeWithAttributes(Some(&blue_attrs));
            let un = up_num.sizeWithAttributes(Some(&white_attrs));

            let gap = 4.0;
            let down_w = da.width + gap + dn.width;
            let up_w = ua.width + gap + un.width;
            let line_h = da.height.max(ua.height);
            let text_w = down_w.max(up_w);

            // Translucent rounded capsule hugging the text (soft corners + alpha).
            let pad_x = 12.0;
            let pad_y = 5.0;
            let line_gap = 2.0;
            let pill_w = (text_w + pad_x * 2.0).min(126.0);
            let pill_h = line_h * 2.0 + line_gap + pad_y * 2.0;
            let pill_x = ((128.0 - pill_w) / 2.0).max(1.0);
            let pill_y = 8.0;
            let pill = NSRect::new(NSPoint::new(pill_x, pill_y), NSSize::new(pill_w, pill_h));
            let radius = pill_h / 2.0;
            let path =
                NSBezierPath::bezierPathWithRoundedRect_xRadius_yRadius(pill, radius, radius);
            NSColor::colorWithCalibratedWhite_alpha(0.0, 0.52).setFill();
            path.fill();

            // Download on top, upload below: colored arrow + white number, centered.
            let up_y = pill_y + pad_y;
            let down_y = up_y + line_h + line_gap;

            let dx = (128.0 - down_w) / 2.0;
            down_arrow.drawAtPoint_withAttributes(NSPoint::new(dx, down_y), Some(&green_attrs));
            down_num.drawAtPoint_withAttributes(
                NSPoint::new(dx + da.width + gap, down_y),
                Some(&white_attrs),
            );

            let ux = (128.0 - up_w) / 2.0;
            up_arrow.drawAtPoint_withAttributes(NSPoint::new(ux, up_y), Some(&blue_attrs));
            up_num.drawAtPoint_withAttributes(
                NSPoint::new(ux + ua.width + gap, up_y),
                Some(&white_attrs),
            );
        }
        image.unlockFocus();

        let iv = NSImageView::imageViewWithImage(&image, mtm);
        unsafe { tile.setContentView(Some(&iv)) };
        tile.display();
    }
}

// ---------------------------------------------------------------------------
// App bootstrap
// ---------------------------------------------------------------------------

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &PathBuf) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Native UPnP IGD port forwarding.
//
// librqbit's built-in forwarder only recognises `WANIPConnection:1` (IGD v1).
// Modern routers — UniFi UDM, many others — advertise only `WANIPConnection:2`
// (IGD v2), so librqbit discovers the gateway but never opens the port. This
// self-contained client speaks both versions: SSDP-discovers the IGD, reads
// its description, and renews an AddPortMapping lease over SOAP.
// ---------------------------------------------------------------------------
mod upnp {
    use anyhow::{anyhow, bail, Context, Result};
    use std::net::{IpAddr, Ipv4Addr, UdpSocket};
    use std::time::{Duration, Instant};
    use tracing::{info, warn};

    const SSDP_ADDR: &str = "239.255.255.250:1900";
    const LEASE_SECS: u32 = 3600;

    struct Igd {
        control_url: String,
        service_type: String,
        local_ip: Ipv4Addr,
    }

    /// The IPv4 default-route gateway (macOS `route -n get default`).
    /// Absolute path: a Finder-launched app has a minimal PATH.
    fn default_gateway() -> Option<Ipv4Addr> {
        let out = std::process::Command::new("/sbin/route")
            .args(["-n", "get", "default"])
            .output()
            .ok()?;
        let s = String::from_utf8(out.stdout).ok()?;
        for line in s.lines() {
            if let Some(rest) = line.trim().strip_prefix("gateway:") {
                if let Ok(ip) = rest.trim().parse::<Ipv4Addr>() {
                    return Some(ip);
                }
            }
        }
        None
    }

    /// Local IPv4 to advertise as the internal client: the explicit interface's
    /// address when `dev` is set (macOS `ipconfig getifaddr`), else the address
    /// of the interface that reaches the LAN gateway. We target the gateway
    /// (not a public IP) so VPN/tunnel interfaces — which never carry the LAN
    /// subnet — can't hijack the choice and break SSDP multicast.
    fn local_ipv4(dev: Option<&str>) -> Option<Ipv4Addr> {
        if let Some(dev) = dev {
            if let Ok(out) = std::process::Command::new("/usr/sbin/ipconfig")
                .args(["getifaddr", dev])
                .output()
            {
                if let Ok(s) = String::from_utf8(out.stdout) {
                    if let Ok(ip) = s.trim().parse() {
                        return Some(ip);
                    }
                }
            }
        }
        let target = default_gateway()
            .map(|gw| format!("{gw}:1900"))
            .unwrap_or_else(|| "8.8.8.8:80".to_string());
        let sock = UdpSocket::bind("0.0.0.0:0").ok()?;
        sock.connect(&target).ok()?;
        match sock.local_addr().ok()?.ip() {
            IpAddr::V4(v4) => Some(v4),
            _ => None,
        }
    }

    /// Send an SSDP M-SEARCH and return the first IGD LOCATION. We send to the
    /// multicast group *and* unicast straight to the gateway — the unicast
    /// sidesteps multicast egress-interface selection, which VPN/tunnel
    /// interfaces routinely hijack on multi-interface macOS.
    fn ssdp_discover(local_ip: Ipv4Addr, gateway: Option<Ipv4Addr>) -> Result<String> {
        let sock = UdpSocket::bind((local_ip, 0)).context("bind SSDP socket")?;
        sock.set_read_timeout(Some(Duration::from_secs(2)))?;
        let msg = format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: {SSDP_ADDR}\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\n\
             ST: urn:schemas-upnp-org:device:InternetGatewayDevice:1\r\n\r\n"
        );
        for _ in 0..3 {
            let _ = sock.send_to(msg.as_bytes(), SSDP_ADDR);
            if let Some(gw) = gateway {
                let _ = sock.send_to(msg.as_bytes(), (gw, 1900));
            }
        }
        let mut buf = [0u8; 4096];
        let deadline = Instant::now() + Duration::from_secs(4);
        while Instant::now() < deadline {
            match sock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    let resp = String::from_utf8_lossy(&buf[..n]);
                    for line in resp.lines() {
                        if line.len() >= 9 && line[..9].eq_ignore_ascii_case("location:") {
                            return Ok(line[9..].trim().to_string());
                        }
                    }
                }
                Err(_) => break,
            }
        }
        bail!("no IGD responded to SSDP")
    }

    fn authority_root(url: &str) -> String {
        if let Some(i) = url.find("://") {
            let after = &url[i + 3..];
            let end = after.find('/').map(|p| i + 3 + p).unwrap_or(url.len());
            return url[..end].to_string();
        }
        url.to_string()
    }

    fn resolve_url(location: &str, base: Option<&str>, control: &str) -> String {
        if control.starts_with("http://") || control.starts_with("https://") {
            return control.to_string();
        }
        let root = base
            .filter(|b| b.starts_with("http"))
            .map(|b| b.trim_end_matches('/').to_string())
            .unwrap_or_else(|| authority_root(location));
        let root = root.trim_end_matches('/');
        if control.starts_with('/') {
            format!("{root}{control}")
        } else {
            format!("{root}/{control}")
        }
    }

    /// Fetch and parse the IGD description, picking the WAN connection service
    /// (either version). Returns its absolute control URL + service type.
    async fn fetch_igd(http: &reqwest::Client, location: &str, local_ip: Ipv4Addr) -> Result<Igd> {
        let xml = http
            .get(location)
            .timeout(Duration::from_secs(8))
            .send()
            .await?
            .text()
            .await?;
        let doc = roxmltree::Document::parse(&xml).context("parse IGD description")?;
        let base = doc
            .descendants()
            .find(|n| n.tag_name().name() == "URLBase")
            .and_then(|n| n.text())
            .map(|s| s.trim().to_string());

        // Prefer WANIPConnection; fall back to WANPPPConnection. Any version.
        let mut found: Option<(String, String)> = None;
        for svc in doc.descendants().filter(|n| n.tag_name().name() == "service") {
            let st = svc
                .children()
                .find(|n| n.tag_name().name() == "serviceType")
                .and_then(|n| n.text())
                .unwrap_or("")
                .trim()
                .to_string();
            if !(st.contains("WANIPConnection") || st.contains("WANPPPConnection")) {
                continue;
            }
            let cu = svc
                .children()
                .find(|n| n.tag_name().name() == "controlURL")
                .and_then(|n| n.text())
                .unwrap_or("")
                .trim()
                .to_string();
            if cu.is_empty() {
                continue;
            }
            let prefer = st.contains("WANIPConnection");
            if found.is_none() || prefer {
                found = Some((st, cu));
                if prefer {
                    break;
                }
            }
        }
        let (service_type, control) = found.ok_or_else(|| anyhow!("no WAN connection service"))?;
        Ok(Igd {
            control_url: resolve_url(location, base.as_deref(), &control),
            service_type,
            local_ip,
        })
    }

    async fn add_mapping(http: &reqwest::Client, igd: &Igd, port: u16, proto: &str) -> Result<()> {
        let action = format!("{}#AddPortMapping", igd.service_type);
        let body = format!(
            "<?xml version=\"1.0\"?>\
             <s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" \
             s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\">\
             <s:Body><u:AddPortMapping xmlns:u=\"{st}\">\
             <NewRemoteHost></NewRemoteHost>\
             <NewExternalPort>{port}</NewExternalPort>\
             <NewProtocol>{proto}</NewProtocol>\
             <NewInternalPort>{port}</NewInternalPort>\
             <NewInternalClient>{ip}</NewInternalClient>\
             <NewEnabled>1</NewEnabled>\
             <NewPortMappingDescription>Pulsar</NewPortMappingDescription>\
             <NewLeaseDuration>{lease}</NewLeaseDuration>\
             </u:AddPortMapping></s:Body></s:Envelope>",
            st = igd.service_type,
            port = port,
            proto = proto,
            ip = igd.local_ip,
            lease = LEASE_SECS,
        );
        let resp = http
            .post(&igd.control_url)
            .header("Content-Type", "text/xml; charset=\"utf-8\"")
            .header("SOAPAction", format!("\"{action}\""))
            .timeout(Duration::from_secs(8))
            .body(body)
            .send()
            .await?;
        if !resp.status().is_success() {
            let code = resp.status();
            let txt = resp.text().await.unwrap_or_default();
            bail!("AddPortMapping {code}: {}", txt.chars().take(200).collect::<String>());
        }
        Ok(())
    }

    /// Discover the IGD and (re)create the port mapping, renewing the lease.
    pub async fn run(http: reqwest::Client, port: u16, dev: Option<String>, map_udp: bool) {
        loop {
            let dev2 = dev.clone();
            let discovered = tokio::task::spawn_blocking(move || {
                let ip = local_ipv4(dev2.as_deref()).ok_or_else(|| anyhow!("no local IPv4"))?;
                let gw = default_gateway();
                info!("upnp: discovering (local_ip={ip}, gateway={gw:?})");
                let loc = ssdp_discover(ip, gw)?;
                Ok::<(Ipv4Addr, String), anyhow::Error>((ip, loc))
            })
            .await;

            let result = match discovered {
                Ok(Ok((ip, loc))) => match fetch_igd(&http, &loc, ip).await {
                    Ok(igd) => {
                        let tcp = add_mapping(&http, &igd, port, "TCP").await;
                        if map_udp {
                            if let Err(e) = add_mapping(&http, &igd, port, "UDP").await {
                                warn!("upnp: UDP mapping failed: {e:#}");
                            }
                        }
                        match tcp {
                            Ok(()) => {
                                info!(
                                    "upnp: forwarded port {port} via {} (client {})",
                                    igd.service_type, igd.local_ip
                                );
                                None
                            }
                            Err(e) => Some(e),
                        }
                    }
                    Err(e) => Some(e),
                },
                Ok(Err(e)) => Some(e),
                Err(e) => Some(anyhow!("join: {e}")),
            };

            let wait = if let Some(e) = result {
                warn!("upnp: {e:#} — retrying in 120s");
                120
            } else {
                (LEASE_SECS / 2) as u64 // renew before the lease expires
            };
            tokio::time::sleep(Duration::from_secs(wait)).await;
        }
    }
}

/// Apps started from Finder get a soft limit of 256 open files, which a session
/// of multi-file torrents exceeds at startup: peers fail, and so can a torrent's
/// resume data. Returns the limit in force.
#[cfg(unix)]
fn raise_open_file_limit() -> Option<u64> {
    // macOS rejects more than OPEN_MAX even when the hard limit is "unlimited".
    const WANTED: libc::rlim_t = 10240;
    let mut limit = libc::rlimit { rlim_cur: 0, rlim_max: 0 };
    // SAFETY: both calls only read or write the struct we pass.
    unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut limit) != 0 {
            return None;
        }
        if limit.rlim_cur < WANTED {
            let raised = libc::rlimit { rlim_cur: WANTED.min(limit.rlim_max), ..limit };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &raised) == 0 {
                limit = raised;
            }
        }
    }
    Some(limit.rlim_cur as u64)
}

#[cfg(all(test, unix))]
#[test]
fn open_file_limit_leaves_the_256_default_behind() {
    assert!(raise_open_file_limit().unwrap() >= 1024);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .setup(|app| {
            let cfg_dir = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("com.pulsar.torrent"));
            std::fs::create_dir_all(&cfg_dir)?;

            // Persistent logging (stderr + a rolling file in the config dir) so
            // connectivity issues (UPnP, listener, peers) are diagnosable.
            let log_path = cfg_dir.join("pulsar.log").to_string_lossy().to_string();
            // init_logging spawns a tokio task, so it must run inside the
            // async runtime (the setup closure itself is not in one).
            tauri::async_runtime::block_on(async {
                let _ = librqbit::tracing_subscriber_config_utils::init_logging(
                    librqbit::tracing_subscriber_config_utils::InitLoggingOptions {
                        default_rust_log_value: Some("info"),
                        log_file: Some(&log_path),
                        log_file_rust_log: Some("info,librqbit=debug,librqbit_upnp=debug"),
                        log_file_json: false,
                    },
                );
            });

            #[cfg(unix)]
            tracing::info!(limit = ?raise_open_file_limit(), "open file limit");

            // Load persisted settings; default the download dir on first run.
            let mut settings: Settings = load_json(&cfg_dir.join("settings.json"));
            if settings.download_dir.is_empty() {
                let base = app.path().download_dir().unwrap_or_else(|_| std::env::temp_dir());
                settings.download_dir = base.join("Pulsar").to_string_lossy().to_string();
            }
            let dl = PathBuf::from(&settings.download_dir);
            std::fs::create_dir_all(&dl)?;

            let meta: HashMap<String, TMeta> = load_json(&cfg_dir.join("torrents.json"));
            let rss_seen_vec: Vec<String> = load_json(&cfg_dir.join("rss_seen.json"));
            let rss_seen: HashSet<String> = rss_seen_vec.into_iter().collect();

            // Knobs for our own UPnP forwarder (captured before `settings` moves).
            let upnp_enabled = settings.upnp_enabled;
            let upnp_port = settings.listen_port;
            let upnp_map_udp = settings.utp_enabled;
            let upnp_dev = {
                let d = settings.bind_device.trim();
                if d.is_empty() { None } else { Some(d.to_string()) }
            };

            // Session with fastresume + on-disk persistence so torrents resume.
            let opts = SessionOptions {
                fastresume: true,
                persistence: Some(SessionPersistenceConfig::Json {
                    folder: Some(cfg_dir.join("session")),
                }),
                ratelimits: LimitsConfig {
                    download_bps: kbps_to_bps(settings.global_down_kbps),
                    upload_bps: kbps_to_bps(settings.global_up_kbps),
                },
                bind_device_name: if settings.bind_device.trim().is_empty() {
                    None
                } else {
                    Some(settings.bind_device.trim().to_string())
                },
                // Prefer encrypted (MSE) peer connections, falling back to
                // plaintext — lets us talk to private-tracker peers that
                // require encryption while still reaching plaintext public peers.
                mse_mode: MseMode::Enabled,
                // Accept incoming peer connections so we can actually seed:
                // without a listener, peers can't reach us and the ratio never
                // moves once a torrent finishes. UPnP opens the port on the
                // router automatically; TCP+uTP maximises reachability.
                listen: Some(ListenerOptions {
                    // TCP by default (stable); µTP optional for advanced users
                    // (adds UDP peers + NAT traversal, but macOS caps the UDP
                    // buffer so it can lose packets at high speed).
                    mode: if settings.utp_enabled {
                        ListenerMode::TcpAndUtp
                    } else {
                        ListenerMode::TcpOnly
                    },
                    listen_addr: (Ipv6Addr::UNSPECIFIED, settings.listen_port).into(),
                    // librqbit's forwarder only speaks IGD v1; we run our own
                    // (v1+v2) forwarder below instead.
                    enable_upnp_port_forwarding: false,
                    ..Default::default()
                }),
                ..Default::default()
            };
            let session = tauri::async_runtime::block_on(async move {
                Session::new_with_opts(dl, opts).await
            })?;

            let http = reqwest::Client::builder()
                .user_agent("Pulsar/0.2")
                .build()
                .unwrap_or_default();

            let shared = Shared {
                session,
                cfg_dir: cfg_dir.clone(),
                settings: Arc::new(Mutex::new(settings)),
                meta: Arc::new(Mutex::new(meta)),
                rss_seen: Arc::new(Mutex::new(rss_seen)),
                http,
                last_up: Arc::new(Mutex::new(HashMap::new())),
            };
            shared.apply_limits();
            app.manage(shared.clone());

            // Background workers.
            tauri::async_runtime::spawn(scheduler_loop(shared.clone()));
            tauri::async_runtime::spawn(upload_accounting_loop(shared.clone()));
            if upnp_enabled {
                tauri::async_runtime::spawn(upnp::run(
                    shared.http.clone(),
                    upnp_port,
                    upnp_dev,
                    upnp_map_udp,
                ));
            }
            tauri::async_runtime::spawn(watch_loop(shared.clone()));
            tauri::async_runtime::spawn(rss_loop(shared.clone()));

            // Live ↓/↑ bandwidth painted onto the macOS dock icon (qBittorrent-style).
            #[cfg(target_os = "macos")]
            tauri::async_runtime::spawn(dock_loop(shared.clone(), app.handle().clone()));
            let _ = &shared;

            direct::init(app, &cfg_dir)?;

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            add_torrent,
            add_torrent_bytes,
            list_torrents,
            list_files,
            set_files,
            set_category,
            set_tags,
            list_peers,
            list_trackers,
            pause_torrent,
            resume_torrent,
            reannounce,
            remove_torrent,
            get_settings,
            save_settings,
            download_dir,
            set_download_dir,
            reveal_path,
            pick_folder,
            direct::dd_grab,
            direct::dd_add,
            direct::dd_list,
            direct::dd_pause,
            direct::dd_resume,
            direct::dd_pause_all,
            direct::dd_resume_all,
            direct::dd_remove,
            direct::dd_clear_done,
            direct::dd_get_settings,
            direct::dd_set_settings,
            direct::dd_list_accounts,
            direct::dd_connect_account,
            direct::dd_disconnect_account,
            direct::dd_open_key_page,
            direct::dd_reveal,
            direct::dd_open_url,
            direct::captcha::dd_open_captcha
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
