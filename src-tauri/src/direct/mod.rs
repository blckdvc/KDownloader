//! The Direct Download tab: HTTP downloads and premium link generators.
//! `engine`, `links` and `providers` know nothing about Tauri or torrents.

pub mod captcha;
pub mod cnl;
pub mod engine;
pub mod links;
pub mod providers;

use engine::{Engine, Item, Probe, Settings};
use providers::{Account, ApiError, Provider};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};

const KEYCHAIN_SERVICE: &str = "com.pulsar.torrent";

#[derive(Serialize)]
pub struct AccountView {
    id: Provider,
    name: &'static str,
    connected: bool,
    account: Option<Account>,
    error: Option<String>,
}

fn keychain(provider: Provider) -> Result<keyring::Entry, String> {
    keyring::Entry::new(KEYCHAIN_SERVICE, provider.id()).map_err(|e| e.to_string())
}

#[tauri::command]
pub async fn dd_grab(engine: State<'_, Engine>, text: String, files_only: bool) -> Result<Vec<Probe>, String> {
    Ok(engine.grab(&text, files_only).await)
}

#[tauri::command]
pub fn dd_list_unchecked(engine: State<Engine>, text: String) -> Vec<Probe> {
    engine.list_unchecked(&text)
}

#[tauri::command]
pub fn dd_add(engine: State<Engine>, probes: Vec<Probe>) {
    engine.add(probes);
}

#[tauri::command]
pub fn dd_list(engine: State<Engine>) -> Vec<Item> {
    engine.list()
}

#[tauri::command]
pub fn dd_pause(engine: State<Engine>, id: u64) {
    engine.pause(id);
}

#[tauri::command]
pub fn dd_resume(engine: State<Engine>, id: u64) {
    engine.resume(id);
}

#[tauri::command]
pub fn dd_pause_all(engine: State<Engine>) {
    engine.pause_all();
}

#[tauri::command]
pub fn dd_resume_all(engine: State<Engine>) {
    engine.resume_all();
}

#[tauri::command]
pub fn dd_remove(engine: State<Engine>, id: u64, delete_file: bool) {
    engine.remove(id, delete_file);
}

#[tauri::command]
pub fn dd_clear_done(engine: State<Engine>) {
    engine.clear_done();
}

#[tauri::command]
pub fn dd_get_settings(engine: State<Engine>) -> Settings {
    engine.settings()
}

#[tauri::command]
pub fn dd_set_settings(engine: State<Engine>, settings: Settings) {
    engine.set_settings(settings);
}

/// Connected accounts, with live status. Keys never leave the backend.
#[tauri::command]
pub async fn dd_list_accounts(engine: State<'_, Engine>) -> Result<Vec<AccountView>, String> {
    let mut views = Vec::new();
    for provider in Provider::ALL {
        let key = keychain(provider).ok().and_then(|e| e.get_password().ok());
        let (account, error) = match &key {
            Some(key) => match engine.accounts().verify(provider, key).await {
                Ok(account) => (Some(account), None),
                Err(e) => (None, Some(e.message().to_string())),
            },
            None => (None, None),
        };
        views.push(AccountView {
            id: provider,
            name: provider.name(),
            connected: key.is_some(),
            account,
            error,
        });
    }
    Ok(views)
}

/// A key is stored only once the service has accepted it.
#[tauri::command]
pub async fn dd_connect_account(
    engine: State<'_, Engine>,
    provider: Provider,
    key: String,
) -> Result<Account, String> {
    let key = key.trim().to_string();
    let account = engine.accounts().verify(provider, &key).await.map_err(|e| match e {
        ApiError::Auth(_) => "This API key was rejected.".to_string(),
        other => other.message().to_string(),
    })?;
    keychain(provider)?.set_password(&key).map_err(|e| e.to_string())?;
    engine.accounts().set_key(provider, Some(key));
    Ok(account)
}

#[tauri::command]
pub fn dd_disconnect_account(engine: State<Engine>, provider: Provider) -> Result<(), String> {
    match keychain(provider)?.delete_credential() {
        Ok(()) | Err(keyring::Error::NoEntry) => {}
        Err(e) => return Err(e.to_string()),
    }
    engine.accounts().set_key(provider, None);
    Ok(())
}

#[tauri::command]
pub fn dd_open_key_page(provider: Provider) -> Result<(), String> {
    std::process::Command::new("open")
        .arg(provider.key_page())
        .spawn()
        .map_err(|e| e.to_string())?;
    Ok(())
}

/// Opens a web page the app can't handle, such as a captcha-protected one.
#[tauri::command]
pub fn dd_open_url(url: String) -> Result<(), String> {
    if !url.starts_with("https://") && !url.starts_with("http://") {
        return Err("Only web links can be opened".into());
    }
    std::process::Command::new("open").arg(&url).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

#[tauri::command]
pub fn dd_reveal(path: PathBuf) -> Result<(), String> {
    std::process::Command::new("open").arg("-R").arg(path).spawn().map_err(|e| e.to_string())?;
    Ok(())
}

/// Bumped by macOS on every copy, so copying the same link again counts too.
#[cfg(target_os = "macos")]
fn clipboard_version() -> isize {
    objc2_app_kit::NSPasteboard::generalPasteboard().changeCount()
}

#[cfg(not(target_os = "macos"))]
fn clipboard_version() -> isize {
    0
}

/// Forwards copied links to the UI, which routes them to the right tab.
fn watch_clipboard(app: AppHandle, engine: Engine) {
    std::thread::spawn(move || {
        let mut clipboard = match arboard::Clipboard::new() {
            Ok(clipboard) => clipboard,
            Err(e) => {
                let _ = app.emit("clipboard-error", e.to_string());
                return;
            }
        };
        let mut last = clipboard.get_text().unwrap_or_default();
        let mut version = clipboard_version();
        loop {
            std::thread::sleep(Duration::from_millis(750));
            // Read only when watching and something was copied: macOS can count
            // reading the clipboard against the app.
            let current = clipboard_version();
            if current == version && cfg!(target_os = "macos") {
                continue;
            }
            version = current;
            if !engine.settings().watch_clipboard {
                continue;
            }
            let Ok(text) = clipboard.get_text() else { continue };
            if text == last && !cfg!(target_os = "macos") {
                continue;
            }
            let has_link = text.contains("magnet:?") || !links::extract_urls(&text).is_empty();
            if has_link {
                let _ = app.emit("clipboard", &text);
            }
            last = text;
        }
    });
}

/// Writes the queue on exit, since it is otherwise saved every few seconds.
pub fn flush(app: &AppHandle) {
    if let Some(engine) = app.try_state::<Engine>() {
        engine.save();
    }
}

pub fn init(app: &mut tauri::App, config_dir: &Path) -> tauri::Result<()> {
    let downloads = app.path().download_dir()?;
    let engine = Engine::new(config_dir.join("direct-queue.json"), downloads);
    // A read can wait on the user's answer to a Keychain prompt: not on the main thread.
    let keys = engine.clone();
    std::thread::spawn(move || {
        for provider in Provider::ALL {
            let key = keychain(provider).ok().and_then(|e| e.get_password().ok());
            keys.accounts().set_key(provider, key);
        }
    });
    tauri::async_runtime::spawn(engine.clone().run_loop());
    watch_clipboard(app.handle().clone(), engine.clone());
    captcha::start_click_n_load(app.handle().clone());
    app.manage(engine);
    Ok(())
}
