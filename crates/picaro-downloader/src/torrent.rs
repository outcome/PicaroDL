//! BitTorrent (magnet) download engine, built on `librqbit`.
//!
//! Magnet links are common in torrent index results (Nyaa, The Pirate Bay,
//! 1337x). A single magnet usually holds a whole release, so the flow is:
//! enumerate the torrent's files with a `list_only` add, keep only the audio
//! entries, then re-add the magnet requesting just those file indices and wait
//! for completion. Finally the output folder is walked for the resulting audio
//! files so callers get concrete paths, independent of how the torrent laid
//! its tree out.
//!
//! Everything is off unless the caller explicitly invokes it; the `[torrent]`
//! settings block only tunes behaviour (DHT, seeding, size cap).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session, SessionOptions};
use tracing::{debug, info};

/// Public UDP trackers merged into every magnet add so peer discovery
/// doesn't depend on the single tracker baked into the magnet. The
/// magnet's own `&tr=` params are kept; these are added on top.
const EXTRA_TRACKERS: &[&str] = &[
 "udp://tracker.opentrackr.org:1337/announce",
 "udp://tracker.openbittorrent.com:80/announce",
 "udp://tracker.torrent.eu.org:451/announce",
 "udp://opentracker.i2p.rocks:6969/announce",
 "udp://tracker.tiny-vps.com:6969/announce",
 "udp://retracker01-msk.cmdnet.ru:8080/announce",
 "udp://tracker.dler.org:6969/announce",
 "udp://tracker.moeking.me:6969/announce",
 "udp://ipv4.tracker.harry.lu:80/announce",
 "udp://explodie.org:6969/announce",
];

use picaro_utils::error::{Error, Result};

use crate::downloader::DownloadEvent;
use crate::globals::GlobalSettings;

/// Tunables read from the `[torrent]` settings block.
#[derive(Debug, Clone)]
pub struct TorrentSettings {
    pub enabled: bool,
    pub min_seeders: u64,
    pub max_size_gb: f64,
    pub dht: bool,
    pub listen_port: u16,
    pub seed: bool,
}

impl TorrentSettings {
    pub fn from_globals(g: &GlobalSettings) -> Self {
        Self {
            enabled: g.get_bool_or("torrent", "enabled", false),
            min_seeders: g.get_int_or("torrent", "min_seeders", 5).max(0) as u64,
            max_size_gb: g
                .get("torrent", "max_size_gb")
                .and_then(|v| v.as_f64())
                .unwrap_or(8.0),
            dht: g.get_bool_or("torrent", "dht", true),
            listen_port: g.get_int_or("torrent", "listen_port", 0).clamp(0, 65535) as u16,
            seed: g.get_bool_or("torrent", "seed", false),
        }
    }
}

/// True when `s` is a BitTorrent magnet link.
pub fn is_magnet(s: &str) -> bool {
    s.trim_start().starts_with("magnet:")
}

/// True when `s` is a magnet link or an http(s) URL serving .torrent
/// metainfo (a `.torrent` extension or a DLE `do=download` attachment
/// endpoint). Both are routed through the BitTorrent engine.
pub fn is_torrent(s: &str) -> bool {
    let t = s.trim_start();
    is_magnet(t)
        || ((t.starts_with("http://") || t.starts_with("https://"))
            && (t.contains(".torrent") || t.contains("do=download")))
}

fn is_torrent_url(t: &str) -> bool {
    (t.starts_with("http://") || t.starts_with("https://"))
        && (t.contains(".torrent") || t.contains("do=download"))
}

fn is_audio_ext(path: &Path) -> bool {
    path.extension().map_or(false, |e| {
        matches!(
            e.to_str().unwrap_or("").to_ascii_lowercase().as_str(),
            "flac" | "mp3" | "m4a" | "aac" | "ogg" | "oga" | "opus" | "wav" | "aiff" | "aif" | "ape" | "wv"
        )
    })
}

/// Recursively collect every audio file under `dir`.
fn collect_audio_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_audio_files(&path, out);
        } else if is_audio_ext(&path) {
            out.push(path);
        }
    }
}

fn emit(tx: &Option<crossbeam_channel::Sender<DownloadEvent>>, ev: DownloadEvent) {
    if let Some(tx) = tx {
        let _ = tx.send(ev);
    }
}

/// Download every audio file contained in `magnet` into `output_dir`.
///
/// Returns the concrete audio paths that were produced (sorted by name).
/// `events` receives `TrackProgress` updates while peers are transferring.
pub async fn download_magnet(
    magnet: &str,
    output_dir: &Path,
    settings: &TorrentSettings,
    events: Option<crossbeam_channel::Sender<DownloadEvent>>,
    label: &str,
) -> Result<Vec<PathBuf>> {
    if !settings.enabled {
        return Err(Error::Download(
            "torrent: disabled in settings ([torrent] enabled=false)".into(),
        ));
    }
    tokio::fs::create_dir_all(output_dir).await?;

    // An http(s) .torrent URL is fetched here with a browser UA (some
    // DLE attachment hosts 403 default clients) and fed to the engine
    // as raw metainfo bytes instead of letting it fetch the URL itself.
    let torrent_bytes: Option<bytes::Bytes> = if is_torrent_url(magnet.trim_start()) {
        let client = reqwest::Client::builder()
            .user_agent("Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .build()
            .map_err(|e| Error::Download(format!("torrent: client init: {e}")))?;
        let resp = client
            .get(magnet)
            .send()
            .await
            .map_err(|e| Error::Download(format!("torrent: .torrent fetch failed: {e}")))?;
        if !resp.status().is_success() {
            return Err(Error::Download(format!(
                "torrent: .torrent fetch HTTP {}",
                resp.status()
            )));
        }
        let b = resp
            .bytes()
            .await
            .map_err(|e| Error::Download(format!("torrent: .torrent read: {e}")))?;
        if b.first() != Some(&b'd') {
            return Err(Error::Download(
                "torrent: URL did not serve bencode metainfo".into(),
            ));
        }
        Some(b)
    } else {
        None
    };

 let mut opts = SessionOptions {
 disable_dht: !settings.dht,
 ..Default::default()
 };
 // Always bind a TCP listener. When listen_port=0 the OS picks an
 // ephemeral port; without a listener, no incoming peer connections
 // are accepted and downloads stall on cold DHT caches.
 let p = settings.listen_port;
 opts.listen_port_range = Some(p..p.saturating_add(1).max(p + 1));

    let session = Session::new_with_opts(output_dir.to_path_buf(), opts)
        .await
        .map_err(|e| Error::Download(format!("torrent: session init: {e}")))?;

 // 1. Enumerate the torrent's files without downloading anything.
 // Merge in the extra tracker list so the metadata fetch doesn't
 // depend on the single tracker baked into the magnet.
 let list_trackers: Vec<String> = EXTRA_TRACKERS.iter().map(|s| s.to_string()).collect();
 let list_add = match &torrent_bytes {
     Some(b) => AddTorrent::from_bytes(b.clone()),
     None => AddTorrent::from_url(magnet.to_string()),
 };
 let list_resp = session
  .add_torrent(
  list_add,
  Some(AddTorrentOptions {
  list_only: true,
  trackers: Some(list_trackers),
  ..Default::default()
  }),
  )
  .await
  .map_err(|e| Error::Download(format!("torrent: metadata fetch failed: {e}")))?;

    let listing = match list_resp {
        AddTorrentResponse::ListOnly(l) => l,
        _ => {
            return Err(Error::Download(
                "torrent: expected file listing from list_only add".into(),
            ))
        }
    };

    // 2. Pick the audio file indices and compute the release size.
    let mut audio_indices: Vec<usize> = Vec::new();
    let mut total_bytes: u64 = 0;
    for (idx, details) in listing
        .info
        .iter_file_details()
        .map_err(|e| Error::Download(format!("torrent: enumerate files: {e}")))?
        .enumerate()
    {
        total_bytes = total_bytes.saturating_add(details.len);
        let name = details
            .filename
            .to_pathbuf()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default();
        if is_audio_ext(Path::new(&name)) {
            audio_indices.push(idx);
        }
    }

    if audio_indices.is_empty() {
        return Err(Error::Download(
            "torrent: release contains no audio files".into(),
        ));
    }

    let size_gb = total_bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if settings.max_size_gb > 0.0 && size_gb > settings.max_size_gb {
        return Err(Error::Download(format!(
            "torrent: release is {size_gb:.2} GB, over the {:.2} GB limit",
            settings.max_size_gb
        )));
    }
    info!(
        "torrent: {} audio file(s) of {} ({:.2} GB)",
        audio_indices.len(),
        listing.info.name.as_ref().map(|n| n.to_string()).unwrap_or_else(|| label.to_string()),
        size_gb
    );

 // 3. Re-add the magnet, requesting only the audio files. Merge in
 // the extra tracker list on this add too — the magnet's own `&tr=`
 // params may point at a dead tracker, and we need a working peer
 // source for the real download (not just the metadata fetch).
 let add_trackers: Vec<String> = EXTRA_TRACKERS.iter().map(|s| s.to_string()).collect();
 let real_add = match &torrent_bytes {
     Some(b) => AddTorrent::from_bytes(b.clone()),
     None => AddTorrent::from_url(magnet.to_string()),
 };
 let added = session
  .add_torrent(
  real_add,
  Some(AddTorrentOptions {
  only_files: Some(audio_indices),
  overwrite: true,
  trackers: Some(add_trackers),
  ..Default::default()
  }),
  )
  .await
  .map_err(|e| Error::Download(format!("torrent: add failed: {e}")))?;

    let handle = added
        .into_handle()
        .ok_or_else(|| Error::Download("torrent: no handle after add".into()))?;

    // 4. Poll stats for progress while waiting for completion.
    let done = Arc::new(AtomicBool::new(false));
    let poll = {
        let handle = handle.clone();
        let done = done.clone();
        let events = events.clone();
        let label = label.to_string();
        tokio::spawn(async move {
            while !done.load(Ordering::Relaxed) {
                let stats = handle.stats();
                emit(
                    &events,
                    DownloadEvent::TrackProgress {
                        track_id: "torrent".to_string(),
                        name: label.clone(),
                        bytes: stats.progress_bytes,
                        total: if stats.total_bytes > 0 {
                            Some(stats.total_bytes)
                        } else {
                            None
                        },
                    },
                );
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        })
    };

    let wait = handle.wait_until_completed().await;
    done.store(true, Ordering::Relaxed);
    let _ = poll.await;

    if let Err(e) = wait {
        return Err(Error::Download(format!("torrent: download failed: {e}")));
    }

    // 5. Locate the produced audio files on disk.
    let mut files = Vec::new();
    collect_audio_files(output_dir, &mut files);
    if files.is_empty() {
        // Some torrents place content in a per-torrent sub-folder; a full walk
        // already covers that, so an empty result means the mapping failed.
        let missing = "torrent: no audio files found after download";
        debug!("{missing} in {}", output_dir.display());
        return Err(Error::Download(missing.into()));
    }
    files.sort();
    info!("torrent: finished {} file(s)", files.len());
    Ok(files)
}
