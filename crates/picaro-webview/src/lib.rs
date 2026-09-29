//! Optional minimal "webview": a headless system Chromium that clears a
//! Cloudflare JS challenge for a domain and hands back the session
//! cookies + user agent, which plain `reqwest` then reuses.
//!
//! This crate is only compiled behind the `cf-webview` cargo feature:
//! platforms without a drivable browser (Android CLI, Switch homebrew)
//! never build it, and keep using `PICARO_CF_COOKIE`,
//! `PICARO_FLARESOLVERR`, or a `cf-cookies.json` copied from a PC.

use std::path::PathBuf;

use chromiumoxide::BrowserConfig;

/// A cleared Cloudflare session for one domain.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct CfSession {
    /// Cookie header value, ready for `Cookie: ...`.
    pub cookies: String,
    /// The browser's user agent - `cf_clearance` is bound to it.
    pub user_agent: String,
    /// Host the session was solved for (e.g. "flacmania.biz").
    pub host: String,
    /// Unix seconds when the session was solved.
    pub solved_at: u64,
}

/// Find a Chromium-family browser on this machine, or take
/// `PICARO_BROWSER` as an explicit path.
pub fn find_browser() -> Result<PathBuf, String> {
    if let Ok(p) = std::env::var("PICARO_BROWSER") {
        let p = PathBuf::from(p);
        if p.exists() {
            return Ok(p);
        }
        return Err(format!("PICARO_BROWSER={p:?} does not exist"));
    }
    let candidates: Vec<PathBuf> = [
        r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe",
        r"C:\Program Files (x86)\BraveSoftware\Brave-Browser\Application\brave.exe",
        r"C:\Program Files\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe",
        r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe",
        r"C:\Program Files\Microsoft\Edge\Application\msedge.exe",
        "/usr/bin/brave-browser",
        "/usr/bin/chromium",
        "/usr/bin/chromium-browser",
        "/usr/bin/google-chrome",
        "/usr/bin/google-chrome-stable",
    ]
    .iter()
    .map(PathBuf::from)
    .chain(
        std::env::var("LOCALAPPDATA").map(|la| {
            PathBuf::from(format!("{la}\\Google\\Chrome\\Application\\chrome.exe"))
        }),
    )
    .collect();
    for c in candidates {
        if c.exists() {
            return Ok(c);
        }
    }
    Err("no Chromium-family browser found (set PICARO_BROWSER to a path)".into())
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

/// Load `url` in a headless system Chromium, wait for the Cloudflare
/// challenge to clear, and return the session. `timeout_secs` bounds the
/// whole solve.
pub async fn solve_cf(url: &str, timeout_secs: u64) -> Result<CfSession, String> {
    let exe = find_browser()?;
    let host = host_of(url);

    // Cloudflare fingerprints headless Chromium ("--headless" leaks
    // navigator.webdriver, headless UA tokens, canvas differences) and
    // never clears the challenge. Solve in a REAL browser window parked
    // off-screen instead - invisible to the user, invisible to
    // headless-detection. Set PICARO_CF_HEADLESS=1 to force headless
    // anyway (e.g. a server that must not open windows - though
    // FlareSolverr suits those better).
    let mut builder = BrowserConfig::builder()
        .chrome_executable(exe)
        // A dedicated profile: launching with the default profile while
        // the user's browser is already open makes the new process hand
        // off and exit instantly.
        .user_data_dir(std::env::temp_dir().join("picaro-cf-profile"))
        .arg("--disable-blink-features=AutomationControlled")
        .arg("--disable-gpu")
        .arg("--no-first-run")
        .arg("--window-position=-32000,-32000")
        .arg("--window-size=1280,900");
    if std::env::var("PICARO_CF_HEADLESS").is_ok() {
        // Old `--headless` (chromiumoxide's default) was REMOVED from
        // modern Chromium and makes the process exit instantly.
        builder = builder.new_headless_mode();
    } else {
        builder = builder.with_head();
    }
    let config = builder
        .build()
        .map_err(|e| format!("browser config: {e}"))?;

    let (browser, mut handler) = chromiumoxide::Browser::launch(config)
        .await
        .map_err(|e| format!("browser launch: {e}"))?;
    // The CDP handler must be polled continuously or commands stall. The
    // stream yields transient Errs during startup - keep polling until it
    // ENDS (browser exited); breaking on an Err kills every command.
    tokio::spawn(async move {
        use futures::StreamExt;
        while handler.next().await.is_some() {}
    });

    let page = browser
        .new_page("about:blank")
        .await
        .map_err(|e| format!("new page: {e}"))?;
    page.goto(url)
        .await
        .map_err(|e| format!("goto {url}: {e}"))?;

    let deadline = tokio::time::Instant::now()
        + std::time::Duration::from_secs(timeout_secs.max(10));
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let cookies = page
            .get_cookies()
            .await
            .map_err(|e| format!("get cookies: {e}"))?;
        if cookies.iter().any(|c| c.name == "cf_clearance") {
            let cookie_header = cookies
                .iter()
                .map(|c| format!("{}={}", c.name, c.value))
                .collect::<Vec<_>>()
                .join("; ");
            let ua: String = page
                .evaluate("navigator.userAgent")
                .await
                .and_then(|r| Ok(r.into_value::<String>()?))
                .unwrap_or_default();
            let _ = 0; // handler task is independent
            return Ok(CfSession {
                cookies: cookie_header,
                user_agent: ua,
                host,
                solved_at: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
            });
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(format!(
                "challenge did not clear within {timeout_secs}s (headless browser detected? try a real browser via PICARO_BROWSER or Camoufox/geckodriver)"
            ));
        }
    }
}
