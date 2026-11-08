//! Opening magnet links and .torrent files from the rest of the system.

use std::path::Path;
use std::sync::Mutex;
use tauri::{AppHandle, Emitter, Manager, Url};

use crate::{AddOpts, Shared};

const MAGNET: &str = "magnet";

#[cfg(target_os = "macos")]
pub use mac::keep_default;

/// Links that arrive before the session exists, as when one launches the app.
static WAITING: Mutex<Vec<Url>> = Mutex::new(Vec::new());

pub fn open(app: &AppHandle, urls: Vec<Url>) {
    if app.try_state::<Shared>().is_none() {
        WAITING.lock().unwrap().extend(urls);
        return;
    }
    for url in urls {
        let app = app.clone();
        tauri::async_runtime::spawn(async move { add(app, url).await });
    }
}

/// Handles what arrived during startup.
pub fn flush(app: &AppHandle) {
    let urls = std::mem::take(&mut *WAITING.lock().unwrap());
    open(app, urls);
}

async fn add(app: AppHandle, url: Url) {
    tracing::info!(scheme = url.scheme(), "opened from the system");
    let shared = app.state::<Shared>().inner().clone();
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.set_focus();
    }
    let _ = app.emit("show-torrents", ());
    let added = match url.scheme() {
        MAGNET => shared.add_source(url.to_string(), AddOpts::default()).await,
        _ => match url.to_file_path().map(std::fs::read) {
            Ok(Ok(bytes)) => shared.add_bytes(bytes, AddOpts::default()).await,
            _ => Err(anyhow::anyhow!("this file can't be read")),
        },
    };
    if let Err(e) = added {
        let _ = app.emit("torrent-error", format!("{e:#}"));
    }
}

/// Magnet links and .torrent files among a launch's arguments, as Windows
/// and Linux pass them, to the first instance too.
#[cfg_attr(target_os = "macos", allow(dead_code))]
pub fn from_args(args: &[String], cwd: &Path) -> Vec<Url> {
    args.iter()
        .skip(1)
        .filter_map(|arg| {
            if arg.starts_with("magnet:") {
                return Url::parse(arg).ok();
            }
            let path = cwd.join(arg);
            let torrent = path.extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent"));
            torrent.then(|| Url::from_file_path(&path).ok()).flatten()
        })
        .collect()
}

/// The system-wide choice of the app for magnet links and .torrent files,
/// through Launch Services.
#[cfg(target_os = "macos")]
pub mod mac {
    use crate::migrate;
    use objc2::rc::Retained;
    use objc2_foundation::NSString;
    use std::ffi::c_void;

    use super::MAGNET;

    const TORRENT_TYPE: &str = "org.bittorrent.torrent";
    const ALL_ROLES: u32 = 0xFFFF_FFFF;

    #[link(name = "CoreServices", kind = "framework")]
    extern "C" {
        fn LSCopyDefaultHandlerForURLScheme(scheme: *const c_void) -> *mut c_void;
        fn LSSetDefaultHandlerForURLScheme(scheme: *const c_void, handler: *const c_void) -> i32;
        fn LSSetDefaultRoleHandlerForContentType(kind: *const c_void, role: u32, handler: *const c_void) -> i32;
    }

    fn cf(text: &NSString) -> *const c_void {
        (text as *const NSString).cast()
    }

    fn magnet_handler() -> Option<String> {
        let scheme = NSString::from_str(MAGNET);
        // SAFETY: the scheme is a live NSString, toll-free bridged to CFString, and the
        // result is an owned CFString or null, which `from_raw` handles.
        let handler = unsafe {
            Retained::<NSString>::from_raw(LSCopyDefaultHandlerForURLScheme(cf(&scheme)).cast())
        };
        handler.map(|h| h.to_string())
    }

    /// Whether macOS sends magnet links here.
    #[tauri::command]
    pub fn is_default_handler() -> bool {
        magnet_handler().is_some_and(|h| h.eq_ignore_ascii_case(migrate::NEW_ID))
    }

    /// Carries the user's choice over from the app's former identity.
    pub fn keep_default() {
        if magnet_handler().is_some_and(|h| h.eq_ignore_ascii_case(migrate::OLD_ID)) {
            let _ = make_default_handler();
        }
    }

    /// Makes this app open magnet links and .torrent files. Only ever runs on the
    /// user's request, or to keep it: it changes a system-wide choice.
    #[tauri::command]
    pub fn make_default_handler() -> Result<(), String> {
        let (scheme, kind, bundle) =
            (NSString::from_str(MAGNET), NSString::from_str(TORRENT_TYPE), NSString::from_str(migrate::NEW_ID));
        // SAFETY: all three are live NSStrings, toll-free bridged to CFString.
        let (links, files) = unsafe {
            (
                LSSetDefaultHandlerForURLScheme(cf(&scheme), cf(&bundle)),
                LSSetDefaultRoleHandlerForContentType(cf(&kind), ALL_ROLES, cf(&bundle)),
            )
        };
        match (links, files) {
            (0, 0) => Ok(()),
            (links, files) => Err(format!("macOS refused the change ({links}, {files})")),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_magnets_and_torrent_files_from_arguments() {
        let cwd = std::env::temp_dir();
        let args: Vec<String> = ["kdownloader", "magnet:?xt=urn:btih:abc", "film.torrent", "--flag", "notes.txt"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let urls = from_args(&args, &cwd);
        assert_eq!(urls.len(), 2);
        assert_eq!(urls[0].scheme(), "magnet");
        assert_eq!(urls[1].to_file_path().unwrap(), cwd.join("film.torrent"));
    }
}
