//! The Bandwidth map: where the torrent peers and Direct Download servers we
//! exchange data with are, and how fast.

use crate::direct::engine::Engine;
use crate::geo::{self, Geo, Place};
use crate::Shared;
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::State;

/// Each live peer's downloaded and uploaded bytes, by torrent and address.
type Counters = HashMap<(usize, String), (u64, u64)>;

/// The peers' byte counters at the last look, to turn them into speeds.
#[derive(Default)]
pub struct PeerRates(Mutex<Option<(Instant, Counters)>>);

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

#[tauri::command]
pub fn map_snapshot(
    shared: State<'_, Shared>,
    engine: State<'_, Engine>,
    geo: State<'_, Arc<Geo>>,
    rates: State<'_, PeerRates>,
) -> Value {
    let now = Instant::now();
    let counters: Counters = shared.session.with_torrents(|it| {
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
    });

    let mut last = rates.0.lock().unwrap();
    let previous = last.take();
    let elapsed = previous.as_ref().map(|(at, _)| now.duration_since(*at).as_secs_f64()).filter(|s| *s > 0.2);
    let mut spots: HashMap<String, Spot> = HashMap::new();
    let mut unplaced = 0u32;
    for (key, (down, up)) in &counters {
        let addr = &key.1;
        let (down_rate, up_rate) = match (&previous, elapsed) {
            (Some((_, before)), Some(secs)) => {
                // A peer seen for the first time has no speed yet.
                let (d0, u0) = before.get(key).copied().unwrap_or((*down, *up));
                (down.saturating_sub(d0) as f64 / secs, up.saturating_sub(u0) as f64 / secs)
            }
            _ => (0.0, 0.0),
        };
        let Some(place) = addr.parse::<SocketAddr>().ok().and_then(|a| geo.locate(a.ip())) else {
            unplaced += 1;
            continue;
        };
        let key = format!("{}|{}", place.country.as_deref().unwrap_or(""), place.city.as_deref().unwrap_or(""));
        let spot = spots.entry(key).or_insert(Spot { place, peers: 0, down: 0.0, up: 0.0 });
        spot.peers += 1;
        spot.down += down_rate;
        spot.up += up_rate;
    }
    *last = Some((now, counters));
    drop(last);

    let servers: Vec<Server> = engine
        .active_servers()
        .into_iter()
        .filter_map(|s| Some(Server { place: geo.locate(s.ip)?, name: s.name, speed: s.speed }))
        .collect();
    json!({
        "db": geo.status(),
        "home": geo::home(),
        "spots": spots.into_values().collect::<Vec<_>>(),
        "unplaced": unplaced,
        "servers": servers,
    })
}

/// Fetches the location database, once the user asked for it.
#[tauri::command]
pub fn map_download_db(shared: State<'_, Shared>, geo: State<'_, Arc<Geo>>) {
    let (geo, http) = (geo.inner().clone(), shared.http.clone());
    tauri::async_runtime::spawn(geo.download(http));
}
