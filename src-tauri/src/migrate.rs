//! One-time move from the app's former identity, com.pulsar.torrent.

use std::path::{Path, PathBuf};

pub const OLD_ID: &str = "com.pulsar.torrent";
pub const NEW_ID: &str = "com.kdownloader.desktop";
const LIBRARY_FOLDERS: [&str; 3] = ["Library/Application Support", "Library/WebKit", "Library/Caches"];

/// Renames a folder or file, unless it's gone or its new name is already taken.
pub fn move_once(old: &Path, new: &Path) {
    if old.exists() && !new.exists() {
        let _ = std::fs::rename(old, new);
    }
}

/// Runs before anything opens these folders, the webview included.
pub fn run() {
    let Some(home) = std::env::var_os("HOME").map(PathBuf::from) else {
        return;
    };
    for folder in LIBRARY_FOLDERS {
        move_once(&home.join(folder).join(OLD_ID), &home.join(folder).join(NEW_ID));
    }
    let config = home.join("Library/Application Support").join(NEW_ID);
    move_once(&config.join("pulsar.log"), &config.join("kdownloader.log"));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn move_once_keeps_what_is_already_at_the_new_name() {
        let dir = std::env::temp_dir().join(format!("kdownloader-migrate-{}", std::process::id()));
        let (old, new) = (dir.join("old"), dir.join("new"));
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("settings.json"), "kept").unwrap();

        move_once(&old, &new);
        assert_eq!(std::fs::read_to_string(new.join("settings.json")).unwrap(), "kept");
        assert!(!old.exists());

        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("settings.json"), "stale").unwrap();
        move_once(&old, &new);
        assert_eq!(std::fs::read_to_string(new.join("settings.json")).unwrap(), "kept");
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
