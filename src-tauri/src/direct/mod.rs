//! The Direct Download tab: HTTP downloads and premium link generators.
//! `engine`, `links` and `providers` know nothing about Tauri or torrents.

pub mod captcha;
pub mod cnl;
pub mod engine;
pub mod links;
pub mod providers;

use engine::{AddOptions, Engine, Item, Probe, Settings};
use providers::{Account, ApiError, Provider};
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};


#[derive(Serialize)]
pub struct AccountView {
    id: Provider,
    name: &'static str,
    connected: bool,
    account: Option<Account>,
    error: Option<String>,
}

fn keychain(provider: Provider) -> Result<keyring::Entry, String> {
    keyring::Entry::new(crate::migrate::NEW_ID, provider.id()).map_err(|e| e.to_string())
}

/// The saved key, moved over from the app's former Keychain entry if it's still there.
fn stored_key(provider: Provider) -> Option<String> {
    let entry = keychain(provider).ok()?;
    match entry.get_password() {
        Err(keyring::Error::NoEntry) => {}
        found => return found.ok(),
    }
    let old = keyring::Entry::new(crate::migrate::OLD_ID, provider.id()).ok()?;
    let key = old.get_password().ok()?;
    if entry.set_password(&key).is_ok() {
        let _ = old.delete_credential();
    }
    Some(key)
}

#[tauri::command]
pub async fn dd_grab(engine: State<'_, Engine>, text: String, files_only: bool) -> Result<Vec<Probe>, String> {
    Ok(engine.grab(&text, files_only).await)
}

#[tauri::command]
pub fn dd_list_unchecked(engine: State<Engine>, text: String) -> Vec<Probe> {
    engine.list_unchecked(&text)
}

// The commands that save run async: a sync command runs on the main thread,
// and each save flushes the queue file to disk.
#[tauri::command]
pub async fn dd_add(engine: State<'_, Engine>, probes: Vec<Probe>, options: Option<AddOptions>) -> Result<(), String> {
    engine.add_with(probes, options.unwrap_or_default());
    Ok(())
}

#[tauri::command]
pub async fn dd_list(engine: State<'_, Engine>) -> Result<Vec<Item>, String> {
    Ok(engine.list())
}

#[tauri::command]
pub async fn dd_pause(engine: State<'_, Engine>, id: u64) -> Result<(), String> {
    engine.pause(id);
    Ok(())
}

#[tauri::command]
pub async fn dd_resume(engine: State<'_, Engine>, id: u64) -> Result<(), String> {
    engine.resume(id);
    Ok(())
}

#[tauri::command]
pub async fn dd_pause_all(engine: State<'_, Engine>) -> Result<(), String> {
    engine.pause_all();
    Ok(())
}

#[tauri::command]
pub async fn dd_resume_all(engine: State<'_, Engine>) -> Result<(), String> {
    engine.resume_all();
    Ok(())
}

#[tauri::command]
pub async fn dd_remove(engine: State<'_, Engine>, id: u64, delete_file: bool) -> Result<(), String> {
    engine.remove(id, delete_file);
    Ok(())
}

#[tauri::command]
pub async fn dd_clear_done(engine: State<'_, Engine>) -> Result<(), String> {
    engine.clear_done();
    Ok(())
}

#[tauri::command]
pub async fn dd_retry_extraction(engine: State<'_, Engine>, id: u64) -> Result<(), String> {
    engine.inner().clone().retry_extraction(id).await;
    Ok(())
}

#[tauri::command]
pub async fn dd_move(engine: State<'_, Engine>, ids: Vec<u64>, to_top: bool) -> Result<(), String> {
    engine.move_items(&ids, to_top);
    Ok(())
}

#[tauri::command]
pub fn dd_notify(app: AppHandle, title: String, body: String) -> Result<(), String> {
    use tauri_plugin_notification::NotificationExt;
    app.notification().builder().title(title).body(body).show().map_err(|e| e.to_string())
}

#[tauri::command]
pub fn dd_get_settings(engine: State<Engine>) -> Settings {
    engine.settings()
}

#[tauri::command]
pub async fn dd_set_settings(engine: State<'_, Engine>, settings: Settings) -> Result<(), String> {
    engine.set_settings(settings);
    Ok(())
}

/// Connected accounts, with live status. Keys never leave the backend.
#[tauri::command]
pub async fn dd_list_accounts(engine: State<'_, Engine>) -> Result<Vec<AccountView>, String> {
    let mut views = Vec::new();
    for provider in Provider::ALL {
        let key = stored_key(provider);
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

const DONATION_PAGE: &str = "https://paypal.me/lbenh";

#[tauri::command]
pub fn open_donation_page() -> Result<(), String> {
    std::process::Command::new("open").arg(DONATION_PAGE).spawn().map_err(|e| e.to_string())?;
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
/// Direct Download's share of the speed shown on the dock icon.
pub fn download_speed(app: &AppHandle) -> u64 {
    app.try_state::<Engine>().map_or(0, |engine| engine.total_speed())
}

pub fn flush(app: &AppHandle) {
    if let Some(engine) = app.try_state::<Engine>() {
        engine.save();
    }
}

pub fn init(app: &mut tauri::App, config_dir: &Path) -> tauri::Result<()> {
    let downloads = app.path().download_dir()?;
    let engine = Engine::new(config_dir.join("direct-queue.json"), downloads);
    // A read can wait on the user's answer to a Keychain prompt: not on the main
    // thread. The queue starts after it, as restored items need their account.
    let keys = engine.clone();
    std::thread::spawn(move || {
        for provider in Provider::ALL {
            let key = stored_key(provider);
            keys.accounts().set_key(provider, key);
        }
        tauri::async_runtime::spawn(keys.run_loop());
    });
    watch_clipboard(app.handle().clone(), engine.clone());
    captcha::start_click_n_load(app.handle().clone());
    app.manage(engine);
    Ok(())
}
