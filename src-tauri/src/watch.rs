//! The watch folder: adds dropped .torrent files.

use super::*;

/// Watches a folder for *.torrent files, adds them, and moves them aside.
pub async fn watch_loop(shared: Shared) {
    let mut sizes = std::collections::HashMap::<PathBuf, u64>::new();
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
        let added = dir.join(".kdownloader-added");
        let failed = dir.join(".kdownloader-failed");
        crate::migrate::move_once(&dir.join(".pulsar-added"), &added);
        crate::migrate::move_once(&dir.join(".pulsar-failed"), &failed);
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
            // A file still being copied in would fail to parse and be set aside for
            // good, so it waits until two scans see the same size (copies keep the
            // source's date, so the date can't tell).
            let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
            if sizes.insert(path.clone(), size) != Some(size) || size == 0 {
                continue;
            }
            sizes.remove(&path);
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
