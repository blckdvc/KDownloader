//! RSS auto-download: adds feed items that match the rules.

use super::*;

fn child_text(node: roxmltree::Node, tag: &str) -> Option<String> {
    node.children()
        .find(|c| c.has_tag_name(tag))
        .and_then(|c| c.text())
        .map(|s| s.trim().to_string())
}

/// Picks the torrent/magnet URL out of an RSS <item>.
fn item_torrent_url(item: roxmltree::Node) -> Option<String> {
    // Preferred: <enclosure url="..." type="application/x-bittorrent"/>
    if let Some(enc) = item.children().find(|c| c.has_tag_name("enclosure")) {
        if let Some(url) = enc.attribute("url") {
            return Some(url.to_string());
        }
    }
    // Fallbacks: a <link> or <guid> that is a magnet or .torrent.
    for tag in ["link", "guid"] {
        if let Some(t) = child_text(item, tag) {
            if t.starts_with("magnet:") || t.to_lowercase().ends_with(".torrent") {
                return Some(t);
            }
        }
    }
    None
}

async fn rss_poll_once(shared: &Shared) {
    let (enabled, feeds) = match shared.settings.lock() {
        Ok(s) => (s.rss_enabled, s.feeds.clone()),
        Err(_) => return,
    };
    if !enabled {
        return;
    }
    for feed in feeds {
        if feed.url.is_empty() {
            continue;
        }
        let body = match shared.http.get(&feed.url).send().await {
            Ok(r) => r.text().await.ok(),
            Err(_) => None,
        };
        let Some(body) = body else { continue };
        let doc = match roxmltree::Document::parse(&body) {
            Ok(d) => d,
            Err(_) => continue,
        };
        let regexes: Vec<regex::Regex> = feed
            .rules
            .iter()
            .filter_map(|r| regex::Regex::new(r).ok())
            .collect();
        for item in doc.descendants().filter(|n| n.has_tag_name("item")) {
            let title = child_text(item, "title").unwrap_or_default();
            let Some(url) = item_torrent_url(item) else { continue };
            let guid = child_text(item, "guid").unwrap_or_else(|| url.clone());

            if let Ok(seen) = shared.rss_seen.lock() {
                if seen.contains(&guid) {
                    continue;
                }
            }
            if !feed.rules.is_empty() && !regexes.iter().any(|re| re.is_match(&title)) {
                continue;
            }
            let opts = AddOpts {
                category: feed.category.clone(),
                save_path: feed.save_path.clone(),
                ..Default::default()
            };
            if shared.add_source(url, opts).await.is_ok() {
                if let Ok(mut seen) = shared.rss_seen.lock() {
                    seen.insert(guid);
                }
                shared.save_rss_seen();
            }
        }
    }
}

pub async fn rss_loop(shared: Shared) {
    // small initial delay so the UI is up first
    tokio::time::sleep(Duration::from_secs(10)).await;
    let mut mins: u32 = 0;
    loop {
        let interval = shared
            .settings
            .lock()
            .map(|s| s.rss_interval_min.max(1))
            .unwrap_or(15);
        if mins % interval == 0 {
            rss_poll_once(&shared).await;
        }
        tokio::time::sleep(Duration::from_secs(60)).await;
        mins = mins.wrapping_add(1);
    }
}
