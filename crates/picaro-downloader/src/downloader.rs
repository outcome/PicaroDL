//! The main downloader - mirrors `picaro/music_downloader.py`.

use std::collections::HashMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crossbeam_channel::{Receiver, Sender};
use futures::future::join_all;
use picaro_core::Picaro;
use picaro_tagging::{resize_cover_if_needed, Tagger};
use picaro_utils::error::{Error, Result};
use picaro_utils::error_simplify::simplify_error_message;
use picaro_utils::models::*;
use picaro_utils::util::sanitise_name;
use serde_json::Value;
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::globals::GlobalSettings;
use crate::http::{download_to_path, DownloadProgress};
use crate::paths::{build_album_path, build_playlist_path, build_track_filename};

/// Archive container kinds a release may arrive in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ArchiveKind {
    SevenZip,
    Zip,
    Rar,
}

/// Detect an archive by its magic bytes, reading only the header (the
/// previous implementation slurped the whole file - 344MB for a FLAC
/// album bundle - just to look at 6 bytes).
fn detect_archive(path: &Path) -> Option<ArchiveKind> {
    let mut f = std::fs::File::open(path).ok()?;
    let mut buf = [0u8; 8];
    let n = f.read(&mut buf).ok()?;
    if n >= 6 && &buf[..6] == b"\x37\x7a\xbc\xaf\x27\x1c" {
        Some(ArchiveKind::SevenZip)
    } else if n >= 4 && &buf[..2] == b"PK" {
        Some(ArchiveKind::Zip)
    } else if n >= 7 && buf.starts_with(b"Rar!\x1a\x07") {
        Some(ArchiveKind::Rar)
    } else {
        None
    }
}

/// Extract a RAR archive using an external tool (`7z`/`unrar`/`unar`), since
/// RAR is proprietary. Returns an error if none is installed.
async fn extract_rar(archive: &Path, out: &Path) -> Result<()> {
    let archive = archive.to_path_buf();
    let out = out.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        use std::process::Command;
        let a = archive.to_string_lossy().to_string();
        let o = out.to_string_lossy().to_string();
        let attempts: [(&str, Vec<String>); 4] = [
            (
                "7z",
                vec!["x".into(), "-y".into(), format!("-o{o}"), a.clone()],
            ),
            (
                "7za",
                vec!["x".into(), "-y".into(), format!("-o{o}"), a.clone()],
            ),
            ("unrar", vec!["x".into(), "-y".into(), a.clone(), o.clone()]),
            ("unar", vec!["-o".into(), o.clone(), a.clone()]),
        ];
        for (tool, args) in attempts {
            if which::which(tool).is_ok() {
                if let Ok(status) = Command::new(tool).args(&args).current_dir(&out).status() {
                    if status.success() {
                        return Ok(());
                    }
                }
            }
        }
        Err(Error::Download(
            "rar extraction failed: install 7-Zip (7z) or unrar".into(),
        ))
    })
    .await
    .map_err(|e| Error::Download(format!("rar join: {e}")))?
}

fn is_audio_ext(path: &Path) -> bool {
    path.extension().map_or(false, |e| {
        matches!(
            e.to_str().unwrap_or("").to_ascii_lowercase().as_str(),
            "flac"
                | "mp3"
                | "m4a"
                | "aac"
                | "ogg"
                | "oga"
                | "opus"
                | "wav"
                | "aiff"
                | "aif"
                | "ape"
                | "wv"
        )
    })
}

/// First audio file under `dir`, preferring FLAC.
fn find_first_audio(dir: &Path) -> Option<PathBuf> {
    let entries = std::fs::read_dir(dir).ok()?;
    let mut first: Option<PathBuf> = None;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            if let Some(p) = find_first_audio(&path) {
                if first.is_none() {
                    first = Some(p);
                }
            }
        } else if is_audio_ext(&path) {
            if path
                .extension()
                .map_or(false, |e| e.eq_ignore_ascii_case("flac"))
            {
                return Some(path);
            }
            if first.is_none() {
                first = Some(path);
            }
        }
    }
    first
}

/// Recursively collect every audio file under `dir` (e.g. all the tracks a
/// single-release archive expanded into).
fn collect_audio_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // macOS archive junk (`__MACOSX`) never holds real audio.
            if path.file_name().and_then(|f| f.to_str()) == Some("__MACOSX") {
                continue;
            }
            collect_audio_files(&path, out);
        } else if is_audio_ext(&path) {
            // AppleDouble resource forks (`._track.mp3`) look like audio
            // but are 256-byte metadata stubs.
            let name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
            if name.starts_with("._") {
                continue;
            }
            out.push(path);
        }
    }
}

/// Extract a zip archive into `out`, skipping traversal-unsafe entries.
async fn extract_zip(archive: &Path, out: &Path) -> Result<()> {
    let archive = archive.to_path_buf();
    let out = out.to_path_buf();
    tokio::task::spawn_blocking(move || -> Result<()> {
        let f =
            std::fs::File::open(&archive).map_err(|e| Error::Download(format!("zip open: {e}")))?;
        let mut z =
            zip::ZipArchive::new(f).map_err(|e| Error::Download(format!("zip read: {e}")))?;
        for i in 0..z.len() {
            let mut entry = match z.by_index(i) {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.is_dir() {
                continue;
            }
            let rel = match entry.enclosed_name() {
                Some(p) => p.to_path_buf(),
                None => continue,
            };
            // macOS zip metadata: `__MACOSX/` AppleDouble stubs and
            // `._name` resource-fork files are not real content, and the
            // stubs carry audio-looking extensions (._track.mp3).
            let name = rel
                .file_name()
                .and_then(|f| f.to_str())
                .unwrap_or_default();
            if name.starts_with("._")
                || rel
                    .components()
                    .any(|c| c.as_os_str() == "__MACOSX")
            {
                continue;
            }
            let target = out.join(&rel);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent).ok();
            }
            if let Ok(mut w) = std::fs::File::create(&target) {
                std::io::copy(&mut entry, &mut w).ok();
            }
        }
        Ok(())
    })
    .await
    .map_err(|e| Error::Download(format!("zip join: {e}")))?
}

/// Extract one archive of any supported kind into `out`. Errors bubble
/// the extraction failure.
async fn extract_archive_of_kind(archive: &Path, kind: ArchiveKind, out: &Path) -> Result<()> {
    match kind {
        ArchiveKind::SevenZip => {
            let archive = archive.to_path_buf();
            let out = out.to_path_buf();
            tokio::task::spawn_blocking(move || {
                sevenz_rust::decompress_file(&archive, &out)
                    .map_err(|e| Error::Download(format!("7z extraction failed: {e}")))
            })
            .await
            .map_err(|e| Error::Download(format!("7z join: {e}")))??;
            Ok(())
        }
        ArchiveKind::Zip => extract_zip(archive, out).await,
        ArchiveKind::Rar => extract_rar(archive, out).await,
    }
}

/// Post-process a directory a release archive extracted into:
/// 1. expand any nested archives (some sites zip the 7z; capped depth),
/// 2. purge everything that is not audio or cover art,
/// 3. flatten a single top-level folder (archives usually wrap their
///    content in "Artist - Album (Year)/" which duplicates the library
///    folder the downloader already created).
pub fn postprocess_extraction(dir: &Path) {
    for _ in 0..3 {
        let mut found = false;
        let mut files = Vec::new();
        collect_all_files(dir, &mut files);
        for f in files {
            if let Some(kind) = detect_archive(&f) {
                let out = f
                    .parent()
                    .map(|p| p.to_path_buf())
                    .unwrap_or_else(|| dir.to_path_buf());
                if extract_archive_of_kind_sync(&f, kind, &out) {
                    let _ = std::fs::remove_file(&f);
                    found = true;
                }
            }
        }
        if !found {
            break;
        }
    }
    let _ = picaro_utils::safety::purge_non_audio(dir);
    flatten_single_top_dir(dir);
}

/// Sync extraction for the nested-archive pass (runs on the async runtime
/// thread, same as the historical code; nested archives are rare/small).
fn extract_archive_of_kind_sync(archive: &Path, kind: ArchiveKind, out: &Path) -> bool {
    match kind {
        ArchiveKind::SevenZip => sevenz_rust::decompress_file(archive, out).is_ok(),
        ArchiveKind::Zip => {
            let Ok(f) = std::fs::File::open(archive) else {
                return false;
            };
            let Ok(mut z) = zip::ZipArchive::new(f) else {
                return false;
            };
            for i in 0..z.len() {
                let Ok(mut entry) = z.by_index(i) else {
                    continue;
                };
                if entry.is_dir() {
                    continue;
                }
                let Some(rel) = entry.enclosed_name() else {
                    continue;
                };
                let name = rel.file_name().and_then(|f| f.to_str()).unwrap_or_default();
                if name.starts_with("._")
                    || rel.components().any(|c| c.as_os_str() == "__MACOSX")
                {
                    continue;
                }
                let target = out.join(&rel);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent).ok();
                }
                if let Ok(mut w) = std::fs::File::create(&target) {
                    std::io::copy(&mut entry, &mut w).ok();
                }
            }
            true
        }
        ArchiveKind::Rar => false, // external tool only; async path handles it
    }
}

/// Recursively collect every file under `dir`.
fn collect_all_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
            if name == "__MACOSX" {
                continue;
            }
            collect_all_files(&path, out);
        } else {
            let name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
            if !name.starts_with("._") {
                out.push(path);
            }
        }
    }
}

/// If `dir` contains exactly one subdirectory (and no loose files), move
/// its contents up into `dir` and drop it - so a bundle's own
/// "Artist - Album/" wrapper doesn't nest inside the library folder.
fn flatten_single_top_dir(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let entries: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
    if entries.len() != 1 || !entries[0].is_dir() {
        return;
    }
    let inner = &entries[0];
    let Ok(children) = std::fs::read_dir(inner) else {
        return;
    };
    for child in children.flatten() {
        let from = child.path();
        let to = dir.join(child.file_name());
        if to.exists() {
            continue;
        }
        let _ = std::fs::rename(&from, &to);
    }
    // Remove if empty (a leftover nested dir means a name collision).
    let _ = std::fs::remove_dir(inner);
}

/// Move a staged extraction's contents into `to`, preserving the
/// directory layout. A file whose target already exists is LEFT BEHIND in
/// the staging dir (the caller deletes it with the staging tree) - a
/// re-run must never overwrite or duplicate tracks already in the
/// library. Nothing in `to` is ever deleted.
fn merge_extraction_into(from: &Path, to: &Path) {
    fn visit(dir: &Path, base: &Path, to: &Path) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                visit(&p, base, to);
                continue;
            }
            let Ok(rel) = p.strip_prefix(base) else {
                continue;
            };
            let target = to.join(rel);
            if target.exists() {
                continue;
            }
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if std::fs::rename(&p, &target).is_err() {
                // Cross-device - copy; the staging tree is removed either way.
                let _ = std::fs::copy(&p, &target);
            }
        }
    }
    visit(from, from, to);
}

/// Per-release sidecar recording the exact audio-file count a completed
/// bundle download left in the album folder, so re-runs can recognise a
/// fully-downloaded release without re-fetching it (page-derived track
/// counts can be wrong: a single's promo text may list a whole album).
fn write_release_sidecar(album_path: &Path, service: &str, album: &AlbumInfo, tracks: usize) {
    let doc = serde_json::json!({
        "service": service,
        "album": album.name,
        "tracks": tracks,
    });
    let _ = std::fs::write(album_path.join(".picaro-release.json"), doc.to_string());
}

/// Read the track count a previous run recorded, if any. `path` is the
/// sidecar file itself.
fn read_release_sidecar(path: &Path) -> Option<usize> {
    let s = std::fs::read_to_string(path).ok()?;
    let v: serde_json::Value = serde_json::from_str(&s).ok()?;
    v.get("tracks")?.as_u64().map(|n| n as usize)
}

/// Natural track ordering key: disc folder, then leading "NN." number,
/// then filename - so "02. questioning" sorts after "01. lose the rain"
/// and "10." after "09." (lexicographic would put "10" before "2").
fn track_sort_key(p: &Path) -> (String, u64, String) {
    let dir = p
        .parent()
        .map(|d| d.to_string_lossy().to_lowercase())
        .unwrap_or_default();
    let stem = p
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .to_string();
    let trimmed = stem.trim_start();
    let num = trimmed
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect::<String>();
    let n = num.parse::<u64>().unwrap_or(u64::MAX);
    (dir, n, stem.to_lowercase())
}

/// Sort audio files into release order (see `track_sort_key`).
pub fn sort_tracks_naturally(paths: &mut [PathBuf]) {
    paths.sort_by(|a, b| {
        let ka = track_sort_key(a);
        let kb = track_sort_key(b);
        ka.cmp(&kb)
    });
}

/// A download event sent to the TUI. The TUI subscribes to a channel and
/// renders these as they happen.
#[derive(Debug, Clone)]
pub enum DownloadEvent {
    Started {
        service: String,
        context: String,
    },
    TrackStarted {
        service: String,
        track_id: String,
        name: String,
    },
    TrackProgress {
        track_id: String,
        name: String,
        bytes: u64,
        total: Option<u64>,
    },
    TrackSucceeded {
        track_id: String,
        name: String,
        location: PathBuf,
        bytes: u64,
    },
    TrackSkipped {
        track_id: String,
        name: String,
        location: PathBuf,
    },
    TrackFailed {
        track_id: String,
        name: String,
        reason: String,
    },
    Log {
        level: LogLevel,
        message: String,
    },
    /// Album item lifecycle for machine consumers: one pair per track
    /// (`picaro item-start <name>` / `picaro item-done <name> <path>`),
    /// also fired for tracks unpacked from a release bundle.
    ItemStarted {
        name: String,
    },
    ItemDone {
        name: String,
        location: PathBuf,
    },
    /// Quality step-down announcement: the request wanted `requested` but
    /// the winning source only serves `served` (`picaro tier <req> <got>`).
    /// Always emitted BEFORE the download proceeds - a UI must be able to
    /// warn instead of silently receiving a lower tier.
    TierNotice {
        requested: String,
        served: String,
    },
    Finished {
        service: String,
        succeeded: u32,
        skipped: u32,
        failed: u32,
    },
    SearchResults {
        service: String,
        results: Vec<SearchResult>,
    },
    Error {
        message: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// The actual downloader. Holds a reference to the `Picaro` core and
/// orchestrates the download flow.
pub struct Downloader {
    pub picaro: Arc<Picaro>,
    pub output_path: PathBuf,
    pub events: (Sender<DownloadEvent>, Receiver<DownloadEvent>),
}

impl Downloader {
    pub fn new(picaro: Arc<Picaro>, output_path: PathBuf) -> Self {
        let (tx, rx) = crossbeam_channel::unbounded();
        Self {
            picaro,
            output_path,
            events: (tx, rx),
        }
    }

    /// Choose the output folder for subsequent downloads. Created if missing -
    /// handy for embedders that want a user-selected destination directory.
    pub fn set_output_path(&mut self, path: PathBuf) {
        let _ = std::fs::create_dir_all(&path);
        self.output_path = path;
    }

    /// The output folder currently in use.
    pub fn output_path(&self) -> &Path {
        &self.output_path
    }

    pub fn sender(&self) -> Sender<DownloadEvent> {
        self.events.0.clone()
    }

    pub fn receiver(&self) -> Receiver<DownloadEvent> {
        self.events.1.clone()
    }

    fn log_info(&self, m: impl Into<String>) {
        let _ = self.events.0.send(DownloadEvent::Log {
            level: LogLevel::Info,
            message: m.into(),
        });
    }
    fn log_warn(&self, m: impl Into<String>) {
        let _ = self.events.0.send(DownloadEvent::Log {
            level: LogLevel::Warn,
            message: m.into(),
        });
    }
    fn log_err(&self, m: impl Into<String>) {
        let _ = self.events.0.send(DownloadEvent::Log {
            level: LogLevel::Error,
            message: m.into(),
        });
    }

    pub fn globals(&self) -> GlobalSettings {
        GlobalSettings::from_merged(&self.picaro.merged_globals)
    }

    /// Download a single track. Used by the TUI's "queue item" handler and by
    /// the album/playlist flows.
    pub async fn download_track(&self, service: &str, track_id: &str) -> Result<PathBuf> {
        self.download_track_with_data(service, track_id, HashMap::new())
            .await
    }

    /// Like `download_track` but forwards module-specific `data` (e.g. the
    /// artist/title from a search result) to `get_track_info` /
    /// `get_track_download`. Used by the resolver so track-URL modules can name
    /// and tag the file properly.
    pub async fn download_track_with_data(
        &self,
        service: &str,
        track_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<PathBuf> {
        let module = self.picaro.load_module(service).await?;
        let globals = self.globals();
        let quality = self.picaro.current_quality();
        let codec_options = self.picaro.codec_options();

        // 1. Resolve the track
        let mut track_info = module
            .get_track_info(track_id, quality, &codec_options, data.clone())
            .await?;

        // 2. If module said it cannot be downloaded, fail.
        if let Some(err) = &track_info.error {
            return Err(Error::Download(err.clone()));
        }

        // 2a. Clean/backfill module metadata from the resolver-scored result.
        normalize_track_metadata(&mut track_info, &data);

        // 2b. Backfill missing metadata from keyless sources.
        if globals.get_bool_or("metadata", "fill_misc", true) {
            let client = reqwest::Client::new();
            let filled =
                picaro_utils::metadata_fill::fill_track_metadata(&client, &mut track_info).await;
            if !filled.is_empty() {
                info!("metadata fill: {}", filled.join(", "));
            }
            // Low-trust sources (YouTube uploader names / video thumbnails,
            // Soulseek scene filenames): replace with the real artist/title/
            // album/cover when confidently matched.
            if is_low_trust_source(service) {
                let forced = picaro_utils::metadata_fill::fill_track_metadata_force(
                    &client,
                    &mut track_info,
                )
                .await;
                info!(
                    "metadata override: {}",
                    if forced.is_empty() {
                        "none".to_string()
                    } else {
                        forced.join(", ")
                    }
                );
            }
        }

        // 3. Album-cover download.
        let cover_path = self
            .download_track_cover(&module, &track_info, &globals)
            .await
            .ok();

        // 4. Lyrics / credits via main module (if it supports them).
        if !module.is_authenticated() {
            debug!(
                "not authenticated for {} - skipping lyrics/credits",
                service
            );
        }
        // Lyrics: main module first, then registered lyrics providers.
        if globals.get_bool_or("metadata", "fetch_lyrics", true) {
            if let Ok(lyrics) = module
                .get_track_lyrics(
                    track_id,
                    track_info.lyrics_extra_kwargs.clone().into_iter().collect(),
                )
                .await
            {
                if let Some(l) = lyrics.embedded {
                    track_info.lyrics = Some(l);
                }
                if let Some(s) = lyrics.synced {
                    track_info.synced_lyrics = Some(s);
                }
            }
            if track_info.lyrics.is_none() && track_info.synced_lyrics.is_none() {
                if let Some(l) = self.fetch_lyrics_from_providers(&track_info).await {
                    if let Some(t) = l.embedded {
                        track_info.lyrics = Some(t);
                    }
                    if let Some(s) = l.synced {
                        track_info.synced_lyrics = Some(s);
                    }
                }
            }
        }
        let credits = module
            .get_track_credits(
                track_id,
                track_info
                    .credits_extra_kwargs
                    .clone()
                    .into_iter()
                    .collect(),
            )
            .await
            .unwrap_or_default();
        track_info.credits_list = credits.clone();

        // 5. Pick the destination filename.
        let mut extension = extension_for_codec(track_info.codec);
        let filename = build_track_filename(&globals, &track_info, extension, true)?;
        let mut dest = self.output_path.join(&filename);
        if dest.exists() && !globals.get_bool_or("advanced", "ignore_existing_files", false) {
            let _ = self.events.0.send(DownloadEvent::TrackSkipped {
                track_id: track_id.to_string(),
                name: track_info.name.clone(),
                location: dest.clone(),
            });
            return Ok(dest);
        }

        // 6. Module-supplied download URL or temp file.
        let _ = self.events.0.send(DownloadEvent::TrackStarted {
            service: service.to_string(),
            track_id: track_id.to_string(),
            name: track_info.name.clone(),
        });
        let download = module
            .get_track_download(track_id, quality, &codec_options, data)
            .await?;

        // A module may report the bytes are in a different container than the
        // negotiated codec (e.g. Tidal Atmos remuxed to M4A/FLAC). Re-resolve
        // the extension like Python's `override_codec` and re-check existence.
        if let Some(actual) = download.different_codec {
            let eff = extension_for_codec(actual);
            if eff != extension {
                extension = eff;
                dest.set_extension(eff);
                if dest.exists() && !globals.get_bool_or("advanced", "ignore_existing_files", false)
                {
                    let _ = self.events.0.send(DownloadEvent::TrackSkipped {
                        track_id: track_id.to_string(),
                        name: track_info.name.clone(),
                        location: dest.clone(),
                    });
                    return Ok(dest);
                }
            }
        }

        let bytes = match download.download_type {
            DownloadSource::Url => {
                let url = download.file_url.ok_or_else(|| {
                    Error::Download("module returned URL but no file_url".to_string())
                })?;
                // Magnet links (torrent index providers) go through the
                // BitTorrent engine, not the hoster resolver.
                let torrent_bytes = 'torrent: {
                    if !crate::torrent::is_torrent(&url) {
                        break 'torrent None;
                    }
                    let settings = crate::torrent::TorrentSettings::from_globals(&globals);
                    let dir = dest
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.output_path.clone());
                    let files = crate::torrent::download_magnet(
                        &url,
                        &dir,
                        &settings,
                        Some(self.events.0.clone()),
                        &track_info.name,
                        self.picaro.current_quality().contains(picaro_utils::models::Quality::LOSSLESS),
                    )
                    .await?;
 match crate::mega::pick_best_file(&files, &track_info.name) {
 Some(chosen) => {
 let chosen = chosen.to_path_buf();
 let len = tokio::fs::metadata(&chosen).await?.len();
 // See `download_track`: reject magnet grabs
 // that don't match a known-good reference.
 let dirs = crate::fingerprint::default_search_dirs();
 if let Err(e) = crate::fingerprint::verify_against_reference(
 &chosen,
 &track_info.name,
 &dirs,
 ) {
 let _ = tokio::fs::remove_file(&chosen).await;
 return Err(e);
 }
 dest = chosen;
 Some(len)
 }
 None => None,
 }
 };
 if let Some(bytes) = torrent_bytes {
 bytes
 } else {
 // Keyless MEGA public links are fetched via the `mega` crate
 // rather than the generic hoster resolver.
 let mega_bytes = 'mega: {
                    if !crate::mega::is_mega(&url) {
                        break 'mega None;
                    }
                    let dir = dest
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.output_path.clone());
                    let files = crate::mega::download_mega(&url, &dir).await?;
                    match crate::mega::pick_best_file(&files, &track_info.name) {
                        Some(chosen) => {
                            let chosen = chosen.to_path_buf();
                            let len = tokio::fs::metadata(&chosen).await?.len();
                            dest = chosen;
                            Some(len)
                        }
                        None => None,
                    }
                };
                if let Some(bytes) = mega_bytes {
                    bytes
                } else {
                    let mut headers = reqwest::header::HeaderMap::new();
                    for (k, v) in download.file_url_headers.iter() {
                        if let (Ok(name), Ok(value)) = (
                            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                            reqwest::header::HeaderValue::from_str(v.as_str().unwrap_or("")),
                        ) {
                            headers.insert(name, value);
                        }
                    }
                    let client = reqwest::Client::new();
                    // Band/blog modules often hand us a file-hoster landing page.
                    // Resolve it to a direct URL; fall back to the original.
                    let referer = headers
                        .get(reqwest::header::REFERER)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let resolved = crate::hosters::resolve(&client, &url, &referer).await;
                    // A resolved direct link must not carry the module's foreign
                    // Referer (some hosts, e.g. Yandex Disk, 403 on it).
                    let headers = if resolved.is_some() {
                        reqwest::header::HeaderMap::new()
                    } else {
                        headers
                    };
                    let url = resolved.unwrap_or(url);
                    download_to_path(
                        &client,
                        &url,
                        &dest,
                        Some(headers),
                        DownloadProgress::reporting(
                            self.events.0.clone(),
                            track_id.to_string(),
                            track_info.name.clone(),
                        ),
                    )
                    .await?
                }
                }
            }
            DownloadSource::TempFilePath | DownloadSource::Mpd => {
                // Non-URL modules stage the bytes themselves and hand us a
                // temp path. MPD manifests are resolved module-side; Python
                // likewise `shutil.move`s every non-URL result instead of
                // erroring, so do the same here.
                let temp = download.temp_file_path.ok_or_else(|| {
                    Error::Download("module returned temp path but none set".to_string())
                })?;
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                if tokio::fs::rename(&temp, &dest).await.is_err() {
                    // Cross-device - copy + remove
                    tokio::fs::copy(&temp, &dest).await?;
                    let _ = tokio::fs::remove_file(&temp).await;
                }
                let bytes = tokio::fs::metadata(&dest).await?.len();
                bytes
            }
        };

        // Drop <100KB results as corrupted-at-source, like Python does.
        check_min_size(&dest, bytes).await?;
        if !picaro_utils::safety::looks_like_audio(&dest) {
            // A rejected download must clean up after itself - this path
            // once left a 344MB archive mislabeled as .flac on disk.
            let _ = std::fs::remove_file(&dest);
            return Err(Error::Download(format!(
                "rejected non-audio download (bad source?): {}",
                dest.display()
            )));
        }
        if let Some(reason) = picaro_utils::safety::danger_reason(&dest) {
            let _ = std::fs::remove_file(&dest);
            return Err(Error::Download(format!(
                "rejected unsafe download ({reason}): {}",
                dest.display()
            )));
        }

        // 7. Tag the file. Use the on-disk extension when present (a MEGA
        // node may carry a different container than the negotiated codec).
        let container = dest
            .extension()
            .and_then(|e| e.to_str())
            .map(container_for_extension)
            .unwrap_or_else(|| container_for_extension(extension));
        let meta_sep = globals.get_str_or("formatting", "metadata_separator", ";");
        let split_meta = globals.get_bool_or("formatting", "split_metadata", true);
        let mut tagger = Tagger::new(&track_info, container)
            .with_credits(&credits)
            .with_path(&dest)
            .with_separator(&meta_sep)
            .with_split(split_meta);
        if let Some(ly) = embedded_lyrics(&globals, &track_info) {
            tagger = tagger.with_lyrics(ly);
        }
        if let Some(c) = &cover_path {
            tagger = tagger.with_image(c);
        }
        if let Err(e) = tagger.write() {
            self.log_warn(format!("tagging failed: {e}"));
        }
        // Temp covers are staged per track; never leak them.
        if let Some(c) = &cover_path {
            let _ = std::fs::remove_file(c);
        }

        // 8. Embed external cover if requested
        if globals.get_bool_or("covers", "save_external", false) {
            if let Some(c) = &cover_path {
                let ext = globals.get_str_or("covers", "external_format", "png");
                let res = globals.get_int_or("covers", "external_resolution", 3000) as u32;
                let external_path = dest.with_extension(ext);
                let _ = tokio::fs::copy(c, &external_path).await;
                let _ = res;
            }
        }

        // 9. Save synced lyrics if requested
        if globals.get_bool_or("lyrics", "save_synced_lyrics", false) {
            if let Some(synced) = &track_info.synced_lyrics {
                let lrc_path = dest.with_extension("lrc");
                if let Ok(mut f) = tokio::fs::File::create(&lrc_path).await {
                    let _ = f.write_all(synced.as_bytes()).await;
                }
            }
        }

        let dest = self.maybe_convert(dest, &globals).await;

        let _ = self.events.0.send(DownloadEvent::TrackSucceeded {
            track_id: track_id.to_string(),
            name: track_info.name.clone(),
            location: dest.clone(),
            bytes,
        });
        Ok(dest)
    }

    /// Ask registered lyrics providers (LRCLIB, Lyrics.ovh, ...) for lyrics.
    async fn fetch_lyrics_from_providers(
        &self,
        track: &TrackInfo,
    ) -> Option<picaro_utils::models::LyricsInfo> {
        let artist = track.artists.first().cloned().unwrap_or_default();
        let mut data = HashMap::new();
        data.insert("__artist__".to_string(), Value::String(artist));
        data.insert(
            "__track_name__".to_string(),
            Value::String(track.name.clone()),
        );
        for name in self.picaro.list_modules() {
            let is_lyrics = self
                .picaro
                .registry()
                .get(&name)
                .map(|m| {
                    m.information
                        .module_supported_modes
                        .contains(ModuleModes::lyrics)
                })
                .unwrap_or(false);
            if !is_lyrics {
                continue;
            }
            if let Ok(module) = self.picaro.load_module(&name).await {
                if let Ok(ly) = module.get_track_lyrics("", data.clone()).await {
                    if ly.embedded.is_some() || ly.synced.is_some() {
                        debug!("lyrics from provider {name}");
                        return Some(ly);
                    }
                }
            }
        }
        None
    }

    /// Optionally transcode a finished file to a target codec/bitrate via
    /// ffmpeg. Configured under `[conversion]`; off by default. Returns the
    /// (possibly new) path; on any failure the original file is kept.
    async fn maybe_convert(&self, dest: PathBuf, globals: &GlobalSettings) -> PathBuf {        if !globals.get_bool_or("conversion", "enabled", false) {
            return dest;
        }
        let codec = globals
            .get_str_or("conversion", "codec", "aac")
            .to_lowercase();
        let target_ext = match codec.as_str() {
            "aac" | "m4a" => "m4a",
            "mp3" => "mp3",
            "opus" => "opus",
            "ogg" | "vorbis" => "ogg",
            "flac" => "flac",
            "wav" => "wav",
            _ => {
                self.log_warn(format!("conversion: unknown codec '{codec}'; skipping"));
                return dest;
            }
        };
        let cur_ext = dest
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        if cur_ext == target_ext {
            return dest;
        }
        let kbps = globals.get_int_or("conversion", "bitrate_kbps", 0).max(0) as u32;
        let only_if_larger = globals.get_bool_or("conversion", "only_if_larger", true);
        let ffmpeg_pref = globals.get_str_or("advanced", "ffmpeg_path", "ffmpeg");
        let Some(ffmpeg) = picaro_utils::util::locate_ffmpeg(Some(ffmpeg_pref.as_str())) else {
            self.log_warn("conversion: ffmpeg not found; keeping original".to_string());
            return dest;
        };
        let encoder = match codec.as_str() {
            "aac" | "m4a" => "aac",
            "mp3" => "libmp3lame",
            "opus" => "libopus",
            "ogg" | "vorbis" => "libvorbis",
            "flac" => "flac",
            _ => "aac",
        };
        let out = dest.with_extension(target_ext);
        let mut cmd = tokio::process::Command::new(&ffmpeg);
        cmd.arg("-y")
            .arg("-i")
            .arg(&dest)
            .arg("-map_metadata")
            .arg("0")
            .arg("-vn")
            .arg("-c:a")
            .arg(encoder);
        if kbps > 0 && codec != "flac" {
            cmd.arg("-b:a").arg(format!("{kbps}k"));
        }
        cmd.arg(&out);
        match cmd.status().await {
            Ok(s) if s.success() => {}
            _ => {
                let _ = tokio::fs::remove_file(&out).await;
                self.log_warn(format!(
                    "conversion: ffmpeg failed; keeping {}",
                    dest.display()
                ));
                return dest;
            }
        }
        // Only keep the transcode if it actually reduced the size.
        if only_if_larger {
            let so = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(u64::MAX);
            let sd = std::fs::metadata(&dest).map(|m| m.len()).unwrap_or(0);
            if so >= sd {
                let _ = tokio::fs::remove_file(&out).await;
                return dest;
            }
        }
        let _ = tokio::fs::remove_file(&dest).await;
        out
    }

    async fn download_track_cover(
        &self,
        module: &Arc<dyn picaro_utils::module::ModuleInterface>,
        track: &TrackInfo,
        globals: &GlobalSettings,
    ) -> Result<PathBuf> {
        let fetch = globals.get_bool_or("metadata", "fetch_cover", true)
            && globals.get_bool_or("covers", "embed_cover", true);
        if !fetch {
            return Err(Error::Other("covers disabled".into()));
        }
        let file_type = globals
            .get("covers", "external_format")
            .and_then(|v| v.as_str())
            .map(|s| match s {
                "png" => ImageFileType::Png,
                "webp" => ImageFileType::Webp,
                _ => ImageFileType::Jpg,
            })
            .unwrap_or(ImageFileType::Jpg);
        let resolution = globals.get_int_or("covers", "main_resolution", 1400) as u32;
        let compression = globals
            .get("covers", "main_compression")
            .and_then(|v| v.as_str())
            .map(|s| match s {
                "low" => CoverCompression::Low,
                _ => CoverCompression::High,
            })
            .unwrap_or(CoverCompression::High);
        let opts = CoverOptions {
            file_type,
            resolution,
            compression,
        };
        // Prefer the cover already resolved during metadata fill (e.g. Deezer);
        // only ask the module when the track has none.
        let (cover_url, cover_type) = if !track.cover_url.trim().is_empty() {
            (track.cover_url.clone(), file_type)
        } else {
            let info = module
                .get_track_cover(
                    track.id.as_deref().unwrap_or(""),
                    &opts,
                    track.cover_extra_kwargs.clone().into_iter().collect(),
                )
                .await?;
            (info.url, info.file_type)
        };
        if cover_url.is_empty() {
            return Err(Error::Other("empty cover url".into()));
        }
        let temp = crate::http::create_temp_filename_with_ext(cover_type.extension());
        let client = reqwest::Client::new();
        let _ = download_to_path(
            &client,
            &cover_url,
            &temp,
            None,
            DownloadProgress::hidden(),
        )
        .await?;
        // Cap absurdly large covers like Python (16MB / 3000px). The resizer
        // persists to a new temp path, so drop the original when replaced.
        match resize_cover_if_needed(&temp, 16 * 1024 * 1024) {
            Ok(p) if p != temp => {
                let _ = std::fs::remove_file(&temp);
                Ok(p)
            }
            Ok(_) => Ok(temp),
            Err(_) => Ok(temp),
        }
    }

    /// Download an entire album.
    pub async fn download_album(&self, service: &str, album_id: &str) -> Result<Vec<PathBuf>> {
        let module = self.picaro.load_module(service).await?;
        let globals = self.globals();
        let album = module.get_album_info(album_id, HashMap::new()).await?;
        let album_path = build_album_path(&globals, &self.output_path, &album)?;
        tokio::fs::create_dir_all(&album_path).await?;
        let _ = self.events.0.send(DownloadEvent::Started {
            service: service.to_string(),
            context: format!("album: {}", album.name),
        });
        let mut paths = Vec::new();
        let mut succeeded = 0u32;
        let mut skipped = 0u32;
        let mut failed = 0u32;
        // A release whose album resolves to a single download id is usually one
        // archive holding every track (e.g. CoreRadio's per-album 7z), so keep
        // everything it extracts rather than just the first song.
        let single_archive = album.tracks.len() == 1;
        // Idempotency: a re-run against a release that is already fully on
        // disk must not fetch the (often several-hundred-MB) bundle again.
        // Completeness signal, best first: the sidecar written after a real
        // bundle download (exact), the module's expected track count, or -
        // for per-track releases - the track list length. If the album
        // folder already holds every track, report those files and stop -
        // same events as a real run so a UI's item accounting completes
        // normally.
        if !globals.get_bool_or("advanced", "ignore_existing_files", false) {
            let sidecar = album_path.join(".picaro-release.json");
            let sidecar_tracks = read_release_sidecar(&sidecar);
            let expected = match sidecar_tracks {
                Some(n) => Some(n),
                None => match album.expected_track_count.filter(|n| *n > 0) {
                    Some(n) => Some(n as usize),
                    None if !single_archive => Some(album.tracks.len()),
                    _ => None,
                },
            };
            if let Some(expected) = expected {
                let mut existing = Vec::new();
                collect_audio_files(&album_path, &mut existing);
                if existing.len() >= expected {
                    sort_tracks_naturally(&mut existing);
                    if sidecar_tracks.is_none() {
                        write_release_sidecar(&album_path, service, &album, existing.len());
                    }
                    for f in &existing {
                        let name = f
                            .file_stem()
                            .and_then(|s| s.to_str())
                            .unwrap_or_default()
                            .to_string();
                        let _ = self
                            .events
                            .0
                            .send(DownloadEvent::ItemStarted { name: name.clone() });
                        let _ = self.events.0.send(DownloadEvent::ItemDone {
                            name,
                            location: f.clone(),
                        });
                    }
                    let _ = self.events.0.send(DownloadEvent::Finished {
                        service: service.to_string(),
                        succeeded: existing.len() as u32,
                        skipped: 0,
                        failed: 0,
                    });
                    return Ok(existing);
                }
            }
        }
        // Album context for modules whose album "track" is really the
        // release bundle: names the bundle after the album (progress and
        // event lines read the album name, not a placeholder) and gives
        // the module artist/year/cover metadata.
        let mut album_data: HashMap<String, serde_json::Value> = HashMap::new();
        let mut meta = serde_json::Map::new();
        meta.insert(
            "album".to_string(),
            serde_json::Value::String(album.name.clone()),
        );
        meta.insert(
            "artist".to_string(),
            serde_json::Value::String(album.artist.clone()),
        );
        if let Some(cover) = &album.cover_url {
            meta.insert(
                "cover".to_string(),
                serde_json::Value::String(cover.clone()),
            );
        }
        meta.insert(
            "year".to_string(),
            serde_json::Value::from(album.release_year),
        );
        album_data.insert(
            "__album_meta__".to_string(),
            serde_json::Value::Object(meta),
        );
        if single_archive {
            album_data.insert(
                "__track_name__".to_string(),
                serde_json::Value::String(album.name.clone()),
            );
        }
        for (idx, track) in album.tracks.iter().enumerate() {
            let id = track.id();
            let item_name = match track {
                TrackRef::Full(f) => f.name.clone(),
                TrackRef::Id(_) => {
                    if single_archive {
                        album.name.clone()
                    } else {
                        format!("{} track {}", album.name, idx + 1)
                    }
                }
            };
            let _ = self.events.0.send(DownloadEvent::ItemStarted {
                name: item_name.clone(),
            });
            // Build a per-track filename inside the album folder
            let global_album = self.globals();
            match self
                .download_track_into(
                    service,
                    id,
                    Some(&album_path),
                    &global_album,
                    album_data.clone(),
                )
                .await
            {
                Ok(p) => {
                    succeeded += 1;
                    if single_archive {
                        let dir = p
                            .parent()
                            .map(Path::to_path_buf)
                            .unwrap_or_else(|| album_path.clone());
                        let mut found = Vec::new();
                        collect_audio_files(&dir, &mut found);
                        if found.len() > 1 {
                            sort_tracks_naturally(&mut found);
                            for f in &found {
                                let name = f
                                    .file_stem()
                                    .and_then(|s| s.to_str())
                                    .unwrap_or_default()
                                    .to_string();
                                let _ = self.events.0.send(DownloadEvent::ItemDone {
                                    name,
                                    location: f.clone(),
                                });
                            }
                            paths.extend(found);
                            continue;
                        }
                    }
                    let done_name = p
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or(&item_name)
                        .to_string();
                    let _ = self.events.0.send(DownloadEvent::ItemDone {
                        name: done_name,
                        location: p.clone(),
                    });
                    paths.push(p);
                }
                Err(Error::Download(s))
                    if s.contains("already exists") || s.contains("Not available") =>
                {
                    skipped += 1;
                }
                Err(e) => {
                    failed += 1;
                    warn!("track {id} failed: {e}");
                    let _ = self.events.0.send(DownloadEvent::TrackFailed {
                        track_id: id.to_string(),
                        name: id.to_string(),
                        reason: simplify_error_message(&e.to_string()),
                    });
                }
            }
        }
        let _ = self.events.0.send(DownloadEvent::Finished {
            service: service.to_string(),
            succeeded,
            skipped,
            failed,
        });
        // A completed bundle download knows the release's real file count -
        // record it so the next run can skip the bundle without trusting
        // page-derived tracklists.
        if single_archive && succeeded > 0 {
            let mut all = Vec::new();
            collect_audio_files(&album_path, &mut all);
            write_release_sidecar(&album_path, service, &album, all.len());
        }
        // Tracks land in release order so a UI can report them as an
        // ordered list ("01. ..." before "02. ...", "10." after "09.").
        sort_tracks_naturally(&mut paths);
        Ok(paths)
    }

    /// Download one track out of a BUNDLE release (an album whose module
    /// reports a single download id that is really a 7z/zip/rar holding
    /// every track). Downloads the bundle once, extracts it, picks the
    /// track at 1-based `pos` in natural release order, verifies it
    /// against `expected_secs` when known, names and tags it, and DELETES
    /// the archive and every other extracted file afterwards.
    pub async fn download_track_from_bundle(
        &self,
        service: &str,
        info: &AlbumInfo,
        pos: u32,
        expected_secs: Option<u64>,
    ) -> Result<PathBuf> {
        let module = self.picaro.load_module(service).await?;
        let globals = self.globals();
        let quality = self.picaro.current_quality();
        let codec_options = self.picaro.codec_options();
        let Some(bundle_id) = info.tracks.first().map(|t| t.id().to_string()) else {
            return Err(Error::Download("release has no download id".into()));
        };
        let bundle_name = info.name.clone();
        let _ = self.events.0.send(DownloadEvent::TrackStarted {
            service: service.to_string(),
            track_id: bundle_id.clone(),
            name: bundle_name.clone(),
        });
        let download = module
            .get_track_download(&bundle_id, quality, &codec_options, HashMap::new())
            .await?;
        // Stage the bundle under temp/ - a rejected bundle must never sit
        // in the download dir with a plausible name.
        let staged = match download.download_type {
            DownloadSource::Url => {
                let url = download.file_url.ok_or_else(|| {
                    Error::Download("module returned URL but no file_url".to_string())
                })?;
                let staged = crate::http::create_temp_filename_with_ext("bundle");
                let mut headers = reqwest::header::HeaderMap::new();
                for (k, v) in download.file_url_headers.iter() {
                    if let (Ok(name), Ok(value)) = (
                        reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                        reqwest::header::HeaderValue::from_str(v.as_str().unwrap_or("")),
                    ) {
                        headers.insert(name, value);
                    }
                }
                let client = reqwest::Client::new();
                let referer = headers
                    .get(reqwest::header::REFERER)
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let resolved = crate::hosters::resolve(&client, &url, &referer).await;
                let headers = if resolved.is_some() {
                    reqwest::header::HeaderMap::new()
                } else {
                    headers
                };
                let url = resolved.unwrap_or(url);
                download_to_path(
                    &client,
                    &url,
                    &staged,
                    Some(headers),
                    DownloadProgress::reporting(
                        self.events.0.clone(),
                        bundle_id.clone(),
                        bundle_name.clone(),
                    ),
                )
                .await?;
                staged
            }
            // Module staged the bundle itself; use it directly.
            DownloadSource::TempFilePath | DownloadSource::Mpd => download
                .temp_file_path
                .ok_or_else(|| Error::Download("module returned temp path but none set".to_string()))?,
        };
        // Cleanup closure: bundle + extraction dir.
        let work_dir = staged.with_extension("extracted");
        let cleanup = |staged: &Path, work: &Path| {
            let _ = std::fs::remove_file(staged);
            let _ = std::fs::remove_dir_all(work);
        };
        let Some(kind) = detect_archive(&staged) else {
            // Not an archive: a genuine single-file release. Treat the
            // staged file as the chosen track.
            if !picaro_utils::safety::looks_like_audio(&staged) {
                let msg = format!(
                    "rejected non-audio download (bad source?): {}",
                    staged.display()
                );
                cleanup(&staged, &work_dir);
                return Err(Error::Download(msg));
            }
            // A release known to hold several tracks but served as ONE
            // audio file is the whole album in a single rip, not a
            // track `pos` inside it (coreradio's "(FLAC)" link is really
            // a full-release MP3 — the complete-album incident fetched
            // it twelve times over and delivered it raw each time).
            // Reject so the source fallback can try a per-track source.
            // Modules without an expected count (DLE blogs) are caught
            // by the duration probe instead: a >25-minute "track" from
            // a multi-file release is the rip, not a song in it.
            let whole_rip = info.expected_track_count.map_or(false, |n| n > 1)
                || crate::fingerprint::probe_duration_secs(&staged)
                    .map_or(false, |s| s > 1500.0);
            if whole_rip {
                let msg = format!(
                    "{} serves the whole release as one file{} \u{2014} not fetchable per-track",
                    info.name,
                    info.expected_track_count
                        .map(|n| format!(" ({} tracks)", n))
                        .unwrap_or_default()
                );
                cleanup(&staged, &work_dir);
                return Err(Error::Download(msg));
            }
            let result = self
                .finish_bundle_track(
                    &staged, info, pos, expected_secs, &globals, &module, None,
                )
                .await;
            if result.is_err() {
                cleanup(&staged, &work_dir);
            } else {
                let _ = std::fs::remove_file(&staged);
            }
            return result;
        };
        extract_archive_of_kind(&staged, kind, &work_dir).await?;
        postprocess_extraction(&work_dir);
        let _ = std::fs::remove_file(&staged);
        let mut audio = Vec::new();
        collect_audio_files(&work_dir, &mut audio);
        sort_tracks_naturally(&mut audio);
        let Some(chosen) = audio.get((pos.max(1) as usize).saturating_sub(1)).cloned() else {
            let msg = format!(
                "{} extracted {} track(s) (position {pos} requested)",
                info.name,
                audio.len()
            );
            cleanup(&staged, &work_dir);
            return Err(Error::Download(msg));
        };
        let rest: Vec<PathBuf> = audio.into_iter().filter(|p| *p != chosen).collect();
        let result = self
            .finish_bundle_track(&chosen, info, pos, expected_secs, &globals, &module, Some(&rest))
            .await;
        if result.is_err() {
            cleanup(&staged, &work_dir);
        } else {
            // Success: drop the archive and the other extracted files.
            let _ = std::fs::remove_file(&staged);
            let _ = std::fs::remove_dir_all(&work_dir);
        }
        result
    }

    /// Shared tail of `download_track_from_bundle`: verify duration,
    /// build the library filename, tag, and move `chosen` into place.
    /// `rest` (when present) is deleted only on success.
    async fn finish_bundle_track(
        &self,
        chosen: &Path,
        info: &AlbumInfo,
        pos: u32,
        expected_secs: Option<u64>,
        globals: &GlobalSettings,
        module: &Arc<dyn picaro_utils::module::ModuleInterface>,
        rest: Option<&[PathBuf]>,
    ) -> Result<PathBuf> {
        // Duration fingerprint (W2): a wildly-off duration means the
        // source served the wrong song - reject and let the caller clean up.
        if let Some(exp) = expected_secs {
            crate::fingerprint::verify_expected_duration(chosen, exp)?;
        }
        let ext = chosen
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("flac")
            .to_ascii_lowercase();
        let codec = match ext.as_str() {
            "flac" => CodecFlags::FLAC,
            "mp3" => CodecFlags::MP3,
            "m4a" | "mp4" => CodecFlags::AAC,
            "ogg" | "oga" => CodecFlags::VORBIS,
            "opus" => CodecFlags::OPUS,
            "wav" => CodecFlags::WAV,
            _ => CodecFlags::FLAC,
        };
        // "02. questioning" -> "questioning"
        let raw_stem = chosen
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        let title = raw_stem
            .trim_start_matches(|c: char| c.is_ascii_digit())
            .trim_start_matches(['.', ')', ' ', '-'])
            .trim();
        let title = if title.is_empty() {
            raw_stem.clone()
        } else {
            title.to_string()
        };
        let mut track_info = TrackInfo {
            name: title.to_string(),
            album: info.name.clone(),
            artists: vec![info.artist.clone()],
            tags: Tags {
                track_number: Some(pos),
                disc_number: Some(1),
                release_date: (info.release_year > 0)
                    .then(|| format!("{}-01-01", info.release_year)),
                ..Default::default()
            },
            codec,
            release_year: info.release_year,
            cover_url: info.cover_url.clone().unwrap_or_default(),
            ..Default::default()
        };
        if globals.get_bool_or("metadata", "fill_misc", true) {
            let client = reqwest::Client::new();
            let _ = picaro_utils::metadata_fill::fill_track_metadata(&client, &mut track_info)
                .await;
        }
        let cover_path = self
            .download_track_cover(module, &track_info, globals)
            .await
            .ok();
        let filename = build_track_filename(globals, &track_info, &ext, true)?;
        let dest = self.output_path.join(&filename);
        if dest.exists() && !globals.get_bool_or("advanced", "ignore_existing_files", false) {
            let _ = self.events.0.send(DownloadEvent::TrackSkipped {
                track_id: info.id.clone().unwrap_or_default(),
                name: track_info.name.clone(),
                location: dest.clone(),
            });
            return Ok(dest);
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }
        if tokio::fs::rename(chosen, &dest).await.is_err() {
            tokio::fs::copy(chosen, &dest).await?;
            let _ = tokio::fs::remove_file(chosen).await;
        }
        let container = container_for_extension(&ext);
        let meta_sep = globals.get_str_or("formatting", "metadata_separator", ";");
        let split_meta = globals.get_bool_or("formatting", "split_metadata", true);
        let mut tagger = Tagger::new(&track_info, container)
            .with_path(&dest)
            .with_separator(&meta_sep)
            .with_split(split_meta);
        if let Some(c) = &cover_path {
            tagger = tagger.with_image(c);
        }
        if let Err(e) = tagger.write() {
            self.log_warn(format!("tagging failed: {e}"));
        }
        if let Some(c) = &cover_path {
            let _ = std::fs::remove_file(c);
        }
        if let Some(rest) = rest {
            for p in rest {
                let _ = std::fs::remove_file(p);
            }
        }
        let bytes = tokio::fs::metadata(&dest).await.map(|m| m.len()).unwrap_or(0);
        let _ = self.events.0.send(DownloadEvent::TrackSucceeded {
            track_id: info.id.clone().unwrap_or_default(),
            name: track_info.name.clone(),
            location: dest.clone(),
            bytes,
        });
        Ok(dest)
    }

    async fn download_track_into(
        &self,
        service: &str,
        track_id: &str,
        album_path: Option<&Path>,
        globals: &GlobalSettings,
        data: HashMap<String, serde_json::Value>,
    ) -> Result<PathBuf> {
        // Almost identical to download_track but with the option to use the
        // album-supplied track metadata so the on-disk filename lines up with
        // the album folder name.
        let module = self.picaro.load_module(service).await?;
        let quality = self.picaro.current_quality();
        let codec_options = self.picaro.codec_options();
        let mut track_info = module
            .get_track_info(track_id, quality, &codec_options, data)
            .await?;
        if let Some(err) = &track_info.error {
            return Err(Error::Download(err.clone()));
        }
        // Backfill missing metadata from keyless sources.
        if globals.get_bool_or("metadata", "fill_misc", true) {
            let client = reqwest::Client::new();
            let filled =
                picaro_utils::metadata_fill::fill_track_metadata(&client, &mut track_info).await;
            if !filled.is_empty() {
                self.log_info(format!("metadata fill: {}", filled.join(", ")));
            }
            if is_low_trust_source(service) {
                let forced = picaro_utils::metadata_fill::fill_track_metadata_force(
                    &client,
                    &mut track_info,
                )
                .await;
                if !forced.is_empty() {
                    self.log_info(format!("metadata override: {}", forced.join(", ")));
                }
            }
        }
        let cover_path = self
            .download_track_cover(&module, &track_info, globals)
            .await
            .ok();
        if globals.get_bool_or("metadata", "fetch_lyrics", true) {
            if let Ok(lyrics) = module
                .get_track_lyrics(
                    track_id,
                    track_info.lyrics_extra_kwargs.clone().into_iter().collect(),
                )
                .await
            {
                if let Some(l) = lyrics.embedded {
                    track_info.lyrics = Some(l);
                }
                if let Some(s) = lyrics.synced {
                    track_info.synced_lyrics = Some(s);
                }
            }
            if track_info.lyrics.is_none() && track_info.synced_lyrics.is_none() {
                if let Some(l) = self.fetch_lyrics_from_providers(&track_info).await {
                    if let Some(t) = l.embedded {
                        track_info.lyrics = Some(t);
                    }
                    if let Some(s) = l.synced {
                        track_info.synced_lyrics = Some(s);
                    }
                }
            }
        }
        let credits = module
            .get_track_credits(
                track_id,
                track_info
                    .credits_extra_kwargs
                    .clone()
                    .into_iter()
                    .collect(),
            )
            .await
            .unwrap_or_default();
        track_info.credits_list = credits.clone();

        let mut extension = extension_for_codec(track_info.codec);
        let filename = build_track_filename(globals, &track_info, extension, false)?;
        let mut dest = match album_path {
            Some(p) => p.join(&filename),
            None => self.output_path.join(&filename),
        };
        if dest.exists() && !globals.get_bool_or("advanced", "ignore_existing_files", false) {
            let _ = self.events.0.send(DownloadEvent::TrackSkipped {
                track_id: track_id.to_string(),
                name: track_info.name.clone(),
                location: dest.clone(),
            });
            return Ok(dest);
        }
        let _ = self.events.0.send(DownloadEvent::TrackStarted {
            service: service.to_string(),
            track_id: track_id.to_string(),
            name: track_info.name.clone(),
        });
        let download = module
            .get_track_download(track_id, quality, &codec_options, HashMap::new())
            .await?;
        // See `download_track`: honour a module-reported container override.
        if let Some(actual) = download.different_codec {
            let eff = extension_for_codec(actual);
            if eff != extension {
                extension = eff;
                dest.set_extension(eff);
                if dest.exists() && !globals.get_bool_or("advanced", "ignore_existing_files", false)
                {
                    let _ = self.events.0.send(DownloadEvent::TrackSkipped {
                        track_id: track_id.to_string(),
                        name: track_info.name.clone(),
                        location: dest.clone(),
                    });
                    return Ok(dest);
                }
            }
        }
        let bytes = match download.download_type {
            DownloadSource::Url => {
                let url = download.file_url.ok_or_else(|| {
                    Error::Download("module returned URL but no file_url".to_string())
                })?;
                // Magnet links (torrent index providers) go through the
                // BitTorrent engine, not the hoster resolver.
                let torrent_bytes = 'torrent: {
                    if !crate::torrent::is_torrent(&url) {
                        break 'torrent None;
                    }
                    let settings = crate::torrent::TorrentSettings::from_globals(&globals);
                    let dir = dest
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.output_path.clone());
                    let files = crate::torrent::download_magnet(
                        &url,
                        &dir,
                        &settings,
                        Some(self.events.0.clone()),
                        &track_info.name,
                        self.picaro.current_quality().contains(picaro_utils::models::Quality::LOSSLESS),
                    )
                    .await?;
 match crate::mega::pick_best_file(&files, &track_info.name) {
 Some(chosen) => {
 let chosen = chosen.to_path_buf();
 let len = tokio::fs::metadata(&chosen).await?.len();
 // See `download_track`: reject magnet grabs
 // that don't match a known-good reference.
 let dirs = crate::fingerprint::default_search_dirs();
 if let Err(e) = crate::fingerprint::verify_against_reference(
 &chosen,
 &track_info.name,
 &dirs,
 ) {
 let _ = tokio::fs::remove_file(&chosen).await;
 return Err(e);
 }
 dest = chosen;
 Some(len)
 }
 None => None,
 }
 };
 if let Some(bytes) = torrent_bytes {
 bytes
 } else {
 // Keyless MEGA public links are fetched via the `mega` crate
 // rather than the generic hoster resolver.
 let mega_bytes = 'mega: {
                    if !crate::mega::is_mega(&url) {
                        break 'mega None;
                    }
                    let dir = dest
                        .parent()
                        .map(Path::to_path_buf)
                        .unwrap_or_else(|| self.output_path.clone());
                    let files = crate::mega::download_mega(&url, &dir).await?;
                    match crate::mega::pick_best_file(&files, &track_info.name) {
                        Some(chosen) => {
                            let chosen = chosen.to_path_buf();
                            let len = tokio::fs::metadata(&chosen).await?.len();
                            dest = chosen;
                            Some(len)
                        }
                        None => None,
                    }
                };
                if let Some(bytes) = mega_bytes {
                    bytes
                } else {
                    let mut headers = reqwest::header::HeaderMap::new();
                    for (k, v) in download.file_url_headers.iter() {
                        if let (Ok(name), Ok(value)) = (
                            reqwest::header::HeaderName::from_bytes(k.as_bytes()),
                            reqwest::header::HeaderValue::from_str(v.as_str().unwrap_or("")),
                        ) {
                            headers.insert(name, value);
                        }
                    }
                    let client = reqwest::Client::new();
                    // See `download_track`: resolve hoster landing pages first.
                    let referer = headers
                        .get(reqwest::header::REFERER)
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("")
                        .to_string();
                    let resolved = crate::hosters::resolve(&client, &url, &referer).await;
                    // A resolved direct link must not carry the module's foreign
                    // Referer (some hosts, e.g. Yandex Disk, 403 on it).
                    let headers = if resolved.is_some() {
                        reqwest::header::HeaderMap::new()
                    } else {
                        headers
                    };
                    let url = resolved.unwrap_or(url);
                    download_to_path(
                        &client,
                        &url,
                        &dest,
                        Some(headers),
                        DownloadProgress::reporting(
                            self.events.0.clone(),
                            track_id.to_string(),
                            track_info.name.clone(),
                        ),
                    )
                    .await?
                }
                }
            }
            DownloadSource::TempFilePath | DownloadSource::Mpd => {
                // See `download_track`: every non-URL result is a staged temp
                // file, including module-resolved MPD manifests.
                let temp = download.temp_file_path.ok_or_else(|| {
                    Error::Download("module returned temp path but none set".to_string())
                })?;
                if let Some(parent) = dest.parent() {
                    tokio::fs::create_dir_all(parent).await?;
                }
                if tokio::fs::rename(&temp, &dest).await.is_err() {
                    // Cross-device - copy + remove
                    tokio::fs::copy(&temp, &dest).await?;
                    let _ = tokio::fs::remove_file(&temp).await;
                }
                tokio::fs::metadata(&dest).await?.len()
            }
        };

        // A release bundle (7z/zip/rar) holding every track: extract it,
        // expand nested archives, purge junk (cover art stays), flatten
        // the wrapper folder, and hand back the first audio file - the
        // album loop then keeps everything it extracted.
        if let Some(kind) = detect_archive(&dest) {
            let extract_to = dest
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.output_path.clone());
            // Stage the extraction under temp/ instead of unpacking
            // straight into the library folder: on a re-run the album
            // folder already holds the previous extraction's tracks, and
            // a direct unpack would re-create the bundle's wrapper folder
            // alongside (or nested inside) them. The merge below moves
            // only files that don't already exist.
            let staging = crate::http::create_temp_filename_with_ext("extracted");
            std::fs::create_dir_all(&staging)?;
            extract_archive_of_kind(&dest, kind, &staging).await?;
            postprocess_extraction(&staging);
            let _ = tokio::fs::remove_file(&dest).await;
            merge_extraction_into(&staging, &extract_to);
            let _ = std::fs::remove_dir_all(&staging);
            if let Some(c) = &cover_path {
                let _ = std::fs::remove_file(c);
            }
            if let Some(p) = find_first_audio(&extract_to) {
                let _ = self.events.0.send(DownloadEvent::TrackSucceeded {
                    track_id: track_id.to_string(),
                    name: track_info.name.clone(),
                    location: p.clone(),
                    bytes,
                });
                return Ok(p);
            }
            return Ok(extract_to);
        }

        // Drop <100KB results as corrupted-at-source, like Python does.
        check_min_size(&dest, bytes).await?;
        if !picaro_utils::safety::looks_like_audio(&dest) {
            // Rejected downloads clean up after themselves.
            let _ = std::fs::remove_file(&dest);
            return Err(Error::Download(format!(
                "rejected non-audio download (bad source?): {}",
                dest.display()
            )));
        }
        // A track query must never be answered with the whole album in
        // one giant file (coreradio's "(FLAC)" search hit is a
        // full-release MP3 the free-text fallback kept on disk).
        // Longer than ~25 minutes is not a song; a probe we can't read
        // stays untouched so odd containers never false-block.
        if let Some(secs) = crate::fingerprint::probe_duration_secs(&dest) {
            if secs > 1500.0 {
                let _ = std::fs::remove_file(&dest);
                return Err(Error::Download(format!(
                    "rejected album-sized download ({:.0} min): {}",
                    secs / 60.0,
                    dest.display()
                )));
            }
        }
        if let Some(reason) = picaro_utils::safety::danger_reason(&dest) {
            let _ = std::fs::remove_file(&dest);
            return Err(Error::Download(format!(
                "rejected unsafe download ({reason}): {}",
                dest.display()
            )));
        }

        let container = dest
            .extension()
            .and_then(|e| e.to_str())
            .map(container_for_extension)
            .unwrap_or_else(|| container_for_extension(extension));
        let meta_sep = globals.get_str_or("formatting", "metadata_separator", ";");
        let split_meta = globals.get_bool_or("formatting", "split_metadata", true);
        let mut tagger = Tagger::new(&track_info, container)
            .with_credits(&credits)
            .with_path(&dest)
            .with_separator(&meta_sep)
            .with_split(split_meta);
        if let Some(ly) = embedded_lyrics(globals, &track_info) {
            tagger = tagger.with_lyrics(ly);
        }
        if let Some(c) = &cover_path {
            tagger = tagger.with_image(c);
        }
        if let Err(e) = tagger.write() {
            self.log_warn(format!("tagging failed: {e}"));
        }
        if let Some(c) = &cover_path {
            let _ = std::fs::remove_file(c);
        }

        let dest = self.maybe_convert(dest, globals).await;

        let _ = self.events.0.send(DownloadEvent::TrackSucceeded {
            track_id: track_id.to_string(),
            name: track_info.name.clone(),
            location: dest.clone(),
            bytes,
        });
        Ok(dest)
    }

    /// Download a playlist. Mirrors `download_playlist`.
    pub async fn download_playlist(
        &self,
        service: &str,
        playlist_id: &str,
    ) -> Result<Vec<PathBuf>> {
        let module = self.picaro.load_module(service).await?;
        let globals = self.globals();
        let playlist = module
            .get_playlist_info(playlist_id, HashMap::new())
            .await?;
        let playlist_path = build_playlist_path(&globals, &self.output_path, &playlist)?;
        tokio::fs::create_dir_all(&playlist_path).await?;

        // m3u file (extended header first, like Python)
        if globals.get_bool_or("playlist", "save_m3u", true) {
            let m3u_path = playlist_path.join(format!("{}.m3u", sanitise_name(&playlist.name)));
            let _ = tokio::fs::File::create(&m3u_path).await;
            if globals.get_bool_or("playlist", "extended_m3u", true) {
                if let Ok(mut f) = tokio::fs::OpenOptions::new()
                    .append(true)
                    .open(&m3u_path)
                    .await
                {
                    let _ = f.write_all(b"#EXTM3U\n\n").await;
                }
            }
        }

        let _ = self.events.0.send(DownloadEvent::Started {
            service: service.to_string(),
            context: format!("playlist: {}", playlist.name),
        });

        let mut succeeded = 0u32;
        let mut skipped = 0u32;
        let mut failed = 0u32;
        let mut paths = Vec::new();
        for (idx, track) in playlist.tracks.iter().enumerate() {
            let id = track.id();
            match self
                .download_track_into(service, id, Some(&playlist_path), &globals, HashMap::new())
                .await
            {
                Ok(p) => {
                    succeeded += 1;
                    paths.push(p.clone());
                    // m3u append (EXTINF + absolute/relative, like Python)
                    if globals.get_bool_or("playlist", "save_m3u", true) {
                        let m3u_path =
                            playlist_path.join(format!("{}.m3u", sanitise_name(&playlist.name)));
                        if let Ok(mut f) = tokio::fs::OpenOptions::new()
                            .append(true)
                            .create(true)
                            .open(&m3u_path)
                            .await
                        {
                            let line = path_for_m3u(&globals, &m3u_path, &p);
                            let extinf = m3u_extinf(playlist.tracks.get(idx));
                            let extended = globals.get_bool_or("playlist", "extended_m3u", true);
                            let _ = writeln_m3u(&mut f, &line, extinf.as_deref(), extended).await;
                        }
                    }
                }
                Err(Error::Download(s))
                    if s.contains("already exists") || s.contains("Not available") =>
                {
                    skipped += 1;
                }
                Err(e) => {
                    failed += 1;
                    warn!("track {id} failed: {e}");
                }
            }
        }
        let _ = self.events.0.send(DownloadEvent::Finished {
            service: service.to_string(),
            succeeded,
            skipped,
            failed,
        });
        Ok(paths)
    }

    /// Download an artist. For each album in the artist's catalogue, call
    /// `download_album`.
    pub async fn download_artist(&self, service: &str, artist_id: &str) -> Result<Vec<PathBuf>> {
        let module = self.picaro.load_module(service).await?;
        let globals = self.globals();
        let artist = module
            .get_artist_info(artist_id, true, None, HashMap::new())
            .await?;
        let mut all = Vec::new();
        for album in &artist.albums {
            let id = match album {
                Value::String(s) => s.clone(),
                Value::Object(o) => match o.get("id") {
                    Some(Value::String(s)) => s.clone(),
                    Some(Value::Number(n)) => n.to_string(),
                    _ => continue,
                },
                _ => continue,
            };
            match self.download_album(service, &id).await {
                Ok(p) => all.extend(p),
                Err(e) => warn!("artist album {id} failed: {e}"),
            }
        }
        Ok(all)
    }

    /// Download a `magnet:` link (BitTorrent) into a `torrents/` subfolder of
    /// the output path, returning the audio files the release produced.
    /// Honours the `[torrent]` settings block (disabled by default).
    pub async fn download_magnet_url(&self, magnet: &str) -> Result<Vec<PathBuf>> {
        let globals = self.globals();
        let settings = crate::torrent::TorrentSettings::from_globals(&globals);
        let dir = self.output_path.join("torrents");
        tokio::fs::create_dir_all(&dir).await?;
        let _ = self.events.0.send(DownloadEvent::Started {
            service: "torrent".to_string(),
            context: "magnet".to_string(),
        });
        let files = crate::torrent::download_magnet(
            magnet,
            &dir,
            &settings,
            Some(self.events.0.clone()),
            "magnet",
            false,
        )
        .await?;
        let _ = self.events.0.send(DownloadEvent::Finished {
            service: "torrent".to_string(),
            succeeded: files.len() as u32,
            skipped: 0,
            failed: 0,
        });
        Ok(files)
    }

    pub async fn search(
        &self,
        service: &str,
        query: &str,
        query_type: DownloadType,
    ) -> Result<Vec<SearchResult>> {
        let module = self.picaro.load_module(service).await?;
        module
            .search(
                query_type,
                query,
                None,
                self.picaro
                    .merged_globals
                    .get("general")
                    .and_then(|v| v.get("search_limit"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(25) as u32,
            )
            .await
    }
}

/// File extension for a codec. Mirrors Python's `codec_data` container map:
/// AC3 keeps its own container, MQA/error/unknown fall back to FLAC.
/// Sources whose own metadata comes from a filename / uploader and is not
/// trustworthy enough to keep (YouTube channels, Soulseek scene filenames,
/// direct-MP3 CDN site titles).
fn is_low_trust_source(service: &str) -> bool {
    matches!(
        service.to_ascii_lowercase().as_str(),
        "youtube" | "soulseek" | "zvu4it" | "tancpol" | "freemp3cloud"
    )
}

/// Clean up module-supplied track metadata before it is filled/tagged:
/// backfill anything the resolver knows (it scored the match) and drop
/// filename noise like an "Artist - " prefix or a trailing "(FLAC)".
fn normalize_track_metadata(
    info: &mut TrackInfo,
    data: &std::collections::HashMap<String, serde_json::Value>,
) {
    if info.name.trim().is_empty() {
        if let Some(n) = data.get("__track_name__").and_then(|v| v.as_str()) {
            if !n.trim().is_empty() {
                info.name = n.to_string();
            }
        }
    }
    if info.artists.iter().all(|a| a.trim().is_empty()) {
        if let Some(a) = data.get("__artist__").and_then(|v| v.as_str()) {
            if !a.trim().is_empty() {
                info.artists = vec![a.to_string()];
            }
        }
    }
    if info.album.trim().is_empty() {
        if let Some(al) = data.get("__album__").and_then(|v| v.as_str()) {
            if !al.trim().is_empty() {
                info.album = al.to_string();
            }
        }
    }
    if info.cover_url.trim().is_empty() {
        if let Some(c) = data.get("__cover__").and_then(|v| v.as_str()) {
            if !c.trim().is_empty() {
                info.cover_url = c.to_string();
            }
        }
    }

    let mut name = info.name.trim().to_string();
    for suffix in [
        " (FLAC)",
        " (flac)",
        " (MP3)",
        " (mp3)",
        " (320)",
        " (320kbps)",
        " (HQ)",
        " (Lossless)",
    ] {
        if let Some(stripped) = name.strip_suffix(suffix) {
            name = stripped.trim().to_string();
            break;
        }
    }
    // `<artist> - <title>` where the artist is already a separate field.
    if let Some(a) = info
        .artists
        .iter()
        .find(|a| !a.trim().is_empty())
        .map(|a| a.trim().to_string())
    {
        for sep in [" - ", " · ", " – ", " — ", " | "] {
            if let Some(rest) = name.strip_prefix(&format!("{a}{sep}")) {
                let rest = rest.trim();
                if !rest.is_empty() {
                    name = rest.to_string();
                }
                break;
            }
        }
    }
    info.name = name;
}

fn extension_for_codec(codec: CodecFlags) -> &'static str {
    match codec.pretty().to_lowercase().as_str() {
        "flac" => "flac",
        "mp3" => "mp3",
        "opus" => "opus",
        "vorbis" => "ogg",
        "aac-lc" | "he-aac" | "alac" | "mpeg-h 3d audio" | "e-ac-3 joc" | "ac-4 ims" => "m4a",
        "wave" => "wav",
        "aiff" => "aiff",
        "dolby digital" => "ac3",
        _ => "flac",
    }
}

/// Tagging container for a file extension. MP3/OGG/Opus files previously
/// fell through to `Flac`, writing the wrong tag format entirely.
fn container_for_extension(ext: &str) -> Container {
    match ext {
        "m4a" | "mp4" => Container::M4a,
        "mp3" => Container::Mp3,
        "ogg" => Container::Ogg,
        "opus" => Container::Opus,
        "wav" => Container::Wav,
        "aiff" => Container::Aiff,
        _ => Container::Flac,
    }
}

/// Lyrics to embed, honouring `lyrics.embed_lyrics` / `embed_synced_lyrics`.
/// Returns `None` when embedding is disabled or there is nothing to embed
/// (previously an empty-string tag was always written).
fn embedded_lyrics<'a>(globals: &GlobalSettings, track: &'a TrackInfo) -> Option<&'a str> {
    if !globals.get_bool_or("lyrics", "embed_lyrics", true) {
        return None;
    }
    let lyric = if globals.get_bool_or("lyrics", "embed_synced_lyrics", false) {
        track.synced_lyrics.as_deref().or(track.lyrics.as_deref())
    } else {
        track.lyrics.as_deref()
    };
    lyric.filter(|s| !s.is_empty())
}

/// Drop suspiciously small results as corrupted-at-source, like Python's
/// fixed 100KB threshold (the file is removed, not kept).
async fn check_min_size(dest: &Path, bytes: u64) -> Result<()> {
    const MIN_BYTES: u64 = 100 * 1024;
    if bytes < MIN_BYTES {
        let _ = tokio::fs::remove_file(dest).await;
        return Err(Error::Download(format!(
            "downloaded file suspiciously small ({bytes} bytes, expected >{MIN_BYTES} bytes); removed as likely corrupted"
        )));
    }
    Ok(())
}

/// `#EXTINF:<secs>, Artist - Title` for a playlist entry. Only available
/// when the playlist gave us full track metadata; otherwise the caller
/// writes a plain path line.
fn m3u_extinf(track: Option<&TrackRef>) -> Option<String> {
    let full = match track {
        Some(TrackRef::Full(t)) => t.as_ref(),
        _ => return None,
    };
    let dur = full
        .duration
        .map(|d| d.to_string())
        .unwrap_or_else(|| "-1".to_string());
    let artist = full.artists.first().cloned().unwrap_or_default();
    Some(format!(
        "#EXTINF:{dur}, {artist} - {name}",
        name = full.name
    ))
}

/// Absolute or m3u-relative path, per `playlist.paths_m3u`. The relative
/// branch previously wrote the absolute path; Python uses
/// `os.path.relpath` against the m3u's directory.
fn path_for_m3u(globals: &GlobalSettings, m3u_path: &Path, p: &Path) -> String {
    if globals.get_str_or("playlist", "paths_m3u", "absolute") == "absolute" {
        return std::fs::canonicalize(p)
            .ok()
            .map(|c| c.to_string_lossy().to_string())
            .unwrap_or_else(|| p.to_string_lossy().to_string());
    }
    if let Some(base) = m3u_path.parent() {
        if let Some(rel) = relative_path(p, base) {
            return rel.to_string_lossy().to_string();
        }
    }
    p.to_string_lossy().to_string()
}

/// Lexical `path` relative to `base` (no I/O, so it works for files that
/// were just created). Returns `None` when there is no shared prefix.
fn relative_path(path: &Path, base: &Path) -> Option<PathBuf> {
    use std::path::Component;
    let mut p_comps: Vec<Component> = path.components().collect();
    let b_comps: Vec<Component> = base.components().collect();
    // Strip common prefix (prefix/root must match exactly).
    let mut common = 0;
    while common < p_comps.len() && common < b_comps.len() && p_comps[common] == b_comps[common] {
        common += 1;
    }
    if common == 0 {
        return None;
    }
    // Abort when roots differ (e.g. different Windows drives).
    match (&p_comps[0], &b_comps[0]) {
        (Component::Prefix(a), Component::Prefix(b)) if a != b => return None,
        (Component::RootDir, Component::RootDir) => {}
        (Component::Prefix(_), Component::Prefix(_)) => {}
        _ if p_comps[0] != b_comps[0] => return None,
        _ => {}
    }
    let mut rel = PathBuf::new();
    for _ in common..b_comps.len() {
        rel.push("..");
    }
    for c in p_comps.drain(common..) {
        rel.push(c.as_os_str());
    }
    Some(rel)
}

async fn writeln_m3u(
    f: &mut tokio::fs::File,
    path_line: &str,
    extinf: Option<&str>,
    extended: bool,
) -> std::io::Result<()> {
    if extended {
        if let Some(e) = extinf {
            f.write_all(e.as_bytes()).await?;
            f.write_all(b"\n").await?;
        }
    }
    f.write_all(path_line.as_bytes()).await?;
    f.write_all(b"\n").await?;
    if extended {
        f.write_all(b"\n").await?;
    }
    Ok(())
}

/// Build a download queue from a list of CLI items and process it sequentially.
pub async fn download_queue(
    downloader: Arc<Downloader>,
    items: Vec<(String, String)>,
) -> Vec<DownloadEvent> {
    let mut out = Vec::new();
    for (service, id) in items {
        // dispatch by type heuristics
        let event = if id.starts_with("http") {
            // Try to parse URL
            match picaro_utils::url_decode::default_decode_url(&id, downloader.picaro.registry()) {
                Ok(mi) => {
                    let module = mi.media_type;
                    let s = format!("{:?}", module);
                    let _ = s;
                    DownloadEvent::Log {
                        level: LogLevel::Info,
                        message: format!("Downloading URL {id} via {service}"),
                    }
                }
                Err(e) => DownloadEvent::Error {
                    message: e.to_string(),
                },
            }
        } else {
            DownloadEvent::Log {
                level: LogLevel::Info,
                message: format!("Downloading {service} {id}"),
            }
        };
        out.push(event);
        // We assume id is a track id; for richer types (album/playlist/artist)
        // the CLI / TUI already dispatches via the dedicated methods above.
        if let Err(e) = downloader.download_track(&service, &id).await {
            out.push(DownloadEvent::Error {
                message: e.to_string(),
            });
        }
    }
    out
}

/// Transcode ONE existing local audio file to a lossy codec/bitrate via
/// ffmpeg — the `[conversion]` block's encoder table (`maybe_convert`) as a
/// standalone operation, for callers that need to shrink a file they already
/// own (the player's "redownload at a lower tier" arm reaches for this when
/// a track can't be fetched fresh). The result lands next to the input with
/// the target extension and is kept only when it actually shrank the audio;
/// the input is never modified.
pub async fn convert_local_file(
    input: &Path,
    codec: &str,
    kbps: u32,
) -> anyhow::Result<PathBuf> {
    if !input.is_file() {
        anyhow::bail!("no such file: {}", input.display());
    }
    let target_ext = match codec {
        "aac" | "m4a" => "m4a",
        "mp3" => "mp3",
        "opus" => "opus",
        "ogg" | "vorbis" => "ogg",
        _ => anyhow::bail!("unknown codec '{codec}'"),
    };
    let cur_ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase();
    if cur_ext == target_ext {
        anyhow::bail!("already {target_ext} — a same-codec pass gains nothing");
    }
    let out = input.with_extension(target_ext);
    if out.exists() {
        anyhow::bail!("target already exists: {}", out.display());
    }
    let Some(ffmpeg) = picaro_utils::util::locate_ffmpeg(None) else {
        anyhow::bail!("ffmpeg not found");
    };
    let encoder = match codec {
        "aac" | "m4a" => "aac",
        "mp3" => "libmp3lame",
        "opus" => "libopus",
        "ogg" | "vorbis" => "libvorbis",
        _ => "aac",
    };
    let mut cmd = tokio::process::Command::new(&ffmpeg);
    cmd.arg("-y")
        .arg("-i")
        .arg(input)
        .arg("-map_metadata")
        .arg("0")
        .arg("-vn")
        .arg("-c:a")
        .arg(encoder);
    if kbps > 0 {
        cmd.arg("-b:a").arg(format!("{kbps}k"));
    }
    cmd.arg(&out);
    match cmd.status().await {
        Ok(s) if s.success() => {}
        _ => {
            let _ = tokio::fs::remove_file(&out).await;
            anyhow::bail!("ffmpeg failed for {}", input.display());
        }
    }
    // Only keep the transcode if it actually reduced the size (same rule as
    // the download pipeline's `only_if_larger`): a 50MB FLAC converting to
    // a 50MB MP3 is a bug, not a save.
    let so = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(u64::MAX);
    let si = std::fs::metadata(input).map(|m| m.len()).unwrap_or(0);
    if so >= si {
        let _ = tokio::fs::remove_file(&out).await;
        anyhow::bail!("transcode wasn't smaller ({so}B >= {si}B) — keeping the original");
    }
    Ok(out)
}
