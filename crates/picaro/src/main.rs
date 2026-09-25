//! PicaroDL - CLI / TUI entry point.

use std::path::PathBuf;
use std::sync::Arc;

use clap::{Parser, Subcommand};
use tracing_subscriber::{fmt, EnvFilter};

use picaro_core::Picaro;
use picaro_downloader::Downloader;
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
        /// Only resolve (print the winning source) without downloading.
        #[arg(long)]
        resolve_only: bool,
        /// Restrict to a single provider (e.g. flacmusic).
        #[arg(long)]
        only: Option<String>,
    },

    /// Query every lyrics provider for "artist - title".
    Lyrics { query: String },

    /// Benchmark sources against a fixed query set (timing + result counts).
    Benchmark {
        /// Benchmark a single service (default: all download-capable modules).
        #[arg(short, long)]
        service: Option<String>,
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
    init_logging();
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
            resolve_only,
            only,
        } => {
            let tier = picaro_utils::quality::QualityTier::parse(&quality).ok_or_else(|| {
                anyhow::anyhow!("invalid quality '{quality}' (lossless|high|medium|low)")
            })?;
            let mut resolver = picaro_downloader::resolver::Resolver::new(
                picaro.clone(),
                PathBuf::from("cache/providers.json"),
            );
            resolver.set_only(only);
            if resolve_only {
                match resolver.resolve(&query, tier).await {
                    Ok(r) => println!("{} [{}] -> {}", r.service, r.tier.as_str(), r.result_id),
                    Err(e) => println!("MISS: {e}"),
                }
            } else {
                let downloader = make_downloader(picaro.clone(), &cli);
                let path = resolver
                    .resolve_and_download(&downloader, &query, tier)
                    .await?;
                println!("Downloaded: {}", path.display());
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
    let download_path = cli
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
    std::fs::create_dir_all(&download_path).ok();
    Arc::new(Downloader::new(picaro, download_path))
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
