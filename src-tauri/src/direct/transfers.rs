//! File-transfer services whose share links lead to files through a public
//! API rather than a page: MEGA (and transfer.it, which runs on it) and
//! WeTransfer. Their file addresses are temporary, so each download asks for a
//! fresh one; MEGA's files are encrypted, and decrypted as they arrive.

use aes::cipher::{BlockDecrypt, BlockDecryptMut, KeyInit, KeyIvInit, StreamCipher, StreamCipherSeek};
use base64::Engine as _;
use reqwest::Client;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::time::Duration;
use reqwest::Url;

const MEGA_API: &str = "https://g.api.mega.co.nz/cs";

/// A share link to one of these services, as the queue keeps it: enough to ask
/// for the file again after a restart.
#[derive(Clone, Debug, PartialEq)]
pub enum Link {
    /// A public MEGA file: its handle and key, from `mega.nz/file/<h>#<key>`.
    MegaFile { handle: String, key: Vec<u8> },
    /// A public MEGA folder, or one file in it (`…/folder/<h>#<key>/file/<node>`).
    MegaFolder { handle: String, key: Vec<u8>, node: Option<String> },
    /// A transfer.it transfer, or one file in it (`transfer.it/t/<x>#<node>`).
    Transfer { handle: String, node: Option<String> },
    /// A WeTransfer transfer: its id and security hash, or a `we.tl` short link.
    WeTransfer { url: String },
}

/// Where a file can be fetched right now, and how to decrypt it.
pub struct Resolved {
    pub url: String,
    pub cipher: Option<Cipher>,
}

/// One file of a share link, as the link grabber shows it.
pub struct Entry {
    pub url: String,
    pub name: String,
    pub package: String,
    pub size: Option<u64>,
}

fn b64(s: &str) -> Option<Vec<u8>> {
    let clean: String = s.trim_end_matches('=').chars().map(|c| match c {
        '+' => '-',
        '/' => '_',
        c => c,
    }).collect();
    base64::engine::general_purpose::URL_SAFE_NO_PAD.decode(clean).ok()
}

fn host_is(url: &Url, domains: &[&str]) -> bool {
    let host = url.host_str().unwrap_or("").to_ascii_lowercase();
    domains.iter().any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

pub fn parse(url: &str) -> Option<Link> {
    let u = Url::parse(url).ok()?;
    let fragment = u.fragment().unwrap_or("");
    let segments: Vec<&str> = u.path_segments().map(|s| s.filter(|p| !p.is_empty()).collect()).unwrap_or_default();
    if host_is(&u, &["transfer.it"]) {
        return match segments.as_slice() {
            ["t", handle] => Some(Link::Transfer {
                handle: handle.to_string(),
                node: (!fragment.is_empty()).then(|| fragment.to_string()),
            }),
            _ => None,
        };
    }
    if host_is(&u, &["mega.nz", "mega.co.nz", "mega.io"]) {
        // Current links: /file/<h>#<key> and /folder/<h>#<key>[/file/<node>].
        match segments.as_slice() {
            ["file", handle] => {
                let key = b64(fragment).filter(|k| k.len() == 32)?;
                return Some(Link::MegaFile { handle: handle.to_string(), key });
            }
            ["folder", handle] => {
                let mut parts = fragment.split('/');
                let key = b64(parts.next()?).filter(|k| k.len() == 16)?;
                let node = match (parts.next(), parts.next()) {
                    (Some("file"), Some(node)) => Some(node.to_string()),
                    _ => None,
                };
                return Some(Link::MegaFolder { handle: handle.to_string(), key, node });
            }
            _ => {}
        }
        // Older links: #!<h>!<key> and #F!<h>!<key>[!<node>].
        let parts: Vec<&str> = fragment.split('!').collect();
        return match parts.as_slice() {
            ["", handle, key] => Some(Link::MegaFile {
                handle: handle.to_string(),
                key: b64(key).filter(|k| k.len() == 32)?,
            }),
            ["F", handle, key, rest @ ..] => Some(Link::MegaFolder {
                handle: handle.to_string(),
                key: b64(key).filter(|k| k.len() == 16)?,
                node: rest.first().filter(|n| !n.is_empty()).map(|n| n.to_string()),
            }),
            _ => None,
        };
    }
    if host_is(&u, &["we.tl"]) && segments.len() == 1 {
        return Some(Link::WeTransfer { url: url.to_string() });
    }
    if host_is(&u, &["wetransfer.com"]) && segments.first() == Some(&"downloads") && (3..=4).contains(&segments.len()) {
        return Some(Link::WeTransfer { url: url.to_string() });
    }
    None
}

/// Whether a link is one of these services' share links.
pub fn handles(url: &str) -> bool {
    parse(url).is_some()
}

/// A single MEGA file link, which a debrid account could fetch instead.
pub fn is_single_mega_file(url: &str) -> bool {
    matches!(parse(url), Some(Link::MegaFile { .. }))
}

// --- MEGA -------------------------------------------------------------------

/// AES-128-CTR over a whole file: any byte can be decrypted from its offset,
/// so each connection decrypts its own part.
#[derive(Clone)]
pub struct Cipher {
    key: [u8; 16],
    nonce: [u8; 8],
}

impl Cipher {
    /// From a MEGA file key: 32 bytes, the AES key folded with the nonce and MAC.
    fn from_file_key(k: &[u8]) -> Option<Self> {
        if k.len() != 32 {
            return None;
        }
        let mut key = [0u8; 16];
        for i in 0..16 {
            key[i] = k[i] ^ k[i + 16];
        }
        let mut nonce = [0u8; 8];
        nonce.copy_from_slice(&k[16..24]);
        Some(Self { key, nonce })
    }

    /// Decrypts `data`, which starts at `offset` in the file, in place.
    pub fn apply(&self, offset: u64, data: &mut [u8]) {
        let mut iv = [0u8; 16];
        iv[..8].copy_from_slice(&self.nonce);
        let mut ctr = ctr::Ctr128BE::<aes::Aes128>::new(&self.key.into(), &iv.into());
        ctr.seek(offset);
        ctr.apply_keystream(data);
    }
}

fn fold(k: &[u8]) -> [u8; 16] {
    let mut out = [0u8; 16];
    for i in 0..16 {
        out[i] = k[i] ^ k.get(i + 16).copied().unwrap_or(0);
    }
    out
}

/// A node's name, from its attributes: AES-CBC with a zero IV, "MEGA{json}".
fn node_name(attributes: &str, key: &[u8]) -> Option<String> {
    let mut data = b64(attributes)?;
    if data.is_empty() || data.len() % 16 != 0 {
        return None;
    }
    let plain = cbc::Decryptor::<aes::Aes128>::new(&fold(key).into(), &[0u8; 16].into())
        .decrypt_padded_mut::<aes::cipher::block_padding::NoPadding>(&mut data)
        .ok()?;
    let text = std::str::from_utf8(plain).ok()?.trim_end_matches('\0');
    let json: Value = serde_json::from_str(text.strip_prefix("MEGA")?).ok()?;
    json["n"].as_str().map(str::to_string)
}

/// A folder node's key, encrypted with the folder link's key ("owner:key").
/// transfer.it hands its keys over as they are.
fn node_key(k: &str, folder_key: Option<&[u8]>) -> Option<Vec<u8>> {
    let Some(folder_key) = folder_key else { return b64(k) };
    let encrypted = b64(k.split('/').next()?.split(':').nth(1)?)?;
    if encrypted.len() % 16 != 0 || folder_key.len() != 16 {
        return None;
    }
    let aes = aes::Aes128::new(folder_key.into());
    let mut out = encrypted;
    for block in out.chunks_mut(16) {
        aes.decrypt_block(block.into());
    }
    Some(out)
}

fn mega_error(code: i64) -> String {
    match code {
        -2 | -9 | -11 => "The file or folder doesn't exist anymore".into(),
        -16 => "MEGA blocked this file".into(),
        -17 | -4 => "MEGA's free transfer quota is used up: try again later".into(),
        -18 => "MEGA is temporarily unavailable".into(),
        c => format!("MEGA refused the request ({c})"),
    }
}

/// One MEGA API command. "Try again" answers are retried here.
async fn mega(http: &Client, scope: &str, command: Value) -> Result<Value, String> {
    let mut last = String::new();
    for attempt in 0..4u32 {
        if attempt > 0 {
            tokio::time::sleep(Duration::from_millis(500 << attempt)).await;
        }
        let url = format!("{MEGA_API}?id={}&{scope}", rand_id());
        let sent = http.post(&url).json(&json!([command])).send().await;
        let body: Value = match sent {
            Ok(r) => match r.json().await {
                Ok(v) => v,
                Err(e) => { last = e.to_string(); continue; }
            },
            Err(e) => { last = format!("connection: {e}"); continue; }
        };
        let answer = body.get(0).cloned().unwrap_or(body);
        match answer.as_i64() {
            Some(-3) | Some(-6) => { last = mega_error(-18); continue; }
            Some(code) if code < 0 => return Err(mega_error(code)),
            _ => return Ok(answer),
        }
    }
    Err(last)
}

fn rand_id() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(0)
}

struct Node {
    handle: String,
    parent: String,
    folder: bool,
    name: String,
    key: Vec<u8>,
    size: Option<u64>,
}

/// The nodes of a folder or transfer, with their names decrypted.
async fn folder_nodes(http: &Client, scope: &str, folder_key: Option<&[u8]>) -> Result<Vec<Node>, String> {
    let answer = mega(http, scope, json!({"a": "f", "c": 1, "r": 1})).await?;
    let nodes = answer["f"].as_array().ok_or("MEGA sent an unexpected answer")?;
    Ok(nodes
        .iter()
        .filter_map(|n| {
            let key = node_key(n["k"].as_str()?, folder_key)?;
            Some(Node {
                handle: n["h"].as_str()?.to_string(),
                parent: n["p"].as_str().unwrap_or("").to_string(),
                folder: n["t"].as_i64() == Some(1),
                name: node_name(n["a"].as_str()?, &key).unwrap_or_else(|| "file".into()),
                key,
                size: n["s"].as_u64(),
            })
        })
        .collect())
}

fn scope_of(link: &Link) -> Option<(String, Option<&[u8]>)> {
    match link {
        Link::MegaFolder { handle, key, .. } => Some((format!("n={handle}"), Some(key.as_slice()))),
        Link::Transfer { handle, .. } => Some((format!("x={handle}"), None)),
        _ => None,
    }
}

fn file_link(link: &Link, node: &str) -> String {
    match link {
        Link::MegaFolder { handle, key, .. } => {
            let key = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(key);
            format!("https://mega.nz/folder/{handle}#{key}/file/{node}")
        }
        Link::Transfer { handle, .. } => format!("https://transfer.it/t/{handle}#{node}"),
        _ => String::new(),
    }
}

/// The files behind a share link, with their names and sizes. A folder's
/// files keep its name as their package.
pub async fn list(http: &Client, url: &str) -> Result<Vec<Entry>, String> {
    let link = parse(url).ok_or("Not a share link")?;
    match &link {
        Link::MegaFile { handle, key } => {
            let answer = mega(http, "", json!({"a": "g", "p": handle})).await?;
            let name = answer["at"].as_str().and_then(|a| node_name(a, key)).unwrap_or_else(|| "file".into());
            Ok(vec![Entry {
                url: url.to_string(),
                package: crate::direct::links::package_name(&name),
                name,
                size: answer["s"].as_u64(),
            }])
        }
        Link::MegaFolder { node, .. } | Link::Transfer { node, .. } => {
            let (scope, folder_key) = scope_of(&link).unwrap();
            let nodes = folder_nodes(http, &scope, folder_key).await?;
            let by_handle: HashMap<&str, &Node> = nodes.iter().map(|n| (n.handle.as_str(), n)).collect();
            // The package is the shared folder's name: the node without a parent in the share.
            let root = |n: &Node| root_name(&by_handle, n);
            let entries: Vec<Entry> = nodes
                .iter()
                .filter(|n| !n.folder && node.as_ref().is_none_or(|wanted| *wanted == n.handle))
                .map(|n| Entry {
                    url: file_link(&link, &n.handle),
                    name: n.name.clone(),
                    package: root(n),
                    size: n.size,
                })
                .collect();
            if entries.is_empty() {
                return Err("This share has no files".into());
            }
            Ok(entries)
        }
        Link::WeTransfer { .. } => Err("Not a MEGA link".into()),
    }
}

fn root_name<'a>(by_handle: &HashMap<&str, &'a Node>, mut n: &'a Node) -> String {
    // A broken chain can't loop: each step goes to a node of the share.
    for _ in 0..by_handle.len() {
        match by_handle.get(n.parent.as_str()) {
            Some(parent) => n = parent,
            None => break,
        }
    }
    n.name.clone()
}

async fn resolve_mega(http: &Client, link: &Link) -> Result<Resolved, String> {
    let (scope, node, key) = match link {
        Link::MegaFile { handle, key } => {
            let answer = mega(http, "", json!({"a": "g", "g": 1, "ssl": 1, "p": handle})).await?;
            return mega_download(&answer, key);
        }
        Link::MegaFolder { node: Some(node), .. } | Link::Transfer { node: Some(node), .. } => {
            let (scope, folder_key) = scope_of(link).unwrap();
            let nodes = folder_nodes(http, &scope, folder_key).await?;
            let found = nodes.into_iter().find(|n| n.handle == *node).ok_or_else(|| mega_error(-9))?;
            (scope, node.clone(), found.key)
        }
        _ => return Err("Pick a file of this share".into()),
    };
    let answer = mega(http, &scope, json!({"a": "g", "g": 1, "ssl": 1, "n": node})).await?;
    mega_download(&answer, &key)
}

fn mega_download(answer: &Value, key: &[u8]) -> Result<Resolved, String> {
    if let Some(code) = answer["e"].as_i64().filter(|c| *c < 0) {
        return Err(mega_error(code));
    }
    let url = answer["g"].as_str().filter(|g| g.starts_with("http")).ok_or("MEGA didn't hand over the file")?;
    let cipher = Cipher::from_file_key(key).ok_or("This file's key is incomplete")?;
    Ok(Resolved { url: url.to_string(), cipher: Some(cipher) })
}

// --- WeTransfer ---------------------------------------------------------------

/// A WeTransfer download page's transfer id, recipient and security hash.
fn wetransfer_parts(url: &Url) -> Option<(String, Option<String>, String)> {
    let segments: Vec<&str> = url.path_segments()?.filter(|p| !p.is_empty()).collect();
    match segments.as_slice() {
        ["downloads", id, hash] => Some((id.to_string(), None, hash.to_string())),
        ["downloads", id, recipient, hash] => Some((id.to_string(), Some(recipient.to_string()), hash.to_string())),
        _ => None,
    }
}

async fn resolve_wetransfer(http: &Client, url: &str) -> Result<Resolved, String> {
    let mut page = Url::parse(url).map_err(|e| e.to_string())?;
    if host_is(&page, &["we.tl"]) {
        // The short link leads to the download page.
        let r = http.get(url).send().await.map_err(|e| format!("connection: {e}"))?;
        page = r.url().clone();
    }
    let (id, recipient, hash) = wetransfer_parts(&page).ok_or("This WeTransfer link isn't a download page")?;
    let mut body = json!({"security_hash": hash, "intent": "entire_transfer"});
    if let Some(recipient) = recipient {
        body["recipient_id"] = json!(recipient);
    }
    let r = http
        .post(format!("https://wetransfer.com/api/v4/transfers/{id}/download"))
        .header("X-Requested-With", "XMLHttpRequest")
        .json(&body)
        .send()
        .await
        .map_err(|e| format!("connection: {e}"))?;
    let status = r.status();
    let answer: Value = r.json().await.unwrap_or(Value::Null);
    match answer["direct_link"].as_str() {
        Some(link) => Ok(Resolved { url: link.to_string(), cipher: None }),
        None if status.as_u16() == 404 => Err("This transfer doesn't exist anymore".into()),
        None => Err(answer["message"].as_str().map(str::to_string).unwrap_or_else(|| format!("WeTransfer refused the download ({status})"))),
    }
}

/// A fresh address for a file of a share link.
pub async fn resolve(http: &Client, url: &str) -> Result<Resolved, String> {
    match parse(url).ok_or("Not a share link")? {
        Link::WeTransfer { url } => resolve_wetransfer(http, &url).await,
        link => resolve_mega(http, &link).await,
    }
}

/// Whether a share link stands for several files, listed rather than probed.
pub fn is_listing(url: &str) -> bool {
    matches!(parse(url), Some(Link::MegaFile { .. } | Link::MegaFolder { .. } | Link::Transfer { .. }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_share_links() {
        assert_eq!(
            parse("https://transfer.it/t/6lK92xRHSR6S"),
            Some(Link::Transfer { handle: "6lK92xRHSR6S".into(), node: None })
        );
        assert_eq!(
            parse("https://transfer.it/t/6lK92xRHSR6S#HtJXFBCa"),
            Some(Link::Transfer { handle: "6lK92xRHSR6S".into(), node: Some("HtJXFBCa".into()) })
        );
        let key = "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA";
        assert!(matches!(parse(&format!("https://mega.nz/file/abc#{key}")), Some(Link::MegaFile { .. })));
        assert!(matches!(parse(&format!("https://mega.nz/#!abc!{key}")), Some(Link::MegaFile { .. })));
        assert!(matches!(
            parse("https://mega.nz/folder/abc#AAAAAAAAAAAAAAAAAAAAAA/file/xyz"),
            Some(Link::MegaFolder { node: Some(n), .. }) if n == "xyz"
        ));
        assert!(matches!(parse("https://mega.nz/#F!abc!AAAAAAAAAAAAAAAAAAAAAA"), Some(Link::MegaFolder { node: None, .. })));
        assert!(parse("https://mega.nz/file/abc").is_none(), "a file link needs its key");
        assert!(parse("https://mega.nz/").is_none());
        assert!(matches!(parse("https://we.tl/t-AbCdEf123"), Some(Link::WeTransfer { .. })));
        assert!(matches!(parse("https://wetransfer.com/downloads/0123abcd/9f8e7d"), Some(Link::WeTransfer { .. })));
        assert!(parse("https://wetransfer.com/").is_none());
        assert!(parse("https://pixeldrain.com/u/abc").is_none());
    }

    #[test]
    fn folder_file_links_round_trip() {
        let link = parse("https://mega.nz/folder/abc#AAAAAAAAAAAAAAAAAAAAAA").unwrap();
        let file = file_link(&link, "xyz");
        assert_eq!(file, "https://mega.nz/folder/abc#AAAAAAAAAAAAAAAAAAAAAA/file/xyz");
        assert!(matches!(parse(&file), Some(Link::MegaFolder { node: Some(n), .. }) if n == "xyz"));
    }

    #[test]
    fn decrypts_names_and_data_like_mega() {
        // A transfer.it node, as its API lists it.
        let key = b64("x9Kkuagtk4hGql0E0lcpmjb12xjbA91hSVDDcVjdTlg").unwrap();
        let attributes = "-gygcRWkwrIZMqCFWC7U8QZncw4PjBhK76tUVlTQHaiJ4F9kt7QPYqZld9oINzYg_VKkBiT7dELhlkORR7wJuDxtODhJp4nhO2cNd9OxT-FSdDOFPRTgk9UE7U8LWqJj";
        assert_eq!(node_name(attributes, &key).as_deref(), Some("com.imaginando.bam.retro_future_pack"));

        // CTR from any offset matches decrypting from the start.
        let cipher = Cipher::from_file_key(&key).unwrap();
        let mut whole = vec![7u8; 100];
        cipher.apply(0, &mut whole);
        let mut tail = vec![7u8; 63];
        cipher.apply(37, &mut tail);
        assert_eq!(&whole[37..], &tail[..]);
    }

    /// Reaches MEGA: `cargo test live_transfer_it -- --ignored`.
    #[tokio::test]
    #[ignore]
    async fn live_transfer_it() {
        let http = Client::new();
        let files = list(&http, "https://transfer.it/t/6lK92xRHSR6S").await.unwrap();
        assert_eq!(files.len(), 11);
        let first = files.iter().find(|f| f.name == "com.imaginando.bam.retro_future_pack").unwrap();
        assert_eq!(first.package, "BAM Expansions");
        let resolved = resolve(&http, &first.url).await.unwrap();
        let start = http.get(&resolved.url).header("Range", "bytes=0-3").send().await.unwrap();
        assert_eq!(start.status().as_u16(), 206);
        let mut head = start.bytes().await.unwrap().to_vec();
        resolved.cipher.unwrap().apply(0, &mut head);
        assert_eq!(&head, b"qres");
    }

    #[test]
    fn folder_keys_decrypt_with_the_link_key() {
        // Encrypt a node key with a folder key, as MEGA stores it, and read it back.
        let folder_key = [3u8; 16];
        let node = [9u8; 32];
        let aes = aes::Aes128::new(&folder_key.into());
        let mut encrypted = node;
        for block in encrypted.chunks_mut(16) {
            aes::cipher::BlockEncrypt::encrypt_block(&aes, block.into());
        }
        let k = format!("owner:{}", base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(encrypted));
        assert_eq!(node_key(&k, Some(&folder_key)).unwrap(), node);
    }
}
