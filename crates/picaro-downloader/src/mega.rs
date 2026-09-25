//! Keyless downloads from public MEGA (mega.nz) links.
//!
//! Uses the `mega` crate's anonymous client: no login, just
//! `fetch_public_nodes` + `download_node`. The AES keys are carried in the
//! URL fragment, so they never reach the server.

use std::path::{Path, PathBuf};

use picaro_utils::error::{Error, Result};

/// Returns `true` when the URL's host is `mega.nz` or one of its subdomains.
pub fn is_mega(url: &str) -> bool {
    url::Url::parse(url)
        .ok()
        .and_then(|parsed| {
            parsed
                .host_str()
                .map(|host| host.trim_end_matches('.').to_ascii_lowercase())
        })
        .map(|host| host == "mega.nz" || host.ends_with(".mega.nz"))
        .unwrap_or(false)
}

/// Sanitises a MEGA node name for on-disk use, preserving its extension.
fn sanitise_node_name(name: &str) -> String {
    let sanitised = picaro_utils::util::sanitise_name(name);
    let trimmed = sanitised.trim();
    if trimmed.is_empty() {
        "mega_download".to_string()
    } else {
        trimmed.to_string()
    }
}

/// Downloads every file node of a public MEGA link into `dest_dir`.
///
/// Returns the paths that were written, in node iteration order. A single-file
/// link therefore yields exactly one entry. Folder links yield every file in
/// the folder (flattened).
pub async fn download_mega(url: &str, dest_dir: &Path) -> Result<Vec<PathBuf>> {
    let client = mega::ClientBuilder::new()
        .build(reqwest::Client::new())
        .map_err(|e| Error::Other(format!("mega client init failed: {e}")))?;

    let nodes = client
        .fetch_public_nodes(url)
        .await
        .map_err(|e| Error::Other(format!("mega fetch failed: {e}")))?;

    tokio::fs::create_dir_all(dest_dir).await?;

    let mut written = Vec::new();
    for node in nodes.iter() {
        if node.kind() != mega::NodeKind::File {
            continue;
        }

        let path = dest_dir.join(sanitise_node_name(node.name()));
        if let Some(parent) = path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        let file = tokio::fs::File::create(&path).await?;
        let writer = tokio_util::compat::TokioAsyncWriteCompatExt::compat_write(file);

        client.download_node(node, writer).await.map_err(|e| {
            Error::Other(format!("mega download failed for '{}': {e}", node.name()))
        })?;

        written.push(path);
    }

    Ok(written)
}

/// Picks the most likely audio file from a MEGA download set.
///
/// Preference order:
/// 1. a file whose stem matches `track_name` (case-insensitive, either
///    direction) and has an extension,
/// 2. the first file with a recognised audio extension,
/// 3. the first file that has any extension.
///
/// Files without an extension are never returned so callers can fall back to
/// their normal hoster handling.
pub fn pick_best_file<'a>(files: &'a [PathBuf], track_name: &str) -> Option<&'a Path> {
    let wanted = track_name.trim().to_lowercase();

    let has_ext = |p: &PathBuf| p.extension().is_some();

    if !wanted.is_empty() {
        if let Some(found) = files.iter().filter(|p| has_ext(p)).find(|p| {
            p.file_stem()
                .and_then(|stem| stem.to_str())
                .map(|stem| {
                    let stem = stem.to_lowercase();
                    stem.contains(&wanted) || wanted.contains(&stem)
                })
                .unwrap_or(false)
        }) {
            return Some(found.as_path());
        }
    }

    if let Some(found) = files
        .iter()
        .filter(|p| has_ext(p))
        .find(|p| picaro_utils::safety::is_audio_extension(p))
    {
        return Some(found.as_path());
    }

    files.iter().find(|p| has_ext(p)).map(|p| p.as_path())
}
