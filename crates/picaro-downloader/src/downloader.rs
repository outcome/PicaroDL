//! The main downloader - mirrors `picaro/music_downloader.py`.

use std::collections::HashMap;
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

async fn is_7z_archive(path: &Path) -> bool {
    match tokio::fs::read(path).await {
        Ok(bytes) if bytes.len() >= 6 => &bytes[0..6] == b"\x37\x7a\xbc\xaf\x27\x1c",
        _ => false,
    }
}

fn is_zip_archive(path: &Path) -> bool {
    std::fs::read(path)
        .map(|b| b.len() >= 4 && &b[0..2] == b"PK")
        .unwrap_or(false)
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

        // 2b. Backfill missing metadata from keyless sources.
        if globals.get_bool_or("metadata", "fill_misc", true) {
            let client = reqwest::Client::new();
            let filled =
                picaro_utils::metadata_fill::fill_track_metadata(&client, &mut track_info).await;
            if !filled.is_empty() {
                info!("metadata fill: {}", filled.join(", "));
            }
            // Low-trust sources (YouTube uploader names / video thumbnails):
            // replace with real artist/album/cover when confidently matched.
            if service.eq_ignore_ascii_case("youtube") {
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
                    DownloadProgress::hidden(),
                )
                .await?
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
            return Err(Error::Download(format!(
                "rejected non-audio download (bad source?): {}",
                dest.display()
            )));
        }

        // 7. Tag the file.
        let container = container_for_extension(extension);
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
        let cover_info = module
            .get_track_cover(
                track.id.as_deref().unwrap_or(""),
                &opts,
                track.cover_extra_kwargs.clone().into_iter().collect(),
            )
            .await?;
        if cover_info.url.is_empty() {
            return Err(Error::Other("empty cover url".into()));
        }
        let temp = crate::http::create_temp_filename_with_ext(cover_info.file_type.extension());
        let client = reqwest::Client::new();
        let _ = download_to_path(
            &client,
            &cover_info.url,
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
        for track in &album.tracks {
            let id = track.id();
            // Build a per-track filename inside the album folder
            let global_album = self.globals();
            match self
                .download_track_into(service, id, Some(&album_path), &global_album)
                .await
            {
                Ok(p) => {
                    succeeded += 1;
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
        Ok(paths)
    }

    async fn download_track_into(
        &self,
        service: &str,
        track_id: &str,
        album_path: Option<&Path>,
        globals: &GlobalSettings,
    ) -> Result<PathBuf> {
        // Almost identical to download_track but with the option to use the
        // album-supplied track metadata so the on-disk filename lines up with
        // the album folder name.
        let module = self.picaro.load_module(service).await?;
        let quality = self.picaro.current_quality();
        let codec_options = self.picaro.codec_options();
        let mut track_info = module
            .get_track_info(track_id, quality, &codec_options, HashMap::new())
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
                    DownloadProgress::hidden(),
                )
                .await?
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

        if is_7z_archive(&dest).await {
            let extract_to = dest
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.output_path.clone());
            sevenz_rust::decompress_file(&dest, &extract_to)
                .map_err(|e| Error::Download(format!("7z extraction failed: {e}")))?;
            let removed = picaro_utils::safety::purge_non_audio(&extract_to);
            if !removed.is_empty() {
                self.log_warn(format!(
                    "removed {} non-audio file(s) from archive",
                    removed.len()
                ));
            }
            let _ = tokio::fs::remove_file(&dest).await;
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
        if is_zip_archive(&dest) {
            let extract_to = dest
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| self.output_path.clone());
            extract_zip(&dest, &extract_to).await?;
            let removed = picaro_utils::safety::purge_non_audio(&extract_to);
            if !removed.is_empty() {
                self.log_warn(format!(
                    "removed {} non-audio file(s) from archive",
                    removed.len()
                ));
            }
            let _ = tokio::fs::remove_file(&dest).await;
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
            return Err(Error::Download(format!(
                "rejected non-audio download (bad source?): {}",
                dest.display()
            )));
        }

        let container = container_for_extension(extension);
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
                .download_track_into(service, id, Some(&playlist_path), &globals)
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
