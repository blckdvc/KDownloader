//! A popup showing a link protector's page, so that the user can solve its
//! captcha. Pulsar never answers it: it only reads the links the page reveals.

use super::cnl;
use super::engine::Engine;
use super::links;
use percent_encoding::percent_decode_str;
use serde::Serialize;
use std::sync::atomic::{AtomicU64, Ordering};
use tauri::webview::NewWindowResponse;
use tauri::{AppHandle, Emitter, Manager, Url, WebviewUrl, WebviewWindowBuilder};

/// The popup has no access to app commands: its only way to report links is to
/// navigate here, which `on_navigation` intercepts and cancels.
const REPORT_HOST: &str = "pulsar.invalid";

/// Domains the popup may navigate to besides the protector: captcha providers.
/// Anything else that takes over the page is an ad.
const CAPTCHA_HOSTS: [&str; 8] = [
    "google.com", "gstatic.com", "recaptcha.net", "hcaptcha.com",
    "cloudflare.com", "cutcaptcha.net", "keycaptcha.com", "solvemedia.com",
];

/// Reports only links to known hosters: a navigation, even a cancelled one, can
/// interrupt what the page is loading, such as its captcha.
const REPORT_LINKS: &str = r#"(() => {
  if (window.top !== window.self) return;
  const hosters = __HOSTERS__;
  const isHoster = (u) => {
    try {
      const { hostname, pathname } = new URL(u);
      return pathname.length > 1 && hosters.some((d) => hostname === d || hostname.endsWith("." + d));
    } catch { return false; }
  };
  const sent = new Set();
  const urls = /https?:\/\/[^\s"'<>]+/g;
  const captcha = '#captcha, #captcha-form, .h-captcha, .cf-turnstile, .g-recaptcha, [id*=captcha i]';
  const scan = () => {
    // These pages carry hoster ads: anything before the captcha is solved isn't a result.
    if (document.querySelector(captcha)) return;
    const found = new Set();
    document.querySelectorAll("a[href]").forEach((a) => { if (!a.querySelector("img")) found.add(a.href); });
    document.querySelectorAll("textarea, input").forEach((e) => (e.value.match(urls) || []).forEach((u) => found.add(u)));
    (document.body?.textContent.match(urls) || []).forEach((u) => found.add(u));
    const fresh = [...found].filter((u) => isHoster(u) && !sent.has(u));
    if (!fresh.length) return;
    fresh.forEach((u) => sent.add(u));
    location.href = "https://pulsar.invalid/links#" + encodeURIComponent(fresh.join(" "));
  };
  let timer;
  const later = () => { clearTimeout(timer); timer = setTimeout(scan, 500); };
  new MutationObserver(later).observe(document, { childList: true, subtree: true, characterData: true });
  addEventListener("load", later);
})();"#;

/// A page's Click'n'Load button posts its encrypted links to the local port, which
/// a secure page may be refused. This catches that post, whichever way the page
/// sends it, and reports what it carries, so the port isn't needed.
const CATCH_CLICK_N_LOAD: &str = r#"(() => {
  if (window.top !== window.self) return;
  const isCnl = (u) => /^https?:\/\/(127\.0\.0\.1|localhost):9666\//i.test(String(u || ""));
  const report = (url, body) => {
    const path = new URL(url).pathname;
    location.href = `https://pulsar.invalid/cnl${path}#${encodeURIComponent(String(body))}`;
  };
  const fromForm = (form) => {
    const action = form.getAttribute("action") || "";
    if (!isCnl(action)) return false;
    report(action, new URLSearchParams(new FormData(form)).toString());
    return true;
  };
  // FileCrypt's page already holds the data its button would send, once its
  // captcha is solved: no click needed.
  let read = false;
  const readPage = () => {
    if (read) return;
    const pop = document.documentElement.outerHTML.match(/CNLPOP\(\s*'([^']*)'\s*,\s*'([^']*)'\s*,\s*'([^']*)'/);
    const form = document.querySelector("input[name=jk]")?.form;
    // The key is recognised by its shape: argument order has changed over time.
    const args = pop ? pop.slice(1) : [];
    const key = args.find((a) => /^[0-9a-f]{32}$/i.test(a)) ?? args[1];
    const data = args.filter((a) => a !== key).sort((a, b) => b.length - a.length)[0];
    const fields = pop
      ? { jk: `function f(){ return '${key}';}`, crypted: data }
      : form && { jk: form.elements.jk.value, crypted: form.elements.crypted?.value };
    if (!fields?.jk || !fields.crypted) return;
    read = true;
    report("http://127.0.0.1:9666/flash/addcrypted2", new URLSearchParams(fields).toString());
  };
  let pending;
  const later = () => { clearTimeout(pending); pending = setTimeout(readPage, 700); };
  new MutationObserver(later).observe(document, { childList: true, subtree: true, attributes: true });
  addEventListener("load", later);
  const submit = HTMLFormElement.prototype.submit;
  HTMLFormElement.prototype.submit = function () { if (!fromForm(this)) submit.call(this); };
  addEventListener("submit", (e) => { if (fromForm(e.target)) e.preventDefault(); }, true);
  const fetchPage = window.fetch;
  window.fetch = function (input, init) {
    const url = input?.url || input;
    if (!isCnl(url)) return fetchPage.apply(this, arguments);
    report(url, init?.body);
    return Promise.resolve(new Response("success"));
  };
  const open = XMLHttpRequest.prototype.open;
  const send = XMLHttpRequest.prototype.send;
  XMLHttpRequest.prototype.open = function (method, url) { this.pulsarUrl = url; return open.apply(this, arguments); };
  XMLHttpRequest.prototype.send = function (body) {
    if (isCnl(this.pulsarUrl)) report(this.pulsarUrl, body); else return send.apply(this, arguments);
  };
})();"#;

/// Reduces a Keeplinks page to its captcha form, in the real page, so the user
/// only sees what they have to answer. Its first step is a button with nothing
/// to answer, so it is pressed here.
const FOCUS_KEEPLINKS: &str = r#"(() => {
  if (window.top !== window.self) return;
  window.open = () => null;
  addEventListener("DOMContentLoaded", () => {
    if (document.getElementById("showpageval")) {
      document.frmprotect?.submit();
      return;
    }
    const form = document.getElementById("frmprotect");
    if (!form) return;
    const style = document.createElement("style");
    style.textContent = `
      html, body { background: #fff !important; }
      body * { visibility: hidden !important; pointer-events: none !important; }
      #frmprotect, #frmprotect * { visibility: visible !important; pointer-events: auto !important; }
      #frmprotect { position: fixed !important; inset: 0 !important; overflow: auto !important;
        padding: 16px !important; background: #fff !important; z-index: 2147483647 !important; }
      #frmprotect .orange-box { display: none !important; }`;
    document.documentElement.appendChild(style);
    document.getElementById("captcha-form")?.focus();
  });
})();"#;

/// Shows only the captcha of any other protector, for as long as there is one, by
/// hiding what surrounds it when found. Whatever appears later stays alone: widgets
/// open their puzzle in a layer of their own, which must not be hidden or covered.
const FOCUS_CAPTCHA: &str = r#"(() => {
  if (window.top !== window.self) return;
  // Widgets build their puzzle in a shadow root: keep it readable to recognise it.
  const attachShadow = Element.prototype.attachShadow;
  Element.prototype.attachShadow = function (init) { return attachShadow.call(this, { ...init, mode: "open" }); };
  HTMLMediaElement.prototype.play = () => Promise.resolve();
  const owned = '.cc, .ccm, [id*=captcha i], [class*=captcha i], iframe[src*=captcha i], iframe[src*=turnstile i], iframe[src*=recaptcha i]';
  const isCaptcha = (e) => e.matches(owned) || !!e.querySelector(owned) || !!e.shadowRoot?.querySelector(owned)
    || [...e.querySelectorAll("*")].some((n) => n.shadowRoot?.querySelector(owned));
  // Ad overlays land at the top of the page. An observer runs before the browser
  // paints, so anything that isn't the captcha's is never seen.
  const watched = new Set();
  const judge = (e) => {
    if (isCaptcha(e)) { e.style.removeProperty("display"); watched.delete(e); }
    else { e.style.setProperty("display", "none", "important"); watched.add(e); }
  };
  new MutationObserver((records) => {
    for (const { target, addedNodes } of records) {
      if (target !== document.body && target !== document.documentElement) continue;
      for (const node of addedNodes) {
        if (node.nodeType === 1 && !node.matches("head, body, script, style, link")) judge(node);
      }
    }
  }).observe(document, { childList: true, subtree: true });
  setInterval(() => watched.forEach(judge), 200);
  const widgets = '.cutcaptcha, .h-captcha, .cf-turnstile, .g-recaptcha, #puzzle-captcha';
  const images = '[id*=captcha i], [class*=captcha i], img[src*=captcha i], iframe[src*=captcha i]';
  let focused = null;
  let hidden = [];
  const reveal = () => {
    hidden.forEach((e) => e.style.removeProperty("display"));
    hidden = [];
    focused = null;
  };
  const focus = (target) => {
    focused = target;
    for (let e = target; e !== document.body; e = e.parentElement) {
      for (const other of e.parentElement.children) {
        if (other === e || other.matches("script, style, input[type=hidden]")) continue;
        other.style.setProperty("display", "none", "important");
        hidden.push(other);
      }
    }
    document.body.style.setProperty("background", "white", "important");
  };
  const apply = () => {
    const found = document.body?.querySelector(widgets);
    const other = found ? null : document.body?.querySelector(images);
    const target = found || other?.closest("form") || other;
    if (target === focused) return;
    reveal();
    if (target) focus(target);
  };
  let timer;
  const later = () => { clearTimeout(timer); timer = setTimeout(apply, 100); };
  new MutationObserver(later).observe(document, { childList: true, subtree: true });
  addEventListener("DOMContentLoaded", apply);
})();"#;

static NEXT_POPUP: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Serialize)]
struct ProtectedLinks {
    source: String,
    links: String,
    /// Came from outside a known protector: wait for the user's review.
    review: bool,
}

/// The candidate links in a report. A bare domain is a site's home page, like
/// the hoster logos these pages show, not a file.
fn reported_links(report: &Url) -> Vec<String> {
    let text = percent_decode_str(report.fragment().unwrap_or("")).decode_utf8_lossy();
    links::extract_urls(&text)
        .into_iter()
        .filter(|u| Url::parse(u).is_ok_and(|url| url.path().len() > 1))
        .filter(|u| !links::is_affiliate_link(u))
        .collect()
}

/// Keeps file hoster links, which is all a page can make us act on: an ad's link
/// or a hostile page's download goes nowhere.
async fn deliver(app: AppHandle, source: String, candidates: Vec<String>, review: bool) -> bool {
    let engine = app.state::<Engine>().inner().clone();
    let mut kept = Vec::new();
    for link in candidates {
        if !links::is_affiliate_link(&link) && engine.accounts().is_known_hoster(&link).await {
            kept.push(link);
        }
    }
    if kept.is_empty() {
        return false;
    }
    let _ = app.emit("protected-links", ProtectedLinks { source, links: kept.join(" "), review });
    true
}

async fn on_report(app: AppHandle, label: String, source: String, candidates: Vec<String>) {
    if deliver(app.clone(), source, candidates, false).await {
        if let Some(window) = app.get_webview_window(&label) {
            let _ = window.close();
        }
    }
}

fn caught_links(report: &Url) -> Option<Vec<String>> {
    let path = report.path().strip_prefix("/cnl")?;
    let body = percent_decode_str(report.fragment()?).collect::<Vec<u8>>();
    Some(cnl::links_from(path, &body)?.1)
}

async fn on_click_n_load(app: AppHandle, label: String, source: String, report: Url) {
    match caught_links(&report) {
        Some(found) if !found.is_empty() => on_report(app, label, source, found).await,
        _ => {
            let _ = app.emit("clipboard-error", "Click'n'Load data couldn't be read");
        }
    }
}

/// Listens for the links sites send to Click'n'Load. Without the port, for
/// instance when JDownloader holds it, everything else keeps working.
pub fn start_click_n_load(app: AppHandle) {
    tauri::async_runtime::spawn(async move {
        let listener = match tokio::net::TcpListener::bind(cnl::ADDRESS).await {
            Ok(listener) => listener,
            Err(e) => {
                tracing::warn!("Click'n'Load unavailable on {}: {e}", cnl::ADDRESS);
                return;
            }
        };
        cnl::serve(listener, move |received: cnl::Received| {
            let app = app.clone();
            tauri::async_runtime::spawn(async move {
                let review = !received.trusted;
                if deliver(app.clone(), received.source, received.links, review).await && received.trusted {
                    for (label, window) in app.webview_windows() {
                        if label.starts_with("captcha-") {
                            let _ = window.close();
                        }
                    }
                }
            });
        })
        .await;
    });
}

fn in_domain(host: &str, domain: &str) -> bool {
    host == domain || host.ends_with(&format!(".{domain}"))
}

/// Whether a navigation belongs to solving the page, rather than being an ad.
fn belongs_to_page(target: &Url, page: &Url) -> bool {
    if !matches!(target.scheme(), "http" | "https") {
        return true;
    }
    let host = target.host_str().unwrap_or("");
    // Click'n'Load: the site hands its links to us.
    if host == "127.0.0.1" && target.port() == Some(9666) {
        return true;
    }
    let page_host = page.host_str().unwrap_or("");
    in_domain(host, page_host.trim_start_matches("www."))
        || CAPTCHA_HOSTS.iter().any(|d| in_domain(host, d))
}

#[tauri::command]
pub async fn dd_open_captcha(app: AppHandle, url: String) -> Result<(), String> {
    if !links::is_link_protector(&url) {
        return Err("Not a protected link".into());
    }
    let page = Url::parse(&url).map_err(|e| e.to_string())?;
    let hosters = app.state::<Engine>().accounts().hoster_domains().await;
    let mut script = REPORT_LINKS.replace("__HOSTERS__", &serde_json::json!(hosters).to_string());
    script.push_str(CATCH_CLICK_N_LOAD);
    let keeplinks = page.host_str().is_some_and(|h| h.contains("keeplinks."));
    script.push_str(if keeplinks { FOCUS_KEEPLINKS } else { FOCUS_CAPTCHA });
    let label = format!("captcha-{}", NEXT_POPUP.fetch_add(1, Ordering::Relaxed));
    let (nav_app, nav_label, nav_source, nav_page) = (app.clone(), label.clone(), url.clone(), page.clone());
    let (win_app, win_label, win_source, win_page) = (app.clone(), label.clone(), url.clone(), page.clone());
    WebviewWindowBuilder::new(&app, &label, WebviewUrl::External(page))
        .title("Solve the captcha")
        .inner_size(620.0, 520.0)
        .initialization_script(&script)
        .on_navigation(move |target| {
            let reported = target.host_str() == Some(REPORT_HOST);
            if !reported && belongs_to_page(target, &nav_page) {
                return true;
            }
            if reported && target.path().starts_with("/cnl/") {
                let job = on_click_n_load(nav_app.clone(), nav_label.clone(), nav_source.clone(), target.clone());
                tauri::async_runtime::spawn(job);
                return false;
            }
            // Leaving for a hoster is a revealed link; leaving for anything else is an ad.
            let candidates = if reported { reported_links(target) } else { vec![target.to_string()] };
            let job = on_report(nav_app.clone(), nav_label.clone(), nav_source.clone(), candidates);
            tauri::async_runtime::spawn(job);
            false
        })
        .on_new_window(move |target, _| {
            if belongs_to_page(&target, &win_page) {
                // FileCrypt opens each link this way. Elsewhere it's an ad script,
                // and following it would reload the page and lose the captcha.
                if target.path().starts_with("/Link/") {
                    if let Some(window) = win_app.get_webview_window(&win_label) {
                        let _ = window.navigate(target);
                    }
                }
            } else {
                let candidates = vec![target.to_string()];
                let job = on_report(win_app.clone(), win_label.clone(), win_source.clone(), candidates);
                tauri::async_runtime::spawn(job);
            }
            NewWindowResponse::Deny
        })
        .build()
        .map_err(|e| e.to_string())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_page_and_its_captcha_stay_in_the_popup() {
        let page = Url::parse("https://www.keeplinks.org/p16/abc").unwrap();
        let allowed = |u: &str| belongs_to_page(&Url::parse(u).unwrap(), &page);
        assert!(allowed("https://www.keeplinks.org/go"));
        assert!(allowed("https://www.google.com/recaptcha/api2/anchor"));
        assert!(allowed("https://newassets.hcaptcha.com/x"));
        assert!(allowed("https://challenges.cloudflare.com/x"));
        assert!(allowed("about:blank"));
        assert!(allowed("http://127.0.0.1:9666/flash/addcrypted2"));
        assert!(!allowed("http://127.0.0.1:8080/admin"));
        assert!(!allowed("https://casino-ads.example/win"));
        assert!(!allowed("https://rapidgator.net/file/abc"));
    }

    #[test]
    fn caught_posts_decode_like_the_local_port() {
        let body = "urls=https%3A%2F%2Frapidgator.net%2Ffile%2Fabc&source=x";
        let encoded = percent_encoding::utf8_percent_encode(body, percent_encoding::NON_ALPHANUMERIC);
        let report = Url::parse(&format!("https://{REPORT_HOST}/cnl/flash/add#{encoded}")).unwrap();
        assert_eq!(caught_links(&report).unwrap(), ["https://rapidgator.net/file/abc"]);
    }

    #[test]
    fn reports_keep_file_links_only() {
        let text = "https://rapidgator.net/file/abc https://ddownload.com/xyz/f.rar https://rapidgator.net/ https://ddownload.com/free334766.html";
        let report = format!("https://{REPORT_HOST}/links#{}", percent_encoding::utf8_percent_encode(text, percent_encoding::NON_ALPHANUMERIC));
        assert_eq!(
            reported_links(&Url::parse(&report).unwrap()),
            ["https://rapidgator.net/file/abc", "https://ddownload.com/xyz/f.rar"]
        );
    }
}
