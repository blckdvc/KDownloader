//! Extracts the archives of torrents that finish downloading, when the user
//! turned it on, and optionally deletes them afterwards. A torrent whose
//! archives are deleted can't seed them any more, so it's paused.

use crate::direct::engine::{extract_archive, unique_path};
use crate::direct::links;
use crate::Shared;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

/// One archive of a torrent: the file to open, every file it's made of, and
/// the folder it unpacks into.
#[derive(Debug, PartialEq)]
struct Archive {
    lead: PathBuf,
    volumes: Vec<PathBuf>,
    stem: String,
}

/// The archives among a torrent's files. A RAR set counts once, and only when
/// its first volume is there.
fn archives(files: &[PathBuf]) -> Vec<Archive> {
    let mut sets: BTreeMap<(PathBuf, String), Vec<(u32, PathBuf)>> = BTreeMap::new();
    let mut found = Vec::new();
    for file in files {
        let Some(name) = file.file_name().map(|n| n.to_string_lossy().to_string()) else { continue };
        let dir = file.parent().unwrap_or(Path::new("")).to_path_buf();
        if let Some((set, number)) = links::rar_volume(&name) {
            sets.entry((dir, set)).or_default().push((number, file.clone()));
        } else if links::is_extractable(&name) {
            found.push(Archive { lead: file.clone(), volumes: vec![file.clone()], stem: links::archive_stem(&name) });
        }
    }
    for (_, mut volumes) in sets {
        volumes.sort();
        let Some((1, lead)) = volumes.first().cloned() else { continue };
        let name = lead.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        found.push(Archive { lead, volumes: volumes.into_iter().map(|(_, f)| f).collect(), stem: links::package_name(&name) });
    }
    found
}

/// Extracts a finished torrent's archives next to them.
async fn unpack(shared: Shared, handle: Arc<librqbit::ManagedTorrent>) {
    let root = handle.output_folder().to_path_buf();
    let files: Vec<PathBuf> = handle
        .with_metadata(|m| m.file_infos.iter().map(|f| root.join(&f.relative_filename)).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .filter(|f| f.exists())
        .collect();
    let found = archives(&files);
    if found.is_empty() {
        return;
    }
    let name = handle.name().unwrap_or_default();
    let mut deleted = false;
    for archive in found {
        let dir = archive.lead.parent().unwrap_or(&root).to_path_buf();
        let target = unique_path(&dir, &archive.stem);
        if let Err(e) = tokio::fs::create_dir_all(&target).await {
            tracing::warn!("torrent extraction of {}: {e}", archive.lead.display());
            continue;
        }
        tracing::info!("extracting {} from torrent {name}", archive.lead.display());
        if let Err(e) = extract_archive(&archive.lead, &target).await {
            tracing::warn!("torrent extraction of {} failed: {e}", archive.lead.display());
            let _ = tokio::fs::remove_dir_all(&target).await;
            continue;
        }
        // Read again: the setting may have changed during a long extraction.
        if shared.settings.lock().is_ok_and(|s| s.torrent_delete_archive) {
            for volume in &archive.volumes {
                deleted |= tokio::fs::remove_file(volume).await.is_ok();
            }
        }
    }
    if deleted {
        tracing::info!("archives of torrent {name} deleted: pausing it, as it can't seed them");
        if let Err(e) = shared.session.pause(&handle).await {
            tracing::warn!("pausing torrent {name}: {e}");
        }
    }
}

/// Watches for torrents that finish downloading during this session. One that
/// was already complete when first seen is left alone, so turning the setting
/// on doesn't unpack every old torrent.
pub async fn run(shared: Shared) {
    let mut finished: HashMap<String, bool> = HashMap::new();
    loop {
        tokio::time::sleep(Duration::from_secs(3)).await;
        let now: Vec<(String, bool, Arc<librqbit::ManagedTorrent>)> = shared.session.with_torrents(|it| {
            it.map(|(_, h)| (format!("{:?}", h.info_hash()), h.stats_brief().finished, h.clone())).collect()
        });
        let enabled = shared.settings.lock().is_ok_and(|s| s.torrent_extract);
        let mut seen = HashMap::with_capacity(now.len());
        for (hash, done, handle) in now {
            if done && finished.get(&hash) == Some(&false) && enabled {
                tauri::async_runtime::spawn(unpack(shared.clone(), handle));
            }
            seen.insert(hash, done);
        }
        finished = seen;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_whole_archives_and_rar_sets() {
        let files: Vec<PathBuf> = [
            "/t/Show/show.part1.rar",
            "/t/Show/show.part2.rar",
            "/t/Show/show.nfo",
            "/t/Old/old.rar",
            "/t/Old/old.r00",
            "/t/extras.zip",
            "/t/half/half.part2.rar",
        ]
        .iter()
        .map(PathBuf::from)
        .collect();
        let mut found = archives(&files);
        found.sort_by(|a, b| a.lead.cmp(&b.lead));
        let leads: Vec<&str> = found.iter().map(|a| a.lead.to_str().unwrap()).collect();
        assert_eq!(leads, ["/t/Old/old.rar", "/t/Show/show.part1.rar", "/t/extras.zip"], "a set without its first volume is skipped");
        assert_eq!(found[0].volumes.len(), 2);
        assert_eq!(found[1].volumes.len(), 2);
        assert_eq!(found[1].stem, "show");
        assert_eq!(found[2].stem, "extras");
    }
}
