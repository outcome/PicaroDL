//! Resolve various URL schemes into `MediaIdentification`.
//!
//! Each module that overrides `custom_url_parse` returns a media kind+id
//! instead of going through this default decoder.

use percent_encoding::percent_decode_str;
use url::Url;

use crate::error::{Error, Result};
use crate::models::{DownloadType, MediaIdentification};

/// Map a URL fragment (like `qobuz`, `tidal`, `deezer`) to a module name.
pub fn netloc_to_module(netloc: &str, modules: &crate::ModuleRegistry) -> Option<String> {
    let base_host = {
        let parts: Vec<&str> = netloc.split('.').collect();
        if parts.len() >= 2 {
            parts[parts.len() - 2]
        } else {
            netloc
        }
    };
    for module in modules.iter() {
        let nlc = &module.information.netlocation_constant;
        match nlc {
            crate::models::NetlocConstants::Single(s) => {
                if let Some(inner) = s.strip_prefix("setting.") {
                    if let Some(value) = module
                        .current_settings
                        .as_ref()
                        .and_then(|s| s.get(inner))
                        .and_then(|v| v.as_str())
                    {
                        if value.eq_ignore_ascii_case(netloc)
                            || value.eq_ignore_ascii_case(base_host)
                        {
                            return Some(module.information.service_name.clone());
                        }
                    }
                }
                if s.eq_ignore_ascii_case(netloc) || s.eq_ignore_ascii_case(base_host) {
                    return Some(module.information.service_name.clone());
                }
            }
            crate::models::NetlocConstants::Multi(list) => {
                for s in list {
                    if s.eq_ignore_ascii_case(netloc) || s.eq_ignore_ascii_case(base_host) {
                        return Some(module.information.service_name.clone());
                    }
                }
            }
            crate::models::NetlocConstants::Empty => {}
        }
    }
    None
}

/// Default URL decoder: takes a URL like `https://qobuz.com/album/12345` and
/// produces a `MediaIdentification` of the right type.
pub fn default_decode_url(
    url: &str,
    modules: &crate::ModuleRegistry,
) -> Result<MediaIdentification> {
    let parsed = Url::parse(url).map_err(|e| Error::Other(format!("Invalid URL: {e}")))?;
    let host = parsed.host_str().unwrap_or("").to_lowercase();
    // Use base domain (second-level domain) to match modules, handling subdomains like "open.qobuz.com"
    let module_name = netloc_to_module(&host, modules)
        .ok_or_else(|| Error::Other(format!("No module handles host '{host}'")))?;
    let info = modules
        .get(&module_name)
        .ok_or_else(|| Error::Other(format!("Module '{module_name}' not found")))?;

    // Find the right key/path segment. We scan the URL for each configured
    // url_constant key.
    let path = parsed.path().to_lowercase();
    for (key, dtype) in &info.information.url_constants {
        let key_lower = key.to_lowercase();
        if let Some(idx) = path.find(&format!("/{key_lower}/")) {
            // extract the segment
            let after = &path[idx + key_lower.len() + 2..];
            let id: String = after.chars().take_while(|c| *c != '/').collect();
            if !id.is_empty() {
                return Ok(MediaIdentification {
                    media_type: *dtype,
                    media_id: id,
                    extra_kwargs: serde_json::Map::new(),
                });
            }
        }
    }
    // Fallback: try the last non-empty path segment
    let last = parsed
        .path_segments()
        .and_then(|mut s| s.next_back())
        .unwrap_or("")
        .to_string();
    if !last.is_empty() {
        Ok(MediaIdentification {
            media_type: DownloadType::track,
            media_id: last,
            extra_kwargs: serde_json::Map::new(),
        })
    } else {
        Err(Error::Other(format!(
            "Could not parse media id from URL: {url}"
        )))
    }
}

/// Resolve a Deezer share URL to the canonical https://www.deezer.com/...
/// form, mirroring `resolve_deezer_share_url` in OrpheusDL.
pub async fn resolve_deezer_share_url(url: &str, client: &reqwest::Client) -> Option<String> {
    if !url.contains("link.deezer.com") {
        return Some(url.to_string());
    }
    let resp = client.get(url).send().await.ok()?;
    let final_url = resp.url().to_string();
    if !final_url.contains("deezer.com") || final_url.contains("link.deezer.com") {
        return Some(url.to_string());
    }
    let parsed = Url::parse(&final_url).ok()?;
    Some(format!(
        "{}://{}{}",
        parsed.scheme(),
        parsed.host_str().unwrap_or(""),
        parsed.path().trim_end_matches('/')
    ))
}

/// Percent-decode a string.
pub fn url_decode(s: &str) -> String {
    percent_decode_str(s).decode_utf8_lossy().to_string()
}
