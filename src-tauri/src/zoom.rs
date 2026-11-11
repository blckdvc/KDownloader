//! Interface zoom: View > Zoom In, Zoom Out and Actual Size, with ⌘+ ⌘- ⌘0
//! on macOS and Ctrl elsewhere. The level is kept across launches.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use tauri::{AppHandle, Manager, Runtime};

/// The levels the zoom steps through, as in a browser.
const LEVELS: [f64; 11] = [0.67, 0.75, 0.8, 0.9, 1.0, 1.1, 1.25, 1.5, 1.75, 2.0, 2.5];

#[derive(Serialize, Deserialize)]
struct Saved {
    zoom: f64,
}

pub struct Zoom {
    level: Mutex<f64>,
    file: PathBuf,
}

impl Zoom {
    pub fn load(cfg_dir: &Path) -> Self {
        let file = cfg_dir.join("zoom.json");
        let level = std::fs::read(&file)
            .ok()
            .and_then(|b| serde_json::from_slice::<Saved>(&b).ok())
            .map(|s| s.zoom)
            .filter(|z| z.is_finite())
            .map_or(1.0, |z| z.clamp(LEVELS[0], LEVELS[LEVELS.len() - 1]));
        Zoom { level: Mutex::new(level), file }
    }

    pub fn level(&self) -> f64 {
        *self.level.lock().unwrap()
    }
}

/// The next level up or down from `now`, which may sit between two levels.
fn next(now: f64, up: bool) -> f64 {
    let found = if up {
        LEVELS.iter().find(|&&l| l > now + 0.001)
    } else {
        LEVELS.iter().rev().find(|&&l| l < now - 0.001)
    };
    found.copied().unwrap_or(now)
}

/// Sets the main window's zoom and remembers it.
pub fn set<R: Runtime>(app: &AppHandle<R>, level: f64) {
    let Some(zoom) = app.try_state::<Zoom>() else { return };
    *zoom.level.lock().unwrap() = level;
    if let Some(window) = app.get_webview_window("main") {
        if let Err(e) = window.set_zoom(level) {
            tracing::warn!("zooming the window: {e}");
        }
    }
    let saved = serde_json::to_vec(&Saved { zoom: level }).unwrap_or_default();
    let _ = crate::direct::engine::write_atomic(&zoom.file, &saved);
}

/// Runs a zoom menu command; false for any other id.
pub fn handle_menu<R: Runtime>(app: &AppHandle<R>, id: &str) -> bool {
    let Some(now) = app.try_state::<Zoom>().map(|z| z.level()) else { return false };
    let level = match id {
        "zoom-in" => next(now, true),
        "zoom-out" => next(now, false),
        "zoom-reset" => 1.0,
        _ => return false,
    };
    set(app, level);
    true
}

/// For the keys the menu doesn't catch: ⌘⇧= and the keypad's + and -.
#[tauri::command]
pub fn ui_zoom(app: AppHandle, step: String) {
    let id = match step.as_str() {
        "in" => "zoom-in",
        "out" => "zoom-out",
        _ => "zoom-reset",
    };
    handle_menu(&app, id);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_through_the_levels_and_stops_at_the_ends() {
        assert_eq!(next(1.0, true), 1.1);
        assert_eq!(next(1.0, false), 0.9);
        assert_eq!(next(1.05, false), 1.0, "a level between two snaps to the nearer one below");
        assert_eq!(next(2.5, true), 2.5);
        assert_eq!(next(0.67, false), 0.67);
    }
}
