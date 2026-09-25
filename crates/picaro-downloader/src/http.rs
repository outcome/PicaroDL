//! HTTP / file-system helpers used by the downloader and the modules.

use std::path::{Path, PathBuf};

use picaro_utils::error::{Error, Result};
use picaro_utils::util::is_missing_executable_error;
use reqwest::header::{HeaderMap, HeaderValue, RANGE};
use tokio::fs;
use tokio::io::AsyncWriteExt;

use indicatif::{ProgressBar, ProgressStyle};

/// Track a single in-flight download with an optional progress bar.
pub struct DownloadProgress {
    pub bar: Option<ProgressBar>,
}

impl Drop for DownloadProgress {
    fn drop(&mut self) {
        if let Some(bar) = self.bar.take() {
            bar.finish_and_clear();
        }
    }
}

impl DownloadProgress {
    pub fn hidden() -> Self {
        Self { bar: None }
    }

    pub fn with_bar(bytes: u64, label: &str) -> Self {
        let bar = ProgressBar::new(bytes);
        bar.set_style(
            ProgressStyle::default_bar()
                .template("{msg} [{bar:30.cyan/blue}] {bytes}/{total_bytes} ({eta})")
                .unwrap()
                .progress_chars("##-"),
        );
        bar.set_message(label.to_string());
        Self { bar: Some(bar) }
    }

    pub fn update(&self, delta: u64) {
        if let Some(bar) = &self.bar {
            bar.inc(delta);
        }
    }
}

/// Download `url` to `dest` using a fresh HTTP client. Returns the number of
/// bytes downloaded. Optionally uses a progress bar and an `Authorization`
/// header.
pub async fn download_to_path(
    client: &reqwest::Client,
    url: &str,
    dest: &Path,
    headers: Option<HeaderMap>,
    progress: DownloadProgress,
) -> Result<u64> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }
    // NOTE: no `dest.exists()` early-return here. The caller owns the
    // skip-vs-overwrite decision (`ignore_existing_files`); returning Ok(0)
    // here used to silently defeat forced re-downloads and left a 0-byte
    // success signal. Python's `download_file` likewise only skips when the
    // track-level check says so.
    let mut req = client.get(url);
    if let Some(h) = headers {
        req = req.headers(h);
    }
    let mut resp = req.send().await?.error_for_status()?;
    let total = resp.content_length().unwrap_or(0);
    let mut file = fs::File::create(dest).await?;
    let mut bytes: u64 = 0;
    while let Some(chunk) = resp.chunk().await? {
        file.write_all(&chunk).await?;
        bytes += chunk.len() as u64;
        progress.update(chunk.len() as u64);
    }
    file.flush().await?;
    if let Some(bar) = &progress.bar {
        if total == 0 {
            bar.set_position(bytes);
        }
    }
    Ok(bytes)
}

/// Create a uniquely-named file inside the `temp/` dir.
pub fn create_temp_filename() -> PathBuf {
    let dir = std::env::current_dir()
        .unwrap_or_else(|_| PathBuf::from("."))
        .join("temp");
    let _ = std::fs::create_dir_all(&dir);
    let id: String = (0..16)
        .map(|_| {
            let idx = rand::random::<u8>() % 16;
            format!("{:x}", idx)
        })
        .collect();
    dir.join(id)
}

/// Same but with a specific extension appended.
pub fn create_temp_filename_with_ext(ext: &str) -> PathBuf {
    let base = create_temp_filename();
    if ext.is_empty() {
        base
    } else {
        base.with_extension(ext)
    }
}

/// Move a temp file to a destination, removing the temp on success.
pub async fn move_temp_to(temp: &Path, dest: &Path) -> Result<()> {
    if let Some(parent) = dest.parent() {
        fs::create_dir_all(parent).await?;
    }
    if dest.exists() {
        let _ = fs::remove_file(temp).await;
        return Ok(());
    }
    if let Err(_) = fs::rename(temp, dest).await {
        // Cross-device - fall back to copy
        fs::copy(temp, dest).await?;
        let _ = fs::remove_file(temp).await;
    }
    Ok(())
}

/// Detect missing-ffmpeg style errors and surface a nicer message.
pub fn explain_external_failure(tool: &str, err: &str) -> String {
    if is_missing_executable_error(err) {
        format!(
            "{tool} not found on PATH. Install {tool} and configure its path under Settings > Advanced > ffmpeg_path."
        )
    } else {
        err.to_string()
    }
}

/// Build a Range header for partial downloads.
pub fn range_header(start: u64, end: u64) -> HeaderMap {
    let mut h = HeaderMap::new();
    h.insert(
        RANGE,
        HeaderValue::from_str(&format!("bytes={start}-{end}")).unwrap(),
    );
    h
}

/// Tidy: rename a file to `<stem>.tmp` if it's empty or unreadable.
pub fn cleanup_zero_byte(path: &Path) {
    if path.exists() {
        if let Ok(meta) = std::fs::metadata(path) {
            if meta.len() == 0 {
                let _ = std::fs::remove_file(path);
            }
        }
    }
}
