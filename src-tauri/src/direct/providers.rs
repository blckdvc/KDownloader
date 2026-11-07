//! Premium link generators (debrid services), through their official APIs.
//! API keys live in memory here; the app persists them in the Keychain.

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Client, Url};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Mutex;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use tokio::time::sleep;

const HOSTS_TTL: Duration = Duration::from_secs(6 * 3600);
/// After a failed fetch, links count as unsupported for this long rather than
/// each one waiting for the same unreachable service.
const HOSTS_RETRY: Duration = Duration::from_secs(120);
const DELAYED_POLLS: u32 = 24;
const FORM: &str = "application/x-www-form-urlencoded";

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Provider {
    RealDebrid,
    AllDebrid,
    Premiumize,
    DebridLink,
    UploadedPremiumLink,
    Torbox,
    MegaDebrid,
    Cocoleech,
    Fakirdebrid,
}

impl Provider {
    pub const ALL: [Provider; 9] = [
        Provider::RealDebrid,
        Provider::AllDebrid,
        Provider::Premiumize,
        Provider::DebridLink,
        Provider::UploadedPremiumLink,
        Provider::Torbox,
        Provider::MegaDebrid,
        Provider::Cocoleech,
        Provider::Fakirdebrid,
    ];

    pub fn name(self) -> &'static str {
        match self {
            Provider::RealDebrid => "Real-Debrid",
            Provider::AllDebrid => "AllDebrid",
            Provider::Premiumize => "Premiumize",
            Provider::DebridLink => "Debrid-Link",
            Provider::UploadedPremiumLink => "Uploadedpremiumlink",
            Provider::Torbox => "TorBox",
            Provider::MegaDebrid => "Mega-Debrid",
            Provider::Cocoleech => "Cocoleech",
            Provider::Fakirdebrid => "Fakirdebrid",
        }
    }

    pub fn id(self) -> &'static str {
        match self {
            Provider::RealDebrid => "real-debrid",
            Provider::AllDebrid => "all-debrid",
            Provider::Premiumize => "premiumize",
            Provider::DebridLink => "debrid-link",
            Provider::UploadedPremiumLink => "uploaded-premium-link",
            Provider::Torbox => "torbox",
            Provider::MegaDebrid => "mega-debrid",
            Provider::Cocoleech => "cocoleech",
            Provider::Fakirdebrid => "fakirdebrid",
        }
    }

    /// The page where users find their API key.
    pub fn key_page(self) -> &'static str {
        match self {
            Provider::RealDebrid => "https://real-debrid.com/apitoken",
            Provider::AllDebrid => "https://alldebrid.com/apikeys/",
            Provider::Premiumize => "https://www.premiumize.me/account",
            Provider::DebridLink => "https://debrid-link.com/webapp/apikey",
            Provider::UploadedPremiumLink => "https://www.uploadedpremiumlink.net/myaccount/",
            Provider::Torbox => "https://torbox.app/settings",
            Provider::MegaDebrid => "https://www.mega-debrid.eu/index.php?page=api",
            Provider::Cocoleech => "https://members.cocoleech.com/settings",
            Provider::Fakirdebrid => "https://fakirdebrid.net/api/login.php",
        }
    }

    fn base(self) -> &'static str {
        match self {
            Provider::RealDebrid => "https://api.real-debrid.com/rest/1.0",
            Provider::AllDebrid => "https://api.alldebrid.com/v4",
            Provider::Premiumize => "https://www.premiumize.me/api",
            Provider::DebridLink => "https://debrid-link.com/api/v2",
            Provider::UploadedPremiumLink => "https://api.uploadedpremiumlink.net/wp-json/api",
            Provider::Torbox => "https://api.torbox.app/v1/api",
            Provider::MegaDebrid => "https://www.mega-debrid.eu/api.php",
            Provider::Cocoleech => "https://members.cocoleech.com/auth/api",
            Provider::Fakirdebrid => "https://fakirdebrid.net/api",
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct Account {
    pub username: String,
    pub premium: bool,
    /// Unix timestamp.
    pub expires: Option<u64>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Unlocked {
    pub url: String,
    pub name: Option<String>,
    pub size: Option<u64>,
    pub max_chunks: Option<usize>,
}

#[derive(Debug, PartialEq)]
pub enum ApiError {
    Auth(String),
    Unsupported(String),
    Failed(String),
}

impl ApiError {
    /// The service couldn't be reached, as opposed to refusing the request.
    pub fn is_network(&self) -> bool {
        matches!(self, ApiError::Failed(m) if m.starts_with("connection:") || m.starts_with("read:"))
    }

    pub fn message(&self) -> &str {
        match self {
            ApiError::Auth(m) | ApiError::Unsupported(m) | ApiError::Failed(m) => m,
        }
    }
}

pub struct Accounts {
    http: Client,
    keys: Mutex<HashMap<Provider, String>>,
    /// Each list with the time it expires.
    hosts: Mutex<HashMap<Provider, (Instant, Vec<String>)>>,
    /// One fetch at a time, so a cold cache isn't fetched once per link.
    fetching: tokio::sync::Mutex<()>,
    bases: Mutex<HashMap<Provider, String>>,
    delayed_wait: Mutex<Duration>,
}

impl Accounts {
    pub fn new(http: Client) -> Self {
        Self {
            http,
            keys: Mutex::default(),
            hosts: Mutex::default(),
            fetching: tokio::sync::Mutex::const_new(()),
            bases: Mutex::default(),
            delayed_wait: Mutex::new(Duration::from_secs(5)),
        }
    }

    pub fn set_key(&self, provider: Provider, key: Option<String>) {
        let mut keys = self.keys.lock().unwrap();
        match key {
            Some(key) => keys.insert(provider, key),
            None => keys.remove(&provider),
        };
        // A list fetched (or failed) under the old key says nothing about the new one.
        self.hosts.lock().unwrap().remove(&provider);
    }

    pub fn has_key(&self, provider: Provider) -> bool {
        self.keys.lock().unwrap().contains_key(&provider)
    }

    #[cfg(test)]
    pub fn point_at(&self, provider: Provider, base: String, delayed_wait: Duration) {
        self.bases.lock().unwrap().insert(provider, base);
        *self.delayed_wait.lock().unwrap() = delayed_wait;
    }

    fn url(&self, provider: Provider, path: &str) -> String {
        let bases = self.bases.lock().unwrap();
        let base = bases.get(&provider).map(String::as_str).unwrap_or(provider.base());
        format!("{base}{path}")
    }

    /// Tries the connected providers that support this link's host, in order.
    /// `None` means no provider handles it: the link stays a plain download.
    pub async fn unlock_any(&self, url: &str) -> Option<Result<(Provider, Unlocked), String>> {
        let host = Url::parse(url).ok()?.host_str()?.to_ascii_lowercase();
        let mut last = None;
        for provider in Provider::ALL {
            if !self.has_key(provider) || !self.supports(provider, &host).await {
                continue;
            }
            match self.unlock(provider, url).await {
                Ok(unlocked) => return Some(Ok((provider, unlocked))),
                Err(e) => last = Some(format!("{}: {}", provider.name(), e.message())),
            }
        }
        last.map(Err)
    }

    /// Whether a connected account can fetch this link.
    pub async fn has_account_for(&self, url: &str) -> bool {
        let Some(host) = Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        else {
            return false;
        };
        for provider in Provider::ALL {
            if self.has_key(provider) && self.supports(provider, &host).await {
                return true;
            }
        }
        false
    }

    /// Whether a link points at a file hoster, whichever accounts are connected.
    /// Real-Debrid's list is public and broad, and asking for it reveals no link.
    pub async fn is_known_hoster(&self, url: &str) -> bool {
        let Some(host) = Url::parse(url).ok().and_then(|u| u.host_str().map(str::to_ascii_lowercase))
        else {
            return false;
        };
        self.supports(Provider::RealDebrid, &host).await
    }

    /// The domains of known file hosters, from the same public list.
    pub async fn hoster_domains(&self) -> Vec<String> {
        self.supports(Provider::RealDebrid, "").await;
        let hosts = self.hosts.lock().unwrap();
        hosts.get(&Provider::RealDebrid).map(|(_, d)| d.clone()).unwrap_or_default()
    }

    async fn supports(&self, provider: Provider, host: &str) -> bool {
        let cached = || {
            let hosts = self.hosts.lock().unwrap();
            hosts.get(&provider).filter(|(until, _)| *until > Instant::now()).map(|(_, d)| d.clone())
        };
        let domains = match cached() {
            Some(domains) => domains,
            None => {
                let _one = self.fetching.lock().await;
                match cached() {
                    Some(domains) => domains,
                    None => {
                        let fetched = self.fetch_hosts(provider).await;
                        let (ttl, domains) = match fetched {
                            Ok(domains) => (HOSTS_TTL, domains),
                            Err(_) => (HOSTS_RETRY, Vec::new()),
                        };
                        self.hosts.lock().unwrap().insert(provider, (Instant::now() + ttl, domains.clone()));
                        domains
                    }
                }
            }
        };
        domains.iter().any(|d| host_matches(host, d))
    }

    async fn fetch_hosts(&self, provider: Provider) -> Result<Vec<String>, ApiError> {
        let body = match provider {
            Provider::RealDebrid => self.get(provider, "/hosts/domains").await?,
            Provider::AllDebrid => self.get(provider, "/hosts/domains").await?,
            Provider::Premiumize => self.get(provider, "/services/list").await?,
            Provider::DebridLink => self.get(provider, "/downloader/hosts?types=host").await?,
            Provider::UploadedPremiumLink => {
                let key = self.key(provider).unwrap_or_default();
                self.post_form(provider, "/user", &[("api_key", &key)]).await?
            }
            Provider::Torbox => self.get(provider, "/webdl/hosters").await?,
            Provider::MegaDebrid => self.get(provider, "?action=getHostersList").await?,
            Provider::Cocoleech => self.get(provider, "/hosts-status").await?,
            Provider::Fakirdebrid => {
                let pin = enc(&self.key(provider).unwrap_or_default());
                self.get(provider, &format!("/supportedhosts.php?pin={pin}")).await?
            }
        };
        let strings = |v: &Value| -> Vec<String> {
            v.as_array().into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect()
        };
        Ok(match provider {
            Provider::RealDebrid => strings(&body),
            Provider::AllDebrid => {
                let data = &body["data"];
                [strings(&data["hosts"]), strings(&data["redirectors"])].concat()
            }
            Provider::Premiumize => {
                let mut all = strings(&body["directdl"]);
                for name in all.clone() {
                    all.extend(strings(&body["aliases"][&name]));
                }
                all
            }
            Provider::DebridLink => body["value"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|host| host["status"].as_i64().unwrap_or(0) >= 1)
                .flat_map(|host| strings(&host["domains"]))
                .collect(),
            Provider::UploadedPremiumLink => entries(&body["data"]["hosters"])
                .filter(|host| host["status"] == "online")
                .flat_map(|host| {
                    let primary = host["primary_domain"].as_str().map(str::to_string);
                    primary.into_iter().chain(strings(&host["alternatives_domain"]))
                })
                .collect(),
            Provider::Torbox => entries(&body["data"])
                .filter(|host| host["status"] == true)
                .flat_map(|host| strings(&host["domains"]))
                .collect(),
            Provider::MegaDebrid => entries(&body["hosters"])
                .filter(|host| host["status"] == "up")
                .flat_map(|host| strings(&host["domains"]))
                .collect(),
            Provider::Cocoleech => entries(&body["result"])
                .filter(|host| host["status"] == "Online")
                .filter_map(|host| host["host"].as_str().map(str::to_string))
                .collect(),
            Provider::Fakirdebrid => entries(&body["supportedhosts"])
                .filter(|host| host["currently_working"] == true)
                .filter_map(|host| host["host"].as_str().map(str::to_string))
                .collect(),
        })
    }

    /// Checks a key against the provider and describes the account.
    pub async fn verify(&self, provider: Provider, key: &str) -> Result<Account, ApiError> {
        let encoded = enc(key);
        let path = match provider {
            Provider::RealDebrid | Provider::AllDebrid => "/user".to_string(),
            Provider::Premiumize => "/account/info".to_string(),
            Provider::DebridLink => "/account/infos".to_string(),
            Provider::UploadedPremiumLink => "/user".to_string(),
            Provider::Torbox => "/user/me?settings=true".to_string(),
            // The only documented call that proves an application token works.
            Provider::MegaDebrid => format!("?action=getUserHistory&token={encoded}"),
            Provider::Cocoleech => format!("/info?key={encoded}"),
            Provider::Fakirdebrid => format!("/info.php?pin={encoded}"),
        };
        let url = self.url(provider, &path);
        let request = if provider == Provider::UploadedPremiumLink {
            self.http
                .post(url)
                .header(CONTENT_TYPE, FORM)
                .body(format!("api_key={encoded}"))
        } else {
            self.http.get(url).bearer_auth(key)
        };
        let body = parse(provider, request.send().await).await?;
        Ok(match provider {
            Provider::RealDebrid => {
                let seconds = body["premium"].as_u64().unwrap_or(0);
                Account {
                    username: text(&body["username"]),
                    premium: body["type"] == "premium",
                    expires: (seconds > 0).then(|| now() + seconds),
                }
            }
            Provider::AllDebrid => {
                let user = &body["data"]["user"];
                Account {
                    username: text(&user["username"]),
                    premium: user["isPremium"].as_bool().unwrap_or(false),
                    expires: user["premiumUntil"].as_u64().filter(|t| *t > 0),
                }
            }
            Provider::Premiumize => {
                let until = body["premium_until"].as_u64();
                Account {
                    username: text(&body["customer_id"]),
                    premium: until.is_some_and(|t| t > now()),
                    expires: until,
                }
            }
            Provider::DebridLink => {
                let value = &body["value"];
                let seconds = value["premiumLeft"].as_u64().unwrap_or(0);
                Account {
                    username: text(&value["username"]),
                    premium: value["accountType"].as_u64().unwrap_or(0) > 0,
                    expires: (seconds > 0).then(|| now() + seconds),
                }
            }
            Provider::UploadedPremiumLink => {
                let data = &body["data"];
                let until = data["expire_date_timestamp"].as_u64();
                Account {
                    username: text(&data["username"]),
                    premium: until.is_some_and(|t| t > now()),
                    expires: until,
                }
            }
            Provider::Torbox => {
                let data = &body["data"];
                Account {
                    username: text(&data["email"]),
                    premium: data["plan"].as_u64().unwrap_or(0) > 0,
                    expires: data["premium_expires_at"].as_u64(),
                }
            }
            // The API is premium-only and exposes nothing about the account.
            Provider::MegaDebrid => {
                Account { username: "Application token".into(), premium: true, expires: None }
            }
            Provider::Cocoleech => {
                let until = body["expire_date"].as_str().and_then(parse_datetime);
                Account {
                    username: text(&body["username"]),
                    premium: until.is_some_and(|t| t > now()),
                    expires: until,
                }
            }
            Provider::Fakirdebrid => {
                let until = body["premium_until"].as_u64().filter(|t| *t > 0);
                Account {
                    username: text(&body["username"]),
                    premium: until.is_some_and(|t| t > now()),
                    expires: until,
                }
            }
        })
    }

    pub async fn unlock(&self, provider: Provider, link: &str) -> Result<Unlocked, ApiError> {
        match provider {
            Provider::RealDebrid => {
                let body = self.post(provider, "/unrestrict/link", "link", link).await?;
                // Several qualities come back as an array; the first is the best.
                let entry = body.as_array().and_then(|a| a.first()).unwrap_or(&body);
                let url = entry["download"].as_str().ok_or_else(no_link)?.to_string();
                Ok(Unlocked {
                    url,
                    name: entry["filename"].as_str().map(str::to_string),
                    size: entry["filesize"].as_u64(),
                    max_chunks: entry["chunks"].as_u64().filter(|c| *c > 0).map(|c| c as usize),
                })
            }
            Provider::AllDebrid => {
                let body = self.post(provider, "/link/unlock", "link", link).await?;
                let data = &body["data"];
                let url = match delayed_id(data) {
                    Some(id) => self.wait_delayed(provider, "/link/delayed", &[("id", &id)]).await?,
                    None => data["link"].as_str().ok_or_else(no_link)?.to_string(),
                };
                Ok(Unlocked {
                    url,
                    name: data["filename"].as_str().map(str::to_string),
                    size: data["filesize"].as_u64(),
                    max_chunks: None,
                })
            }
            Provider::Premiumize => {
                let body = self.post(provider, "/transfer/directdl", "src", link).await?;
                let entry = &body["content"][0];
                let url = entry["link"].as_str().ok_or_else(no_link)?.to_string();
                let path = entry["path"].as_str().unwrap_or("");
                Ok(Unlocked {
                    url,
                    name: path.rsplit('/').next().filter(|n| !n.is_empty()).map(str::to_string),
                    size: entry["size"].as_u64(),
                    max_chunks: None,
                })
            }
            Provider::DebridLink => {
                let body = self.post(provider, "/downloader/add", "url", link).await?;
                // A folder link answers with an array.
                let value = &body["value"];
                let entry = value.as_array().and_then(|a| a.first()).unwrap_or(value);
                if entry["isProcessing"] == true {
                    return Err(ApiError::Failed("the service is still preparing this file".into()));
                }
                Ok(Unlocked {
                    url: entry["downloadUrl"].as_str().ok_or_else(no_link)?.to_string(),
                    name: entry["name"].as_str().map(str::to_string),
                    size: entry["size"].as_u64(),
                    max_chunks: entry["chunk"].as_u64().filter(|c| *c > 0).map(|c| c as usize),
                })
            }
            Provider::UploadedPremiumLink => {
                let key = self.key(provider).unwrap_or_default();
                let fields = [("api_key", key.as_str()), ("link", link)];
                let body = self.post_form(provider, "/generate_link_by_api", &fields).await?;
                let data = &body["data"];
                let url = match delayed_id(data) {
                    Some(id) => {
                        let fields = [("api_key", key.as_str()), ("id", id.as_str())];
                        self.wait_delayed(provider, "/check_delayed_link", &fields).await?
                    }
                    None => data["link"].as_str().ok_or_else(no_link)?.to_string(),
                };
                Ok(Unlocked {
                    url,
                    name: data["filename"].as_str().map(str::to_string),
                    size: number(&data["filesize"]),
                    max_chunks: number(&data["maxchunks"]).filter(|c| *c > 0).map(|c| c as usize),
                })
            }
            Provider::Torbox => {
                let body = self.post(provider, "/webdl/createwebdownload", "link", link).await?;
                let id = text(&body["data"]["webdownload_id"]);
                let token = enc(&self.key(provider).unwrap_or_default());
                let path = format!("/webdl/requestdl?token={token}&web_id={}", enc(&id));
                let body = self.get(provider, &path).await?;
                let url = body["data"].as_str().ok_or_else(no_link)?.to_string();
                Ok(Unlocked { url, name: None, size: None, max_chunks: None })
            }
            Provider::MegaDebrid => {
                let token = enc(&self.key(provider).unwrap_or_default());
                let path = format!("?action=getLink&token={token}");
                let body = self.post_form(provider, &path, &[("link", link)]).await?;
                let url = body["debridLink"].as_str().ok_or_else(no_link)?.trim_matches('"');
                if url == "cantDebridLink" {
                    return Err(ApiError::Unsupported("this link can't be generated".into()));
                }
                Ok(Unlocked { url: url.to_string(), name: None, size: None, max_chunks: None })
            }
            Provider::Cocoleech => {
                let key = enc(&self.key(provider).unwrap_or_default());
                let path = format!("?key={key}&link={}", enc(link));
                let body = self.get(provider, &path).await?;
                Ok(Unlocked {
                    url: body["download"].as_str().ok_or_else(no_link)?.to_string(),
                    name: None,
                    size: None,
                    max_chunks: number(&body["chunks"]).filter(|c| *c > 0).map(|c| c as usize),
                })
            }
            Provider::Fakirdebrid => {
                let pin = enc(&self.key(provider).unwrap_or_default());
                let path = format!("/generate.php?pin={pin}");
                let body = self.post_form(provider, &path, &[("url", link)]).await?;
                let data = &body["data"];
                let status_url = data["apilink"].as_str().ok_or_else(no_link)?;
                Ok(Unlocked {
                    url: self.wait_transfer(status_url).await?,
                    name: None,
                    size: None,
                    max_chunks: number(&data["maxchunks"]).filter(|c| *c > 0).map(|c| c as usize),
                })
            }
        }
    }

    /// Some services generate links in the background.
    async fn wait_delayed(
        &self,
        provider: Provider,
        path: &str,
        fields: &[(&str, &str)],
    ) -> Result<String, ApiError> {
        for _ in 0..DELAYED_POLLS {
            let wait = *self.delayed_wait.lock().unwrap();
            sleep(wait).await;
            let body = self.post_form(provider, path, fields).await?;
            let data = &body["data"];
            match data["status"].as_u64() {
                Some(2) => return data["link"].as_str().map(str::to_string).ok_or_else(no_link),
                Some(3) => return Err(ApiError::Failed("link generation failed".into())),
                _ => {}
            }
        }
        Err(ApiError::Failed("link generation timed out".into()))
    }

    /// Fakirdebrid fetches the file first, then hands over the link.
    async fn wait_transfer(&self, status_url: &str) -> Result<String, ApiError> {
        for _ in 0..DELAYED_POLLS {
            let resp = self.http.get(status_url).send().await;
            let body = parse(Provider::Fakirdebrid, resp).await?;
            let data = &body["data"];
            if data["files_done"].as_u64() == Some(1) {
                return data["link"].as_str().map(str::to_string).ok_or_else(no_link);
            }
            let wait = *self.delayed_wait.lock().unwrap();
            sleep(wait).await;
        }
        Err(ApiError::Failed("link generation timed out".into()))
    }

    async fn get(&self, provider: Provider, path: &str) -> Result<Value, ApiError> {
        let mut request = self.http.get(self.url(provider, path));
        if let Some(key) = self.key(provider) {
            request = request.bearer_auth(key);
        }
        parse(provider, request.send().await).await
    }

    async fn post(
        &self,
        provider: Provider,
        path: &str,
        field: &str,
        value: &str,
    ) -> Result<Value, ApiError> {
        let key = self.key(provider).ok_or_else(|| ApiError::Auth("not connected".into()))?;
        let (content_type, payload) = if provider == Provider::DebridLink {
            ("application/json", serde_json::json!({ field: value }).to_string())
        } else {
            (FORM, format!("{field}={}", enc(value)))
        };
        let resp = self
            .http
            .post(self.url(provider, path))
            .header(AUTHORIZATION, format!("Bearer {key}"))
            .header(CONTENT_TYPE, content_type)
            .body(payload)
            .send()
            .await;
        parse(provider, resp).await
    }

    /// A form POST whose credentials are in `path` or `fields`.
    async fn post_form(
        &self,
        provider: Provider,
        path: &str,
        fields: &[(&str, &str)],
    ) -> Result<Value, ApiError> {
        if !matches!(provider, Provider::UploadedPremiumLink | Provider::MegaDebrid | Provider::Fakirdebrid) {
            let [(field, value)] = fields else {
                return Err(ApiError::Failed("unexpected request".into()));
            };
            return self.post(provider, path, field, value).await;
        }
        let form: Vec<String> = fields.iter().map(|(k, v)| format!("{k}={}", enc(v))).collect();
        let resp = self
            .http
            .post(self.url(provider, path))
            .header(CONTENT_TYPE, FORM)
            .body(form.join("&"))
            .send()
            .await;
        parse(provider, resp).await
    }

    fn key(&self, provider: Provider) -> Option<String> {
        self.keys.lock().unwrap().get(&provider).cloned()
    }
}

async fn parse(
    provider: Provider,
    resp: Result<reqwest::Response, reqwest::Error>,
) -> Result<Value, ApiError> {
    // Several services take the key in the URL, so errors must not quote it.
    let resp = resp.map_err(|e| ApiError::Failed(format!("connection: {}", e.without_url())))?;
    let status = resp.status();
    let bytes = resp
        .bytes()
        .await
        .map_err(|e| ApiError::Failed(format!("read: {}", e.without_url())))?;
    let body: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
    if let Some(error) = api_error(provider, &body) {
        return Err(error);
    }
    if !status.is_success() {
        return Err(if status.as_u16() == 401 {
            ApiError::Auth("invalid API key".into())
        } else {
            ApiError::Failed(format!("HTTP {}", status.as_u16()))
        });
    }
    Ok(body)
}

/// Each service reports errors in its own shape, often with HTTP 200.
fn api_error(provider: Provider, body: &Value) -> Option<ApiError> {
    match provider {
        Provider::RealDebrid => {
            let message = body["error"].as_str()?.to_string();
            Some(match body["error_code"].as_i64() {
                Some(8) => ApiError::Auth(message),
                Some(16) => ApiError::Unsupported(message),
                _ => ApiError::Failed(message),
            })
        }
        Provider::AllDebrid => {
            if body["status"] != "error" {
                return None;
            }
            let code = body["error"]["code"].as_str().unwrap_or("");
            let message = text(&body["error"]["message"]);
            Some(if code.starts_with("AUTH_") {
                ApiError::Auth(message)
            } else if matches!(code, "LINK_HOST_NOT_SUPPORTED" | "LINK_NOT_SUPPORTED") {
                ApiError::Unsupported(message)
            } else {
                ApiError::Failed(message)
            })
        }
        Provider::Premiumize => {
            if body["status"] != "error" {
                return None;
            }
            let message = text(&body["message"]);
            Some(match body["code"].as_str() {
                Some("authentication_failed") => ApiError::Auth(message),
                Some("service_unsupported") => ApiError::Unsupported(message),
                _ => ApiError::Failed(message),
            })
        }
        Provider::DebridLink => {
            if body["success"] != false {
                return None;
            }
            let code = text(&body["error"]);
            Some(match code.as_str() {
                "badToken" | "hidedToken" => ApiError::Auth(code),
                "hostNotValid" | "notDebrid" => ApiError::Unsupported(code),
                _ => ApiError::Failed(code),
            })
        }
        Provider::UploadedPremiumLink => {
            if body["success"] != false {
                return None;
            }
            let data = &body["data"];
            let code = data["category_error"].as_str().unwrap_or("");
            let message = text(&data["message"]);
            Some(if code.starts_with("AUTH_") {
                ApiError::Auth(message)
            } else if matches!(code, "UNSUPPORTED_HOSTER" | "LINK_TYPE_UNSUPPORTED" | "HOSTER_NOT_AVAILABLE_BY_API") {
                ApiError::Unsupported(message)
            } else {
                ApiError::Failed(message)
            })
        }
        Provider::Torbox => {
            if body["success"] != false {
                return None;
            }
            let detail = text(&body["detail"]);
            Some(ApiError::Failed(if detail.is_empty() { text(&body["error"]) } else { detail }))
        }
        Provider::MegaDebrid => {
            let code = body["response_code"].as_str()?;
            if code == "ok" {
                return None;
            }
            let message = text(&body["response_text"]);
            let message = if message.is_empty() { code.to_string() } else { message };
            Some(if matches!(code, "TOKEN_ERROR" | "PARAMETER_MISSING") {
                ApiError::Auth(message)
            } else {
                ApiError::Failed(message)
            })
        }
        Provider::Cocoleech => {
            let message = body["message"].as_str().filter(|m| !m.is_empty())?.to_string();
            Some(if message.starts_with("Incorrect") {
                ApiError::Auth(message)
            } else {
                ApiError::Failed(message)
            })
        }
        Provider::Fakirdebrid => {
            if body["error"] != true {
                return None;
            }
            let code = text(&body["code"]);
            let message = text(&body["message"]);
            Some(if code.starts_with("PIN_") || matches!(code.as_str(), "Banned_Account" | "Free_Account") {
                ApiError::Auth(message)
            } else if code == "NOT_Supported" {
                ApiError::Unsupported(message)
            } else {
                ApiError::Failed(message)
            })
        }
    }
}

fn enc(value: &str) -> String {
    utf8_percent_encode(value, NON_ALPHANUMERIC).to_string()
}

/// A list that some services send as an array and others as a map.
fn entries(v: &Value) -> impl Iterator<Item = &Value> {
    let list = v.as_array().map(|a| a.iter().collect::<Vec<_>>());
    let map = v.as_object().map(|m| m.values().collect::<Vec<_>>());
    list.or(map).unwrap_or_default().into_iter()
}

/// A number, which some services send as a string.
fn number(v: &Value) -> Option<u64> {
    v.as_u64().or_else(|| v.as_str()?.trim().parse().ok())
}

fn delayed_id(data: &Value) -> Option<String> {
    match &data["delayed"] {
        Value::Number(n) if n.as_u64() != Some(0) => Some(n.to_string()),
        Value::String(s) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

/// Unix time of a `yyyy-MM-dd HH:mm:ss` UTC date.
fn parse_datetime(value: &str) -> Option<u64> {
    let parts: Vec<i64> =
        value.split(['-', ' ', ':']).map(|p| p.parse().ok()).collect::<Option<_>>()?;
    let [year, month, day, hour, minute, second] = parts[..] else { return None };
    // Days from civil date, from Howard Hinnant's algorithm.
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    u64::try_from(days * 86400 + hour * 3600 + minute * 60 + second).ok()
}

fn no_link() -> ApiError {
    ApiError::Failed("the service returned no download link".into())
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `domain` is a domain or, for some Premiumize services, a bare name.
fn host_matches(host: &str, domain: &str) -> bool {
    let domain = domain.to_ascii_lowercase();
    if domain.contains('.') {
        host == domain || host.ends_with(&format!(".{domain}"))
    } else {
        host.split('.').any(|label| label == domain)
    }
}

#[cfg(test)]
pub mod tests {
    use super::*;
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    #[derive(Default)]
    pub struct Mock {
        pub routes: Mutex<HashMap<String, (u16, String)>>,
        pub requests: Mutex<Vec<(String, String, String)>>,
    }

    /// A JSON API on localhost. Each request is logged as (path, auth, body).
    pub async fn mock_api(routes: &[(&str, u16, &str)]) -> (String, Arc<Mock>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let mock = Arc::new(Mock::default());
        for (path, status, body) in routes {
            mock.routes.lock().unwrap().insert(path.to_string(), (*status, body.to_string()));
        }
        let shared = mock.clone();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = listener.accept().await {
                let mock = shared.clone();
                tokio::spawn(async move {
                    let mut raw = Vec::new();
                    let mut buf = [0u8; 4096];
                    let (head, length) = loop {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        raw.extend_from_slice(&buf[..n]);
                        if let Some(end) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&raw[..end]).to_string();
                            let length = head
                                .lines()
                                .find_map(|l| l.to_lowercase().strip_prefix("content-length: ")?.parse().ok())
                                .unwrap_or(0usize);
                            raw.drain(..end + 4);
                            break (head, length);
                        }
                    };
                    while raw.len() < length {
                        let n = sock.read(&mut buf).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        raw.extend_from_slice(&buf[..n]);
                    }
                    let path = head.split_whitespace().nth(1).unwrap_or("").to_string();
                    let auth = head
                        .lines()
                        .find_map(|l| l.to_lowercase().strip_prefix("authorization: ").map(str::to_string))
                        .unwrap_or_default();
                    let body = String::from_utf8_lossy(&raw).to_string();
                    mock.requests.lock().unwrap().push((path.clone(), auth, body));
                    let (status, json) = mock
                        .routes
                        .lock()
                        .unwrap()
                        .get(&path)
                        .cloned()
                        .unwrap_or((404, "{}".to_string()));
                    let kind = if json.starts_with('<') { "text/html" } else { "application/json" };
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\nContent-Type: {kind}\r\nConnection: close\r\nContent-Length: {}\r\n\r\n{json}",
                        json.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                });
            }
        });
        (base, mock)
    }

    fn accounts(provider: Provider, base: String) -> Accounts {
        let accounts = Accounts::new(Client::new());
        accounts.set_key(provider, Some("key".into()));
        accounts.point_at(provider, base, Duration::from_millis(5));
        accounts
    }

    const LINK: &str = "https://rapidgator.net/file/abc";

    #[tokio::test]
    async fn real_debrid_unrestricts_with_a_form_and_bearer_token() {
        let (base, mock) = mock_api(&[
            ("/hosts/domains", 200, r#"["rapidgator.net"]"#),
            ("/unrestrict/link", 200, r#"{"filename":"a.rar","filesize":5,"chunks":8,"download":"https://dl/a.rar"}"#),
        ])
        .await;
        let (provider, unlocked) =
            accounts(Provider::RealDebrid, base).unlock_any(LINK).await.unwrap().unwrap();
        assert_eq!(provider, Provider::RealDebrid);
        assert_eq!(
            unlocked,
            Unlocked { url: "https://dl/a.rar".into(), name: Some("a.rar".into()), size: Some(5), max_chunks: Some(8) }
        );
        let requests = mock.requests.lock().unwrap();
        let (_, auth, body) = requests.last().unwrap();
        assert_eq!(auth, "bearer key");
        assert_eq!(body, "link=https%3A%2F%2Frapidgator%2Enet%2Ffile%2Fabc");
    }

    #[tokio::test]
    async fn alldebrid_unlocks_directly() {
        let (base, _) = mock_api(&[(
            "/link/unlock", 200,
            r#"{"status":"success","data":{"link":"https://dl/b.zip","filename":"b.zip","filesize":9}}"#,
        )])
        .await;
        let unlocked = accounts(Provider::AllDebrid, base).unlock(Provider::AllDebrid, LINK).await.unwrap();
        assert_eq!(unlocked.url, "https://dl/b.zip");
        assert_eq!(unlocked.size, Some(9));
    }

    #[tokio::test]
    async fn alldebrid_waits_for_delayed_links() {
        let (base, mock) = mock_api(&[
            ("/link/unlock", 200, r#"{"status":"success","data":{"filename":"c.zip","delayed":77}}"#),
            ("/link/delayed", 200, r#"{"status":"success","data":{"status":1,"time_left":3}}"#),
        ])
        .await;
        let shared = mock.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(30)).await;
            shared.routes.lock().unwrap().insert(
                "/link/delayed".into(),
                (200, r#"{"status":"success","data":{"status":2,"link":"https://dl/c.zip"}}"#.into()),
            );
        });
        let unlocked = accounts(Provider::AllDebrid, base).unlock(Provider::AllDebrid, LINK).await.unwrap();
        assert_eq!(unlocked.url, "https://dl/c.zip");
        assert_eq!(mock.requests.lock().unwrap()[1].2, "id=77");
    }

    #[tokio::test]
    async fn premiumize_reads_the_first_content_entry() {
        let (base, _) = mock_api(&[(
            "/transfer/directdl", 200,
            r#"{"status":"success","content":[{"path":"dir/d.mkv","size":12,"link":"https://dl/d.mkv"}]}"#,
        )])
        .await;
        let unlocked = accounts(Provider::Premiumize, base).unlock(Provider::Premiumize, LINK).await.unwrap();
        assert_eq!(unlocked.name.as_deref(), Some("d.mkv"));
        assert_eq!(unlocked.size, Some(12));
    }

    #[tokio::test]
    async fn debrid_link_posts_json_and_skips_offline_hosts() {
        let (base, mock) = mock_api(&[
            ("/downloader/hosts?types=host", 200,
             r#"{"success":true,"value":[{"status":1,"domains":["rapidgator.net"]},{"status":0,"domains":["down.io"]}]}"#),
            ("/downloader/add", 200,
             r#"{"success":true,"value":{"name":"f.bin","downloadUrl":"https://dl6/f.bin","chunk":8,"size":125}}"#),
        ])
        .await;
        let accounts = accounts(Provider::DebridLink, base);
        assert!(accounts.unlock_any("https://down.io/x").await.is_none());
        let (_, unlocked) = accounts.unlock_any(LINK).await.unwrap().unwrap();
        assert_eq!(unlocked.url, "https://dl6/f.bin");
        assert_eq!(unlocked.max_chunks, Some(8));
        let requests = mock.requests.lock().unwrap();
        assert_eq!(requests.last().unwrap().2, format!(r#"{{"url":"{LINK}"}}"#));
    }

    #[tokio::test]
    async fn uploadedpremiumlink_resolves_delayed_links() {
        let (base, mock) = mock_api(&[
            ("/user", 200, r#"{"success":true,"data":{"username":"u","expire_date_timestamp":1999999999,
              "hosters":[{"primary_domain":"rapidgator.net","alternatives_domain":["rg.to"],"status":"online"},
                         {"primary_domain":"down.io","status":"offline"}]}}"#),
            ("/generate_link_by_api", 200, r#"{"success":true,"data":{"filename":"g.bin","filesize":"33","maxchunks":4,"delayed":9}}"#),
            ("/check_delayed_link", 200, r#"{"success":true,"data":{"status":2,"link":"https://dl/g.bin"}}"#),
        ])
        .await;
        let accounts = accounts(Provider::UploadedPremiumLink, base);
        assert!(accounts.unlock_any("https://down.io/x").await.is_none());
        let (_, unlocked) = accounts.unlock_any("https://rg.to/file/a").await.unwrap().unwrap();
        assert_eq!(
            unlocked,
            Unlocked { url: "https://dl/g.bin".into(), name: Some("g.bin".into()), size: Some(33), max_chunks: Some(4) }
        );
        assert_eq!(mock.requests.lock().unwrap().last().unwrap().2, "api_key=key&id=9");
        let account = accounts.verify(Provider::UploadedPremiumLink, "key").await.unwrap();
        assert!(account.premium && account.username == "u");
    }

    #[tokio::test]
    async fn torbox_creates_then_requests_the_download() {
        let (base, _) = mock_api(&[
            ("/webdl/hosters", 200, r#"{"success":true,"data":[{"domains":["rapidgator.net"],"status":true},{"domains":["down.io"],"status":false}]}"#),
            ("/webdl/createwebdownload", 200, r#"{"success":true,"data":{"webdownload_id":12}}"#),
            ("/webdl/requestdl?token=key&web_id=12", 200, r#"{"success":true,"data":"https://dl/t.bin"}"#),
            ("/user/me?settings=true", 200, r#"{"success":true,"data":{"email":"a@b.c","plan":2}}"#),
        ])
        .await;
        let accounts = accounts(Provider::Torbox, base);
        assert!(accounts.unlock_any("https://down.io/x").await.is_none());
        let (_, unlocked) = accounts.unlock_any(LINK).await.unwrap().unwrap();
        assert_eq!(unlocked.url, "https://dl/t.bin");
        assert_eq!(accounts.verify(Provider::Torbox, "key").await.unwrap().username, "a@b.c");
    }

    #[tokio::test]
    async fn mega_debrid_sends_the_token_in_the_query() {
        let (base, mock) = mock_api(&[
            ("/m?action=getHostersList", 200, r#"{"hosters":[{"status":"up","domains":["rapidgator.net"]}]}"#),
            ("/m?action=getLink&token=key", 200, r#"{"response_code":"ok","debridLink":"\"https://dl/m.bin\""}"#),
            ("/m?action=getUserHistory&token=key", 200, r#"{"response_code":"TOKEN_ERROR","response_text":"bad token"}"#),
        ])
        .await;
        let accounts = accounts(Provider::MegaDebrid, format!("{base}/m"));
        let (_, unlocked) = accounts.unlock_any(LINK).await.unwrap().unwrap();
        assert_eq!(unlocked.url, "https://dl/m.bin");
        assert_eq!(mock.requests.lock().unwrap().last().unwrap().2, "link=https%3A%2F%2Frapidgator%2Enet%2Ffile%2Fabc");
        let e = accounts.verify(Provider::MegaDebrid, "key").await.unwrap_err();
        assert_eq!(e, ApiError::Auth("bad token".into()));
    }

    #[tokio::test]
    async fn cocoleech_reads_download_and_expiry() {
        let (base, _) = mock_api(&[
            ("/c/hosts-status", 200, r#"{"result":[{"host":"Rapidgator.net","status":"Online"},{"host":"down.io","status":"Offline"}]}"#),
            ("/c?key=key&link=https%3A%2F%2Frapidgator%2Enet%2Ffile%2Fabc", 200, r#"{"status":"100","message":"","download":"https://dl/c.bin","chunks":"6"}"#),
            ("/c/info?key=key", 200, r#"{"status":"100","message":"Incorrect API key."}"#),
        ])
        .await;
        let accounts = accounts(Provider::Cocoleech, format!("{base}/c"));
        assert!(accounts.unlock_any("https://down.io/x").await.is_none());
        let (_, unlocked) = accounts.unlock_any(LINK).await.unwrap().unwrap();
        assert_eq!(unlocked.url, "https://dl/c.bin");
        assert_eq!(unlocked.max_chunks, Some(6));
        let e = accounts.verify(Provider::Cocoleech, "key").await.unwrap_err();
        assert_eq!(e, ApiError::Auth("Incorrect API key.".into()));
    }

    #[tokio::test]
    async fn fakirdebrid_polls_the_transfer() {
        let (base, mock) = mock_api(&[]).await;
        let routes = [
            ("/supportedhosts.php?pin=key".to_string(), r#"{"supportedhosts":{"a":{"host":"rapidgator.net","currently_working":true},"b":{"host":"down.io","currently_working":false}}}"#.to_string()),
            ("/generate.php?pin=key".to_string(), format!(r#"{{"data":{{"apilink":"{base}/status","maxchunks":3}}}}"#)),
            ("/status".to_string(), r#"{"data":{"files_done":0}}"#.to_string()),
            ("/info.php?pin=key".to_string(), r#"{"username":"f","premium_until":1999999999}"#.to_string()),
        ];
        for (path, body) in routes {
            mock.routes.lock().unwrap().insert(path, (200, body));
        }
        let shared = mock.clone();
        tokio::spawn(async move {
            sleep(Duration::from_millis(30)).await;
            shared.routes.lock().unwrap().insert(
                "/status".into(),
                (200, r#"{"data":{"files_done":1,"link":"https://dl/f.bin"}}"#.into()),
            );
        });
        let accounts = accounts(Provider::Fakirdebrid, base);
        assert!(accounts.unlock_any("https://down.io/x").await.is_none());
        let (_, unlocked) = accounts.unlock_any(LINK).await.unwrap().unwrap();
        assert_eq!(unlocked.url, "https://dl/f.bin");
        assert_eq!(unlocked.max_chunks, Some(3));
        assert!(accounts.verify(Provider::Fakirdebrid, "key").await.unwrap().premium);
    }

    #[tokio::test]
    async fn connection_errors_never_quote_a_key_in_the_url() {
        let accounts = accounts(Provider::Cocoleech, "http://127.0.0.1:9/c".into());
        accounts.set_key(Provider::Cocoleech, Some("TOPSECRET".into()));
        let e = accounts.unlock(Provider::Cocoleech, LINK).await.unwrap_err();
        assert!(e.message().starts_with("connection:"));
        assert!(!e.message().contains("TOPSECRET"), "{}", e.message());
    }

    #[test]
    fn dates_convert_to_unix_time() {
        assert_eq!(parse_datetime("1970-01-01 00:00:00"), Some(0));
        assert_eq!(parse_datetime("2026-11-01 12:30:45"), Some(1793536245));
        assert_eq!(parse_datetime("soon"), None);
    }

    #[tokio::test]
    async fn errors_are_classified_per_service() {
        let (base, _) = mock_api(&[("/unrestrict/link", 401, r#"{"error":"bad_token","error_code":8}"#)]).await;
        let e = accounts(Provider::RealDebrid, base).unlock(Provider::RealDebrid, LINK).await.unwrap_err();
        assert_eq!(e, ApiError::Auth("bad_token".into()));

        let (base, _) = mock_api(&[(
            "/link/unlock", 200,
            r#"{"status":"error","error":{"code":"LINK_HOST_NOT_SUPPORTED","message":"nope"}}"#,
        )])
        .await;
        let e = accounts(Provider::AllDebrid, base).unlock(Provider::AllDebrid, LINK).await.unwrap_err();
        assert_eq!(e, ApiError::Unsupported("nope".into()));

        let (base, _) = mock_api(&[(
            "/transfer/directdl", 200,
            r#"{"status":"error","code":"service_limit_reached","message":"limit"}"#,
        )])
        .await;
        let e = accounts(Provider::Premiumize, base).unlock(Provider::Premiumize, LINK).await.unwrap_err();
        assert_eq!(e, ApiError::Failed("limit".into()));

        let (base, _) = mock_api(&[("/downloader/add", 400, r#"{"success":false,"error":"hostNotValid"}"#)]).await;
        let e = accounts(Provider::DebridLink, base).unlock(Provider::DebridLink, LINK).await.unwrap_err();
        assert_eq!(e, ApiError::Unsupported("hostNotValid".into()));

        let (base, _) = mock_api(&[(
            "/generate_link_by_api", 400,
            r#"{"success":false,"data":{"message":"no","category_error":"UNSUPPORTED_HOSTER"}}"#,
        )])
        .await;
        let e = accounts(Provider::UploadedPremiumLink, base)
            .unlock(Provider::UploadedPremiumLink, LINK)
            .await
            .unwrap_err();
        assert_eq!(e, ApiError::Unsupported("no".into()));

        let (base, _) = mock_api(&[(
            "/generate.php?pin=key", 200,
            r#"{"error":true,"code":"PIN_Invalid","message":"bad pin"}"#,
        )])
        .await;
        let e = accounts(Provider::Fakirdebrid, base).unlock(Provider::Fakirdebrid, LINK).await.unwrap_err();
        assert_eq!(e, ApiError::Auth("bad pin".into()));
    }

    #[tokio::test]
    async fn links_of_unsupported_hosts_never_reach_the_service() {
        let (base, mock) = mock_api(&[("/hosts/domains", 200, r#"["rapidgator.net"]"#)]).await;
        let accounts = accounts(Provider::RealDebrid, base);
        assert!(accounts.unlock_any("https://proof.ovh.net/files/10Mb.dat").await.is_none());
        assert_eq!(mock.requests.lock().unwrap().len(), 1);
        assert_eq!(mock.requests.lock().unwrap()[0].0, "/hosts/domains");
    }

    #[tokio::test]
    async fn next_provider_takes_over_after_a_failure() {
        let (rd, _) = mock_api(&[
            ("/hosts/domains", 200, r#"["rapidgator.net"]"#),
            ("/unrestrict/link", 503, r#"{"error":"hoster_unavailable","error_code":19}"#),
        ])
        .await;
        let (ad, _) = mock_api(&[
            ("/hosts/domains", 200, r#"{"status":"success","data":{"hosts":["rapidgator.net"]}}"#),
            ("/link/unlock", 200, r#"{"status":"success","data":{"link":"https://dl/e"}}"#),
        ])
        .await;
        let accounts = accounts(Provider::RealDebrid, rd);
        accounts.set_key(Provider::AllDebrid, Some("key".into()));
        accounts.point_at(Provider::AllDebrid, ad, Duration::from_millis(5));
        let (provider, _) = accounts.unlock_any(LINK).await.unwrap().unwrap();
        assert_eq!(provider, Provider::AllDebrid);
    }

    #[tokio::test]
    async fn verify_describes_each_account() {
        let (base, _) = mock_api(&[("/user", 200, r#"{"username":"ana","type":"premium","premium":3600}"#)]).await;
        let account = accounts(Provider::RealDebrid, base).verify(Provider::RealDebrid, "key").await.unwrap();
        assert!(account.premium && account.username == "ana" && account.expires.unwrap() > now());

        let (base, _) = mock_api(&[(
            "/user", 200,
            r#"{"status":"success","data":{"user":{"username":"bo","isPremium":true,"premiumUntil":1999999999}}}"#,
        )])
        .await;
        let account = accounts(Provider::AllDebrid, base).verify(Provider::AllDebrid, "key").await.unwrap();
        assert_eq!(account.expires, Some(1999999999));

        let (base, _) = mock_api(&[("/account/info", 200, r#"{"status":"success","customer_id":42,"premium_until":null}"#)]).await;
        let account = accounts(Provider::Premiumize, base).verify(Provider::Premiumize, "key").await.unwrap();
        assert_eq!(account, Account { username: "42".into(), premium: false, expires: None });

        let (base, _) = mock_api(&[(
            "/account/infos", 200,
            r#"{"success":true,"value":{"username":"amy","accountType":2,"premiumLeft":0}}"#,
        )])
        .await;
        let account = accounts(Provider::DebridLink, base).verify(Provider::DebridLink, "key").await.unwrap();
        assert_eq!(account, Account { username: "amy".into(), premium: true, expires: None });
    }

    #[test]
    fn hosts_match_domains_and_subdomains() {
        assert!(host_matches("www.rapidgator.net", "rapidgator.net"));
        assert!(host_matches("rapidgator.net", "Rapidgator.net"));
        assert!(!host_matches("notrapidgator.net", "rapidgator.net"));
        assert!(host_matches("nitroflare.com", "nitroflare"));
    }
}
