//! Chrome TLS-fingerprint emulation (feature `cf-impersonate`).
//!
//! Some WAFs (khinsider's album/track pages) block every non-browser TLS
//! fingerprint outright — no cookie can pass. wreq emulates a real
//! Chrome ClientHello + HTTP/2 frame fingerprint, and with a realistic
//! header set the fetch is indistinguishable enough: verified HTTP 200
//! against pages that 403 plain reqwest and curl. No browser, no
//! webview, no external solver — works anywhere BoringSSL builds.

/// Fetch `url` with a full Chrome-131 fingerprint (TLS + HTTP/2 +
/// headers). Only compiled behind `cf-impersonate`.
#[cfg(feature = "cf-impersonate")]
pub async fn impersonate_fetch(url: &str) -> Result<String, String> {
    use std::sync::OnceLock;
    static CLIENT: OnceLock<wreq::Client> = OnceLock::new();
    let client = CLIENT.get_or_init(|| {
        wreq::Client::builder()
            .emulation(wreq_util::Emulation::Chrome131)
            .build()
            .expect("wreq client init")
    });
    let resp = client
        .get(url)
        .header(
            "user-agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
             (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
        )
        .header(
            "accept",
            "text/html,application/xhtml+xml,application/xml;q=0.9,image/avif,image/webp,*/*;q=0.8",
        )
        .header("accept-language", "en-US,en;q=0.9")
        .header(
            "sec-ch-ua",
            "\"Google Chrome\";v=\"131\", \"Chromium\";v=\"131\", \"Not_A Brand\";v=\"24\"",
        )
        .header("sec-ch-ua-mobile", "?0")
        .header("sec-ch-ua-platform", "\"Windows\"")
        .header("sec-fetch-dest", "document")
        .header("sec-fetch-mode", "navigate")
        .header("sec-fetch-site", "none")
        .header("sec-fetch-user", "?1")
        .header("upgrade-insecure-requests", "1")
        .send()
        .await
        .map_err(|e| format!("impersonate request: {e}"))?;
    let status = resp.status();
    let body = resp
        .text()
        .await
        .map_err(|e| format!("impersonate read: {e}"))?;
    if !status.is_success() {
        return Err(format!("impersonate HTTP {status}"));
    }
    Ok(body)
}
