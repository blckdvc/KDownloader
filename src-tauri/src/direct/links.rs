//! Pure text helpers: finding URLs, naming files, grouping them into packages.

use percent_encoding::percent_decode_str;
use regex::Regex;
use reqwest::Url;
use std::collections::HashSet;
use std::sync::LazyLock;

static URL_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)https?://[^\s"'<>\\^`{|}]+"#).unwrap());
static HREF_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r#"(?i)(?:href|src)\s*=\s*["']([^"'#]+)["']"#).unwrap());
static FILE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?i)\.(zip|7z|rar|r\d\d|tar|gz|tgz|bz2|xz|iso|dmg|pkg|exe|msi|deb|rpm|apk|pdf|epub|mp3|flac|wav|m4a|mp4|mkv|avi|mov|webm|bin|\d{3})$",
    )
    .unwrap()
});
static VOLUME_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)\.(part\d+\.rar|r\d\d|z\d\d|\d{3})$").unwrap());
static STEM_RE: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"(?i)(\.tar)?\.[a-z0-9]{1,5}$").unwrap());

/// Every distinct http(s) URL in free-form text, in order of appearance.
pub fn extract_urls(text: &str) -> Vec<String> {
    let mut seen = HashSet::new();
    URL_RE
        .find_iter(text)
        .map(|m| m.as_str().trim_end_matches(['.', ',', ';', ':', '!', '?', ')', ']']).to_string())
        .filter(|u| seen.insert(u.clone()))
        .collect()
}

/// Links to downloadable files in an HTML page (one level, no JS).
pub fn extract_file_links(html: &str, base: &str) -> Vec<String> {
    let Ok(base) = Url::parse(base) else { return vec![] };
    let mut seen = HashSet::new();
    HREF_RE
        .captures_iter(html)
        .filter_map(|c| base.join(c[1].trim()).ok())
        .filter(|u| matches!(u.scheme(), "http" | "https") && FILE_RE.is_match(u.path()))
        .map(|u| u.to_string())
        .filter(|u| seen.insert(u.clone()))
        .collect()
}

/// Makes a server-supplied name safe to use as a single path component.
pub fn sanitize(name: &str) -> String {
    let base = name.rsplit(['/', '\\']).next().unwrap_or("");
    let clean: String = base
        .chars()
        .map(|c| if c.is_control() || ":*?\"<>|".contains(c) { '_' } else { c })
        .collect();
    let clean = clean.trim().trim_start_matches('.').trim();
    if clean.is_empty() { "download".to_string() } else { clean.to_string() }
}

/// Filename from a Content-Disposition header; RFC 5987 `filename*` wins.
pub fn name_from_disposition(value: &str) -> Option<String> {
    let mut plain = None;
    for part in value.split(';').map(str::trim) {
        let Some((key, val)) = part.split_once('=') else { continue };
        match key.trim().to_ascii_lowercase().as_str() {
            "filename*" => {
                let encoded = val.trim().rsplit('\'').next()?;
                return percent_decode_str(encoded).decode_utf8().ok().map(|s| sanitize(&s));
            }
            "filename" => plain = Some(sanitize(val.trim().trim_matches('"'))),
            _ => {}
        }
    }
    plain
}

pub fn name_from_url(url: &str) -> String {
    let path = Url::parse(url).map(|u| u.path().to_string()).unwrap_or_default();
    let last = path.rsplit('/').find(|s| !s.is_empty()).unwrap_or("");
    sanitize(&percent_decode_str(last).decode_utf8_lossy())
}

/// Package a file belongs to: its name without extension or volume suffix, so
/// that `film.part1.rar` and `film.part2.rar` land together.
pub fn package_name(file: &str) -> String {
    let stem = VOLUME_RE.replace(file, "");
    let stem = STEM_RE.replace(&stem, "");
    if stem.is_empty() { file.to_string() } else { stem.to_string() }
}

/// Single-file archives that `tar` can unpack. Multi-volume sets are excluded:
/// bsdtar cannot join them.
pub fn is_extractable(file: &str) -> bool {
    let lower = file.to_ascii_lowercase();
    !VOLUME_RE.is_match(&lower)
        && [".zip", ".7z", ".rar", ".tar", ".tar.gz", ".tgz", ".tar.bz2", ".tbz2", ".tar.xz", ".txz"]
            .iter()
            .any(|ext| lower.ends_with(ext))
}

/// Folder name an archive unpacks into.
pub fn archive_stem(file: &str) -> String {
    let stem = STEM_RE.replace(file, "");
    if stem.is_empty() { file.to_string() } else { stem.to_string() }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_urls_in_text() {
        let text = "voir (https://a.com/x.zip), http://b.org/y.rar. et https://a.com/x.zip";
        assert_eq!(extract_urls(text), ["https://a.com/x.zip", "http://b.org/y.rar"]);
    }

    #[test]
    fn finds_file_links_in_html() {
        let html = r#"<a href="a.zip">a</a><a href="/about">x</a><a href="https://c.io/b.mkv">b</a>"#;
        assert_eq!(
            extract_file_links(html, "https://h.io/dir/"),
            ["https://h.io/dir/a.zip", "https://c.io/b.mkv"]
        );
    }

    #[test]
    fn names_cannot_escape_the_folder() {
        assert_eq!(sanitize("../../etc/passwd"), "passwd");
        assert_eq!(sanitize("..\\..\\boot.ini"), "boot.ini");
        assert_eq!(sanitize(".."), "download");
        assert_eq!(sanitize(".hidden"), "hidden");
        assert_eq!(name_from_disposition("attachment; filename=\"../evil.sh\"").unwrap(), "evil.sh");
    }

    #[test]
    fn reads_content_disposition() {
        assert_eq!(name_from_disposition("attachment; filename=\"a b.zip\"").unwrap(), "a b.zip");
        assert_eq!(
            name_from_disposition("attachment; filename=x; filename*=UTF-8''caf%C3%A9.zip").unwrap(),
            "café.zip"
        );
        assert_eq!(name_from_disposition("inline"), None);
    }

    #[test]
    fn decodes_url_names() {
        assert_eq!(name_from_url("https://a.com/d/mon%20film.mkv?t=1"), "mon film.mkv");
        assert_eq!(name_from_url("https://a.com/"), "download");
    }

    #[test]
    fn groups_volumes_into_one_package() {
        for f in ["film.part1.rar", "film.part02.rar", "film.r00", "film.rar", "film.001", "film.tar.gz"] {
            assert_eq!(package_name(f), "film", "{f}");
        }
    }

    #[test]
    fn only_whole_archives_are_extractable() {
        assert!(is_extractable("a.zip") && is_extractable("a.tar.gz") && is_extractable("a.RAR"));
        assert!(!is_extractable("a.part1.rar") && !is_extractable("a.r00") && !is_extractable("a.mkv"));
        assert_eq!(archive_stem("a.b.tar.gz"), "a.b");
    }
}
