//! Click'n'Load: the local HTTP endpoint link protectors post their links to,
//! on the port JDownloader made the standard. It only listens on loopback.

use super::links;
use aes::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use base64::Engine as _;
use percent_encoding::percent_decode_str;
use regex::Regex;
use std::collections::HashMap;
use std::sync::LazyLock;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

pub const ADDRESS: &str = "127.0.0.1:9666";
const MAX_REQUEST: usize = 2 * 1024 * 1024;

static KEY_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[0-9a-fA-F]{32}").unwrap());

/// Links received from a page, and whether that page is a known link protector.
pub struct Received {
    pub source: String,
    pub links: Vec<String>,
    pub trusted: bool,
}

fn form(body: &[u8]) -> HashMap<String, String> {
    let decode = |s: &str| percent_decode_str(&s.replace('+', " ")).decode_utf8_lossy().to_string();
    String::from_utf8_lossy(body)
        .split('&')
        .filter_map(|pair| pair.split_once('='))
        .map(|(k, v)| (decode(k), decode(v)))
        .collect()
}

/// `jk` is JavaScript returning the key as hex. It is never run: pages always
/// write the key out as a literal, so reading that is enough.
fn decrypt(jk: &str, crypted: &str) -> Option<String> {
    let hex = KEY_RE.find(jk)?.as_str();
    let mut key = [0u8; 16];
    for (i, byte) in key.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&hex[i * 2..i * 2 + 2], 16).ok()?;
    }
    // Form decoding turns the `+` of base64 into spaces.
    let clean: String = crypted.split_whitespace().collect::<Vec<_>>().join("+");
    let mut data = base64::engine::general_purpose::STANDARD.decode(clean).ok()?;
    let plain = cbc::Decryptor::<aes::Aes128>::new(&key.into(), &key.into())
        .decrypt_padded_mut::<NoPadding>(&mut data)
        .ok()?;
    Some(String::from_utf8_lossy(plain).trim_end_matches('\0').to_string())
}

pub fn links_from(path: &str, body: &[u8]) -> Option<(String, Vec<String>)> {
    let fields = form(body);
    let text = match path {
        "/flash/addcrypted2" => decrypt(fields.get("jk")?, fields.get("crypted")?)?,
        "/flash/add" => fields.get("urls")?.clone(),
        _ => return None,
    };
    let source = fields.get("source").cloned().unwrap_or_default();
    Some((source, links::extract_urls(&text)))
}

fn header<'a>(head: &'a str, name: &str) -> Option<&'a str> {
    head.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.eq_ignore_ascii_case(name).then(|| value.trim())
    })
}

async fn handle(mut sock: TcpStream, on_links: impl Fn(Received)) -> Option<()> {
    let mut raw = Vec::new();
    let mut buf = [0u8; 8192];
    let (head, length) = loop {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 || raw.len() > MAX_REQUEST {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            let head = String::from_utf8_lossy(&raw[..end]).to_string();
            let length: usize = header(&head, "content-length").and_then(|v| v.parse().ok()).unwrap_or(0);
            raw.drain(..end + 4);
            break (head, length);
        }
    };
    while raw.len() < length.min(MAX_REQUEST) {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            return None;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let mut parts = head.split_whitespace();
    let (method, path) = (parts.next()?, parts.next()?.split('?').next()?);
    let body = match (method, path) {
        ("POST", "/flash/add" | "/flash/addcrypted2") => match links_from(path, &raw) {
            Some((source, found)) => {
                // A browser sets Origin itself, so a page can't pose as a protector.
                let trusted = header(&head, "origin").is_some_and(links::is_link_protector);
                on_links(Received { source, links: found, trusted });
                "success\r\n"
            }
            None => "failed\r\n",
        },
        ("GET", "/jdcheck.js") => "jdownloader=true;\r\n",
        ("GET", "/flash") => "JDownloader\r\n",
        _ => "",
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nAccess-Control-Allow-Origin: *\r\nAccess-Control-Allow-Private-Network: true\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{body}",
        body.len()
    );
    sock.write_all(response.as_bytes()).await.ok()
}

pub async fn serve(listener: TcpListener, on_links: impl Fn(Received) + Clone + Send + 'static) {
    while let Ok((sock, _)) = listener.accept().await {
        let on_links = on_links.clone();
        tokio::spawn(async move { handle(sock, on_links).await });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    const JK: &str = "function f(){ return '31323334353637383930313233343536';}";
    const CRYPTED: &str = "DYefff0tlDQlnl6NN999LR/o4p3+OHYviXKRl9KYqBUyO7MrKQsrlR3QFCX6eWcWG4c50+LgUmQMOA0Y1/hlcw==";

    #[test]
    fn decrypts_links_from_a_page() {
        let text = decrypt(JK, CRYPTED).unwrap();
        assert_eq!(links::extract_urls(&text), ["https://rapidgator.net/file/abc", "https://ddownload.com/xyz/f.rar"]);
        assert_eq!(decrypt(JK, &CRYPTED.replace('+', " ")).as_deref(), Some(text.as_str()));
        assert!(decrypt("function f(){}", CRYPTED).is_none());
    }

    #[tokio::test]
    async fn accepts_a_post_and_trusts_only_protectors() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        tokio::spawn(serve(listener, move |r: Received| log.lock().unwrap().push((r.source, r.links, r.trusted))));
        let client = reqwest::Client::new();

        let body = |crypted: &str| {
            let enc = |s: &str| percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC).to_string();
            format!("source=https%3A%2F%2Ffilecrypt.cc%2Fx&jk={}&crypted={}", enc(JK), enc(crypted))
        };
        for (origin, trusted) in [("https://filecrypt.cc", true), ("https://evil.example", false)] {
            let reply = client
                .post(format!("{base}/flash/addcrypted2"))
                .header("Origin", origin)
                .body(body(CRYPTED))
                .send()
                .await
                .unwrap();
            assert_eq!(reply.text().await.unwrap(), "success\r\n");
            let seen = seen.lock().unwrap();
            let last = seen.last().unwrap();
            assert_eq!(last.1.len(), 2);
            assert_eq!(last.0, "https://filecrypt.cc/x");
            assert_eq!(last.2, trusted);
        }
        let check = client.get(format!("{base}/jdcheck.js")).send().await.unwrap();
        assert_eq!(check.text().await.unwrap(), "jdownloader=true;\r\n");
    }
}
