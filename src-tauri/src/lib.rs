use std::collections::{HashMap, HashSet};
use std::num::NonZeroU32;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use librqbit::limits::LimitsConfig;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, Session, SessionOptions,
    SessionPersistenceConfig,
};
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
        }
    }
}

/// Per-torrent app-side metadata, keyed by info-hash so it survives restarts.
#[derive(Clone, Default, Serialize, Deserialize)]
struct TMeta {
    category: String,
    tags: Vec<String>,
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

    async fn add_source(&self, source: String, o: AddOpts) -> anyhow::Result<u64> {
        let add = AddTorrent::from_cli_argument(&source)?;
        let resp = self.session.add_torrent(add, Some(self.build_opts(&o))).await?;
        self.finish_add(resp, &o).await
    }

    async fn add_bytes(&self, bytes: Vec<u8>, o: AddOpts) -> anyhow::Result<u64> {
        let add = AddTorrent::from_bytes(bytes);
        let resp = self.session.add_torrent(add, Some(self.build_opts(&o))).await?;
        self.finish_add(resp, &o).await
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
    let meta = state.meta.lock().map_err(|_| "lock".to_string())?;
    let out = state.session.with_torrents(|it| {
        let mut v = Vec::new();
        for (id, handle) in it {
            let stats = handle.stats();
            let stats_json = serde_json::to_value(&stats).unwrap_or(Value::Null);
            let name = handle.name().unwrap_or_else(|| format!("torrent {id}"));
            let hash = format!("{:?}", handle.info_hash());
            let folder = handle.output_folder().to_string_lossy().to_string();
            let m = meta.get(&hash).cloned().unwrap_or_default();
            v.push(json!({
                "id": id,
                "name": name,
                "info_hash": hash,
                "output_folder": folder,
                "paused": handle.is_paused(),
                "category": m.category,
                "tags": m.tags,
                "stats": stats_json,
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
// App bootstrap
// ---------------------------------------------------------------------------

fn load_json<T: for<'de> Deserialize<'de> + Default>(path: &PathBuf) -> T {
    std::fs::read(path)
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default()
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
                cfg_dir,
                settings: Arc::new(Mutex::new(settings)),
                meta: Arc::new(Mutex::new(meta)),
                rss_seen: Arc::new(Mutex::new(rss_seen)),
                http,
            };
            shared.apply_limits();
            app.manage(shared.clone());

            // Background workers.
            tauri::async_runtime::spawn(scheduler_loop(shared.clone()));
            tauri::async_runtime::spawn(watch_loop(shared.clone()));
            tauri::async_runtime::spawn(rss_loop(shared));

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
            pause_torrent,
            resume_torrent,
            remove_torrent,
            get_settings,
            save_settings,
            download_dir,
            set_download_dir,
            reveal_path,
            pick_folder
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}
