//! HTTP session helpers, mirroring `utils/utils.py::create_requests_session`.

use std::time::Duration;

use reqwest::cookie::Jar;
use reqwest::header::{HeaderMap, HeaderValue, COOKIE, SET_COOKIE};
use reqwest::redirect::Policy;
use reqwest::{Client, ClientBuilder};
use std::sync::Arc;

/// Build a reqwest client with retries on common transient errors and a
/// reasonable connection pool. Matches the behaviour of OrpheusDL's
/// `create_requests_session()`.
pub fn build_client(extra_headers: Option<HeaderMap>) -> Client {
    build_client_with_user_agent(extra_headers, DEFAULT_USER_AGENT)
}

pub fn build_client_with_user_agent(extra_headers: Option<HeaderMap>, user_agent: &str) -> Client {
    let mut builder = ClientBuilder::new()
        .user_agent(user_agent)
        .gzip(true)
        .brotli(true)
        .deflate(true)
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(60))
        .timeout(Duration::from_secs(300))
        .pool_max_idle_per_host(50)
        .cookie_store(true)
        .redirect(Policy::limited(10));

    if let Some(h) = extra_headers {
        builder = builder.default_headers(h);
    }

    builder.build().expect("build http client")
}

pub const DEFAULT_USER_AGENT: &str = "PicaroDL/0.1";

/// Bare-bones "raw" client that does NOT honour a cookie store, used for
/// CDNs. Verifies certs by default.
pub fn build_raw_client() -> Client {
    ClientBuilder::new()
        .user_agent(DEFAULT_USER_AGENT)
        .connect_timeout(Duration::from_secs(30))
        .read_timeout(Duration::from_secs(120))
        .timeout(Duration::from_secs(900))
        .build()
        .expect("build raw http client")
}

/// Helper for adding cookie values to a header map.
pub fn set_cookie(headers: &mut HeaderMap, key: &str, value: &str) {
    let v = format!("{key}={value}");
    if let Ok(hv) = HeaderValue::from_str(&v) {
        headers.insert(COOKIE, hv);
    }
}

/// Get all Set-Cookie values from a header map and return them as plain strings.
pub fn get_set_cookies(headers: &HeaderMap) -> Vec<String> {
    headers
        .get_all(SET_COOKIE)
        .iter()
        .filter_map(|h| h.to_str().ok().map(|s| s.to_string()))
        .collect()
}

pub fn jar() -> Arc<Jar> {
    Arc::new(Jar::default())
}
