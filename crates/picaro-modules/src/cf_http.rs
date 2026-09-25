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
        .header(reqwest::header::USER_AGENT, UA)
        .header(reqwest::header::REFERER, referer)
        .header(
            reqwest::header::ACCEPT,
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        )
        .header(reqwest::header::ACCEPT_LANGUAGE, "en-US,en;q=0.9");
    // Android-friendly bypass: a `cf_clearance` cookie harvested by a WebView (or
    // any browser) can be injected here so the normal reqwest path passes
    // Cloudflare on-device, with no Docker/FlareSolverr needed.
    if let Ok(cookie) = std::env::var("PICARO_CF_COOKIE") {
        if !cookie.trim().is_empty() {
            req = req.header(reqwest::header::COOKIE, cookie);
        }
    }
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
