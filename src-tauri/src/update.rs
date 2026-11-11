//! Updates: KDownloader looks for a newer release on GitHub at launch and
//! every few hours, and offers it. It installs only when the user says so.
//!
//! Releases carry a `latest.json` and installers signed with the project's
//! update key, which the app checks against the public key in its config.

use serde::Serialize;
use std::sync::Mutex;
use std::time::Duration;
use tauri::{AppHandle, Emitter, Manager, State};
use tauri_plugin_updater::{Update, UpdaterExt};

const FIRST_CHECK_AFTER: Duration = Duration::from_secs(20);
const CHECK_EVERY: Duration = Duration::from_secs(6 * 3600);
const RELEASES: &str = "https://github.com/blckdvc/KDownloader/releases/latest";

/// The update found, once the user has been told about it.
#[derive(Default)]
pub struct Updates {
    pending: Mutex<Option<Update>>,
}

#[derive(Serialize, Clone)]
pub struct UpdateInfo {
    version: String,
    current: String,
    notes: String,
    /// False where the app can't replace itself (a Linux .deb install): the
    /// user gets the release page instead.
    can_install: bool,
    page: String,
}

fn info(update: &Update) -> UpdateInfo {
    UpdateInfo {
        version: update.version.clone(),
        current: update.current_version.clone(),
        notes: update.body.clone().unwrap_or_default(),
        can_install: can_install(),
        page: RELEASES.into(),
    }
}

/// On Linux, only the AppImage can update itself; a .deb belongs to the
/// package manager.
fn can_install() -> bool {
    !cfg!(target_os = "linux") || std::env::var_os("APPIMAGE").is_some()
}

async fn check(app: &AppHandle) -> Result<Option<UpdateInfo>, String> {
    let update = app.updater().map_err(|e| e.to_string())?.check().await.map_err(|e| e.to_string())?;
    let found = update.as_ref().map(info);
    *app.state::<Updates>().pending.lock().unwrap() = update;
    Ok(found)
}

/// Checks at launch, then every few hours, and tells the window about a new
/// version once.
pub async fn watch(app: AppHandle) {
    tokio::time::sleep(FIRST_CHECK_AFTER).await;
    let mut announced = String::new();
    loop {
        match check(&app).await {
            Ok(Some(found)) if found.version != announced => {
                tracing::info!("KDownloader {} is available", found.version);
                announced = found.version.clone();
                let _ = app.emit("update-available", found);
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("update check failed: {e}"),
        }
        tokio::time::sleep(CHECK_EVERY).await;
    }
}

/// The update already found, for a window that opened after the announcement.
#[tauri::command]
pub fn update_pending(updates: State<'_, Updates>) -> Option<UpdateInfo> {
    updates.pending.lock().unwrap().as_ref().map(info)
}

/// Checks now, from the menu.
#[tauri::command]
pub async fn update_check(app: AppHandle) -> Result<Option<UpdateInfo>, String> {
    check(&app).await
}

/// Downloads and installs the update found, then restarts into it.
#[tauri::command]
pub async fn update_install(app: AppHandle) -> Result<(), String> {
    let Some(update) = app.state::<Updates>().pending.lock().unwrap().clone() else {
        return Err("No update to install".into());
    };
    if !can_install() {
        return Err("This copy of KDownloader is managed by your package manager".into());
    }
    #[cfg(target_os = "macos")]
    let signer = mac::signer();
    let (mut got, emitter) = (0u64, app.clone());
    update
        .download_and_install(
            move |chunk, total| {
                got += chunk as u64;
                let _ = emitter.emit("update-progress", total.map(|t| got as f64 / t as f64));
            },
            || {},
        )
        .await
        .map_err(|e| e.to_string())?;
    tracing::info!("KDownloader {} installed, restarting", update.version);
    #[cfg(target_os = "macos")]
    if let Some(identity) = signer {
        mac::sign(&identity);
    }
    app.restart();
}

/// The update replaces the app with the release build, signed ad hoc. A copy
/// the user signed with their own certificate, so macOS keeps its Local
/// Network permission, is signed again with that certificate.
#[cfg(target_os = "macos")]
mod mac {
    use crate::platform::command;
    use std::path::PathBuf;

    fn bundle() -> Option<PathBuf> {
        let exe = std::env::current_exe().ok()?;
        let app = exe.ancestors().nth(3)?.to_path_buf();
        app.extension().is_some_and(|e| e == "app").then_some(app)
    }

    /// The certificate this copy is signed with, if it's one in the keychain.
    pub fn signer() -> Option<String> {
        let app = bundle()?;
        let out = command("codesign").arg("-dvv").arg(&app).output().ok()?;
        // codesign describes the signature on stderr.
        let text = String::from_utf8_lossy(&out.stderr);
        let identity = text.lines().find_map(|l| l.strip_prefix("Authority="))?.to_string();
        let ids = command("security").args(["find-identity", "-v", "-p", "codesigning"]).output().ok()?;
        String::from_utf8_lossy(&ids.stdout).contains(&format!("\"{identity}\"")).then_some(identity)
    }

    pub fn sign(identity: &str) {
        let Some(app) = bundle() else { return };
        match command("codesign").args(["--force", "--sign", identity, "--timestamp=none"]).arg(&app).status() {
            Ok(s) if s.success() => tracing::info!("updated app signed with {identity}"),
            other => tracing::warn!("signing the updated app with {identity}: {other:?}"),
        }
    }
}
