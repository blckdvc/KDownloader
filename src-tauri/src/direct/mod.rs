//! The Direct Download tab: HTTP downloads and premium link generators.
//! `engine`, `links` and `providers` know nothing about Tauri or torrents.

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

#[tauri::command]
pub fn dd_reveal(path: PathBuf) -> Result<(), String> {
    std::process::Command::new("open").arg("-R").arg(path).spawn().map_err(|e| e.to_string())?;
    Ok(())
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
        loop {
            std::thread::sleep(Duration::from_millis(750));
            let Ok(text) = clipboard.get_text() else { continue };
            if text == last {
                continue;
            }
            let has_link = text.contains("magnet:?") || !links::extract_urls(&text).is_empty();
            if engine.settings().watch_clipboard && has_link {
                let _ = app.emit("clipboard", &text);
            }
            last = text;
        }
    });
}

pub fn init(app: &mut tauri::App, config_dir: &Path) -> tauri::Result<()> {
    let downloads = app.path().download_dir()?;
    let engine = Engine::new(config_dir.join("direct-queue.json"), downloads);
    for provider in Provider::ALL {
        let key = keychain(provider).ok().and_then(|e| e.get_password().ok());
        engine.accounts().set_key(provider, key);
    }
    tauri::async_runtime::spawn(engine.clone().run_loop());
    watch_clipboard(app.handle().clone(), engine.clone());
    app.manage(engine);
    Ok(())
}
