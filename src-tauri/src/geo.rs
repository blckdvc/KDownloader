//! Where the addresses we exchange data with are, for the Bandwidth map.
//!
//! Lookups use DB-IP's free city database (CC BY 4.0), downloaded once into the
//! app's folder when the user asks for it: no address is ever sent anywhere to
//! be located. The user's own place comes from the Mac's time zone.

use maxminddb::Reader;
use maxminddb::Mmap;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::io::Write;
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const DB_FILE: &str = "dbip-city-lite.mmdb";

#[derive(Clone, Serialize, PartialEq)]
pub struct Place {
    pub lat: f64,
    pub lon: f64,
    pub city: Option<String>,
    pub country: Option<String>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "state", rename_all = "lowercase")]
pub enum DbStatus {
    Missing,
    Downloading { done: u64, total: Option<u64> },
    Ready { built: String },
    Failed { error: String },
}

pub struct Geo {
    dir: PathBuf,
    reader: RwLock<Option<Arc<Reader<Mmap>>>>,
    status: Mutex<DbStatus>,
    cache: Mutex<HashMap<IpAddr, Option<Place>>>,
}

#[derive(Deserialize)]
struct Record {
    city: Option<Named>,
    country: Option<Named>,
    location: Option<Location>,
}

#[derive(Deserialize)]
struct Named {
    names: Option<HashMap<String, String>>,
}

#[derive(Deserialize)]
struct Location {
    latitude: Option<f64>,
    longitude: Option<f64>,
}

impl Named {
    fn english(&self) -> Option<String> {
        self.names.as_ref()?.get("en").cloned()
    }
}

fn open(path: &Path) -> Option<Reader<Mmap>> {
    // SAFETY: the file is only ever replaced by a rename, never written in
    // place, so the mapped bytes stay valid.
    unsafe { Reader::open_mmap(path) }.ok()
}

fn built(reader: &Reader<Mmap>) -> String {
    let at = UNIX_EPOCH + Duration::from_secs(reader.metadata().build_epoch);
    let (y, m) = year_month(at);
    format!("{y}-{m:02}")
}

impl Geo {
    pub fn new(dir: PathBuf) -> Self {
        let reader = open(&dir.join(DB_FILE));
        let status = match &reader {
            Some(r) => DbStatus::Ready { built: built(r) },
            None => DbStatus::Missing,
        };
        Geo { dir, reader: RwLock::new(reader.map(Arc::new)), status: Mutex::new(status), cache: Mutex::default() }
    }

    pub fn status(&self) -> DbStatus {
        self.status.lock().unwrap().clone()
    }

    /// Where an address is, if it's public and the database knows it.
    pub fn locate(&self, ip: IpAddr) -> Option<Place> {
        if !is_public(ip) {
            return None;
        }
        if let Some(hit) = self.cache.lock().unwrap().get(&ip) {
            return hit.clone();
        }
        let reader = self.reader.read().unwrap().clone()?;
        let record: Option<Record> = reader.lookup(ip).ok().and_then(|r| r.decode().ok().flatten());
        let place = record.and_then(|r| {
            let loc = r.location?;
            Some(Place {
                lat: loc.latitude?,
                lon: loc.longitude?,
                city: r.city.and_then(|c| c.english()),
                country: r.country.and_then(|c| c.english()),
            })
        });
        let mut cache = self.cache.lock().unwrap();
        if cache.len() > 20_000 {
            cache.clear();
        }
        cache.insert(ip, place.clone());
        place
    }

    /// Fetches this month's database (or last month's, early in the month),
    /// unpacks it beside the app's settings and starts using it.
    pub async fn download(self: Arc<Self>, http: reqwest::Client) {
        {
            let mut status = self.status.lock().unwrap();
            if matches!(*status, DbStatus::Downloading { .. }) {
                return;
            }
            *status = DbStatus::Downloading { done: 0, total: None };
        }
        let result = self.fetch(&http).await;
        let status = match result {
            Ok(reader) => {
                let built = built(&reader);
                *self.reader.write().unwrap() = Some(Arc::new(reader));
                self.cache.lock().unwrap().clear();
                tracing::info!("location database ready ({built})");
                DbStatus::Ready { built }
            }
            Err(error) => {
                tracing::warn!("location database download failed: {error}");
                DbStatus::Failed { error }
            }
        };
        *self.status.lock().unwrap() = status;
    }

    async fn fetch(&self, http: &reqwest::Client) -> Result<Reader<Mmap>, String> {
        let (y, m) = year_month(SystemTime::now());
        let (py, pm) = if m == 1 { (y - 1, 12) } else { (y, m - 1) };
        let mut resp = None;
        for (y, m) in [(y, m), (py, pm)] {
            let url = format!("https://download.db-ip.com/free/dbip-city-lite-{y}-{m:02}.mmdb.gz");
            let r = http.get(&url).send().await.map_err(|e| format!("connection: {e}"))?;
            if r.status().is_success() {
                resp = Some(r);
                break;
            }
        }
        let mut resp = resp.ok_or("DB-IP didn't offer its database")?;
        let total = resp.content_length();
        std::fs::create_dir_all(&self.dir).map_err(|e| e.to_string())?;
        let gz = self.dir.join(format!("{DB_FILE}.gz.part"));
        let mut file = std::fs::File::create(&gz).map_err(|e| e.to_string())?;
        let mut done = 0;
        while let Some(chunk) = resp.chunk().await.map_err(|e| format!("connection: {e}"))? {
            file.write_all(&chunk).map_err(|e| e.to_string())?;
            done += chunk.len() as u64;
            *self.status.lock().unwrap() = DbStatus::Downloading { done, total };
        }
        drop(file);
        let dir = self.dir.clone();
        tokio::task::spawn_blocking(move || unpack(&gz, &dir)).await.map_err(|e| e.to_string())??;
        open(&self.dir.join(DB_FILE)).ok_or_else(|| "the downloaded database can't be read".into())
    }
}

/// Unpacks the downloaded archive next to the database, then swaps it in.
fn unpack(gz: &Path, dir: &Path) -> Result<(), String> {
    let part = dir.join(format!("{DB_FILE}.part"));
    let mut input = flate2::read::GzDecoder::new(std::fs::File::open(gz).map_err(|e| e.to_string())?);
    let mut output = std::fs::File::create(&part).map_err(|e| e.to_string())?;
    let unpacked = std::io::copy(&mut input, &mut output);
    let _ = std::fs::remove_file(gz);
    unpacked.map_err(|e| format!("the download is damaged: {e}"))?;
    std::fs::rename(&part, dir.join(DB_FILE)).map_err(|e| e.to_string())
}

fn is_public(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !(v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() || v4.is_broadcast()
                || v4.octets()[0] == 100 && (v4.octets()[1] & 0xc0) == 64)
        }
        IpAddr::V6(v6) => {
            let first = v6.segments()[0];
            !(v6.is_loopback() || v6.is_unspecified() || (first & 0xfe00) == 0xfc00 || (first & 0xffc0) == 0xfe80)
        }
    }
}

/// The calendar year and month of a moment, in UTC.
fn year_month(at: SystemTime) -> (i64, u32) {
    let days = at.duration_since(UNIX_EPOCH).unwrap_or_default().as_secs() as i64 / 86_400;
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400 + if m <= 2 { 1 } else { 0 };
    (y, m)
}

/// The Mac's own place: its time zone's main city.
pub fn home() -> Option<Place> {
    let zone = std::fs::read_link("/etc/localtime").ok()?;
    let zone = zone.to_string_lossy();
    let zone = zone.split("zoneinfo/").nth(1)?.to_string();
    let table = ["/var/db/timezone/zoneinfo/zone.tab", "/usr/share/zoneinfo/zone.tab"]
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok())?;
    let line = table.lines().find(|l| l.split('\t').nth(2) == Some(zone.as_str()))?;
    let (lat, lon) = parse_coordinates(line.split('\t').nth(1)?)?;
    let city = zone.rsplit('/').next()?.replace('_', " ");
    Some(Place { lat, lon, city: Some(city), country: None })
}

/// zone.tab's ±DDMM±DDDMM or ±DDMMSS±DDDMMSS.
fn parse_coordinates(text: &str) -> Option<(f64, f64)> {
    let split = text[1..].find(['+', '-'])? + 1;
    let angle = |s: &str, degree_digits: usize| -> Option<f64> {
        let sign = if s.starts_with('-') { -1.0 } else { 1.0 };
        let d = &s[1..];
        let deg: f64 = d.get(..degree_digits)?.parse().ok()?;
        let min: f64 = d.get(degree_digits..degree_digits + 2)?.parse().ok()?;
        let sec: f64 = d.get(degree_digits + 2..).filter(|x| !x.is_empty()).map_or(Some(0.0), |x| x.parse().ok())?;
        Some(sign * (deg + min / 60.0 + sec / 3600.0))
    };
    Some((angle(&text[..split], 2)?, angle(&text[split..], 3)?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_time_zone_coordinates() {
        let (lat, lon) = parse_coordinates("+4852+00220").unwrap();
        assert!((lat - 48.866).abs() < 0.01 && (lon - 2.333).abs() < 0.01);
        let (lat, lon) = parse_coordinates("-334520+1510500").unwrap();
        assert!((lat + 33.755).abs() < 0.01 && (lon - 151.083).abs() < 0.01);
    }

    #[test]
    fn knows_the_month() {
        // 2026-11-08
        assert_eq!(year_month(UNIX_EPOCH + Duration::from_secs(1_794_134_400)), (2026, 11));
        assert_eq!(year_month(UNIX_EPOCH + Duration::from_secs(951_782_400)), (2000, 2)); // 2000-02-29
    }

    #[test]
    fn private_addresses_have_no_place() {
        for ip in ["192.168.1.2", "10.0.0.1", "127.0.0.1", "100.64.0.1", "fe80::1", "fd00::1"] {
            assert!(!is_public(ip.parse().unwrap()), "{ip}");
        }
        assert!(is_public("203.0.113.9".parse().unwrap()));
    }
}
