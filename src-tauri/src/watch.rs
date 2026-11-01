//! The watch folder: adds dropped .torrent files.

use super::*;

/// Watches a folder for *.torrent files, adds them, and moves them aside.
pub async fn watch_loop(shared: Shared) {
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
