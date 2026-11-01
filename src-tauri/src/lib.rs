mod direct;
#[cfg(target_os = "macos")]
mod dock;
mod rss;
mod upnp;
mod watch;

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

#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
struct Feed {
    url: String,
    category: String,
    save_path: Option<String>,
    /// Title regexes; ANY match adds the item. Empty = match everything.
    rules: Vec<String>,
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
    /// Last counts the trackers gave. Trackers answer with nothing to an early
    /// re-announce, so a restart would otherwise blank them for up to an hour.
    #[serde(default)]
    tracker_seeds: Option<u64>,
    #[serde(default)]
    tracker_peers: Option<u64>,
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
            let _ = direct::engine::write_atomic(
                &self.settings_path(),
                &serde_json::to_vec_pretty(&*s).unwrap_or_default(),
            );
        }
    }
    fn save_meta(&self) {
        if let Ok(m) = self.meta.lock() {
            let _ = direct::engine::write_atomic(
                &self.meta_path(),
                &serde_json::to_vec_pretty(&*m).unwrap_or_default(),
            );
        }
    }
    fn save_rss_seen(&self) {
        if let Ok(s) = self.rss_seen.lock() {
            let v: Vec<&String> = s.iter().collect();
            let _ = direct::engine::write_atomic(&self.rss_seen_path(), &serde_json::to_vec(&v).unwrap_or_default());
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

// Async so that it runs off the main thread: its cost grows with the torrent count.
#[tauri::command]
async fn list_torrents(state: State<'_, Shared>) -> Result<Vec<Value>, String> {
    // Lock order: last_up then meta (matches the accounting loop).
    let last_up = state.last_up.lock().map_err(|_| "lock".to_string())?;
    let mut meta = state.meta.lock().map_err(|_| "lock".to_string())?;
    let fresh_counts = std::cell::RefCell::new(Vec::new());
    let out = state.session.with_torrents(|it| {
        let mut v = Vec::new();
        for (id, handle) in it {
            let stats = handle.stats();
            let mut stats_json = serde_json::to_value(&stats).unwrap_or(Value::Null);
            let swarm = state.session.tracker_swarm(&handle.info_hash());
            let hash = format!("{:?}", handle.info_hash());
            if let Some(counts) = swarm {
                fresh_counts.borrow_mut().push((hash.clone(), counts));
            }
            let name = handle.name().unwrap_or_else(|| format!("torrent {id}"));
            let folder = handle.output_folder().to_string_lossy().to_string();
            let m = meta.get(&hash).cloned().unwrap_or_default();
            // Show cumulative upload (persisted base + this session's delta).
            let total_up = m.uploaded_total
                + stats.uploaded_bytes.saturating_sub(last_up.get(&hash).copied().unwrap_or(0));
            if let Some(obj) = stats_json.as_object_mut() {
                obj.insert("uploaded_bytes".to_string(), json!(total_up));
                // One entry per file, sent every second, and nothing reads it.
                obj.remove("file_progress");
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
                "tracker_seeds": swarm.map(|s| s.0).or(m.tracker_seeds),
                "tracker_peers": swarm.map(|s| s.1).or(m.tracker_peers),
            }));
        }
        v
    });
    for (hash, (seeds, peers)) in fresh_counts.into_inner() {
        let known = meta.entry(hash).or_default();
        known.tracker_seeds = Some(seeds);
        known.tracker_peers = Some(peers);
    }
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
    let handle = state.session.get((id as usize).into());
    let hash = handle.as_ref().map(hash_hex);
    // Listed before the torrent goes: Pulsar deletes through the Trash, so the
    // session only forgets it.
    let files = handle.as_ref().filter(|_| delete_files).map(|h| torrent_paths(h)).unwrap_or_default();
    state.session.delete((id as usize).into(), false).await.map_err(e2s)?;
    if !files.is_empty() {
        direct::engine::to_trash(&files).map_err(e2s)?;
    }
    if let Some(hash) = hash {
        if let Ok(mut m) = state.meta.lock() {
            m.remove(&hash);
        }
        state.save_meta();
    }
    Ok(())
}

/// A torrent's files on disk, or its own folder when it has one.
fn torrent_paths(handle: &librqbit::ManagedTorrent) -> Vec<PathBuf> {
    let root = handle.output_folder().to_path_buf();
    let files = handle
        .with_metadata(|m| m.file_infos.iter().map(|f| root.join(&f.relative_filename)).collect::<Vec<_>>())
        .unwrap_or_default();
    files.into_iter().filter(|p| p.exists()).collect()
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

// ---------------------------------------------------------------------------
// App bootstrap
// ---------------------------------------------------------------------------

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &std::path::Path) -> T {
    direct::engine::read_state(path)
}

const MAX_LOG_BYTES: u64 = 20 * 1024 * 1024;

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
    Some(limit.rlim_cur)
}

#[cfg(all(test, unix))]
#[test]
fn open_file_limit_leaves_the_256_default_behind() {
    assert!(raise_open_file_limit().unwrap() >= 1024);
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    tauri::Builder::default()
        // Both copies of the app share one session and port: focus the running one.
        .plugin(tauri_plugin_single_instance::init(|app, _, _| {
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
        }))
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_notification::init())
        // Remembers the main window only: captcha popups keep their own size.
        .plugin(tauri_plugin_window_state::Builder::default().with_filter(|label| label == "main").build())
        .setup(|app| {
            let cfg_dir = app
                .path()
                .app_config_dir()
                .unwrap_or_else(|_| std::env::temp_dir().join("com.pulsar.torrent"));
            std::fs::create_dir_all(&cfg_dir)?;

            // Persistent logging (stderr + a rolling file in the config dir) so
            // connectivity issues (UPnP, listener, peers) are diagnosable.
            // Restarted past a size cap, as the log only grows while running.
            let log_file = cfg_dir.join("pulsar.log");
            if std::fs::metadata(&log_file).is_ok_and(|m| m.len() > MAX_LOG_BYTES) {
                let _ = std::fs::remove_file(&log_file);
            }
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
            let mut notices = Vec::new();
            let mut dl = PathBuf::from(&settings.download_dir);
            if let Err(e) = std::fs::create_dir_all(&dl) {
                // An unplugged disk mustn't stop the app from opening.
                let base = app.path().download_dir().unwrap_or_else(|_| std::env::temp_dir());
                let fallback = base.join("Pulsar");
                notices.push(format!(
                    "The download folder {} isn't available ({e}). Using {} for now.",
                    dl.display(),
                    fallback.display()
                ));
                std::fs::create_dir_all(&fallback)?;
                dl = fallback;
            }

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
            let session_opts = |with_listener: bool| {
                let mut opts = SessionOptions {
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
                if !with_listener {
                    opts.listen = None;
                }
                opts
            };
            let open_session = |opts| tauri::async_runtime::block_on(Session::new_with_opts(dl.clone(), opts));
            let session = match open_session(session_opts(true)) {
                Ok(session) => session,
                // Usually the port is taken. Downloads still work, just without incoming peers.
                Err(e) => {
                    notices.push(format!("Pulsar can't accept incoming peers: {e:#}. Check the port in Settings."));
                    open_session(session_opts(false))?
                }
            };

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
            tauri::async_runtime::spawn(watch::watch_loop(shared.clone()));
            tauri::async_runtime::spawn(rss::rss_loop(shared.clone()));

            // Live ↓/↑ bandwidth painted onto the macOS dock icon (qBittorrent-style).
            #[cfg(target_os = "macos")]
            tauri::async_runtime::spawn(dock::dock_loop(shared.clone(), app.handle().clone()));
            let _ = &shared;

            direct::init(app, &cfg_dir)?;
            for notice in notices {
                app.dialog().message(notice).title("Pulsar").show(|_| {});
            }

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
            direct::dd_list_unchecked,
            direct::dd_retry_extraction,
            direct::dd_move,
            direct::dd_notify,
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
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|app, event| {
            if let tauri::RunEvent::Exit = event {
                direct::flush(app);
                if let Some(shared) = app.try_state::<Shared>() {
                    shared.save_meta();
                }
            }
        });
}
