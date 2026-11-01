//! Download engine: queue, segmented HTTP transfers, resume, speed limit,
//! extraction. Independent of Tauri so it can be tested on its own.

use crate::direct::links;
use crate::direct::providers::{Accounts, Provider, Unlocked};
use reqwest::header::{
    HeaderMap, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG, IF_RANGE,
    LAST_MODIFIED, RANGE, RETRY_AFTER,
};
use reqwest::{Client, Response, StatusCode};
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio::sync::{Notify, Semaphore};
use tokio::task::JoinSet;
use tokio::time::{interval, sleep, Instant};
use tokio_util::sync::CancellationToken;

const MIN_SEGMENT: u64 = 4 * 1024 * 1024;
const WRITE_BUFFER: usize = 256 * 1024;
const MAX_RETRIES: u32 = 5;
const PROBE_RETRIES: u32 = 3;
const MAX_RETRY_AFTER: u64 = 300;
const MAX_HTML: usize = 2 * 1024 * 1024;

#[derive(Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct Settings {
    pub download_dir: PathBuf,
    pub max_active: usize,
    pub chunks: usize,
    pub speed_limit_kib: u64,
    pub auto_extract: bool,
    pub delete_archive: bool,
    pub watch_clipboard: bool,
    /// Start copied links at once instead of leaving them in the grabber.
    pub auto_start_clipboard: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            download_dir: PathBuf::new(),
            max_active: 3,
            chunks: 4,
            speed_limit_kib: 0,
            auto_extract: true,
            delete_archive: false,
            watch_clipboard: true,
            auto_start_clipboard: true,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Active,
    Paused,
    Extracting,
    Done,
    Failed,
}

/// A byte range of the file. `end` is inclusive; `None` means "until the
/// stream ends", for servers that don't support ranges.
#[derive(Clone, Serialize, Deserialize)]
pub struct Segment {
    pub start: u64,
    pub end: Option<u64>,
    pub done: u64,
}

impl Segment {
    fn complete(&self) -> bool {
        self.end.is_some_and(|e| self.start + self.done > e)
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub struct Item {
    pub id: u64,
    pub url: String,
    pub name: String,
    pub package: String,
    pub dir: PathBuf,
    pub size: Option<u64>,
    pub resumable: bool,
    pub validator: Option<String>,
    pub status: Status,
    pub error: Option<String>,
    pub output: Option<PathBuf>,
    pub segments: Vec<Segment>,
    /// Debrid service this link goes through; `direct` is its generated URL.
    #[serde(default)]
    pub provider: Option<Provider>,
    #[serde(default)]
    pub direct: Option<String>,
    #[serde(default)]
    pub max_chunks: Option<usize>,
    #[serde(default)]
    pub done: u64,
    #[serde(default)]
    pub speed: u64,
}

impl Item {
    fn bytes_done(&self) -> u64 {
        self.segments.iter().map(|s| s.done).sum()
    }
    fn part_path(&self) -> PathBuf {
        self.dir.join(format!(".{}.qpart", self.id))
    }
}

/// What the link grabber learned about a URL.
#[derive(Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct Probe {
    pub url: String,
    pub name: String,
    pub package: String,
    pub size: Option<u64>,
    pub resumable: bool,
    pub validator: Option<String>,
    pub online: bool,
    pub error: Option<String>,
    pub duplicate: bool,
    pub provider: Option<Provider>,
    pub direct: Option<String>,
    pub max_chunks: Option<usize>,
    /// A web page to open in a browser, rather than a file.
    pub page: bool,
    /// The page hides its links behind a captcha the user has to solve.
    pub protected: bool,
    #[serde(skip)]
    html: bool,
    #[serde(skip)]
    transient: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct State {
    next_id: u64,
    settings: Settings,
    items: Vec<Item>,
    #[serde(skip)]
    running: HashMap<u64, CancellationToken>,
}

/// Global token bucket: each transfer reserves its bytes on one shared timeline.
#[derive(Default)]
struct Limiter {
    bps: AtomicU64,
    next: Mutex<Option<Instant>>,
}

impl Limiter {
    async fn take(&self, bytes: u64) {
        let bps = self.bps.load(Ordering::Relaxed);
        if bps == 0 {
            return;
        }
        let now = Instant::now();
        let wait = {
            let mut next = self.next.lock().unwrap();
            let start = next.unwrap_or(now).max(now);
            *next = Some(start + Duration::from_secs_f64(bytes as f64 / bps as f64));
            start - now
        };
        if !wait.is_zero() {
            sleep(wait).await;
        }
    }
}

enum DlError {
    Cancelled,
    /// The server no longer honours our ranges: start over from zero.
    Restart,
    Fatal(String),
}

enum Attempt {
    Stop(DlError),
    Retry { msg: String, wait: Option<Duration>, progressed: bool },
}

#[derive(Clone)]
struct Job {
    id: u64,
    index: usize,
    url: String,
    part: PathBuf,
    resumable: bool,
    validator: Option<String>,
}

struct Inner {
    http: Client,
    accounts: Arc<Accounts>,
    path: PathBuf,
    state: Mutex<State>,
    limiter: Limiter,
    wake: Notify,
}

#[derive(Clone)]
pub struct Engine(Arc<Inner>);

impl Engine {
    /// Loads the queue from `path`. Interrupted transfers go back in the queue.
    pub fn new(path: PathBuf, default_dir: PathBuf) -> Self {
        let mut state: State = std::fs::read(&path)
            .ok()
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if state.settings.download_dir.as_os_str().is_empty() {
            state.settings.download_dir = default_dir;
        }
        for item in &mut state.items {
            if matches!(item.status, Status::Active | Status::Extracting) {
                item.status = Status::Queued;
            }
        }
        let http = Client::builder()
            .user_agent(concat!("Pulsar/", env!("CARGO_PKG_VERSION")))
            .connect_timeout(Duration::from_secs(20))
            .read_timeout(Duration::from_secs(60))
            .build()
            .expect("http client");
        let limiter = Limiter::default();
        limiter.bps.store(state.settings.speed_limit_kib * 1024, Ordering::Relaxed);
        let accounts = Arc::new(Accounts::new(http.clone()));
        Self(Arc::new(Inner { http, accounts, path, state: Mutex::new(state), limiter, wake: Notify::new() }))
    }

    /// Scheduler: starts queued items and refreshes speeds. Runs forever.
    pub async fn run_loop(self) {
        let mut tick = interval(Duration::from_secs(1));
        let mut ticks = 0u32;
        loop {
            tokio::select! {
                _ = tick.tick() => {
                    ticks += 1;
                    let busy = self.update_speeds();
                    if busy && ticks % 5 == 0 {
                        self.save();
                    }
                }
                _ = self.0.wake.notified() => {}
            }
            self.schedule();
        }
    }

    pub fn accounts(&self) -> &Accounts {
        &self.0.accounts
    }

    pub fn settings(&self) -> Settings {
        self.0.state.lock().unwrap().settings.clone()
    }

    pub fn set_settings(&self, mut settings: Settings) {
        settings.max_active = settings.max_active.clamp(1, 20);
        settings.chunks = settings.chunks.clamp(1, 16);
        self.0.limiter.bps.store(settings.speed_limit_kib * 1024, Ordering::Relaxed);
        self.0.state.lock().unwrap().settings = settings;
        self.save();
        self.0.wake.notify_one();
    }

    pub fn list(&self) -> Vec<Item> {
        let st = self.0.state.lock().unwrap();
        let mut items = st.items.clone();
        for item in &mut items {
            item.done = item.bytes_done();
        }
        items
    }

    /// Link grabber: finds URLs in `text`, follows HTML pages one level down
    /// to the files they link to, and reports each file's status. With
    /// `files_only`, plain web pages are dropped: copying an article's URL
    /// shouldn't offer to download the article.
    pub async fn grab(&self, text: &str, files_only: bool) -> Vec<Probe> {
        let first = self.probe_all(links::extract_urls(text)).await;
        let mut found = Vec::new();
        let mut followed = Vec::new();
        for probe in first {
            if probe.html {
                let urls = self.page_links(&probe.url).await;
                if !urls.is_empty() {
                    followed.extend(urls);
                    continue;
                }
                if files_only {
                    continue;
                }
                found.push(Probe {
                    online: false,
                    html: false,
                    page: true,
                    error: Some("This is a web page, not a file".into()),
                    ..probe
                });
                continue;
            }
            found.push(probe);
        }
        found.extend(self.probe_all(followed).await);

        let known: HashSet<String> =
            self.0.state.lock().unwrap().items.iter().map(|i| i.url.clone()).collect();
        let mut seen = HashSet::new();
        found.retain(|p| seen.insert(p.url.clone()));
        for probe in &mut found {
            probe.duplicate = known.contains(&probe.url);
        }
        found.sort_by(|a, b| (&a.package, &a.name).cmp(&(&b.package, &b.name)));
        found
    }

    async fn probe_all(&self, urls: Vec<String>) -> Vec<Probe> {
        let gate = Arc::new(Semaphore::new(8));
        let mut set = JoinSet::new();
        for url in urls {
            let http = self.0.http.clone();
            let accounts = self.0.accounts.clone();
            let gate = gate.clone();
            set.spawn(async move {
                let _permit = gate.acquire().await;
                inspect(&http, &accounts, &url).await
            });
        }
        set.join_all().await
    }

    async fn page_links(&self, url: &str) -> Vec<String> {
        let Ok(mut resp) = self.0.http.get(url).send().await else { return vec![] };
        let base = resp.url().to_string();
        let mut body = Vec::new();
        while let Ok(Some(chunk)) = resp.chunk().await {
            body.extend_from_slice(&chunk);
            if body.len() >= MAX_HTML {
                break;
            }
        }
        links::extract_file_links(&String::from_utf8_lossy(&body), &base)
    }

    pub fn add(&self, probes: Vec<Probe>) {
        {
            let mut st = self.0.state.lock().unwrap();
            for p in probes {
                st.next_id += 1;
                let package = links::sanitize(&p.package);
                let item = Item {
                    id: st.next_id,
                    dir: st.settings.download_dir.join(&package),
                    url: p.url,
                    name: links::sanitize(&p.name),
                    package,
                    size: p.size,
                    resumable: p.resumable,
                    validator: p.validator,
                    status: Status::Queued,
                    error: None,
                    output: None,
                    segments: vec![],
                    provider: p.provider,
                    direct: p.direct,
                    max_chunks: p.max_chunks,
                    done: 0,
                    speed: 0,
                };
                st.items.push(item);
            }
        }
        self.save();
        self.0.wake.notify_one();
    }

    pub fn pause(&self, id: u64) {
        {
            let mut st = self.0.state.lock().unwrap();
            let Some(item) = st.items.iter_mut().find(|i| i.id == id) else { return };
            if !matches!(item.status, Status::Queued | Status::Active) {
                return;
            }
            item.status = Status::Paused;
            item.speed = 0;
            if let Some(token) = st.running.get(&id) {
                token.cancel();
            }
        }
        self.save();
    }

    pub fn resume(&self, id: u64) {
        self.update(id, |item| {
            if matches!(item.status, Status::Paused | Status::Failed) {
                item.status = Status::Queued;
                item.error = None;
            }
        });
        self.save();
        self.0.wake.notify_one();
    }

    pub fn pause_all(&self) {
        for id in self.ids() {
            self.pause(id);
        }
    }

    pub fn resume_all(&self) {
        for id in self.ids() {
            self.resume(id);
        }
    }

    fn ids(&self) -> Vec<u64> {
        self.0.state.lock().unwrap().items.iter().map(|i| i.id).collect()
    }

    /// Drops an item. The partial file always goes; the finished file only
    /// when `delete_file` is set.
    pub fn remove(&self, id: u64, delete_file: bool) {
        let removed = {
            let mut st = self.0.state.lock().unwrap();
            if let Some(token) = st.running.get(&id) {
                token.cancel();
            }
            let pos = st.items.iter().position(|i| i.id == id);
            pos.map(|p| st.items.remove(p))
        };
        if let Some(item) = removed {
            let _ = std::fs::remove_file(item.part_path());
            if let (true, Some(output)) = (delete_file, &item.output) {
                // After extraction the output is the unpacked folder.
                let _ = match output.is_dir() {
                    true => std::fs::remove_dir_all(output),
                    false => std::fs::remove_file(output),
                };
            }
            let _ = std::fs::remove_dir(&item.dir); // only succeeds when empty
        }
        self.save();
    }

    /// Forgets finished items, keeping their files.
    pub fn clear_done(&self) {
        self.0.state.lock().unwrap().items.retain(|i| i.status != Status::Done);
        self.save();
    }

    fn update<T>(&self, id: u64, f: impl FnOnce(&mut Item) -> T) -> Option<T> {
        let mut st = self.0.state.lock().unwrap();
        st.items.iter_mut().find(|i| i.id == id).map(f)
    }

    fn snapshot(&self, id: u64) -> Option<Item> {
        self.update(id, |item| item.clone())
    }

    fn save(&self) {
        let bytes = {
            let st = self.0.state.lock().unwrap();
            serde_json::to_vec_pretty(&*st).expect("state serializes")
        };
        let tmp = self.0.path.with_extension("tmp");
        if std::fs::write(&tmp, bytes).is_ok() {
            let _ = std::fs::rename(&tmp, &self.0.path);
        }
    }

    fn update_speeds(&self) -> bool {
        let mut st = self.0.state.lock().unwrap();
        let mut busy = false;
        for item in &mut st.items {
            if item.status == Status::Active {
                busy = true;
                let now = item.bytes_done();
                item.speed = now.saturating_sub(item.done);
                item.done = now;
            } else {
                item.speed = 0;
            }
        }
        busy
    }

    fn schedule(&self) {
        let mut started = Vec::new();
        {
            let mut guard = self.0.state.lock().unwrap();
            let st = &mut *guard;
            while st.running.len() < st.settings.max_active {
                let Some(item) = st
                    .items
                    .iter_mut()
                    .find(|i| i.status == Status::Queued && !st.running.contains_key(&i.id))
                else {
                    break;
                };
                item.status = Status::Active;
                item.done = item.bytes_done();
                let token = CancellationToken::new();
                st.running.insert(item.id, token.clone());
                started.push((item.id, token));
            }
        }
        for (id, token) in started {
            tokio::spawn(self.clone().run_item(id, token));
        }
    }

    async fn run_item(self, id: u64, token: CancellationToken) {
        let part = self.snapshot(id).map(|item| item.part_path());
        let result = self.download(id, &token).await;
        // Removed mid-flight: the transfer may have recreated the partial file.
        if let (None, Some(part)) = (self.snapshot(id), &part) {
            let _ = fs::remove_file(part).await;
        }
        match result {
            Ok(()) => self.finish(id).await,
            Err(DlError::Fatal(msg)) => {
                self.update(id, |item| {
                    if item.status == Status::Active {
                        item.status = Status::Failed;
                        item.error = Some(msg);
                        item.speed = 0;
                    }
                });
            }
            Err(_) => {}
        }
        self.0.state.lock().unwrap().running.remove(&id);
        self.save();
        self.0.wake.notify_one();
    }

    async fn download(&self, id: u64, token: &CancellationToken) -> Result<(), DlError> {
        for _ in 0..2 {
            match self.transfer(id, token).await {
                Err(DlError::Restart) => {
                    self.update(id, |item| item.segments.clear());
                }
                other => return other,
            }
        }
        Err(DlError::Fatal("the file changed on the server".into()))
    }

    async fn transfer(&self, id: u64, token: &CancellationToken) -> Result<(), DlError> {
        let mut item = self.snapshot(id).ok_or(DlError::Cancelled)?;
        let part = item.part_path();
        let mut fresh = item.segments.is_empty() || !part.exists();
        // Generated links expire and may move servers: get a new one every time.
        if let Some(provider) = item.provider {
            let unlocked = tokio::select! {
                _ = token.cancelled() => return Err(DlError::Cancelled),
                r = self.0.accounts.unlock(provider, &item.url) => r,
            };
            let Unlocked { url, max_chunks, .. } = unlocked
                .map_err(|e| DlError::Fatal(format!("{}: {}", provider.name(), e.message())))?;
            item = self
                .update(id, |it| {
                    it.direct = Some(url);
                    it.max_chunks = max_chunks;
                    it.clone()
                })
                .ok_or(DlError::Cancelled)?;
        }
        let target = item.direct.clone().unwrap_or_else(|| item.url.clone());
        if fresh || item.provider.is_some() {
            let info = tokio::select! {
                _ = token.cancelled() => return Err(DlError::Cancelled),
                p = probe(&self.0.http, &target) => p,
            };
            if !info.online {
                return Err(DlError::Fatal(info.error.unwrap_or_else(|| "offline".into())));
            }
            if info.html {
                return Err(DlError::Fatal("The server sent a web page, not a file".into()));
            }
            fresh |= info.size != item.size || info.resumable != item.resumable;
            let chunks = item.max_chunks.unwrap_or(usize::MAX).min(self.settings().chunks);
            item = self
                .update(id, |it| {
                    if fresh {
                        it.size = info.size;
                        it.resumable = info.resumable;
                        it.validator = info.validator;
                        it.segments = plan_segments(info.size, info.resumable, chunks);
                    } else {
                        // Another server's ETag says nothing about our bytes.
                        it.validator = None;
                    }
                    it.clone()
                })
                .ok_or(DlError::Cancelled)?;
        }

        fs::create_dir_all(&item.dir).await.map_err(io_error)?;
        let file = OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&part)
            .await
            .map_err(io_error)?;
        if let (Some(size), true) = (item.size, item.resumable) {
            file.set_len(size).await.map_err(io_error)?;
        }
        drop(file);

        let group = token.child_token();
        let mut set = JoinSet::new();
        for (index, segment) in item.segments.iter().enumerate() {
            if segment.complete() {
                continue;
            }
            let job = Job {
                id,
                index,
                url: target.clone(),
                part: part.clone(),
                resumable: item.resumable,
                validator: item.validator.clone(),
            };
            set.spawn(self.clone().run_segment(job, group.clone()));
        }
        let mut outcome = Ok(());
        while let Some(joined) = set.join_next().await {
            let result = joined.unwrap_or_else(|e| Err(DlError::Fatal(e.to_string())));
            if let (Err(e), true) = (result, outcome.is_ok()) {
                outcome = Err(e);
                group.cancel();
            }
        }
        outcome
    }

    async fn run_segment(self, job: Job, token: CancellationToken) -> Result<(), DlError> {
        let mut attempts = 0;
        loop {
            let (msg, wait) = match self.stream_segment(&job, &token).await {
                Ok(()) => return Ok(()),
                Err(Attempt::Stop(e)) => return Err(e),
                Err(Attempt::Retry { msg, wait, progressed }) => {
                    attempts = if progressed { 1 } else { attempts + 1 };
                    (msg, wait)
                }
            };
            if attempts > MAX_RETRIES {
                return Err(DlError::Fatal(msg));
            }
            let backoff = wait.unwrap_or(Duration::from_secs(1 << (attempts - 1).min(5)));
            tokio::select! {
                _ = token.cancelled() => return Err(DlError::Cancelled),
                _ = sleep(backoff) => {}
            }
        }
    }

    /// One connection's worth of a segment. Data reaches the disk before it is
    /// counted, so a saved `done` never promises bytes the file doesn't hold.
    async fn stream_segment(&self, job: &Job, token: &CancellationToken) -> Result<(), Attempt> {
        let stop = |e| Attempt::Stop(e);
        let segment = self
            .update(job.id, |item| {
                if !job.resumable {
                    item.segments[job.index].done = 0;
                }
                item.segments[job.index].clone()
            })
            .ok_or(Attempt::Stop(DlError::Cancelled))?;
        let pos = segment.start + segment.done;

        let mut request = self.0.http.get(&job.url);
        if job.resumable {
            let end = segment.end.map(|e| e.to_string()).unwrap_or_default();
            request = request.header(RANGE, format!("bytes={pos}-{end}"));
            if let Some(validator) = &job.validator {
                request = request.header(IF_RANGE, validator);
            }
        }
        let sent = tokio::select! {
            _ = token.cancelled() => return Err(stop(DlError::Cancelled)),
            r = request.send() => r,
        };
        let mut resp = sent.map_err(|e| Attempt::Retry {
            msg: format!("connection: {e}"),
            wait: None,
            progressed: false,
        })?;
        check_status(&resp, job.resumable)?;

        let mut file = OpenOptions::new()
            .write(true)
            .truncate(!job.resumable)
            .open(&job.part)
            .await
            .map_err(|e| stop(io_error(e)))?;
        file.seek(SeekFrom::Start(pos)).await.map_err(|e| stop(io_error(e)))?;

        let mut buf = Vec::with_capacity(WRITE_BUFFER * 2);
        let mut remaining = segment.end.map(|e| e + 1 - pos);
        let mut progressed = false;
        let outcome = loop {
            let next = tokio::select! {
                _ = token.cancelled() => break Err(None),
                r = resp.chunk() => r,
            };
            match next {
                Ok(Some(mut chunk)) => {
                    if let Some(left) = remaining {
                        chunk.truncate(left.min(usize::MAX as u64) as usize);
                    }
                    let n = chunk.len() as u64;
                    tokio::select! {
                        _ = token.cancelled() => break Err(None),
                        _ = self.0.limiter.take(n) => {}
                    }
                    buf.extend_from_slice(&chunk);
                    if let Some(left) = remaining.as_mut() {
                        *left -= n;
                    }
                    if buf.len() >= WRITE_BUFFER {
                        self.commit(job, &mut file, &mut buf).await.map_err(stop)?;
                        progressed = true;
                    }
                    if remaining == Some(0) {
                        break Ok(());
                    }
                }
                Ok(None) if remaining.is_some_and(|left| left > 0) => {
                    break Err(Some("connection dropped by the server".to_string()));
                }
                Ok(None) => break Ok(()),
                Err(e) => break Err(Some(format!("read: {e}"))),
            }
        };
        progressed |= !buf.is_empty();
        self.commit(job, &mut file, &mut buf).await.map_err(stop)?;
        match outcome {
            Ok(()) => Ok(()),
            Err(Some(msg)) => Err(Attempt::Retry { msg, wait: None, progressed }),
            Err(None) => Err(stop(DlError::Cancelled)),
        }
    }

    async fn commit(&self, job: &Job, file: &mut fs::File, buf: &mut Vec<u8>) -> Result<(), DlError> {
        file.write_all(buf).await.map_err(io_error)?;
        file.flush().await.map_err(io_error)?;
        let n = buf.len() as u64;
        buf.clear();
        self.update(job.id, |item| item.segments[job.index].done += n)
            .ok_or(DlError::Cancelled)
    }

    /// Moves the completed file into place, then unpacks it if asked to.
    async fn finish(&self, id: u64) {
        let Some(item) = self.snapshot(id) else { return };
        let output = unique_path(&item.dir, &item.name);
        if let Err(e) = fs::rename(item.part_path(), &output).await {
            self.update(id, |it| {
                it.status = Status::Failed;
                it.error = Some(format!("disk: {e}"));
            });
            return;
        }
        let settings = self.settings();
        let extract = settings.auto_extract && links::is_extractable(&item.name);
        self.update(id, |it| {
            it.output = Some(output.clone());
            it.speed = 0;
            it.status = if extract { Status::Extracting } else { Status::Done };
        });
        if !extract {
            return;
        }
        let target = unique_path(&item.dir, &links::archive_stem(&item.name));
        let error = extract_archive(&output, &target).await.err();
        if error.is_some() {
            let _ = fs::remove_dir_all(&target).await;
        } else if settings.delete_archive {
            let _ = fs::remove_file(&output).await;
        }
        self.update(id, |it| {
            it.status = Status::Done;
            it.error = error.map(|e| format!("extraction failed: {e}"));
            if it.error.is_none() {
                it.output = Some(target);
            }
        });
    }
}

fn io_error(e: std::io::Error) -> DlError {
    DlError::Fatal(format!("disk: {e}"))
}

fn check_status(resp: &Response, resumable: bool) -> Result<(), Attempt> {
    let status = resp.status();
    match status {
        StatusCode::PARTIAL_CONTENT if resumable => Ok(()),
        StatusCode::OK if !resumable => Ok(()),
        StatusCode::OK | StatusCode::RANGE_NOT_SATISFIABLE => Err(Attempt::Stop(DlError::Restart)),
        StatusCode::TOO_MANY_REQUESTS | StatusCode::REQUEST_TIMEOUT => Err(retry_status(resp)),
        s if s.is_server_error() => Err(retry_status(resp)),
        s => Err(Attempt::Stop(DlError::Fatal(format!("HTTP {}", s.as_u16())))),
    }
}

fn retry_status(resp: &Response) -> Attempt {
    let wait = resp
        .headers()
        .get(RETRY_AFTER)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
        .map(|s| Duration::from_secs(s.min(MAX_RETRY_AFTER)));
    Attempt::Retry { msg: format!("HTTP {}", resp.status().as_u16()), wait, progressed: false }
}

fn plan_segments(size: Option<u64>, resumable: bool, chunks: usize) -> Vec<Segment> {
    let Some(size) = size.filter(|s| resumable && *s > 0) else {
        return vec![Segment { start: 0, end: None, done: 0 }];
    };
    let count = (size / MIN_SEGMENT).clamp(1, chunks.max(1) as u64);
    let step = size / count;
    (0..count)
        .map(|i| Segment {
            start: i * step,
            end: Some(if i + 1 == count { size - 1 } else { (i + 1) * step - 1 }),
            done: 0,
        })
        .collect()
}

fn unique_path(dir: &Path, name: &str) -> PathBuf {
    let first = dir.join(name);
    if !first.exists() {
        return first;
    }
    let (stem, ext) = match name.rsplit_once('.') {
        Some((s, e)) if !s.is_empty() => (s, format!(".{e}")),
        _ => (name, String::new()),
    };
    (1..)
        .map(|n| dir.join(format!("{stem} ({n}){ext}")))
        .find(|p| !p.exists())
        .expect("a free name exists")
}

/// macOS `tar` is bsdtar: it reads zip, tar, 7z and rar, and by default refuses
/// paths that climb out of the target folder.
async fn extract_archive(archive: &Path, target: &Path) -> Result<(), String> {
    fs::create_dir_all(target).await.map_err(|e| e.to_string())?;
    let output = tokio::process::Command::new("/usr/bin/tar")
        .arg("-xf")
        .arg(archive)
        .arg("-C")
        .arg(target)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

/// Sends the link through a debrid service if one handles its host. The queue
/// keeps the original URL; a generated one doesn't outlive the session.
async fn inspect(http: &Client, accounts: &Accounts, url: &str) -> Probe {
    let (provider, unlocked) = match accounts.unlock_any(url).await {
        None => {
            if links::is_link_protector(url) {
                let name = links::name_from_url(url);
                return Probe {
                    url: url.to_string(),
                    package: links::package_name(&name),
                    name,
                    page: true,
                    protected: true,
                    error: Some("Protected page: solve its captcha to get the links".into()),
                    ..Default::default()
                };
            }
            let mut result = probe(http, url).await;
            // A hoster answers with its landing page, or blocks us outright.
            if (result.html || !result.online) && accounts.is_known_hoster(url).await {
                result.online = false;
                result.html = false;
                result.error = Some("No connected account supports this host".into());
            }
            return result;
        }
        Some(Ok(found)) => found,
        Some(Err(message)) => {
            let name = links::name_from_url(url);
            return Probe {
                url: url.to_string(),
                package: links::package_name(&name),
                name,
                error: Some(message),
                ..Default::default()
            };
        }
    };
    let mut result = probe(http, &unlocked.url).await;
    result.url = url.to_string();
    result.html = false;
    result.provider = Some(provider);
    result.direct = Some(unlocked.url);
    result.max_chunks = unlocked.max_chunks;
    if let Some(name) = unlocked.name {
        result.name = links::sanitize(&name);
        result.package = links::package_name(&result.name);
    }
    result.size = result.size.or(unlocked.size);
    result
}

/// A pooled connection the server just closed, or a passing outage, must not
/// read as a dead link.
async fn probe(http: &Client, url: &str) -> Probe {
    let mut result = probe_once(http, url).await;
    for attempt in 0..PROBE_RETRIES {
        if !result.transient {
            break;
        }
        sleep(Duration::from_millis(500 << attempt)).await;
        result = probe_once(http, url).await;
    }
    result
}

/// Asks for the first byte: a 206 tells us both the size and range support.
async fn probe_once(http: &Client, url: &str) -> Probe {
    let mut result = Probe { url: url.to_string(), ..Default::default() };
    let mut sent = http.get(url).header(RANGE, "bytes=0-0").send().await;
    if matches!(&sent, Ok(r) if r.status() == StatusCode::RANGE_NOT_SATISFIABLE) {
        sent = http.get(url).send().await; // empty file
    }
    let resp = match sent {
        Ok(resp) => resp,
        Err(e) => {
            result.name = links::name_from_url(url);
            result.package = links::package_name(&result.name);
            result.error = Some(format!("connection: {e}"));
            result.transient = true;
            return result;
        }
    };
    let headers = resp.headers();
    let status = resp.status();
    result.name = header(headers, CONTENT_DISPOSITION)
        .and_then(links::name_from_disposition)
        .unwrap_or_else(|| links::name_from_url(resp.url().as_str()));
    result.package = links::package_name(&result.name);
    if !status.is_success() {
        result.error = Some(format!("HTTP {}", status.as_u16()));
        result.transient = status.is_server_error() || status == StatusCode::TOO_MANY_REQUESTS;
        return result;
    }
    result.online = true;
    result.html = header(headers, CONTENT_TYPE).is_some_and(|t| t.contains("text/html"));
    if status == StatusCode::PARTIAL_CONTENT {
        let total = header(headers, CONTENT_RANGE)
            .and_then(|v| v.rsplit('/').next())
            .and_then(|v| v.parse::<u64>().ok());
        result.size = total;
        result.resumable = total.is_some();
    } else {
        result.size = header(headers, CONTENT_LENGTH).and_then(|v| v.parse().ok());
    }
    result.validator = header(headers, ETAG)
        .filter(|v| !v.starts_with("W/"))
        .or_else(|| header(headers, LAST_MODIFIED))
        .map(str::to_string);
    result
}

fn header(headers: &HeaderMap, name: reqwest::header::HeaderName) -> Option<&str> {
    headers.get(name).and_then(|v| v.to_str().ok())
}

#[cfg(test)]
#[path = "engine_tests.rs"]
mod tests;
