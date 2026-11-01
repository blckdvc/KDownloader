//! End-to-end engine tests against a small real HTTP server on localhost.

use super::*;
use crate::direct::providers::tests::mock_api;
use std::sync::atomic::AtomicBool;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

#[derive(Default)]
struct Behavior {
    no_ranges: bool,
    /// The next response on this path is cut off halfway through.
    cut_once: AtomicBool,
    /// Pretends the file changed when a resume presents the old validator.
    changed: AtomicBool,
    delay_ms: u64,
}

struct Server {
    base: String,
    ranges: Arc<Mutex<Vec<u64>>>,
}

fn test_bytes(len: usize) -> Vec<u8> {
    let mut x = 0x2545F491u32;
    (0..len)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 17;
            x ^= x << 5;
            x as u8
        })
        .collect()
}

async fn serve(body: Vec<u8>, behavior: Behavior) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let ranges = Arc::new(Mutex::new(Vec::new()));
    let (body, behavior, seen) = (Arc::new(body), Arc::new(behavior), ranges.clone());
    tokio::spawn(async move {
        while let Ok((sock, _)) = listener.accept().await {
            tokio::spawn(handle(sock, body.clone(), behavior.clone(), seen.clone()));
        }
    });
    Server { base, ranges }
}

async fn handle(
    mut sock: TcpStream,
    body: Arc<Vec<u8>>,
    behavior: Arc<Behavior>,
    seen: Arc<Mutex<Vec<u64>>>,
) {
    let mut head = Vec::new();
    let mut byte = [0u8; 1];
    while !head.ends_with(b"\r\n\r\n") {
        if sock.read(&mut byte).await.unwrap_or(0) == 0 {
            return;
        }
        head.push(byte[0]);
    }
    let head = String::from_utf8_lossy(&head).to_lowercase();
    let path = head.split_whitespace().nth(1).unwrap_or("/").to_string();
    if path.starts_with("/missing") {
        let _ = sock.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n").await;
        return;
    }
    if path.starts_with("/index") {
        let page = r#"<a href="/a.bin">a</a> <a href="/about">x</a>"#;
        let resp = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\n\r\n{page}",
            page.len()
        );
        let _ = sock.write_all(resp.as_bytes()).await;
        return;
    }
    let range = head.lines().find_map(|l| l.strip_prefix("range: bytes=")).map(str::to_string);
    let range = range.filter(|_| !behavior.no_ranges).filter(|_| {
        !(head.contains("if-range:") && behavior.changed.swap(false, Ordering::SeqCst))
    });
    let total = body.len();
    let (start, end, status) = match range {
        Some(r) => {
            let (a, b) = r.trim().split_once('-').unwrap();
            let start: usize = a.parse().unwrap();
            let end = b.parse().unwrap_or(total - 1).min(total - 1);
            seen.lock().unwrap().push(start as u64);
            (start, end, "206 Partial Content")
        }
        None => (0, total - 1, "200 OK"),
    };
    let accept = format!(
        "ETag: \"v1\"\r\nContent-Range: bytes {start}-{end}/{total}\r\nContent-Length: {}\r\n\r\n",
        end + 1 - start
    );
    let headers = if status.starts_with("206") {
        accept
    } else {
        format!("ETag: \"v1\"\r\nContent-Length: {total}\r\n\r\n")
    };
    if sock.write_all(format!("HTTP/1.1 {status}\r\n{headers}").as_bytes()).await.is_err() {
        return;
    }
    let mut slice = &body[start..=end];
    if slice.len() > 1 && behavior.cut_once.swap(false, Ordering::SeqCst) {
        slice = &slice[..slice.len() / 2];
        let _ = sock.write_all(slice).await;
        return;
    }
    for chunk in slice.chunks(256 * 1024) {
        if sock.write_all(chunk).await.is_err() {
            return;
        }
        if behavior.delay_ms > 0 {
            sleep(Duration::from_millis(behavior.delay_ms)).await;
        }
    }
}

struct Fixture {
    engine: Engine,
    dir: PathBuf,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn fixture() -> Fixture {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let dir = std::env::temp_dir().join(format!(
        "pulsar-direct-test-{}-{}",
        std::process::id(),
        NEXT.fetch_add(1, Ordering::SeqCst)
    ));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.join("queue.json"), dir.join("dl"));
    // Nothing listens here: tests must not reach the real service.
    engine.accounts().point_at(Provider::RealDebrid, "http://127.0.0.1:9".into(), Duration::from_millis(5));
    tokio::spawn(engine.clone().run_loop());
    Fixture { engine, dir }
}

async fn add(engine: &Engine, url: &str) -> Probe {
    let probes = engine.grab(url, false).await;
    assert_eq!(probes.len(), 1);
    engine.add(probes.clone());
    probes.into_iter().next().unwrap()
}

async fn wait_for(engine: &Engine, want: impl Fn(&Item) -> bool) -> Item {
    for _ in 0..1500 {
        if let Some(item) = engine.list().into_iter().next() {
            if want(&item) {
                return item;
            }
        }
        sleep(Duration::from_millis(20)).await;
    }
    panic!("timed out; items: {:?}", engine.list().iter().map(|i| i.error.clone()).collect::<Vec<_>>());
}

async fn wait_done(engine: &Engine) -> Item {
    wait_for(engine, |i| matches!(i.status, Status::Done | Status::Failed)).await
}

const SIZE: usize = 16 * 1024 * 1024 + 123;

#[tokio::test(flavor = "multi_thread")]
async fn segmented_download_is_byte_exact() {
    let body = test_bytes(SIZE);
    let server = serve(body.clone(), Behavior::default()).await;
    let fx = fixture();
    add(&fx.engine, &format!("{}/film.mkv", server.base)).await;
    let item = wait_done(&fx.engine).await;
    assert!(item.status == Status::Done, "{:?}", item.error);
    assert_eq!(item.segments.len(), 4);
    assert_eq!(std::fs::read(item.output.unwrap()).unwrap(), body);
    assert!(!item.dir.join(format!(".{}.qpart", item.id)).exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn server_without_ranges_uses_one_stream() {
    let body = test_bytes(SIZE);
    let server = serve(body.clone(), Behavior { no_ranges: true, ..Default::default() }).await;
    let fx = fixture();
    let probe = add(&fx.engine, &format!("{}/a.bin", server.base)).await;
    assert!(!probe.resumable);
    let item = wait_done(&fx.engine).await;
    assert_eq!(item.segments.len(), 1);
    assert_eq!(std::fs::read(item.output.unwrap()).unwrap(), body);
}

#[tokio::test(flavor = "multi_thread")]
async fn pause_then_resume_continues_where_it_stopped() {
    let body = test_bytes(SIZE);
    let server = serve(body.clone(), Behavior { delay_ms: 15, ..Default::default() }).await;
    let fx = fixture();
    add(&fx.engine, &format!("{}/a.bin", server.base)).await;
    let item = wait_for(&fx.engine, |i| i.segments.iter().map(|s| s.done).sum::<u64>() > 2 << 20).await;
    fx.engine.pause(item.id);
    sleep(Duration::from_millis(200)).await;
    let paused = fx.engine.list().remove(0);
    assert!(paused.status == Status::Paused);
    assert!(paused.done > 0 && paused.done < SIZE as u64);

    let plan: HashSet<u64> = paused.segments.iter().map(|s| s.start).collect();
    server.ranges.lock().unwrap().clear();
    fx.engine.resume(item.id);
    let item = wait_done(&fx.engine).await;
    assert_eq!(std::fs::read(item.output.unwrap()).unwrap(), body);
    assert!(
        server.ranges.lock().unwrap().iter().any(|p| !plan.contains(p)),
        "the resume should request ranges from mid-segment"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_survives_a_restart() {
    let body = test_bytes(SIZE);
    let server = serve(body.clone(), Behavior { delay_ms: 15, ..Default::default() }).await;
    let fx = fixture();
    add(&fx.engine, &format!("{}/a.bin", server.base)).await;
    let item = wait_for(&fx.engine, |i| i.segments.iter().map(|s| s.done).sum::<u64>() > 2 << 20).await;
    fx.engine.pause(item.id);
    sleep(Duration::from_millis(200)).await;

    let reborn = Engine::new(fx.dir.join("queue.json"), fx.dir.join("dl"));
    let loaded = reborn.list().remove(0);
    assert!(loaded.status == Status::Paused && loaded.done > 0);
    tokio::spawn(reborn.clone().run_loop());
    reborn.resume(item.id);
    let item = wait_done(&reborn).await;
    assert_eq!(std::fs::read(item.output.unwrap()).unwrap(), body);
}

#[tokio::test(flavor = "multi_thread")]
async fn dropped_connection_is_retried() {
    let body = test_bytes(SIZE);
    let cutting = serve(body.clone(), Behavior { cut_once: AtomicBool::new(true), ..Default::default() }).await;
    let fx = fixture();
    // The 1-byte probe is exempt (slice.len() > 1), so the transfer takes the cut.
    add(&fx.engine, &format!("{}/b.bin", cutting.base)).await;
    let item = wait_done(&fx.engine).await;
    assert!(item.status == Status::Done, "{:?}", item.error);
    assert_eq!(std::fs::read(item.output.unwrap()).unwrap(), body);
}

#[tokio::test(flavor = "multi_thread")]
async fn file_changed_on_server_restarts_from_zero() {
    let body = test_bytes(SIZE);
    // The first conditional range answers 200, as for a replaced file.
    let changing = serve(
        body.clone(),
        Behavior { delay_ms: 15, changed: AtomicBool::new(true), ..Default::default() },
    )
    .await;
    let fx = fixture();
    add(&fx.engine, &format!("{}/b.bin", changing.base)).await;
    let item = wait_done(&fx.engine).await;
    assert!(item.status == Status::Done, "{:?}", item.error);
    assert_eq!(std::fs::read(item.output.unwrap()).unwrap(), body);
}

#[tokio::test(flavor = "multi_thread")]
async fn missing_file_is_reported_offline() {
    let server = serve(test_bytes(16), Behavior::default()).await;
    let fx = fixture();
    let probes = fx.engine.grab(&format!("{}/missing.zip", server.base), false).await;
    assert!(!probes[0].online);
    assert_eq!(probes[0].error.as_deref(), Some("HTTP 404"));
    fx.engine.add(probes);
    let item = wait_done(&fx.engine).await;
    assert!(item.status == Status::Failed);
    assert_eq!(item.error.as_deref(), Some("HTTP 404"));
}

#[tokio::test(flavor = "multi_thread")]
async fn html_page_expands_to_its_files() {
    let server = serve(test_bytes(1024), Behavior::default()).await;
    let fx = fixture();
    let probes = fx.engine.grab(&format!("{}/index.html", server.base), false).await;
    assert_eq!(probes.len(), 1);
    assert_eq!(probes[0].name, "a.bin");
    assert_eq!(probes[0].size, Some(1024));
}

#[tokio::test(flavor = "multi_thread")]
async fn speed_limit_is_respected() {
    let body = test_bytes(1024 * 1024);
    let server = serve(body.clone(), Behavior::default()).await;
    let fx = fixture();
    let mut settings = fx.engine.settings();
    settings.speed_limit_kib = 512;
    fx.engine.set_settings(settings);
    let started = std::time::Instant::now();
    add(&fx.engine, &format!("{}/a.bin", server.base)).await;
    let item = wait_done(&fx.engine).await;
    assert!(started.elapsed() >= Duration::from_millis(1500), "{:?}", started.elapsed());
    assert_eq!(std::fs::read(item.output.unwrap()).unwrap(), body);
}

#[tokio::test(flavor = "multi_thread")]
async fn archives_are_extracted() {
    let work = std::env::temp_dir().join(format!("pulsar-direct-arch-{}", std::process::id()));
    std::fs::create_dir_all(&work).unwrap();
    std::fs::write(work.join("hello.txt"), "bonjour").unwrap();
    let archive = work.join("pack.tar.gz");
    let ok = std::process::Command::new("/usr/bin/tar")
        .args(["-czf", archive.to_str().unwrap(), "-C", work.to_str().unwrap(), "hello.txt"])
        .status()
        .unwrap();
    assert!(ok.success());
    let body = std::fs::read(&archive).unwrap();
    std::fs::remove_dir_all(&work).unwrap();

    let server = serve(body, Behavior::default()).await;
    let fx = fixture();
    add(&fx.engine, &format!("{}/pack.tar.gz", server.base)).await;
    let item = wait_done(&fx.engine).await;
    assert!(item.error.is_none(), "{:?}", item.error);
    let out = item.output.unwrap();
    assert_eq!(out.file_name().unwrap(), "pack");
    assert_eq!(std::fs::read_to_string(out.join("hello.txt")).unwrap(), "bonjour");
}

#[tokio::test(flavor = "multi_thread")]
async fn corrupt_archive_keeps_the_file_and_reports() {
    let server = serve(test_bytes(4096), Behavior::default()).await;
    let fx = fixture();
    add(&fx.engine, &format!("{}/bad.zip", server.base)).await;
    let item = wait_done(&fx.engine).await;
    assert!(item.status == Status::Done);
    assert!(item.error.unwrap().starts_with("extraction failed:"));
    assert!(item.output.unwrap().exists());
}

#[tokio::test(flavor = "multi_thread")]
async fn same_name_never_overwrites() {
    let body = test_bytes(2048);
    let server = serve(body.clone(), Behavior::default()).await;
    let fx = fixture();
    let dir = fx.engine.settings().download_dir.join("a");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("a.bin"), "existing").unwrap();
    add(&fx.engine, &format!("{}/a.bin", server.base)).await;
    let item = wait_done(&fx.engine).await;
    assert_eq!(item.output.unwrap().file_name().unwrap(), "a (1).bin");
    assert_eq!(std::fs::read_to_string(dir.join("a.bin")).unwrap(), "existing");
}

#[tokio::test(flavor = "multi_thread")]
async fn clipboard_grab_ignores_plain_web_pages() {
    let fx = fixture();
    let article = serve_page().await;
    assert!(fx.engine.grab(&article, true).await.is_empty());
    let probes = fx.engine.grab(&article, false).await;
    assert_eq!(probes.len(), 1);
    assert!(!probes[0].online && probes[0].page, "a page is never offered as a file");
}

#[tokio::test(flavor = "multi_thread")]
async fn web_page_answering_a_queued_link_fails_instead_of_saving_html() {
    let article = serve_page().await;
    let fx = fixture();
    fx.engine.add(vec![Probe { url: article, name: "movie.mkv".into(), package: "movie".into(), online: true, ..Default::default() }]);
    let item = wait_done(&fx.engine).await;
    assert!(item.status == Status::Failed);
    assert_eq!(item.error.as_deref(), Some("The server sent a web page, not a file"));
    assert!(item.output.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn protected_links_are_kept_for_the_browser_even_from_the_clipboard() {
    let fx = fixture();
    let probes = fx.engine.grab("https://www.keeplinks.org/p16/6ae2926a663d8", true).await;
    assert_eq!(probes.len(), 1);
    assert!(!probes[0].online && probes[0].protected);
}

#[tokio::test(flavor = "multi_thread")]
async fn hoster_page_without_an_account_is_flagged_not_offered() {
    let article = serve_page().await;
    let (api, _) = mock_api(&[("/hosts/domains", 200, r#"["127.0.0.1"]"#)]).await;
    let fx = fixture();
    fx.engine.accounts().point_at(Provider::RealDebrid, api, Duration::from_millis(5));
    for files_only in [true, false] {
        let probes = fx.engine.grab(&article, files_only).await;
        assert_eq!(probes.len(), 1, "kept so that the user learns why it can't download");
        assert!(!probes[0].online);
        assert_eq!(probes[0].error.as_deref(), Some("No connected account supports this host"));
    }
}

async fn serve_page() -> String {
    let (base, mock) = mock_api(&[]).await;
    let mut routes = mock.routes.lock().unwrap();
    routes.insert("/article".into(), (200, "<p>news</p>".into()));
    drop(routes);
    format!("{base}/article")
}

#[tokio::test(flavor = "multi_thread")]
async fn hoster_link_downloads_through_the_debrid_service() {
    let body = test_bytes(SIZE);
    let files = serve(body.clone(), Behavior { delay_ms: 15, ..Default::default() }).await;
    let direct = format!(r#"{{"filename":"film.mkv","filesize":{SIZE},"chunks":2,"download":"{}/dl.bin"}}"#, files.base);
    let (api, mock) = mock_api(&[
        ("/hosts/domains", 200, r#"["hoster.test"]"#),
        ("/unrestrict/link", 200, &direct),
    ])
    .await;
    let fx = fixture();
    let accounts = fx.engine.accounts();
    accounts.set_key(Provider::RealDebrid, Some("secret".into()));
    accounts.point_at(Provider::RealDebrid, api, Duration::from_millis(5));

    let link = "https://hoster.test/file/abc";
    let probe = add(&fx.engine, link).await;
    assert_eq!(probe.name, "film.mkv");
    assert_eq!(probe.url, link);
    assert_eq!(probe.provider, Some(Provider::RealDebrid));

    let item = wait_for(&fx.engine, |i| i.segments.iter().map(|s| s.done).sum::<u64>() > 2 << 20).await;
    assert_eq!(item.segments.len(), 2, "the service's chunk limit applies");
    fx.engine.pause(item.id);
    sleep(Duration::from_millis(200)).await;
    let done_before = fx.engine.list().remove(0).done;
    fx.engine.resume(item.id);
    let item = wait_done(&fx.engine).await;
    assert!(item.status == Status::Done, "{:?}", item.error);
    assert_eq!(item.url, link);
    assert_eq!(std::fs::read(item.output.unwrap()).unwrap(), body);
    assert!(done_before > 0);
    let unlocks = mock.requests.lock().unwrap().iter().filter(|r| r.0 == "/unrestrict/link").count();
    assert_eq!(unlocks, 2, "the start reuses the analysis's link; only the resume needs a new one");
    assert!(!std::fs::read_to_string(fx.dir.join("queue.json")).unwrap().contains("secret"));
}

/// Needs internet: `cargo test -- --ignored real_server`.
#[tokio::test(flavor = "multi_thread")]
#[ignore]
async fn real_server_download_matches_a_plain_get() {
    let url = "https://proof.ovh.net/files/10Mb.dat";
    let expected = reqwest::get(url).await.unwrap().bytes().await.unwrap();
    let fx = fixture();
    let probe = add(&fx.engine, url).await;
    assert!(probe.resumable);
    let item = wait_done(&fx.engine).await;
    assert!(item.status == Status::Done, "{:?}", item.error);
    assert_eq!(item.segments.len(), 2);
    assert!(std::fs::read(item.output.unwrap()).unwrap() == expected);
}

#[tokio::test(flavor = "multi_thread")]
async fn network_failure_waits_instead_of_failing() {
    let fx = fixture();
    fx.engine.add(vec![Probe { url: "http://127.0.0.1:9/file.bin".into(), name: "file.bin".into(), package: "file".into(), online: true, ..Default::default() }]);
    let item = wait_for(&fx.engine, |i| i.status == Status::Waiting).await;
    assert!(item.error.unwrap().starts_with("connection:"));
    fx.engine.pause(item.id);
    assert!(fx.engine.list()[0].status == Status::Paused);
}

#[tokio::test(flavor = "multi_thread")]
async fn queue_doesnt_run_through_during_an_outage() {
    let fx = fixture();
    let dead = |n: u32| Probe { url: format!("http://127.0.0.1:9/{n}.bin"), name: format!("{n}.bin"), package: "p".into(), online: true, ..Default::default() };
    let mut settings = fx.engine.settings();
    settings.max_active = 1;
    fx.engine.set_settings(settings);
    fx.engine.add(vec![dead(1), dead(2), dead(3)]);
    wait_for(&fx.engine, |i| i.status == Status::Waiting).await;
    sleep(Duration::from_millis(1500)).await;
    let items = fx.engine.list();
    assert!(items[1].status == Status::Queued && items[2].status == Status::Queued);
}

#[tokio::test(flavor = "multi_thread")]
async fn quitting_during_extraction_doesnt_download_again() {
    let server = serve(test_bytes(4096), Behavior::default()).await;
    let fx = fixture();
    add(&fx.engine, &format!("{}/a.bin", server.base)).await;
    let item = wait_done(&fx.engine).await;
    sleep(Duration::from_millis(100)).await;
    let path = fx.dir.join("queue.json");
    let saved = std::fs::read_to_string(&path).unwrap().replace("\"status\":\"done\"", "\"status\":\"extracting\"");
    std::fs::write(&path, saved).unwrap();
    let reborn = Engine::new(path, fx.dir.join("dl"));
    let loaded = reborn.list().remove(0);
    assert!(loaded.status == Status::Done);
    assert_eq!(loaded.error.as_deref(), Some("extraction interrupted"));
    assert!(item.output.unwrap().exists());
}

#[test]
fn simultaneous_saves_never_leave_a_broken_file() {
    let dir = std::env::temp_dir().join(format!("pulsar-saves-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("queue.json");
    let engine = Engine::new(path.clone(), dir.clone());
    let probe = |n: usize| Probe { url: format!("http://x/{n}"), name: format!("{n}"), package: "p".into(), online: true, ..Default::default() };
    let mut writers = Vec::new();
    for t in 0..8 {
        let engine = engine.clone();
        writers.push(std::thread::spawn(move || {
            for n in 0..40 {
                engine.add(vec![probe(t * 100 + n)]);
                engine.save();
            }
        }));
    }
    for _ in 0..300 {
        if let Ok(bytes) = std::fs::read(&path) {
            serde_json::from_slice::<serde_json::Value>(&bytes).expect("a complete file at every moment");
        }
    }
    for writer in writers {
        writer.join().unwrap();
    }
    engine.save();
    let saved: serde_json::Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(saved["items"].as_array().unwrap().len(), 320, "the last save wins");
    assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temporary file left behind");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn stray_bytes_after_a_complete_queue_lose_nothing() {
    let dir = std::env::temp_dir().join(format!("pulsar-stray-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("queue.json");
    let engine = Engine::new(path.clone(), dir.clone());
    engine.add(vec![Probe { url: "http://x/a".into(), name: "a".into(), package: "p".into(), online: true, ..Default::default() }]);
    let mut bytes = std::fs::read(&path).unwrap();
    bytes.extend_from_slice(b"16,\"done\":0,\"speed\":0}]}");
    std::fs::write(&path, bytes).unwrap();
    assert_eq!(Engine::new(path, dir.clone()).list().len(), 1);
    assert!(dir.join("queue.corrupt").exists(), "the damaged file stays available");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn corrupt_queue_is_set_aside_not_overwritten() {
    let dir = std::env::temp_dir().join(format!("pulsar-corrupt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("queue.json");
    std::fs::write(&path, "{ not json").unwrap();
    let engine = Engine::new(path.clone(), dir.clone());
    assert!(engine.list().is_empty());
    assert_eq!(std::fs::read_to_string(dir.join("queue.corrupt")).unwrap(), "{ not json");
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn copying_a_link_visits_only_likely_downloads() {
    let (base, mock) = mock_api(&[("/unsubscribe", 200, "<p>done</p>"), ("/report.pdf", 200, "<p>x</p>")]).await;
    let fx = fixture();
    let text = format!("{base}/unsubscribe {base}/report.pdf");
    fx.engine.grab(&text, true).await;
    let requests = mock.requests.lock().unwrap();
    assert!(requests.iter().any(|r| r.0 == "/report.pdf"));
    assert!(!requests.iter().any(|r| r.0 == "/unsubscribe"), "a copied link must not be visited");
}

#[test]
fn unchecked_links_are_listed_without_contact() {
    let engine = Engine::new(std::env::temp_dir().join("pulsar-unchecked.json"), PathBuf::from("/tmp"));
    let probes = engine.list_unchecked("https://rapidgator.net/file/abc/film.rar");
    assert!(probes[0].unchecked && probes[0].provider.is_none());
    assert_eq!(probes[0].name, "film.rar");
}

/// Touches the user's real Trash: `cargo test -- --ignored deleting`.
#[test]
#[ignore]
fn deleting_moves_a_file_to_the_trash() {
    let dir = std::env::temp_dir().join(format!("pulsar-trash-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("pulsar-trash-test.txt");
    std::fs::write(&file, "x").unwrap();
    to_trash(std::slice::from_ref(&file)).unwrap();
    assert!(!file.exists());
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn dead_service_link_is_replaced_once() {
    use crate::direct::providers::Provider;
    let body = test_bytes(4096);
    let files = serve(body.clone(), Behavior::default()).await;
    let fresh = format!(r#"{{"filename":"f.bin","filesize":4096,"download":"{}/dl.bin"}}"#, files.base);
    let (api, mock) = mock_api(&[("/unrestrict/link", 200, &fresh)]).await;
    let fx = fixture();
    let accounts = fx.engine.accounts();
    accounts.set_key(Provider::RealDebrid, Some("secret".into()));
    accounts.point_at(Provider::RealDebrid, api, Duration::from_millis(5));
    fx.engine.add(vec![Probe {
        url: "https://hoster.test/file/abc".into(),
        name: "f.bin".into(),
        package: "f".into(),
        online: true,
        provider: Some(Provider::RealDebrid),
        direct: Some(format!("{}/missing.bin", files.base)),
        ..Default::default()
    }]);
    let item = wait_done(&fx.engine).await;
    assert!(item.status == Status::Done, "{:?}", item.error);
    assert_eq!(std::fs::read(item.output.unwrap()).unwrap(), body);
    assert_eq!(mock.requests.lock().unwrap().iter().filter(|r| r.0 == "/unrestrict/link").count(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn start_and_pause_all_touch_only_what_they_should() {
    let server = serve(test_bytes(2048), Behavior::default()).await;
    let fx = fixture();
    add(&fx.engine, &format!("{}/a.bin", server.base)).await;
    wait_done(&fx.engine).await;
    fx.engine.pause_all();
    fx.engine.resume_all();
    assert!(fx.engine.list()[0].status == Status::Done);
}

#[tokio::test(flavor = "multi_thread")]
async fn rar_archives_are_extracted() {
    let server = serve(include_bytes!("testdata/solid.rar").to_vec(), Behavior::default()).await;
    let fx = fixture();
    add(&fx.engine, &format!("{}/solid.rar", server.base)).await;
    let item = wait_for(&fx.engine, |i| i.status == Status::Done && i.output.as_ref().is_some_and(|o| o.is_dir())).await;
    assert!(item.error.is_none(), "{:?}", item.error);
    assert!(std::fs::read_dir(item.output.unwrap()).unwrap().count() > 0);
}

#[tokio::test(flavor = "multi_thread")]
async fn volumes_wait_for_each_other_then_report_a_missing_one() {
    let body = include_bytes!("testdata/archive.part1.rar").to_vec();
    let server = serve(body, Behavior::default()).await;
    let fx = fixture();
    let one = fx.engine.grab(&format!("{}/set.part1.rar", server.base), false).await;
    fx.engine.add(one);
    let item = wait_for(&fx.engine, |i| i.status == Status::Done && i.error.is_some()).await;
    assert!(item.error.unwrap().starts_with("extraction failed:"), "only one volume is here");
    assert!(item.output.as_ref().unwrap().is_file(), "the volume stays for a retry");
}

#[tokio::test(flavor = "multi_thread")]
async fn second_volume_alone_extracts_nothing() {
    let server = serve(test_bytes(2048), Behavior::default()).await;
    let fx = fixture();
    let two = fx.engine.grab(&format!("{}/set.part2.rar", server.base), false).await;
    fx.engine.add(two);
    let item = wait_done(&fx.engine).await;
    assert!(item.error.is_none() && item.output.unwrap().is_file());
}

#[tokio::test(flavor = "multi_thread")]
async fn extraction_can_be_retried() {
    let server = serve(test_bytes(4096), Behavior::default()).await;
    let fx = fixture();
    add(&fx.engine, &format!("{}/bad.zip", server.base)).await;
    let item = wait_for(&fx.engine, |i| i.error.is_some()).await;
    fx.engine.retry_extraction(item.id).await;
    let again = fx.engine.list().remove(0);
    assert!(again.status == Status::Done && again.error.unwrap().starts_with("extraction failed:"));
}

#[tokio::test]
async fn rar_size_is_known_before_extraction() {
    let dir = std::env::temp_dir().join(format!("pulsar-size-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let archive = dir.join("solid.rar");
    std::fs::write(&archive, include_bytes!("testdata/solid.rar")).unwrap();
    let total = unpacked_size(&archive).await.unwrap();
    assert!(total > 0);
    let target = dir.join("out");
    extract_archive(&archive, &target).await.unwrap();
    assert_eq!(folder_size(&target), total, "the measured folder reaches the announced size");
    assert_eq!(unpacked_size(&dir.join("a.zip")).await, None);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn add_options_apply_to_the_batch() {
    let dir = std::env::temp_dir().join(format!("pulsar-opts-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.join("q.json"), dir.clone());
    let probe = Probe { url: "http://x/a.zip".into(), name: "a.zip".into(), package: "a".into(), online: true, ..Default::default() };
    engine.add_with(vec![probe], AddOptions { save_to: Some(dir.join("other")), paused: true, skip_extraction: true });
    let item = engine.list().remove(0);
    assert!(item.status == Status::Paused && item.skip_extraction);
    assert_eq!(item.dir, dir.join("other").join("a"));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn connection_setting_limits_the_segments() {
    let server = serve(test_bytes(SIZE), Behavior::default()).await;
    let fx = fixture();
    let mut settings = fx.engine.settings();
    settings.chunks = 2;
    fx.engine.set_settings(settings);
    let item = { add(&fx.engine, &format!("{}/a.bin", server.base)).await; wait_done(&fx.engine).await };
    assert_eq!(item.segments.len(), 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn skipped_extraction_leaves_the_archive() {
    let server = serve(test_bytes(4096), Behavior::default()).await;
    let fx = fixture();
    let probes = fx.engine.grab(&format!("{}/bad.zip", server.base), false).await;
    fx.engine.add_with(probes, AddOptions { skip_extraction: true, ..Default::default() });
    let item = wait_done(&fx.engine).await;
    assert!(item.error.is_none(), "an unread archive can't fail");
}

#[test]
fn queue_order_can_change() {
    let dir = std::env::temp_dir().join(format!("pulsar-order-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let engine = Engine::new(dir.join("q.json"), dir.clone());
    let probe = |n: &str| Probe { url: format!("http://x/{n}"), name: n.into(), package: "p".into(), online: true, ..Default::default() };
    engine.add(vec![probe("a"), probe("b"), probe("c")]);
    let ids: Vec<u64> = engine.list().iter().map(|i| i.id).collect();
    engine.move_items(&[ids[2]], true);
    assert_eq!(engine.list().iter().map(|i| i.name.clone()).collect::<Vec<_>>(), ["c", "a", "b"]);
    engine.move_items(&[ids[2]], false);
    assert_eq!(engine.list().iter().map(|i| i.name.clone()).collect::<Vec<_>>(), ["a", "b", "c"]);
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn free_space_reads_the_disk() {
    assert!(free_space(&std::env::temp_dir()).unwrap() > 0);
}

#[test]
fn settings_saved_before_a_new_option_get_its_default() {
    let settings: Settings = serde_json::from_str(r#"{"chunks":2}"#).unwrap();
    assert_eq!(settings.chunks, 2);
    assert!(settings.auto_start_clipboard);
}

#[test]
fn segments_cover_the_whole_file() {
    let size = 10 * MIN_SEGMENT + 7;
    let plan = plan_segments(Some(size), true, 4);
    assert_eq!(plan.len(), 4);
    assert_eq!(plan[0].start, 0);
    assert_eq!(plan[3].end, Some(size - 1));
    for pair in plan.windows(2) {
        assert_eq!(pair[0].end.unwrap() + 1, pair[1].start);
    }
    assert_eq!(plan_segments(Some(1024), true, 8).len(), 1);
    assert_eq!(plan_segments(None, false, 8)[0].end, None);
}
