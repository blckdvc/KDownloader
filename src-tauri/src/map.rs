//! The Bandwidth map: where the torrent peers and Direct Download servers we
//! exchange data with are, how fast right now, and how much since launch.
//!
//! A background task samples the peers' byte counters every couple of
//! seconds, whether the map is open or not. Traffic is kept by address and
//! placed only when the map asks, so it shows up there even when the location
//! database arrives later.

use crate::direct::engine::Engine;
use crate::geo::{self, Geo, Place};
use crate::Shared;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tauri::{AppHandle, Manager, State};

const SAMPLE_EVERY: Duration = Duration::from_secs(2);

/// Each live peer's downloaded and uploaded bytes, by torrent and address.
type Counters = HashMap<(usize, String), (u64, u64)>;

#[derive(Default)]
struct Recorder {
    /// The counters at the last sample, to turn them into speeds and totals.
    last: Option<(Instant, Counters)>,
    /// Speeds at the last sample, by peer address.
    rates: Vec<(IpAddr, f64, f64)>,
    /// Peers whose address can't be read, at the last sample.
    unreadable: u32,
    /// Bytes exchanged with each address since launch: down, up.
    peers: HashMap<IpAddr, (u64, u64)>,
    /// The name each Direct Download server was shown with: its debrid
    /// service or its site.
    server_names: HashMap<IpAddr, String>,
}

/// What the map shows, kept up to date by `record`.
pub struct MapHistory {
    since: u64,
    recorder: Mutex<Recorder>,
}

impl Default for MapHistory {
    fn default() -> Self {
        let since = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        MapHistory { since, recorder: Mutex::default() }
    }
}

/// Bytes since the last sample: a counter that went down belongs to a new
/// connection, and one seen for the first time counts from zero.
fn delta(now: u64, before: Option<u64>) -> u64 {
    match before {
        Some(before) if now >= before => now - before,
        _ => now,
    }
}

impl MapHistory {
    fn sample(&self, counters: Counters, servers: Vec<crate::direct::engine::ServerActivity>) {
        let now = Instant::now();
        let mut rec = self.recorder.lock().unwrap();
        let previous = rec.last.take();
        let secs = previous.as_ref().map(|(at, _)| now.duration_since(*at).as_secs_f64()).filter(|s| *s > 0.2);
        let mut rates = Vec::with_capacity(counters.len());
        let mut unreadable = 0;
        for (key, &(down, up)) in &counters {
            let before = previous.as_ref().and_then(|(_, c)| c.get(key));
            let (d, u) = (delta(down, before.map(|b| b.0)), delta(up, before.map(|b| b.1)));
            let Ok(addr) = key.1.parse::<SocketAddr>() else {
                unreadable += 1;
                continue;
            };
            let total = rec.peers.entry(addr.ip()).or_default();
            total.0 += d;
            total.1 += u;
            // A peer seen for the first time has no speed yet.
            let (dr, ur) = match (secs, before) {
                (Some(secs), Some(_)) => (d as f64 / secs, u as f64 / secs),
                _ => (0.0, 0.0),
            };
            rates.push((addr.ip(), dr, ur));
        }
        for s in servers {
            rec.server_names.insert(s.ip, s.name);
        }
        rec.rates = rates;
        rec.unreadable = unreadable;
        rec.last = Some((now, counters));
    }
}

fn live_counters(shared: &Shared) -> Counters {
    shared.session.with_torrents(|it| {
        let mut counters = HashMap::new();
        for (id, handle) in it {
            let Some(live) = handle.live() else { continue };
            for (addr, peer) in live.per_peer_stats_snapshot(Default::default()).peers {
                if peer.state == "live" {
                    counters.insert((id, addr), (peer.counters.fetched_bytes, peer.counters.uploaded_bytes));
                }
            }
        }
        counters
    })
}

/// Samples the peers and servers for as long as the app runs.
pub async fn record(app: AppHandle) {
    loop {
        tokio::time::sleep(SAMPLE_EVERY).await;
        let (Some(shared), Some(history)) = (app.try_state::<Shared>(), app.try_state::<MapHistory>()) else {
            continue;
        };
        let counters = live_counters(&shared);
        let servers = app.try_state::<Engine>().map(|e| e.active_servers()).unwrap_or_default();
        history.sample(counters, servers);
    }
}

/// Peers in one place: a city, or a country when the city is unknown.
#[derive(Serialize)]
struct Spot {
    #[serde(flatten)]
    place: Place,
    peers: u32,
    down: f64,
    up: f64,
}

#[derive(Serialize)]
struct Server {
    #[serde(flatten)]
    place: Place,
    name: String,
    speed: u64,
}

#[derive(Serialize)]
struct PastServer {
    #[serde(flatten)]
    place: Place,
    name: String,
    bytes: u64,
}

/// Groups per-address values by place; the second value counts what can't be placed.
fn by_place(geo: &Geo, values: impl Iterator<Item = (IpAddr, f64, f64)>) -> (Vec<Spot>, u32) {
    let mut spots: HashMap<String, Spot> = HashMap::new();
    let mut unplaced = 0;
    for (ip, down, up) in values {
        let Some(place) = geo.locate(ip) else {
            unplaced += 1;
            continue;
        };
        let key = format!("{}|{}", place.country.as_deref().unwrap_or(""), place.city.as_deref().unwrap_or(""));
        let spot = spots.entry(key).or_insert(Spot { place, peers: 0, down: 0.0, up: 0.0 });
        spot.peers += 1;
        spot.down += down;
        spot.up += up;
    }
    (spots.into_values().collect(), unplaced)
}

#[tauri::command]
pub fn map_snapshot(engine: State<'_, Engine>, geo: State<'_, Arc<Geo>>, history: State<'_, MapHistory>) -> Value {
    let rec = history.recorder.lock().unwrap();
    let (spots, unplaced) = by_place(&geo, rec.rates.iter().copied());
    let (past, past_unplaced) = by_place(&geo, rec.peers.iter().map(|(ip, (d, u))| (*ip, *d as f64, *u as f64)));
    let past_servers: Vec<PastServer> = engine
        .received_by_server()
        .into_iter()
        .filter(|(_, bytes)| *bytes > 0)
        .filter_map(|(ip, bytes)| {
            let name = rec.server_names.get(&ip).cloned().unwrap_or_else(|| "Direct Download".into());
            Some(PastServer { place: geo.locate(ip)?, name, bytes })
        })
        .collect();
    let unreadable = rec.unreadable;
    drop(rec);

    let servers: Vec<Server> = engine
        .active_servers()
        .into_iter()
        .filter_map(|s| Some(Server { place: geo.locate(s.ip)?, name: s.name, speed: s.speed }))
        .collect();
    json!({
        "db": geo.status(),
        "home": geo::home(),
        "spots": spots,
        "unplaced": unplaced + unreadable,
        "servers": servers,
        "history": {
            "since": history.since,
            "spots": past,
            "unplaced": past_unplaced,
            "servers": past_servers,
        },
    })
}

/// Fetches the location database, once the user asked for it.
#[tauri::command]
pub fn map_download_db(shared: State<'_, Shared>, geo: State<'_, Arc<Geo>>) {
    let (geo, http) = (geo.inner().clone(), shared.http.clone());
    tauri::async_runtime::spawn(geo.download(http));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn counters(entries: &[(&str, u64, u64)]) -> Counters {
        entries.iter().map(|(a, d, u)| ((0, a.to_string()), (*d, *u))).collect()
    }

    #[test]
    fn totals_add_up_across_samples_and_reconnections() {
        let history = MapHistory::default();
        history.sample(counters(&[("203.0.113.9:6881", 1000, 10)]), vec![]);
        history.sample(counters(&[("203.0.113.9:6881", 3000, 30)]), vec![]);
        // The peer reconnected: its counters started over.
        history.sample(counters(&[("203.0.113.9:6881", 500, 5), ("198.51.100.7:51413", 0, 70)]), vec![]);
        let rec = history.recorder.lock().unwrap();
        assert_eq!(rec.peers[&"203.0.113.9".parse::<IpAddr>().unwrap()], (3500, 35));
        assert_eq!(rec.peers[&"198.51.100.7".parse::<IpAddr>().unwrap()], (0, 70));
        assert_eq!(rec.rates.len(), 2);
    }
}
