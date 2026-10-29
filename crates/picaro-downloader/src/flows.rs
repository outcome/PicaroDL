//! Release-level download flows shared by the CLI and embedding hosts.
//!
//! `get-album` (a whole release from ONE source) and `get-track` (one track
//! of a release, addressed by disc/position) used to live inside the CLI's
//! `main.rs`, and Verdania carried its own hand-ported copy. The two drifted:
//! the embedded copy had no source fallback for albums, never cleaned up a
//! failed source's partial files, and reported raw multi-line resolver
//! errors. Both now call these functions, which:
//!
//!  * resolve the RELEASE with an album search (track-only sources sit out),
//!  * announce a tier step-down as a `TierNotice` event before downloading,
//!    or refuse it when `allow_mixed_quality` is off,
//!  * walk the remaining ranked sources when one fails (`source_fallback`),
//!  * remove a failed attempt's partial files before the next source runs,
//!  * fail with a structured [`FlowError`] whose [`FlowError::summary`] is a
//!    short sentence fit for a UI row, while `Display` keeps the full detail
//!    for logs.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use picaro_core::Picaro;
use picaro_utils::models::{AlbumInfo, TrackRef};
use picaro_utils::quality::QualityTier;

use crate::downloader::{DownloadEvent, Downloader};
use crate::resolver::{Resolution, Resolver};

/// Why one source attempt failed.
#[derive(Debug, Clone, PartialEq)]
pub enum AttemptError {
    /// The source only serves a lower tier and step-downs are off.
    BelowTier { served: QualityTier },
    /// The release info, the download or a verification step failed.
    Failed(String),
}

/// One failed source attempt, for the error report.
#[derive(Debug, Clone, PartialEq)]
pub struct SourceAttempt {
    pub service: String,
    pub error: AttemptError,
}

/// A get-album / get-track flow failure.
#[derive(Debug, Clone, PartialEq)]
pub enum FlowError {
    /// No source matched the query at the allowed tiers.
    NotFound {
        query: String,
        tier: QualityTier,
        /// Step-downs were off (`allow_mixed_quality=false`), so only the
        /// requested tier and higher ones were searched.
        strict: bool,
    },
    /// Sources matched, and every one of them failed.
    AllFailed {
        query: String,
        tier: QualityTier,
        attempts: Vec<SourceAttempt>,
    },
}

/// What kind of item the flow was fetching (for the wording only).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlowItem {
    Album,
    Track,
}

impl FlowItem {
    fn noun(self) -> &'static str {
        match self {
            FlowItem::Album => "album",
            FlowItem::Track => "track",
        }
    }
}

/// Human name of a tier for UI text ("lossless (FLAC)", "high quality").
pub fn tier_label(t: QualityTier) -> &'static str {
    match t {
        QualityTier::Lossless => "lossless (FLAC)",
        QualityTier::High => "high quality",
        QualityTier::Medium => "medium quality",
        QualityTier::Low => "low quality",
    }
}

/// Capitalised tier name as the quality picker shows it.
fn tier_option(t: QualityTier) -> &'static str {
    match t {
        QualityTier::Lossless => "Lossless",
        QualityTier::High => "High",
        QualityTier::Medium => "Medium",
        QualityTier::Low => "Low",
    }
}

/// The next tier down, when there is one (the "try X instead" hint).
fn lower_tier(t: QualityTier) -> Option<QualityTier> {
    match t {
        QualityTier::Lossless => Some(QualityTier::High),
        QualityTier::High => Some(QualityTier::Medium),
        QualityTier::Medium => Some(QualityTier::Low),
        QualityTier::Low => None,
    }
}

/// Broad class of a raw failure reason, for picking the summary sentence.
fn failure_class(reason: &str) -> &'static str {
    let l = reason.to_ascii_lowercase();
    if l.contains("timed out")
        || l.contains("timeout")
        || l.contains("gateway")
        || l.contains("connection")
        || l.contains("dns")
        || l.contains("http status server error")
        || l.contains(" 50")
        || l.contains("network")
    {
        "network"
    } else if l.contains("login")
        || l.contains("credential")
        || l.contains("token")
        || l.contains("auth")
        || l.contains("cookie")
    {
        "auth"
    } else if l.contains("blocked") || l.contains("cloudflare") || l.contains("403") {
        "blocked"
    } else {
        "other"
    }
}

impl FlowError {
    /// One short sentence for a UI row (no source dumps, no newlines).
    pub fn summary(&self, item: FlowItem) -> String {
        let noun = item.noun();
        match self {
            FlowError::NotFound { tier, strict, .. } => {
                if *strict && *tier != QualityTier::Low {
                    match lower_tier(*tier) {
                        Some(lower) => format!(
                            "No source had this {noun} in {}; try {} quality",
                            tier_label(*tier),
                            tier_option(lower)
                        ),
                        None => format!("No source had this {noun} in {}", tier_label(*tier)),
                    }
                } else {
                    format!("Couldn't find this {noun} on any source")
                }
            }
            FlowError::AllFailed { tier, attempts, .. } => {
                let below: Vec<QualityTier> = attempts
                    .iter()
                    .filter_map(|a| match a.error {
                        AttemptError::BelowTier { served } => Some(served),
                        _ => None,
                    })
                    .collect();
                if !below.is_empty() && below.len() == attempts.len() {
                    let best = below.iter().copied().max_by_key(|t| t.rank()).unwrap_or(*tier);
                    return format!(
                        "No source had this {noun} in {}; best available is {}",
                        tier_label(*tier),
                        tier_label(best)
                    );
                }
                let classes: Vec<&str> = attempts
                    .iter()
                    .filter_map(|a| match &a.error {
                        AttemptError::Failed(r) => Some(failure_class(r)),
                        _ => None,
                    })
                    .collect();
                let n = attempts.len();
                let sources = if n == 1 {
                    "1 source".to_string()
                } else {
                    format!("{n} sources")
                };
                let (dl, every) = if n == 1 {
                    ("the download", "the download")
                } else {
                    ("the downloads", "every download")
                };
                if !classes.is_empty() && classes.iter().all(|c| *c == "network") {
                    format!("Found on {sources}, but {dl} failed (network errors); try again later")
                } else if classes.iter().any(|c| *c == "auth") {
                    format!("Found on {sources}, but the download needs a login that isn't set up")
                } else {
                    format!("Found on {sources}, but {every} failed")
                }
            }
        }
    }

    /// Every attempt, one per line, for logs and an expandable detail view.
    pub fn detail(&self) -> String {
        self.to_string()
    }
}

impl std::fmt::Display for FlowError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FlowError::NotFound { query, tier, strict } => {
                write!(f, "no source has '{query}' at {}", tier.as_str())?;
                if *strict {
                    write!(f, " or higher (allow_mixed_quality=false)")?;
                }
                Ok(())
            }
            FlowError::AllFailed { query, attempts, .. } => {
                write!(f, "all sources failed for '{query}':")?;
                for a in attempts {
                    match &a.error {
                        AttemptError::BelowTier { served } => {
                            write!(f, "\n  {}: only has {}", a.service, served.as_str())?
                        }
                        AttemptError::Failed(r) => {
                            write!(f, "\n  {}: {}", a.service, r.trim().replace('\n', " "))?
                        }
                    }
                }
                Ok(())
            }
        }
    }
}

impl std::error::Error for FlowError {}

/// A finished get-album download.
#[derive(Debug, Clone)]
pub struct AlbumOutcome {
    pub service: String,
    /// Release name as the source reported it.
    pub name: String,
    /// The tier the source actually serves (may be below the request when
    /// step-downs are allowed; a `TierNotice` was sent in that case).
    pub served: QualityTier,
    /// Audio files delivered, in release order.
    pub files: Vec<PathBuf>,
    /// Tracks the source listed but failed to deliver.
    pub failed_tracks: u32,
}

/// A finished get-track download.
#[derive(Debug, Clone)]
pub struct TrackOutcome {
    pub service: String,
    pub path: PathBuf,
}

/// Best tier a resolved release can actually serve, from the module's own
/// quality metadata (falling back to track codecs / known source classes).
pub fn album_served_tier(info: &AlbumInfo, service: &str) -> QualityTier {
    let q = info.quality.as_deref().unwrap_or_default().to_ascii_lowercase();
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
    if info.tracks.iter().any(|t| matches!(t, TrackRef::Full(f) if f.codec.is_lossless())) {
        return QualityTier::Lossless;
    }
    match service {
        "technicaldeathmetal" | "coreradio" | "ektoplazm" | "relisten" | "khinsider" => {
            QualityTier::Lossless
        }
        "youtube" | "soundcloud" => QualityTier::Low,
        _ => QualityTier::High,
    }
}

/// Every file and directory under `dir`, recursively.
fn snapshot(dir: &Path) -> HashSet<PathBuf> {
    fn walk(dir: &Path, out: &mut HashSet<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                out.insert(p.clone());
                walk(&p, out);
            } else {
                out.insert(p);
            }
        }
    }
    let mut out = HashSet::new();
    walk(dir, &mut out);
    out
}

/// Remove everything under `dir` that was not there in `before` (a failed
/// attempt's partial files and the folders it created). Pre-existing files
/// and folders are never touched.
pub(crate) fn remove_newcomers(dir: &Path, before: &HashSet<PathBuf>) {
    let after = snapshot(dir);
    let mut new_dirs: Vec<&PathBuf> = Vec::new();
    for p in after.difference(before) {
        if p.is_dir() {
            new_dirs.push(p);
        } else {
            let _ = std::fs::remove_file(p);
        }
    }
    // Deepest first, and only folders this attempt created.
    new_dirs.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for d in new_dirs {
        let _ = std::fs::remove_dir(d);
    }
}

/// Is `path` an audio file the flows count as a delivered track?
fn is_audio(path: &Path) -> bool {
    matches!(
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
            .as_deref(),
        Some(
            "flac" | "mp3" | "m4a" | "aac" | "ogg" | "opus" | "wav" | "aiff" | "aif" | "wv"
                | "ape" | "alac" | "webm"
        )
    )
}

/// Ordered candidate walk shared by both flows: the fast single resolve
/// first, then (on failure, with fallback on) every other ranked source.
async fn walk_sources<T, F, Fut>(
    resolver: &mut Resolver,
    query: &str,
    tier: QualityTier,
    release: bool,
    usable: fn(&str) -> bool,
    mut attempt: F,
) -> Result<T, FlowError>
where
    F: FnMut(Resolution) -> Fut,
    Fut: std::future::Future<Output = Result<T, AttemptError>>,
{
    let strict = !resolver.allow_mixed_quality();
    let first = if release {
        resolver.resolve_release(query, tier).await
    } else {
        resolver.resolve(query, tier).await
    };
    // A first hit from a source this flow can't use falls through to the
    // ranked list below (when fallback is on) instead of being attempted.
    let first = match first {
        Ok(r) if usable(&r.service) => r,
        Ok(r) if resolver.source_fallback() => {
            tracing::info!("flow: skipping {} for this flow", r.service);
            Resolution {
                service: String::new(),
                ..r
            }
        }
        Ok(r) => {
            return Err(FlowError::AllFailed {
                query: query.to_string(),
                tier,
                attempts: vec![SourceAttempt {
                    service: r.service,
                    error: AttemptError::Failed("source can't serve this request".into()),
                }],
            })
        }
        Err(e) => {
            tracing::info!("flow: resolve '{query}' failed: {e}");
            return Err(FlowError::NotFound {
                query: query.to_string(),
                tier,
                strict,
            });
        }
    };
    let first_service = first.service.clone();
    let mut attempts: Vec<SourceAttempt> = Vec::new();
    if !first_service.is_empty() {
        match attempt(first).await {
            Ok(v) => return Ok(v),
            Err(e) => {
                tracing::info!("flow: {first_service} failed ({e:?}), trying the next source");
                attempts.push(SourceAttempt {
                    service: first_service.clone(),
                    error: e,
                });
            }
        }
    }
    if resolver.source_fallback() {
        let rest = if release {
            resolver.resolve_all_releases(query, tier).await
        } else {
            resolver.resolve_all(query, tier).await
        };
        if let Ok(rest) = rest {
            for r in rest
                .into_iter()
                .filter(|r| r.service != first_service && usable(&r.service))
            {
                let service = r.service.clone();
                match attempt(r).await {
                    Ok(v) => return Ok(v),
                    Err(e) => {
                        tracing::info!("flow: {service} failed ({e:?}), trying the next source");
                        attempts.push(SourceAttempt { service, error: e });
                    }
                }
            }
        }
    }
    if attempts.is_empty() {
        // Only unusable sources matched.
        return Err(FlowError::NotFound {
            query: query.to_string(),
            tier,
            strict,
        });
    }
    Err(FlowError::AllFailed {
        query: query.to_string(),
        tier,
        attempts,
    })
}

/// Any source may serve a whole album.
fn any_source(_: &str) -> bool {
    true
}

/// Sources whose release track ORDER means something. A YouTube/SoundCloud
/// playlist matched by title is often a fan list in arbitrary order (a
/// "Complete album" gap fill delivered a Clone Hero chart video as track
/// 2), so get-track never picks a position out of one; the host's free-text
/// fallback ("artist - title") is the right tool there.
fn ordered_release_source(service: &str) -> bool {
    !matches!(service, "youtube" | "soundcloud")
}

/// Download a whole release ("artist - album") from one source, walking
/// the ranked sources when one fails.
pub async fn get_album(
    picaro: &Arc<Picaro>,
    downloader: &Downloader,
    resolver: &mut Resolver,
    query: &str,
    tier: QualityTier,
) -> Result<AlbumOutcome, FlowError> {
    let allow_mixed = resolver.allow_mixed_quality();
    let root = downloader.output_path().to_path_buf();
    walk_sources(resolver, query, tier, true, any_source, |r| {
        let root = root.clone();
        async move {
            let before = snapshot(&root);
            let res = try_album_source(picaro, downloader, &r, tier, allow_mixed).await;
            if res.is_err() {
                remove_newcomers(&root, &before);
            }
            res
        }
    })
    .await
}

async fn try_album_source(
    picaro: &Arc<Picaro>,
    downloader: &Downloader,
    r: &Resolution,
    tier: QualityTier,
    allow_mixed: bool,
) -> Result<AlbumOutcome, AttemptError> {
    let module = picaro
        .load_module(&r.service)
        .await
        .map_err(|e| AttemptError::Failed(format!("load {}: {e}", r.service)))?;
    let info = module
        .get_album_info(&r.result_id, HashMap::new())
        .await
        .map_err(|e| AttemptError::Failed(format!("album info: {e}")))?;
    let served = album_served_tier(&info, &r.service);
    if served.rank() < tier.rank() {
        if !allow_mixed {
            return Err(AttemptError::BelowTier { served });
        }
        // Announce the step-down FIRST so a UI can warn, and only on the
        // attempt that actually proceeds to downloading.
        let _ = downloader.sender().send(DownloadEvent::TierNotice {
            requested: tier.as_str().to_string(),
            served: served.as_str().to_string(),
        });
    }
    let listed = info.tracks.len();
    let files: Vec<PathBuf> = downloader
        .download_album(&r.service, &r.result_id)
        .await
        .map_err(|e| AttemptError::Failed(e.to_string()))?
        .into_iter()
        .filter(|p| p.is_file() && is_audio(p))
        .collect();
    if files.is_empty() {
        return Err(AttemptError::Failed(format!(
            "no audio delivered ({listed} item(s) listed)"
        )));
    }
    // Bundle releases list ONE archive and extract many files, so the
    // shortfall only means something for per-track listings.
    let failed_tracks = if listed > 1 && files.len() < listed {
        (listed - files.len()) as u32
    } else {
        0
    };
    Ok(AlbumOutcome {
        service: r.service.clone(),
        name: info.name.clone(),
        served,
        files,
        failed_tracks,
    })
}

/// Download ONE track of a release, addressed by disc/position ("Complete
/// album"), walking the ranked sources when one fails.
pub async fn get_track(
    picaro: &Arc<Picaro>,
    downloader: &Downloader,
    resolver: &mut Resolver,
    album_query: &str,
    disc: u32,
    pos: u32,
    tier: QualityTier,
    expected_secs: Option<u64>,
) -> Result<TrackOutcome, FlowError> {
    let root = downloader.output_path().to_path_buf();
    walk_sources(resolver, album_query, tier, true, ordered_release_source, |r| {
        let root = root.clone();
        async move {
            let before = snapshot(&root);
            let res = try_track_source(picaro, downloader, &r, disc, pos, expected_secs).await;
            if res.is_err() {
                remove_newcomers(&root, &before);
            }
            res
        }
    })
    .await
}

async fn try_track_source(
    picaro: &Arc<Picaro>,
    downloader: &Downloader,
    r: &Resolution,
    disc: u32,
    pos: u32,
    expected_secs: Option<u64>,
) -> Result<TrackOutcome, AttemptError> {
    let fail = |e: String| AttemptError::Failed(e);
    let module = picaro
        .load_module(&r.service)
        .await
        .map_err(|e| fail(format!("load {}: {e}", r.service)))?;
    let info = module
        .get_album_info(&r.result_id, HashMap::new())
        .await
        .map_err(|e| fail(format!("album info: {e}")))?;
    // A release that resolves to a SINGLE download id is usually a bundle
    // (7z/zip/rar) holding every track: fetch it once, extract, pick `pos`.
    if info.tracks.len() == 1 {
        let path = downloader
            .download_track_from_bundle(&r.service, &info, pos.max(1), expected_secs)
            .await
            .map_err(|e| fail(e.to_string()))?;
        return Ok(TrackOutcome {
            service: r.service.clone(),
            path,
        });
    }
    let disc_tracks: Vec<&TrackRef> = info
        .tracks
        .iter()
        .filter(|t| match t {
            TrackRef::Full(f) => f.tags.disc_number.unwrap_or(1) == disc.max(1),
            TrackRef::Id(_) => disc.max(1) == 1,
        })
        .collect();
    let Some(track) = disc_tracks.get((pos.max(1) - 1) as usize) else {
        return Err(fail(format!(
            "{} has {} track(s) on disc {disc} (position {pos} requested)",
            info.name,
            disc_tracks.len()
        )));
    };
    let (track_id, track_name) = match track {
        TrackRef::Full(f) => (f.id.clone().unwrap_or_default(), f.name.clone()),
        TrackRef::Id(id) => (id.clone(), String::new()),
    };
    let track_name = if track_name.is_empty() {
        format!("{} track {pos}", info.name)
    } else {
        track_name
    };
    let mut data = HashMap::new();
    data.insert("__track_name__".to_string(), serde_json::Value::String(track_name));
    if !info.artist.is_empty() {
        data.insert(
            "__artist__".to_string(),
            serde_json::Value::String(info.artist.clone()),
        );
    }
    let path = downloader
        .download_track_with_data(&r.service, &track_id, data)
        .await
        .map_err(|e| fail(e.to_string()))?;
    if let Some(exp) = expected_secs {
        if let Err(e) = crate::fingerprint::verify_expected_duration(&path, exp) {
            crate::fingerprint::remove_with_sidecars(&path);
            return Err(fail(e.to_string()));
        }
    }
    Ok(TrackOutcome {
        service: r.service.clone(),
        path,
    })
}

/// Short, human sentence for ANY download error string (free-text `get`
/// failures, setup errors): the resolver's raw wording rephrased, the
/// source dump dropped. Flow errors should use [`FlowError::summary`].
pub fn summarize_error(raw: &str, item: FlowItem, tier: Option<QualityTier>) -> String {
    let first = raw.lines().next().unwrap_or("").trim();
    let l = first.to_ascii_lowercase();
    let noun = item.noun();
    if l.contains("no source has") || l.contains("no source delivered") {
        if l.contains("allow_mixed_quality=false") {
            if let Some(t) = tier {
                return match lower_tier(t) {
                    Some(lower) => format!(
                        "No source had this {noun} in {}; try {} quality",
                        tier_label(t),
                        tier_option(lower)
                    ),
                    None => format!("No source had this {noun} in {}", tier_label(t)),
                };
            }
        }
        return format!("Couldn't find this {noun} on any source");
    }
    if l.contains("no source handles links") {
        return "No source can download links from this site".to_string();
    }
    if l.contains("cancelled") {
        return "Download cancelled".to_string();
    }
    if l.contains("doesn't match") && l.contains("quality") {
        return "The only copy found was a fake or low-quality file".to_string();
    }
    if l.contains("duration") && (l.contains("wrong") || l.contains("expected")) {
        return "Only found a different song with the same name".to_string();
    }
    match failure_class(first) {
        "network" => "Download failed (network error); try again later".to_string(),
        "auth" => "This source needs a login that isn't set up".to_string(),
        "blocked" => "The source blocked the download".to_string(),
        _ => {
            let mut s = picaro_utils::error_simplify::simplify_error_message(first);
            if s.is_empty() {
                s = "Download failed".to_string();
            }
            s
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn attempt(service: &str, e: AttemptError) -> SourceAttempt {
        SourceAttempt {
            service: service.into(),
            error: e,
        }
    }

    #[test]
    fn strict_not_found_suggests_the_next_tier() {
        let e = FlowError::NotFound {
            query: "Glare - Sunset Funeral".into(),
            tier: QualityTier::Lossless,
            strict: true,
        };
        assert_eq!(
            e.summary(FlowItem::Album),
            "No source had this album in lossless (FLAC); try High quality"
        );
        let lax = FlowError::NotFound {
            query: "x".into(),
            tier: QualityTier::Lossless,
            strict: false,
        };
        assert_eq!(lax.summary(FlowItem::Track), "Couldn't find this track on any source");
    }

    #[test]
    fn all_below_tier_names_the_best_available() {
        let e = FlowError::AllFailed {
            query: "q".into(),
            tier: QualityTier::Lossless,
            attempts: vec![
                attempt("youtube", AttemptError::BelowTier { served: QualityTier::Low }),
                attempt("butterboy", AttemptError::BelowTier { served: QualityTier::High }),
            ],
        };
        assert_eq!(
            e.summary(FlowItem::Album),
            "No source had this album in lossless (FLAC); best available is high quality"
        );
    }

    #[test]
    fn network_failures_say_try_later_and_detail_keeps_every_source() {
        let e = FlowError::AllFailed {
            query: "Loathe - I Let It In".into(),
            tier: QualityTier::High,
            attempts: vec![
                attempt(
                    "coreradio",
                    AttemptError::Failed("HTTP status server error (504 Gateway Time-out)".into()),
                ),
                attempt("tancpol", AttemptError::Failed("connection reset".into())),
            ],
        };
        let s = e.summary(FlowItem::Album);
        assert_eq!(s, "Found on 2 sources, but the downloads failed (network errors); try again later");
        assert!(!s.contains('\n'));
        let d = e.detail();
        assert!(d.contains("coreradio: HTTP status server error"));
        assert!(d.contains("tancpol: connection reset"));
    }

    #[test]
    fn summarize_error_rephrases_resolver_wording() {
        assert_eq!(
            summarize_error(
                "Other error: wanted lossless, no source has 'a - b' at that tier (allow_mixed_quality=false)",
                FlowItem::Track,
                Some(QualityTier::Lossless)
            ),
            "No source had this track in lossless (FLAC); try High quality"
        );
        assert_eq!(
            summarize_error("Other error: resolver: no source has 'a - b'", FlowItem::Track, None),
            "Couldn't find this track on any source"
        );
        assert_eq!(
            summarize_error("HTTP error: error sending request: operation timed out", FlowItem::Track, None),
            "Download failed (network error); try again later"
        );
    }

    #[test]
    fn remove_newcomers_keeps_preexisting_files_and_folders() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("Artist/Old")).unwrap();
        std::fs::create_dir_all(root.join("EmptyButOld")).unwrap();
        std::fs::write(root.join("Artist/Old/keep.flac"), b"x").unwrap();
        let before = snapshot(root);
        std::fs::create_dir_all(root.join("Artist/New/Disc 1")).unwrap();
        std::fs::write(root.join("Artist/New/Disc 1/partial.flac.part"), b"x").unwrap();
        std::fs::write(root.join("Artist/Old/stray.mp3"), b"x").unwrap();
        remove_newcomers(root, &before);
        assert!(root.join("Artist/Old/keep.flac").is_file());
        assert!(root.join("EmptyButOld").is_dir(), "pre-existing empty folders survive");
        assert!(!root.join("Artist/Old/stray.mp3").exists());
        assert!(!root.join("Artist/New").exists());
    }

    #[test]
    fn get_track_never_picks_positions_out_of_stream_playlists() {
        assert!(!ordered_release_source("youtube"));
        assert!(!ordered_release_source("soundcloud"));
        assert!(ordered_release_source("coreradio"));
        assert!(any_source("youtube"));
    }

    #[test]
    fn served_tier_reads_quality_then_service_class() {
        let mut info = AlbumInfo::default();
        info.quality = Some("MP3 320".into());
        assert_eq!(album_served_tier(&info, "coreradio"), QualityTier::High);
        info.quality = None;
        assert_eq!(album_served_tier(&info, "coreradio"), QualityTier::Lossless);
        assert_eq!(album_served_tier(&info, "youtube"), QualityTier::Low);
    }
}
