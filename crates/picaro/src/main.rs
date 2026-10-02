//! PicaroDL - CLI / TUI entry point.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

/// Absolute path without Windows' `\\?\` verbatim prefix, so downstream
/// parsers (and users) see a normal path.
fn display_path(p: &std::path::Path) -> std::path::PathBuf {
    let s = p.to_string_lossy();
    std::path::PathBuf::from(s.trim_start_matches(r"\\?\"))
}

use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, EnvFilter};

use picaro_core::Picaro;
use picaro_downloader::{DownloadEvent, Downloader};
use picaro_utils::ModuleRegistry;

#[derive(Parser, Debug)]
#[command(
    name = "picaro",
    about = "Modular music downloader (Rust rewrite of OrpheusDL)",
    long_about = None,
    version = "0.1.0"
)]
struct Cli {
    /// Path to the config directory (containing settings.json). Defaults to ./config.
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    /// Override download output path. Defaults to general.download_path in settings.json.
    #[arg(long, global = true)]
    download_path: Option<PathBuf>,

    /// Disable concurrent downloads (sequential only).
    #[arg(long, global = true)]
    sequential: bool,

    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Subcommand, Debug, Clone)]
enum Command {
    /// Launch the TUI (default).
    Tui,

    /// Print the resolved configuration.
    ShowConfig,

    /// Print which modules are loaded.
    Modules,

    /// Print the global settings (defaults + user overrides).
    Settings,

    /// Download a single track by id.
    Track {
        /// Service module name (e.g. qobuz, deezer).
        #[arg(short, long)]
        service: String,
        /// Track id.
        track_id: String,
    },

    /// Download an entire album.
    Album {
        #[arg(short, long)]
        service: String,
        album_id: String,
    },

    /// Download a playlist.
    Playlist {
        #[arg(short, long)]
        service: String,
        playlist_id: String,
    },

    /// Download an artist's discography.
    Artist {
        #[arg(short, long)]
        service: String,
        artist_id: String,
    },

    /// Download from a URL (auto-detects service and media type).
    Url { url: String },

    /// Search a module.
    Search {
        #[arg(short, long)]
        service: String,
        query: String,
        /// track|album|artist|playlist
        #[arg(short = 't', long, default_value = "track")]
        kind: String,
    },

    /// Resolve "artist - title" to the fastest provider and download it.
    Get {
        /// Query, e.g. "Soulkeeper - Heavy Glow".
        query: String,
        /// Target quality: lossless|high|medium|low.
        #[arg(short, long, default_value = "high")]
        quality: String,
        /// Download a whole album instead of one track.
        #[arg(short = 't', long, default_value = "track")]
        kind: String,
        /// Only resolve (print the winning source) without downloading.
        #[arg(long)]
        resolve_only: bool,
        /// Machine output (with --resolve-only): one JSON object.
        #[arg(long)]
        json: bool,
        /// Restrict to a single provider (e.g. flacmusic).
        #[arg(long)]
        only: Option<String>,
        /// Expected track duration in seconds; a downloaded file wildly
        /// off (<50% / >200%) is rejected as the wrong song.
        #[arg(long)]
        expected_seconds: Option<u64>,
    },

    /// Query every lyrics provider for "artist - title".
    Lyrics { query: String },

    /// Look up cover art for "artist - title" (or --artist/--album).
    Cover {
        query: Option<String>,
        /// Explicit artist (overrides query parsing).
        #[arg(long)]
        artist: Option<String>,
        /// Explicit album name (album-first lookup: fetch the ALBUM's cover).
        #[arg(long)]
        album: Option<String>,
        /// Download the image to this file (or directory) instead of
        /// printing the URL.
        #[arg(long, visible_alias = "save")]
        out: Option<std::path::PathBuf>,
        /// Machine output: one JSON object.
        #[arg(long)]
        json: bool,
        /// Skip sources that can't meet this size (pixels, square assumed).
        #[arg(long)]
        min_size: Option<u32>,
    },

    /// Download one track from an album by disc/position ("Complete album").
    GetTrack {
        /// "artist - album title" to resolve the release.
        #[arg(long)]
        album: String,
        #[arg(long, default_value = "1")]
        disc: u32,
        /// 1-based position on the disc.
        #[arg(long)]
        pos: u32,
        /// Target quality: lossless|high|medium|low.
        #[arg(short, long, default_value = "high")]
        quality: String,
        /// Restrict to a single provider.
        #[arg(long)]
        only: Option<String>,
        /// Expected track duration in seconds; a downloaded file wildly
        /// off (<50% / >200%) is rejected as the wrong song.
        #[arg(long)]
        expected_seconds: Option<u64>,
    },

    /// Download a whole album release ("artist - album", one source).
    GetAlbum {
        /// "artist - album title" to resolve the release.
        #[arg(long)]
        album: String,
        /// Target quality: lossless|high|medium|low.
        #[arg(short, long, default_value = "high")]
        quality: String,
        /// Restrict to a single provider.
        #[arg(long)]
        only: Option<String>,
        /// Machine output: one JSON object (no Downloaded lines).
        #[arg(long)]
        json: bool,
    },

    /// Benchmark sources against a fixed query set (timing + result counts).
    Benchmark {
        /// Benchmark a single service (default: all download-capable modules).
        #[arg(short, long)]
        service: Option<String>,
    },

    /// Transcode ONE existing local audio file to a lossy tier — the
    /// `[conversion]` block's ffmpeg table as a standalone operation.
    /// The result lands next to the input with the target extension and is
    /// kept only when it actually shrank; the input is never modified.
    /// Success prints `Converted: <path>`.
    Convert {
        /// The local file to convert.
        input: PathBuf,
        /// Target quality: high|medium|low (lossless is not a re-encode
        /// target).
        #[arg(short, long, default_value = "high")]
        quality: String,
    },
}

fn init_logging() {
    let _ = fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            EnvFilter::new("info,picaro=debug,picaro_modules=debug,picaro_downloader=info")
        }))
        .with_target(false)
        .try_init();
}

fn build_registry() -> ModuleRegistry {
    let r = ModuleRegistry::new();
    picaro_modules::registry::register_all(&r).expect("register modules");
    r
}

async fn run_cli(cli: Cli) -> anyhow::Result<()> {
    // The TUI owns the whole screen: tracing's stderr lines would paint
    // over the interface (the TUI shows logs in its Logs tab instead).
    // Keep them for every other (CLI) command.
    if !matches!(cli.command, Some(Command::Tui) | None) {
        init_logging();
    }
    let config_dir = cli
        .config
        .clone()
        .unwrap_or_else(|| PathBuf::from("config"));
    picaro_core::loader::ensure_data_dirs(&config_dir).ok();
    let registry = build_registry();
    let picaro = Arc::new(Picaro::new(config_dir.clone(), registry)?);
    picaro_core::loader::persist_settings_for_modules(&config_dir, picaro.registry())?;
    picaro_core::loader::reload_settings_into_registry(&config_dir, picaro.registry());

    match cli.command.clone().unwrap_or(Command::Tui) {
        Command::Tui => {
            picaro_tui::run(picaro).await?;
        }
        Command::ShowConfig => {
            let s = serde_json::to_string_pretty(
                picaro
                    .settings
                    .get("global")
                    .unwrap_or(&serde_json::Value::Null),
            )?;
            println!("{s}");
        }
        Command::Modules => {
            for name in picaro.list_modules() {
                if let Some(m) = picaro.registry().get(&name) {
                    println!(
                        "{:<14}  {:<10}  {}",
                        m.information.service_name,
                        format!("{:?}", m.information.module_supported_modes),
                        m.information
                            .netlocation_constant
                            .first()
                            .unwrap_or_default()
                    );
                }
            }
        }
        Command::Settings => {
            println!("{}", serde_json::to_string_pretty(&picaro.merged_globals)?);
        }
        Command::Track { service, track_id } => {
            let downloader = make_downloader(picaro.clone(), &cli);
            downloader.download_track(&service, &track_id).await?;
        }
        Command::Album { service, album_id } => {
            let downloader = make_downloader(picaro.clone(), &cli);
            downloader.download_album(&service, &album_id).await?;
        }
        Command::Playlist {
            service,
            playlist_id,
        } => {
            let downloader = make_downloader(picaro.clone(), &cli);
            downloader.download_playlist(&service, &playlist_id).await?;
        }
        Command::Artist { service, artist_id } => {
            let downloader = make_downloader(picaro.clone(), &cli);
            downloader.download_artist(&service, &artist_id).await?;
        }
        Command::Url { url } => {
            // BitTorrent magnet links never reach a module; hand them to the
            // torrent engine directly.
            if url.trim_start().starts_with("magnet:") {
                let downloader = make_downloader(picaro.clone(), &cli);
                downloader.download_magnet_url(url.trim()).await?;
                return Ok(());
            }
            let parsed = url::Url::parse(&url).map_err(|e| anyhow::anyhow!("Invalid URL: {e}"))?;
            let host = parsed.host_str().unwrap_or("").to_lowercase();
            let service = picaro_utils::url_decode::netloc_to_module(&host, picaro.registry())
                .ok_or_else(|| anyhow::anyhow!("No module handles host '{host}'"))?;
            let mi = picaro_utils::url_decode::default_decode_url(&url, picaro.registry())?;
            let downloader = make_downloader(picaro.clone(), &cli);
            match mi.media_type {
                picaro_utils::models::DownloadType::track => {
                    downloader.download_track(&service, &mi.media_id).await?;
                }
                picaro_utils::models::DownloadType::album => {
                    downloader.download_album(&service, &mi.media_id).await?;
                }
                picaro_utils::models::DownloadType::playlist => {
                    downloader.download_playlist(&service, &mi.media_id).await?;
                }
                picaro_utils::models::DownloadType::artist => {
                    downloader.download_artist(&service, &mi.media_id).await?;
                }
                _ => {
                    return Err(anyhow::anyhow!(
                        "Unsupported media type: {:?}",
                        mi.media_type
                    ));
                }
            }
        }
        Command::Search {
            service,
            query,
            kind,
        } => {
            let qt = match kind.as_str() {
                "album" => picaro_utils::models::DownloadType::album,
                "artist" => picaro_utils::models::DownloadType::artist,
                "playlist" => picaro_utils::models::DownloadType::playlist,
                _ => picaro_utils::models::DownloadType::track,
            };
            let m = picaro.load_module(&service).await?;
            let results = m.search(qt, &query, None, 25).await?;
            for r in results {
                let title = r.name.clone().unwrap_or_default();
                let artists = r.artists.clone().map(|v| v.join(", ")).unwrap_or_default();
                let dur = r
                    .duration
                    .map(|d| format!(" [{:02}:{:02}]", d / 60, d % 60))
                    .unwrap_or_default();
                println!("{:<8}  {:<48}  {}{}", r.result_id, title, artists, dur);
            }
        }
        Command::Cover {
            query,
            artist,
            album,
            out,
            json,
            min_size,
        } => {
            // Artist/album flags win over query-string parsing.
            let (artist, title) = match (artist, album) {
                (Some(a), Some(al)) => (a, al),
                (Some(a), None) => {
                    let q = query.clone().unwrap_or_default();
                    (a, q.trim().to_string())
                }
                (None, Some(al)) => {
                    let q = query.clone().unwrap_or_default();
                    (q.trim().to_string(), al)
                }
                (None, None) => {
                    let q = query.unwrap_or_default();
                    match q.find(" - ") {
                        Some(i) => (
                            q[..i].trim().to_string(),
                            q[i + 3..].trim().to_string(),
                        ),
                        None => (String::new(), q.trim().to_string()),
                    }
                }
            };
            let client = picaro_utils::http::build_raw_client();
            let mut candidates = picaro_utils::metadata_fill::lookup_cover_art(
                &client,
                &artist,
                &title,
            )
            .await;
            if let Some(min) = min_size {
                // Known-dimension candidates below the floor are out;
                // unknown-dimension ones stay as last resort.
                let has_small = candidates.iter().any(|c| c.width.map_or(false, |w| w < min));
                let mut filtered: Vec<_> = candidates
                    .iter()
                    .filter(|c| c.width.map_or(true, |w| w >= min))
                    .cloned()
                    .collect();
                if filtered.is_empty() {
                    // keep unknowns as fallback rather than nothing
                    candidates.retain(|c| c.width.is_none());
                } else {
                    candidates = filtered;
                    filtered = Vec::new();
                }
                let _ = has_small;
            }
            let Some(hit) = candidates.first() else {
                if json {
                    println!("{{\"error\":\"no cover found\"}}");
                } else {
                    println!("MISS: no cover found for '{artist} - {title}'");
                }
                std::process::exit(1);
            };
            if let Some(out_path) = out {
                // Download the image bytes and write to disk.
                let resp = client.get(&hit.url).send().await;
                let bytes = match resp {
                    Ok(r) if r.status().is_success() => r.bytes().await.ok(),
                    _ => None,
                };
                let Some(bytes) = bytes.filter(|b| !b.is_empty()) else {
                    if json {
                        println!("{{\"error\":\"download failed\"}}");
                    } else {
                        println!("MISS: cover download failed ({})", hit.url);
                    }
                    std::process::exit(1);
                };
                let ext = match hit.format.as_deref() {
                    Some("png") => "png",
                    _ => "jpg",
                };
                let path = if out_path.is_dir() {
                    let safe = format!("{} - {}.{}", artist, title, ext)
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() || c == ' ' || c == '-' || c == '.' {
                            c
                        } else {
                            '_'
                        })
                        .collect::<String>();
                    out_path.join(safe)
                } else if out_path.extension().is_none() {
                    out_path.with_extension(ext)
                } else {
                    out_path.clone()
                };
                let abs = std::fs::canonicalize(path.parent().unwrap_or(std::path::Path::new(".")))
                    .unwrap_or_default()
                    .join(path.file_name().unwrap_or_default());
                if let Err(e) = std::fs::write(&path, &bytes) {
                    println!("error: cover write failed: {e}");
                    std::process::exit(1);
                }
                if json {
                    println!(
                        "{{\"path\":{}, \"url\":{}, \"source\":\"{}\", \"width\":{}, \"height\":{}, \"format\":\"{}\"}}",
                        serde_json::to_string(&abs.to_string_lossy()).unwrap(),
                        serde_json::to_string(&hit.url).unwrap(),
                        hit.source,
                        hit.width.map(|w| w.to_string()).unwrap_or("null".into()),
                        hit.height.map(|h| h.to_string()).unwrap_or("null".into()),
                        hit.format.as_deref().unwrap_or("jpeg"),
                    );
                } else {
                    println!("Downloaded: {}", display_path(&abs).display());
                }
                std::process::exit(0);
            }
            if json {
                println!(
                    "{{\"url\":{}, \"source\":\"{}\", \"width\":{}, \"height\":{}, \"format\":\"{}\"}}",
                    serde_json::to_string(&hit.url).unwrap(),
                    hit.source,
                    hit.width.map(|w| w.to_string()).unwrap_or("null".into()),
                    hit.height.map(|h| h.to_string()).unwrap_or("null".into()),
                    hit.format.as_deref().unwrap_or("jpeg"),
                );
            } else {
                println!(
                    "{} | album='{}' artist='{}' | {}",
                    hit.source, hit.album, hit.artist, hit.url
                );
            }
            std::process::exit(0);
        }
        Command::GetTrack {
            album,
            disc,
            pos,
            quality,
            only,
            expected_seconds,
        } => {
            let tier = picaro_utils::quality::QualityTier::parse(&quality).ok_or_else(|| {
                anyhow::anyhow!("invalid quality '{quality}' (lossless|high|medium|low)")
            })?;
            let mut resolver = picaro_downloader::resolver::Resolver::new(
                picaro.clone(),
                picaro_core::loader::config_dir().join("providers.json"),
            );
            resolver.set_only(only);
            // 1. Resolve the release.
            let r = match resolver.resolve(&album, tier).await {
                Ok(r) => r,
                Err(e) => {
                    println!("error: {e}");
                    std::process::exit(1);
                }
            };
            // 2. Enumerate its tracks.
            let module = match picaro.load_module(&r.service).await {
                Ok(m) => m,
                Err(e) => {
                    println!("error: load {service}: {e}", service = r.service);
                    std::process::exit(1);
                }
            };
            let info = match module
                .get_album_info(&r.result_id, HashMap::new())
                .await
            {
                Ok(i) => i,
                Err(e) => {
                    println!("error: album info from {}: {e}", r.service);
                    std::process::exit(1);
                }
            };
            let downloader = make_downloader(picaro.clone(), &cli);
            // 3. A release that resolves to a SINGLE download id is usually
            // a bundle (7z/zip/rar) holding every track - CoreRadio's
            // per-album archive. Fetch it once, extract, and pick the track
            // at `pos`; the archive and the rest are deleted afterwards.
            if info.tracks.len() == 1 {
                match downloader
                    .download_track_from_bundle(&r.service, &info, pos, expected_seconds)
                    .await
                {
                    Ok(p) => {
                        let abs = std::fs::canonicalize(&p).unwrap_or(p);
                        println!("Downloaded: {}", display_path(&abs).display());
                    }
                    Err(e) => {
                        println!("error: {e}");
                        std::process::exit(1);
                    }
                }
                return Ok(());
            }
            // 4. Per-track listing: pick the track at disc/pos.
            let disc_tracks: Vec<&picaro_utils::models::TrackRef> = info
                .tracks
                .iter()
                .filter(|t| match t {
                    picaro_utils::models::TrackRef::Full(f) => {
                        f.tags.disc_number.unwrap_or(1) == disc
                    }
                    picaro_utils::models::TrackRef::Id(_) => disc == 1,
                })
                .collect();
            let Some(track) = disc_tracks.get((pos.max(1) - 1) as usize) else {
                println!(
                    "error: {} has {} track(s) on disc {disc} (position {pos} requested)",
                    info.name,
                    disc_tracks.len()
                );
                std::process::exit(1);
            };
            let (track_id, track_name) = match track {
                picaro_utils::models::TrackRef::Full(f) => {
                    (f.id.clone().unwrap_or_default(), f.name.clone())
                }
                picaro_utils::models::TrackRef::Id(id) => (id.clone(), String::new()),
            };
            let track_name = if track_name.is_empty() {
                format!("{} track {pos}", info.name)
            } else {
                track_name
            };
            // 5. Download it (archive containers pick the named file).
            // (make_downloader already spawned the progress printer.)
            let mut data = HashMap::new();
            data.insert(
                "__track_name__".to_string(),
                serde_json::Value::String(track_name.clone()),
            );
            if !info.artist.is_empty() {
                data.insert(
                    "__artist__".to_string(),
                    serde_json::Value::String(info.artist.clone()),
                );
            }
            match downloader
                .download_track_with_data(&r.service, &track_id, data)
                .await
            {
                Ok(p) => {
                    // Duration fingerprint (W2): a wildly-off duration means
                    // the source served the wrong song - reject it.
                    if let Some(exp) = expected_seconds {
                        if let Err(e) =
                            picaro_downloader::fingerprint::verify_expected_duration(&p, exp)
                        {
                            picaro_downloader::fingerprint::remove_with_sidecars(&p);
                            println!("error: {e}");
                            std::process::exit(1);
                        }
                    }
                    let abs = std::fs::canonicalize(&p).unwrap_or(p.clone());
                    println!("Downloaded: {}", display_path(&abs).display());
                }
                Err(e) => {
                    println!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::GetAlbum {
            album,
            quality,
            only,
            json,
        } => {
            use picaro_utils::quality::QualityTier;

            let tier = QualityTier::parse(&quality).ok_or_else(|| {
                anyhow::anyhow!("invalid quality '{quality}' (lossless|high|medium|low)")
            })?;
            let mut resolver = picaro_downloader::resolver::Resolver::new(
                picaro.clone(),
                picaro_core::loader::config_dir().join("providers.json"),
            );
            resolver.set_only(only);
            let emit_err = |msg: String| {
                if json {
                    println!("{{\"error\":{}}}", serde_json::to_string(&msg).unwrap());
                } else {
                    println!("error: {msg}");
                }
                std::process::exit(1);
            };
            // 1. Resolve the release ONCE.
            let r = match resolver.resolve(&album, tier).await {
                Ok(r) => r,
                Err(e) => {
                    emit_err(e.to_string());
                    return Ok(());
                }
            };
            // 2. What tier does this source actually serve?
            let module = match picaro.load_module(&r.service).await {
                Ok(m) => m,
                Err(e) => {
                    emit_err(format!("load {}: {e}", r.service));
                    return Ok(());
                }
            };
            let info = match module.get_album_info(&r.result_id, HashMap::new()).await {
                Ok(i) => i,
                Err(e) => {
                    emit_err(format!("album info from {}: {e}", r.service));
                    return Ok(());
                }
            };
            let served = album_served_tier(&info, &r.service);
            if served.rank() < tier.rank() {
                if !resolver.allow_mixed_quality() {
                    emit_err(format!(
                        "wanted {}, {} only has {}",
                        tier.as_str(),
                        r.service,
                        served.as_str()
                    ));
                    return Ok(());
                }
                // Announce the step-down FIRST so the UI can warn.
                if !json {
                    println!("picaro tier {} {}", tier.as_str(), served.as_str());
                }
            }
            // 3. Download the whole release from that ONE source.
            let downloader = make_downloader(picaro.clone(), &cli);
            let files = match downloader.download_album(&r.service, &r.result_id).await {
                Ok(f) => f,
                Err(e) => {
                    emit_err(e.to_string());
                    return Ok(());
                }
            };
            let files: Vec<PathBuf> = files
                .into_iter()
                .filter(|p| p.is_file())
                .map(|p| {
                    let abs = std::fs::canonicalize(&p).unwrap_or(p);
                    display_path(&abs)
                })
                .collect();
            if files.is_empty() {
                emit_err(format!("album produced no files from {}", r.service));
                return Ok(());
            }
            if json {
                let jfiles: Vec<String> = files
                    .iter()
                    .map(|p| serde_json::to_string(p.to_string_lossy().as_ref()).unwrap())
                    .collect();
                println!(
                    "{{\"album\":{}, \"service\":\"{}\", \"tier\":\"{}\", \"served\":\"{}\", \"files\":[{}]}}",
                    serde_json::to_string(&info.name).unwrap(),
                    r.service,
                    tier.as_str(),
                    served.as_str(),
                    jfiles.join(",")
                );
            } else {
                for f in &files {
                    println!("Downloaded: {}", f.display());
                }
            }
        }
        Command::Lyrics { query } => {
            use picaro_utils::models::ModuleModes;
            let (artist, title) = match query.find(" - ") {
                Some(i) => (
                    query[..i].trim().to_string(),
                    query[i + 3..].trim().to_string(),
                ),
                None => (String::new(), query.trim().to_string()),
            };
            let mut data: std::collections::HashMap<String, serde_json::Value> =
                std::collections::HashMap::new();
            data.insert("__artist__".into(), serde_json::Value::String(artist));
            data.insert("__track_name__".into(), serde_json::Value::String(title));
            let mut any = false;
            for name in picaro.list_modules() {
                let is_lyrics = picaro
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
                match picaro.load_module(&name).await {
                    Ok(m) => match m.get_track_lyrics("", data.clone()).await {
                        Ok(l) => {
                            let n = l.embedded.as_deref().map_or(0, str::len)
                                + l.synced.as_deref().map_or(0, str::len);
                            println!(
                                "{:<12} {:<6} {} chars{}",
                                m.name(),
                                if n > 0 { "OK" } else { "EMPTY" },
                                n,
                                if l.synced.is_some() { " (synced)" } else { "" }
                            );
                            any |= n > 0;
                        }
                        Err(e) => println!("{:<12} ERR    {e}", m.name()),
                    },
                    Err(e) => println!("{name:<12} LOAD ERR {e}"),
                }
            }
            if !any {
                println!("no lyrics found for '{query}'");
            }
        }
        Command::Get {
            query,
            quality,
            kind,
            resolve_only,
            json,
            only,
            expected_seconds,
        } => {
            let tier = picaro_utils::quality::QualityTier::parse(&quality).ok_or_else(|| {
                anyhow::anyhow!("invalid quality '{quality}' (lossless|high|medium|low)")
            })?;
            let mut resolver = picaro_downloader::resolver::Resolver::new(
                picaro.clone(),
                picaro_core::loader::config_dir().join("providers.json"),
            );
            resolver.set_only(only);
            if kind == "album" && !resolve_only {
                // Whole-album download: resolve the best matching release,
                // then fetch every track from it.
                let downloader = make_downloader(picaro.clone(), &cli);
                let r = resolver.resolve(&query, tier).await?;
                println!(
                    "album: {} [{}]",
                    r.service,
                    r.result_id.split('#').next().unwrap_or(&r.result_id)
                );
                let files = match downloader.download_album(&r.service, &r.result_id).await {
                    Ok(f) => f,
                    Err(e) => {
                        println!("error: {e}");
                        std::process::exit(1);
                    }
                };
                if files.is_empty() {
                    println!("error: album produced no files from {}", r.service);
                    std::process::exit(1);
                }
                for f in files {
                    println!("Downloaded: {}", f.display());
                }
            } else if resolve_only {
                match resolver.resolve(&query, tier).await {
                    Ok(r) => {
                        if json {
                            println!(
                                "{{\"service\":{}, \"tier\":\"{}\", \"target\":{}}}",
                                serde_json::to_string(&r.service).unwrap(),
                                r.tier.as_str(),
                                serde_json::to_string(&r.result_id).unwrap(),
                            );
                        } else {
                            println!("{} [{}] -> {}", r.service, r.tier.as_str(), r.result_id);
                        }
                    }
                    Err(e) => {
                        if json {
                            println!(
                                "{{\"error\":{}}}",
                                serde_json::to_string(&e.to_string()).unwrap()
                            );
                        } else {
                            println!("MISS: {e}");
                        }
                        std::process::exit(1);
                    }
                }
            } else {
                let downloader = make_downloader(picaro.clone(), &cli);
                match resolver
                    .resolve_and_download(&downloader, &query, tier, expected_seconds)
                    .await
                {
                    Ok(path) => {
                        let abs = std::fs::canonicalize(&path).unwrap_or(path);
                        println!("Downloaded: {}", display_path(&abs).display());
                    }
                    Err(e) => {
                        println!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
        }
        Command::Convert { input, quality } => {
            // Machine grammar, same as every CLI path: `Converted: <path>`
            // on success, `error: ...` + non-zero exit on failure.
            let kbps = match quality.as_str() {
                "high" => 256,
                "medium" => 192,
                "low" => 128,
                other => {
                    println!("error: unknown quality '{other}' (high|medium|low; lossless is not a re-encode target)");
                    std::process::exit(2);
                }
            };
            match picaro_downloader::downloader::convert_local_file(&input, "aac", kbps).await {
                Ok(out) => println!("Converted: {}", out.display()),
                Err(e) => {
                    println!("error: {e}");
                    std::process::exit(1);
                }
            }
        }
        Command::Benchmark { service } => {
            use picaro_utils::models::{DownloadType, ModuleFlags, ModuleModes};
            let groups: [(&str, &[&str]); 4] = [
                (
                    "popular",
                    &[
                        "radiohead kid a",
                        "pink floyd the wall",
                        "the beatles abbey road",
                    ],
                ),
                (
                    "avg",
                    &[
                        "tool lateralus",
                        "massive attack mezzanine",
                        "portishead dummy",
                    ],
                ),
                (
                    "niche",
                    &[
                        "sunn o))) black one",
                        "boris pink",
                        "godspeed you! black emperor f#a#infinity",
                    ],
                ),
                (
                    "superniche",
                    &["natural snow buildings the dance of the moon and the sun"],
                ),
            ];
            let services: Vec<String> = match &service {
                Some(s) => vec![s.clone()],
                None => picaro
                    .list_modules()
                    .into_iter()
                    .filter(|n| {
                        picaro
                            .registry()
                            .get(n)
                            .map(|m| {
                                let i = &m.information;
                                !i.flags.contains(ModuleFlags::hidden)
                                    && i.module_supported_modes.contains(ModuleModes::download)
                                    && !matches!(i.service_name.as_str(), "LRCLIB" | "Musixmatch")
                            })
                            .unwrap_or(false)
                    })
                    .collect(),
            };
            println!(
                "{:<18} {:<9} {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>7} {:>8}",
                "service", "format", "p1", "p2", "p3", "a1", "a2", "a3", "n1", "n2", "n3", "sn",
                "total", "avg_ms"
            );
            let mut by_format: std::collections::BTreeMap<&'static str, (usize, u128, usize)> =
                Default::default();
            for svc in &services {
                let module = match picaro.load_module(svc).await {
                    Ok(m) => m,
                    Err(_) => continue,
                };
                let fmt = fmt_of(module.name());
                let mut counts: Vec<usize> = Vec::new();
                let mut sum_ms: u128 = 0;
                for (_g, qs) in &groups {
                    for q in *qs {
                        let t0 = std::time::Instant::now();
                        let n = module
                            .search(DownloadType::album, q, None, 25)
                            .await
                            .map(|v| v.len())
                            .unwrap_or(0);
                        sum_ms += t0.elapsed().as_millis();
                        counts.push(n);
                    }
                }
                let total: usize = counts.iter().sum();
                let avg = sum_ms / counts.len().max(1) as u128;
                println!(
                    "{:<18} {:<9} {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>4} {:>7} {:>8}",
                    module.name(),
                    fmt,
                    counts[0], counts[1], counts[2], counts[3], counts[4],
                    counts[5], counts[6], counts[7], counts[8], counts[9],
                    total, avg
                );
                let e = by_format.entry(fmt).or_insert((0, 0, 0));
                e.0 += total;
                e.1 += sum_ms;
                e.2 += counts.len();
            }
            println!();
            println!("by format:");
            for (f, (t, ms, n)) in by_format {
                println!(
                    "  {:<9} total_results={:<5} avg_ms={}",
                    f,
                    t,
                    ms / n.max(1) as u128
                );
            }
        }
    }
    Ok(())
}

/// Best tier a resolved release can actually serve, from the module's
/// own quality metadata (falling back to track codecs / known source
/// classes). Used by `get-album` to announce or refuse a step-down.
fn album_served_tier(
    info: &picaro_utils::models::AlbumInfo,
    service: &str,
) -> picaro_utils::quality::QualityTier {
    use picaro_utils::quality::QualityTier;
    let q = info
        .quality
        .as_deref()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if q.contains("flac")
        || q.contains("lossless")
        || q.contains("alac")
        || q.contains("wav")
        || q.contains("aiff")
    {
        return QualityTier::Lossless;
    }
    if q.contains("320") {
        return QualityTier::High;
    }
    if q.contains("256") || q.contains("192") {
        return QualityTier::Medium;
    }
    if q.contains("128") {
        return QualityTier::Low;
    }
    // Track-listing codecs tell the truth for per-track sources.
    for t in &info.tracks {
        if let picaro_utils::models::TrackRef::Full(f) = t {
            if f.codec.is_lossless() {
                return QualityTier::Lossless;
            }
        }
    }
    // Known FLAC-first direct sources default to lossless; everything
    // else on the no-login chain serves MP3-class audio.
    match service {
        "technicaldeathmetal" | "coreradio" | "ektoplazm" | "relisten" | "khinsider" => {
            QualityTier::Lossless
        }
        "youtube" | "soundcloud" => QualityTier::Low,
        _ => QualityTier::High,
    }
}

fn fmt_of(name: &str) -> &'static str {
    match name.to_lowercase().as_str() {
        "flacmusic" | "losslessalbums" | "coreradio" | "alterportal" | "exystence"
        | "archiveorg" | "themfire" | "discogc" => "Lossless",
        "iplusfree" => "M4A",
        "ccmixter" | "mp3db" | "tancpol" | "zvu4it" | "soundclick" | "deadpulpit" | "butterboy"
        | "primitiveofferings" | "punkcata" | "ezhevika" | "discografias" => "MP3",
        "musicrider" | "intmusic" | "glorybeats" => "Mixed",
        "youtube" | "soundcloud" => "Stream",
        _ => "Unknown",
    }
}

fn make_downloader(picaro: Arc<Picaro>, cli: &Cli) -> Arc<Downloader> {
 let raw = cli
 .download_path
 .clone()
 .or_else(|| {
 picaro
 .merged_globals
 .get("general")
 .and_then(|v| v.get("download_path"))
 .and_then(|v| v.as_str())
 .map(|s| PathBuf::from(s))
 })
 .unwrap_or_else(|| PathBuf::from("./downloads"));
 // A relative download_path used to resolve against the process CWD, so
 // running the binary from anywhere scattered downloads across whatever
 // directory you happened to be in. Anchor relative paths to the project
 // root (config/ sits inside it) instead.
 let download_path = if raw.is_absolute() {
 raw
 } else {
 let root = picaro_core::loader::project_root()
 .parent()
 .map(|p| p.to_path_buf())
 .unwrap_or_else(|| PathBuf::from("."));
 root.join(raw)
 };
 std::fs::create_dir_all(&download_path).ok();
 let downloader = Arc::new(Downloader::new(picaro, download_path));
 spawn_progress_printer(&downloader);
 downloader
}

/// Print download events as one parseable line each on stdout, so a host
/// process (TUI, or an embedding app) can render live progress. Format:
///
/// ```text
/// picaro started <service> <context>
/// picaro track-start <name>
/// picaro progress <bytes> <total|-> <name>
/// picaro ok <name> <path>
/// picaro skip <name> <path>
/// picaro fail <name> <reason>
/// picaro finished <ok> <skipped> <failed>
/// picaro error <message>
/// ```
fn spawn_progress_printer(downloader: &Arc<Downloader>) {
    let rx = downloader.receiver();
    std::thread::spawn(move || {
        for event in rx.iter() {
            match event {
                DownloadEvent::Started { service, context } => {
                    println!("picaro started {service} {context}");
                }
                DownloadEvent::TrackStarted { name, .. } => {
                    println!("picaro track-start {name}");
                }
                DownloadEvent::TrackProgress {
                    name, bytes, total, ..
                } => match total {
                    Some(t) => println!("picaro progress {bytes} {t} {name}"),
                    None => println!("picaro progress {bytes} - {name}"),
                },
                DownloadEvent::TrackSucceeded { name, location, .. } => {
                    println!("picaro ok {name} {}", location.display());
                }
                DownloadEvent::TrackSkipped { name, location, .. } => {
                    println!("picaro skip {name} {}", location.display());
                }
                DownloadEvent::TrackFailed { name, reason, .. } => {
                    println!("picaro fail {name} {reason}");
                }
                DownloadEvent::ItemStarted { name } => {
                    println!("picaro item-start {name}");
                }
                DownloadEvent::ItemDone { name, location } => {
                    println!("picaro item-done {name} {}", location.display());
                }
                DownloadEvent::TierNotice { requested, served } => {
                    println!("picaro tier {requested} {served}");
                }
                DownloadEvent::Finished {
                    succeeded,
                    skipped,
                    failed,
                    ..
                } => {
                    println!("picaro finished {succeeded} {skipped} {failed}");
                }
                DownloadEvent::Error { message } => {
                    println!("picaro error {message}");
                }
                DownloadEvent::Log { .. } | DownloadEvent::SearchResults { .. } => {}
            }
        }
    });
}

fn main() {
    let cli = Cli::parse();
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("build tokio runtime");
    rt.block_on(async move {
        if let Err(e) = run_cli(cli).await {
            eprintln!("error: {e:?}");
            std::process::exit(1);
        }
    });
}
