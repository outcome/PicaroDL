//! Cloudflare-aware fetch helper.
//!
//! Strategy (in order):
//!   1. Plain `reqwest` GET with a browser User-Agent + `Referer`. If it returns
//!      2xx and the body does not look like a Cloudflare interstitial, use it.
//!   2. Optional `cloudscraper` fallback (enabled with the `cloudscraper`
//!      cargo feature). Run inside `tokio::task::spawn_blocking` because the
//!      solver is `!Send`.
//!   3. FlareSolverr fallback when `PICARO_FLARESOLVERR` is set to a base URL.
//!
//! NOTE: there is no `cloudscraper` crate on crates.io. The dependency is the
//! `cloudscraper-rs` package, aliased to the name `cloudscraper` in Cargo.toml.

const UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36";

const CHALLENGE_MARKERS: [&str; 4] = [
    "Just a moment",
    "cf-chl",
    "challenge-platform",
    "Attention Required",
];

/// True when the body looks like a Cloudflare challenge / block page.
pub fn is_challenge(body: &str) -> bool {
    CHALLENGE_MARKERS.iter().any(|m| body.contains(m))
}

async fn direct_get(client: &reqwest::Client, url: &str, referer: &str) -> Result<String, String> {
    let mut req = client
        .get(url)
        .header(reqwest::header::REFERER, referer)
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9");
    let mut ua = UA.to_string();
    // Solved sessions take precedence: their cookie is UA-bound, so the
    // request must use the SAME user agent the browser had.
    if let Some(session) = solved_session_for(url) {
        req = req.header(reqwest::header::COOKIE, &session.cookies);
        if !session.user_agent.is_empty() {
            ua = session.user_agent;
        }
    } else if let Ok(cookie) = std::env::var("PICARO_CF_COOKIE") {
        // Android-friendly bypass: a `cf_clearance` cookie harvested by a
        // WebView (or any browser) can be injected here so the normal
        // reqwest path passes Cloudflare on-device.
        if !cookie.trim().is_empty() {
            req = req.header(reqwest::header::COOKIE, cookie);
        }
    }
    req = req.header(reqwest::header::USER_AGENT, ua);
    let resp = req
        .send()
        .await
        .map_err(|e| format!("cf_http request: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("cf_http read: {e}"))?;
    if status.is_success() {
        Ok(body)
    } else {
        Err(format!("cf_http HTTP {status}"))
    }
}

// ---------------------------------------------------------------------------
// Solved-session store: config/cf-cookies.json
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct SolvedSession {
    cookies: String,
    #[serde(default)]
    user_agent: String,
    #[serde(default)]
    solved_at: u64,
}

fn cookies_store_path() -> std::path::PathBuf {
    if let Ok(p) = std::env::var("PICARO_CF_COOKIES") {
        return std::path::PathBuf::from(p);
    }
    std::path::PathBuf::from("config/cf-cookies.json")
}

fn host_of(url: &str) -> String {
    url.split("://")
        .nth(1)
        .unwrap_or(url)
        .split('/')
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// The stored session for `url`'s host, when one exists.
fn solved_session_for(url: &str) -> Option<SolvedSession> {
    let host = host_of(url);
    let store: std::collections::HashMap<String, SolvedSession> = std::fs::read_to_string(
        cookies_store_path(),
    )
    .ok()
    .and_then(|s| serde_json::from_str(&s).ok())?;
    // Exact host or a registered parent domain (www.flacmania.biz ->
    // flacmania.biz).
    if let Some(s) = store.get(&host) {
        return Some(s.clone());
    }
    store
        .iter()
        .find(|(k, _)| host == *k.as_str() || host.ends_with(&format!(".{k}")))
        .map(|(_, v)| v.clone())
}

#[cfg(feature = "cf-webview")]
fn persist_solved_session(session: &picaro_webview::CfSession) {
    let path = cookies_store_path();
    let mut store: serde_json::Map<String, serde_json::Value> = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default();
    store.insert(
        session.host.clone(),
        serde_json::json!({
            "cookies": session.cookies,
            "user_agent": session.user_agent,
            "solved_at": session.solved_at,
        }),
    );
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    if let Ok(s) = serde_json::to_string_pretty(&store) {
        let _ = std::fs::write(&path, s);
    }
}

#[cfg(feature = "cloudscraper")]
async fn via_cloudscraper(url: &str) -> Result<String, String> {
    let url = url.to_string();
    tokio::task::spawn_blocking(move || {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|e| format!("cloudscraper runtime: {e}"))?;
        rt.block_on(async move {
            let scraper =
                cloudscraper::CloudScraper::new().map_err(|e| format!("cloudscraper init: {e}"))?;
            let resp = scraper
                .get(&url)
                .await
                .map_err(|e| format!("cloudscraper get: {e}"))?;
            resp.text()
                .await
                .map_err(|e| format!("cloudscraper text: {e}"))
        })
    })
    .await
    .map_err(|e| format!("cloudscraper join: {e}"))?
}

async fn via_flaresolverr(client: &reqwest::Client, url: &str) -> Result<String, String> {
    let base = std::env::var("PICARO_FLARESOLVERR")
        .map_err(|_| "PICARO_FLARESOLVERR not set".to_string())?;
    let endpoint = format!("{}/v1", base.trim_end_matches('/'));
    let payload = serde_json::json!({
        "cmd": "request.get",
        "url": url,
        "maxTimeout": 60000
    });
    let resp = client
        .post(&endpoint)
        .json(&payload)
        .send()
        .await
        .map_err(|e| format!("flaresolverr request: {e}"))?;
    let value: serde_json::Value = resp
        .json()
        .await
        .map_err(|e| format!("flaresolverr json: {e}"))?;
    value
        .get("solution")
        .and_then(|s| s.get("response"))
        .and_then(|r| r.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| {
            format!(
                "flaresolverr: no solution.response (status={})",
                value
                    .get("status")
                    .and_then(|s| s.as_str())
                    .unwrap_or("unknown")
            )
        })
}

/// Fetch `url` (with `referer`) and transparently bypass Cloudflare challenges
/// when a solver is available.
pub async fn fetch(client: &reqwest::Client, url: &str, referer: &str) -> Result<String, String> {
    if let Ok(body) = direct_get(client, url, referer).await {
        if !is_challenge(&body) {
            return Ok(body);
        }
    }

    #[cfg(feature = "cf-webview")]
    {
        // Built-in solver: drive the system Chromium once, persist the
        // session, retry. Later fetches reuse the stored cookies with no
        // browser involved at all.
        if std::env::var("PICARO_NO_WEBVIEW").is_err() {
            let timeout = std::env::var("PICARO_CF_SOLVE_TIMEOUT")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(60u64);
            match picaro_webview::solve_cf(url, timeout).await {
                Ok(session) => {
                    persist_solved_session(&session);
                    if let Ok(body) = direct_get(client, url, referer).await {
                        if !is_challenge(&body) {
                            return Ok(body);
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("cf_http: webview solver failed for {url}: {e}");
                }
            }
        }
    }

    #[cfg(feature = "cloudscraper")]
    {
        if let Ok(body) = via_cloudscraper(url).await {
            if !is_challenge(&body) {
                return Ok(body);
            }
        }
    }

    if std::env::var("PICARO_FLARESOLVERR").is_ok() {
        if let Ok(body) = via_flaresolverr(client, url).await {
            if !is_challenge(&body) {
                return Ok(body);
            }
        }
    }

    Err(format!(
        "cf_http: unable to fetch {url} (blocked or Cloudflare challenge; set PICARO_FLARESOLVERR to bypass)"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_challenge_markers() {
        assert!(is_challenge("<title>Just a moment...</title>"));
        assert!(is_challenge(r#"<div class="cf-chl">"#));
        assert!(is_challenge("challenge-platform"));
        assert!(is_challenge("Attention Required! | Cloudflare"));
        assert!(!is_challenge("<html><body>hello world</body></html>"));
    }

    /// Live probe. Only runs when `PICARO_CF_LIVE` is set:
    ///   $env:PICARO_CF_LIVE=1; cargo test -p picaro-modules cf_http -- --nocapture
    #[tokio::test]
    async fn live_probe() {
        if std::env::var("PICARO_CF_LIVE").is_err() {
            return;
        }
        let client = reqwest::Client::new();
        for url in [
            "https://lossless-music.org/",
            "https://newalbumreleases.net/",
            "https://flacattack.net/",
            "https://getrockmusic.net/",
            "https://intmusic.net/",
            "https://discogc.com/",
            "https://glorybeats.com/",
        ] {
            match fetch(&client, url, url).await {
                Ok(body) => println!(
                    "OK len={} challenge={} url={url}",
                    body.len(),
                    is_challenge(&body)
                ),
                Err(e) => println!("ERR {e} url={url}"),
            }
        }
    }
}
