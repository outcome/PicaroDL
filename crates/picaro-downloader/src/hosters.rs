//! File-hoster landing-page resolvers.
//!
//! Band / blog modules frequently hand us a *landing page* (e.g. a MediaFire
//! `/file/…` page) instead of the bytes. `download_to_path` would then happily
//! write the HTML to disk and the "audio" check would reject it. This module
//! turns a handful of common hoster pages into a direct file URL.
//!
//! Only hosters whose direct link can be obtained without solving a captcha
//! are resolved. Captcha/JS-gated hosters are recognised so the caller can log
//! them, but `resolve` returns `None` for them - we never fake a captcha.

use regex::Regex;
use std::sync::OnceLock;
use tracing::{debug, info, warn};

/// A recent desktop Chrome UA. Some hosters serve a bot shell (or a 403) to
/// the default/absent UA, so we always identify as a browser.
const BROWSER_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
     (KHTML, like Gecko) Chrome/126.0.0.0 Safari/537.36";

/// Normalise a URL to its lowercase host, stripping a leading `www.`.
fn host_of(url: &str) -> Option<String> {
    let parsed = url::Url::parse(url).ok()?;
    let host = parsed.host_str()?.to_ascii_lowercase();
    Some(host.trim_start_matches("www.").to_string())
}

/// True when `url` points at a hoster landing page we may be able to resolve.
/// Direct download URLs (e.g. `download1234.mediafire.com/…`) are intentionally
/// *not* matched: they are already resolvable.
pub fn is_hoster(url: &str) -> bool {
    matches!(
        host_of(url).as_deref(),
        Some(
            "mediafire.com"
                | "1fichier.com"
                | "hotlink.cc"
                | "nfile.cc"
                | "uploadbox.com"
                | "turbobit.net"
                | "nitroflare.com"
                | "disk.yandex.ru"
                | "disk.yandex.com"
                | "yadi.sk"
                | "imagenetz.de"
                | "send.now"
                | "pixeldrain.com"
                | "drive.google.com"
                | "dropbox.com"
                | "gofile.io"
                | "catbox.moe"
                | "litterbox.catbox.moe"
                | "transfer.sh"
                | "file.io"
                | "tmpfiles.org"
        )
    )
}

/// Resolve a hoster landing page to a direct download URL.
///
/// Returns `Some(direct_url)` **only** when a real file URL was obtained; the
/// caller keeps the original URL otherwise. `referer` is the module's Referer
/// header (empty when absent) and is forwarded to the hoster request.
pub async fn resolve(client: &reqwest::Client, url: &str, referer: &str) -> Option<String> {
    let host = host_of(url)?;
    match host.as_str() {
        "mediafire.com" => resolve_mediafire(client, url, referer).await,
        "1fichier.com" => resolve_1fichier(client, url, referer).await,
        "disk.yandex.ru" | "disk.yandex.com" | "yadi.sk" => resolve_yandex(client, url).await,
        "pixeldrain.com" => resolve_pixeldrain(url),
        "drive.google.com" => resolve_gdrive(url),
        "dropbox.com" => resolve_dropbox(url),
        "gofile.io" => resolve_gofile(client, url).await,
        // These already hand out direct file URLs.
        "catbox.moe" | "litterbox.catbox.moe" | "transfer.sh" | "file.io" | "tmpfiles.org" => {
            Some(url.to_string())
        }
        // Recognised but captcha / JS gated: do not attempt to bypass.
        "hotlink.cc" | "nfile.cc" | "uploadbox.com" | "turbobit.net" | "nitroflare.com"
        | "imagenetz.de" | "send.now" => {
            info!("hoster {host}: unsupported (captcha/JS gated); keeping original URL");
            None
        }
        other => {
            debug!("hoster {other}: no resolver; keeping original URL");
            None
        }
    }
}

/// Minimal HTML entity decode for the handful of entities that appear inside
/// scraped URLs/attribute values.
fn html_unescape(s: &str) -> String {
    s.replace("&amp;", "&")
        .replace("&#38;", "&")
        .replace("&#x26;", "&")
        .replace("&#x2F;", "/")
        .replace("&#47;", "/")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
}

/// GET a page as text with the browser UA (and optional Referer). Returns the
/// body even on 4xx so callers can detect "file not found" markers.
async fn fetch_text(client: &reqwest::Client, url: &str, referer: &str) -> Option<String> {
    let mut req = client
        .get(url)
        .header(reqwest::header::USER_AGENT, BROWSER_UA)
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,*/*;q=0.8",
        );
    if !referer.is_empty() {
        req = req.header(reqwest::header::REFERER, referer);
    }
    let resp = req.send().await.ok()?;
    let status = resp.status();
    let body = resp.text().await.ok()?;
    if !status.is_success() {
        debug!("hoster fetch {url} -> HTTP {status} (len {})", body.len());
    }
    Some(body)
}

/// MediaFire `/file/<key>/<name>/file` landing page.
///
/// The direct link lives in the download anchor:
/// `href="https://download1234.mediafire.com/<token>/<key>/<name>"`, emitted
/// with the `id="downloadButton"` attribute either *before* or *after* `href`.
async fn resolve_mediafire(client: &reqwest::Client, url: &str, referer: &str) -> Option<String> {
    static RE_HREF: OnceLock<Regex> = OnceLock::new();
    static RE_BTN_BEFORE: OnceLock<Regex> = OnceLock::new();
    static RE_BTN_AFTER: OnceLock<Regex> = OnceLock::new();

    let re_href = RE_HREF.get_or_init(|| {
        Regex::new(r#"href="(https://download[0-9]*\.mediafire\.com/[^"]+)""#).unwrap()
    });
    let re_btn_before = RE_BTN_BEFORE
        .get_or_init(|| Regex::new(r#"id="downloadButton"[^>]*href="([^"]+)""#).unwrap());
    let re_btn_after = RE_BTN_AFTER
        .get_or_init(|| Regex::new(r#"href="([^"]+)"[^>]*id="downloadButton""#).unwrap());

    let body = fetch_text(client, url, referer).await?;
    for re in [re_href, re_btn_before, re_btn_after] {
        if let Some(cap) = re.captures(&body) {
            let direct = html_unescape(cap.get(1).unwrap().as_str());
            if direct.starts_with("http") {
                info!("mediafire: resolved direct link");
                return Some(direct);
            }
        }
    }
    warn!("mediafire: no direct link found (dead/private file?); keeping original URL");
    None
}

/// pixeldrain: `https://pixeldrain.com/u/<id>` -> `.../api/file/<id>`.
fn resolve_pixeldrain(url: &str) -> Option<String> {
    let id = url.split("/u/").nth(1)?.split(['?', '/']).next()?;
    if id.is_empty() {
        return None;
    }
    info!("pixeldrain: resolved direct link");
    Some(format!("https://pixeldrain.com/api/file/{id}"))
}

/// Google Drive: `.../file/d/<id>/view` -> `.../uc?export=download&id=<id>`.
fn resolve_gdrive(url: &str) -> Option<String> {
    let id = url.split("/d/").nth(1)?.split(['?', '/']).next()?;
    if id.is_empty() {
        return None;
    }
    info!("google drive: resolved direct link");
    Some(format!(
        "https://drive.google.com/uc?export=download&id={id}"
    ))
}

/// Dropbox: force `?dl=1`.
fn resolve_dropbox(url: &str) -> Option<String> {
    let base = url.split('?').next().unwrap_or(url);
    info!("dropbox: resolved direct link");
    Some(format!("{base}?dl=1"))
}

/// gofile.io: `https://gofile.io/d/<id>` -> direct link via the public API.
async fn resolve_gofile(client: &reqwest::Client, url: &str) -> Option<String> {
    let id = url.split("/d/").nth(1)?.split(['?', '/']).next()?;
    if id.is_empty() {
        return None;
    }
    fn find_link(v: &serde_json::Value) -> Option<String> {
        if let Some(s) = v.get("link").and_then(|x| x.as_str()) {
            return Some(s.to_string());
        }
        if let Some(o) = v.get("contents").and_then(|x| x.as_object()) {
            for c in o.values() {
                if let Some(l) = find_link(c) {
                    return Some(l);
                }
            }
        }
        None
    }
    let api = format!("https://api.gofile.io/getContent?contentId={id}&wt=4fd6sg89d7s6");
    let v: serde_json::Value = client
        .get(&api)
        .header(reqwest::header::USER_AGENT, BROWSER_UA)
        .send()
        .await
        .ok()?
        .json()
        .await
        .ok()?;
    let link = find_link(&v);
    if link.is_some() {
        info!("gofile: resolved direct link");
    }
    link
}

/// Yandex Disk public share (`disk.yandex.ru/d/<key>`). The public REST API is
/// keyless: it returns a direct, time-limited download `href`.
async fn resolve_yandex(client: &reqwest::Client, url: &str) -> Option<String> {
    let resp = client
        .get("https://cloud-api.yandex.net/v1/disk/public/resources/download")
        .query(&[("public_key", url)])
        .header(reqwest::header::USER_AGENT, BROWSER_UA)
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        warn!(
            "yandex: public API HTTP {} (private/expired?)",
            resp.status()
        );
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    match v.get("href").and_then(|h| h.as_str()) {
        Some(href) if !href.is_empty() => {
            info!("yandex: resolved direct link");
            Some(href.to_string())
        }
        _ => {
            warn!("yandex: no href in public API response");
            None
        }
    }
}

/// 1fichier direct-download host: `https://a-18-4.1fichier.com/…` (always has a
/// hyphen in the subdomain, which excludes `img.`/`www.`).
fn find_1fichier_direct(body: &str) -> Option<String> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r#"https://[a-z0-9]+-[a-z0-9-]+\.1fichier\.com/[^"'<>\s]+"#).unwrap()
    });
    re.find(body).map(|m| html_unescape(m.as_str()))
}

/// 1fichier.com `?<id>` landing page.
///
/// Best-effort: 1fichier's free flow is a wait timer followed by a form POST,
/// and free slots are frequently full or recaptcha-gated. We scrape a direct
/// link when it is already present, otherwise we replay the (captcha-free)
/// download form once. Any wait/captcha/"slots busy" response yields `None`.
async fn resolve_1fichier(client: &reqwest::Client, url: &str, referer: &str) -> Option<String> {
    static RE_INPUT: OnceLock<Regex> = OnceLock::new();
    let re_input = RE_INPUT
        .get_or_init(|| Regex::new(r#"<input[^>]*name="([^"]+)"[^>]*value="([^"]*)""#).unwrap());

    // Dedicated client: session cookies + no redirect following, so a 30x to
    // the file host is visible as a Location header instead of being downloaded.
    let c = reqwest::Client::builder()
        .user_agent(BROWSER_UA)
        .cookie_store(true)
        .redirect(reqwest::redirect::Policy::none())
        .build()
        .unwrap_or_else(|_| client.clone());

    let (page_url, referer) = if referer.is_empty() {
        (url, url)
    } else {
        (url, referer)
    };
    let body = fetch_text(&c, page_url, referer).await?;

    if body.contains("n'existe pas") {
        warn!("1fichier: file does not exist; keeping original URL");
        return None;
    }
    if body.contains("cr\u{e9}neaux gratuits") || body.contains("crneaux gratuits") {
        warn!("1fichier: free download slots busy (login/premium required); keeping original URL");
        return None;
    }
    // Some embeds/premium pages expose the direct link straight away.
    if let Some(direct) = find_1fichier_direct(&body) {
        info!("1fichier: resolved direct link from landing page");
        return Some(direct);
    }

    // Replay the free-download form (the page's `#f1` form). No captcha field
    // exists on the current page, but if the response asks for one we bail out.
    let mut form: Vec<(String, String)> = Vec::new();
    for cap in re_input.captures_iter(&body) {
        let name = cap.get(1).unwrap().as_str().to_string();
        let value = html_unescape(cap.get(2).map(|m| m.as_str()).unwrap_or(""));
        form.push((name, value));
    }
    if !form.iter().any(|(k, _)| k == "dl_no_ssl") {
        form.push(("dl_no_ssl".to_string(), "on".to_string()));
    }
    if !form.iter().any(|(k, _)| k == "dlinline") {
        form.push(("dlinline".to_string(), String::new()));
    }

    let resp = match c
        .post(url)
        .header(reqwest::header::REFERER, url)
        .form(&form)
        .send()
        .await
    {
        Ok(r) => r,
        Err(e) => {
            debug!("1fichier: POST failed: {e}");
            return None;
        }
    };

    if let Some(loc) = resp.headers().get(reqwest::header::LOCATION) {
        if let Ok(loc) = loc.to_str() {
            let direct = html_unescape(loc);
            if direct.starts_with("http") {
                info!("1fichier: resolved direct link via redirect");
                return Some(direct);
            }
        }
    }

    let rb = resp.text().await.ok()?;
    if rb.contains("recaptcha")
        || rb.contains("h-captcha")
        || rb.contains("captcha")
        || rb.contains("cr\u{e9}neaux gratuits")
    {
        warn!("1fichier: free download is wait/captcha gated (UNVERIFIED); keeping original URL");
        return None;
    }
    match find_1fichier_direct(&rb) {
        Some(direct) => {
            info!("1fichier: resolved direct link after form POST");
            Some(direct)
        }
        None => {
            warn!("1fichier: no direct link available (UNVERIFIED); keeping original URL");
            None
        }
    }
}
