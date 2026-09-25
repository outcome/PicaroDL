//! Quality-aware multi-provider resolver.
//!
//! Request "artist - title" and get the fastest provider that actually has it.
//! Providers are raced in parallel with a short timeout (first *relevant* hit
//! wins) and ranked by an EWMA score table that self-tunes. Search results are
//! scored by artist/title token overlap so a fuzzy provider (e.g. a blog or a
//! loose API) can't hand us the wrong item. Tier fallback goes requested ->
//! higher -> lower; if a provider matches but the download fails, it is dropped
//! and the next is tried. Album-based providers use `download_album` and then
//! pick the requested track out of the extracted files.
//!
//! Per-tier order is benchmark-derived and overridable:
//!
//! ```toml
//! [resolver.tier_order]
//! lossless = ["themfire", "flacmusic"]
//! high     = ["zvu4it", "tancpol", "ccmixter"]
//! ```

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use futures::stream::{FuturesUnordered, StreamExt};
use tracing::{info, warn};

use picaro_core::Picaro;
use picaro_utils::error::{Error, Result};
use picaro_utils::models::{DownloadType, ModuleFlags, ModuleModes, SearchResult};
use picaro_utils::quality::QualityTier;
use picaro_utils::textmatch;

use crate::downloader::Downloader;
use crate::globals::GlobalSettings;

#[derive(Debug, Clone)]
pub struct Resolution {
    pub service: String,
    pub result_id: String,
    pub name: String,
    pub artists: Vec<String>,
    pub tier: QualityTier,
}

pub struct Resolver {
    picaro: Arc<Picaro>,
    scores: HashMap<String, f64>,
    scores_path: PathBuf,
    timeout: Duration,
    timeout_lossless: Duration,
    max_parallel: usize,
    min_match: f64,
    allow: Option<Vec<String>>,
    tier_order: HashMap<QualityTier, Vec<String>>,
}

impl Resolver {
    pub fn new(picaro: Arc<Picaro>, scores_path: PathBuf) -> Self {
        let g = GlobalSettings::from_merged(&picaro.merged_globals);
        let timeout = Duration::from_secs(
            g.get_int_or("resolver", "probe_timeout_secs", 4)
                .clamp(1, 30) as u64,
        );
        let timeout_lossless = Duration::from_secs(
            g.get_int_or("resolver", "probe_timeout_lossless_secs", 14)
                .clamp(1, 60) as u64,
        );
        let max_parallel = g.get_int_or("resolver", "max_parallel", 6).clamp(1, 32) as usize;
        let min_match = g
            .get("resolver", "min_match")
            .and_then(|v| v.as_f64())
            .unwrap_or(0.5)
            .clamp(0.0, 1.0);
        let allow = g
            .get("resolver", "providers")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|x| x.as_str().map(|s| s.to_lowercase()))
                    .collect()
            });
        let tier_order = g
            .get("resolver", "tier_order")
            .and_then(|v| v.as_object())
            .map(|o| {
                o.iter()
                    .filter_map(|(k, v)| {
                        QualityTier::parse(k).map(|t| {
                            let list = v
                                .as_array()
                                .map(|a| {
                                    a.iter()
                                        .filter_map(|x| x.as_str().map(|s| s.to_lowercase()))
                                        .collect()
                                })
                                .unwrap_or_default();
                            (t, list)
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        let scores = load_scores(&scores_path);
        Self {
            picaro,
            scores,
            scores_path,
            timeout,
            timeout_lossless,
            max_parallel,
            min_match,
            allow,
            tier_order,
        }
    }

    /// Benchmark-derived default provider order for a tier (all no-signin).
    fn default_chain(tier: QualityTier) -> Vec<&'static str> {
        match tier {
            QualityTier::Lossless => vec![
                "themfire",
                "flacmusic",
                "losslessalbums",
                "exystence",
                "musicrider",
                "intmusic",
                "discogc",
                "coreradio",
                "alterportal",
            ],
            QualityTier::High => vec![
                "zvu4it",
                "tancpol",
                "ccmixter",
                "iplusfree",
                "mp3db",
                "soundcloud",
                "butterboy",
                "punkcata",
                "ezhevika",
                "primitiveofferings",
                "deadpulpit",
                "youtube",
            ],
            QualityTier::Medium => vec![
                "zvu4it",
                "tancpol",
                "iplusfree",
                "soundcloud",
                "youtube",
                "mp3db",
                "ccmixter",
                "butterboy",
                "punkcata",
                "ezhevika",
                "primitiveofferings",
                "deadpulpit",
            ],
            QualityTier::Low => vec![
                "youtube",
                "soundcloud",
                "zvu4it",
                "tancpol",
                "mp3db",
                "ccmixter",
                "butterboy",
                "punkcata",
                "ezhevika",
                "primitiveofferings",
                "deadpulpit",
            ],
        }
    }

    fn registered(&self, name: &str) -> bool {
        if let Some(a) = &self.allow {
            return a.contains(&name.to_lowercase());
        }
        self.picaro
            .registry()
            .get(name)
            .map(|m| {
                let i = &m.information;
                !i.flags.contains(ModuleFlags::hidden)
                    && i.module_supported_modes.contains(ModuleModes::download)
                    && !matches!(i.service_name.as_str(), "LRCLIB" | "Musixmatch")
            })
            .unwrap_or(false)
    }

    fn chain_for(&self, tier: QualityTier) -> Vec<String> {
        let configured = self.tier_order.get(&tier);
        let list: Vec<String> = match configured {
            Some(v) if !v.is_empty() => v.clone(),
            _ => Self::default_chain(tier)
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
        };
        let mut out: Vec<String> = list.into_iter().filter(|s| self.registered(s)).collect();
        out.sort_by(|a, b| {
            self.score_of(b)
                .partial_cmp(&self.score_of(a))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out
    }

    fn score_of(&self, s: &str) -> f64 {
        self.scores.get(s).copied().unwrap_or(0.0)
    }

    fn record(&mut self, service: &str, secs: f64, ok: bool) {
        let val = if ok { 1.0 / (1.0 + secs) } else { 0.0 };
        let alpha = 0.4;
        let e = self.scores.entry(service.to_string()).or_insert(val);
        if *e == 0.0 && val != 0.0 {
            *e = val;
        } else {
            *e = (1.0 - alpha) * *e + alpha * val;
        }
    }

    async fn race(
        &self,
        tier: QualityTier,
        query: &str,
        services: &[String],
    ) -> (Option<Resolution>, Vec<(String, f64, bool)>) {
        let mut obs = Vec::new();
        let mut futs = FuturesUnordered::new();
        for s in services {
            let service = s.clone();
            let q = query.to_string();
            let picaro = self.picaro.clone();
            let to = if tier == QualityTier::Lossless {
                self.timeout_lossless
            } else {
                self.timeout
            };
            let min_match = self.min_match;
            futs.push(async move {
                let start = Instant::now();
                let res = tokio::time::timeout(to, async {
                    let m = picaro.load_module(&service).await.ok()?;
                    let results = m.search(DownloadType::track, &q, None, 8).await.ok()?;
                    let mut best: Option<(f64, SearchResult)> = None;
                    for r in results {
                        let s = score_result(&q, &r);
                        if best.as_ref().map_or(true, |(bs, _)| s > *bs) {
                            best = Some((s, r));
                        }
                    }
                    match best {
                        Some((s, r)) if s >= min_match => Some((
                            r.result_id,
                            r.name.unwrap_or_default(),
                            r.artists.unwrap_or_default(),
                            s,
                        )),
                        _ => None,
                    }
                })
                .await;
                (service, start.elapsed().as_secs_f64(), res)
            });
        }
        while let Some((service, dt, res)) = futs.next().await {
            match res {
                Ok(Some((result_id, name, artists, _s))) => {
                    obs.push((service.clone(), dt, true));
                    return (
                        Some(Resolution {
                            service,
                            result_id,
                            name,
                            artists,
                            tier,
                        }),
                        obs,
                    );
                }
                _ => obs.push((service, dt, false)),
            }
        }
        (None, obs)
    }

    /// Resolve `query` to the first provider with a relevant search hit.
    pub async fn resolve(&mut self, query: &str, tier: QualityTier) -> Result<Resolution> {
        for t in tier.fallback_order() {
            for chunk in self.chain_for(t).chunks(self.max_parallel.max(1)) {
                let batch: Vec<String> = chunk.to_vec();
                let (hit, obs) = self.race(t, query, &batch).await;
                for (s, dt, ok) in obs {
                    self.record(&s, dt, ok);
                }
                if let Some(r) = hit {
                    save_scores(&self.scores_path, &self.scores);
                    return Ok(r);
                }
            }
        }
        Err(Error::Other(format!("resolver: no source has '{query}'")))
    }

    /// Resolve and download, falling back across providers and tiers.
    pub async fn resolve_and_download(
        &mut self,
        downloader: &Downloader,
        query: &str,
        tier: QualityTier,
    ) -> Result<PathBuf> {
        let mut last_err: Option<Error> = None;
        for t in tier.fallback_order() {
            let mut chain = self.chain_for(t);
            if chain.is_empty() {
                continue;
            }
            info!("resolver: tier {} -> {}", t.as_str(), chain.join(", "));
            while !chain.is_empty() {
                let batch: Vec<String> = chain
                    .iter()
                    .take(self.max_parallel.max(1))
                    .cloned()
                    .collect();
                let (hit, obs) = self.race(t, query, &batch).await;
                for (s, dt, ok) in obs {
                    self.record(&s, dt, ok);
                }
                match hit {
                    Some(r) => {
                        let result = if is_direct_track(&r.service) {
                            let mut data = HashMap::new();
                            data.insert(
                                "__track_name__".to_string(),
                                serde_json::Value::String(r.name.clone()),
                            );
                            if let Some(a) = r.artists.first() {
                                data.insert(
                                    "__artist__".to_string(),
                                    serde_json::Value::String(a.clone()),
                                );
                            }
                            downloader
                                .download_track_with_data(&r.service, &r.result_id, data)
                                .await
                        } else {
                            downloader
                                .download_album(&r.service, &r.result_id)
                                .await
                                .and_then(|files| {
                                    pick_track(&files, query)
                                        .or_else(|| files.into_iter().next())
                                        .ok_or_else(|| {
                                            Error::Download("album produced no files".into())
                                        })
                                })
                        };
                        match result {
                            Ok(p) => {
                                self.record(&r.service, 0.5, true);
                                save_scores(&self.scores_path, &self.scores);
                                info!(
                                    "resolver: '{}' -> {} [{}]",
                                    query,
                                    r.service,
                                    r.tier.as_str()
                                );
                                return Ok(p);
                            }
                            Err(e) => {
                                warn!(
                                    "resolver: {} matched '{}' but download failed: {}",
                                    r.service, query, e
                                );
                                self.record(&r.service, 5.0, false);
                                chain.retain(|s| *s != r.service);
                                last_err = Some(e);
                            }
                        }
                    }
                    None => {
                        chain.retain(|s| !batch.contains(s));
                    }
                }
            }
        }
        save_scores(&self.scores_path, &self.scores);
        Err(last_err.unwrap_or_else(|| Error::Other(format!("resolver: no source has '{query}'"))))
    }
}

/// Providers that resolve a single track URL directly (vs album/archive pages).
fn is_direct_track(service: &str) -> bool {
    matches!(
        service,
        "zvu4it" | "tancpol" | "ccmixter" | "soundcloud" | "youtube"
    )
}

/// Score a search result against "artist - title" by token overlap.
fn score_result(query: &str, r: &SearchResult) -> f64 {
    let (qa, qt) = textmatch::split_query(query);
    let artists = r.artists.as_ref().map(|v| v.join(" ")).unwrap_or_default();
    let name = r.name.as_deref().unwrap_or("");
    let combined = format!("{name} {artists}");
    let full = match &qa {
        Some(a) => format!("{a} {qt}"),
        None => qt.clone(),
    };
    let s_full = textmatch::similarity(&full, &combined);
    match &qa {
        Some(a) => {
            let s_artist = r
                .artists
                .as_ref()
                .map(|v| {
                    v.iter()
                        .map(|x| textmatch::similarity(a, x))
                        .fold(0.0f64, f64::max)
                })
                .unwrap_or(0.0);
            let s_title = textmatch::similarity(&qt, name);
            0.4 * s_artist + 0.6 * s_full.max(s_title)
        }
        None => s_full,
    }
}

/// Pick the extracted file whose name best matches the requested title.
fn pick_track(files: &[PathBuf], query: &str) -> Option<PathBuf> {
    let (_, title) = textmatch::split_query(query);
    files
        .iter()
        .max_by(|a, b| {
            let sa =
                textmatch::similarity(&title, a.file_stem().and_then(|s| s.to_str()).unwrap_or(""));
            let sb =
                textmatch::similarity(&title, b.file_stem().and_then(|s| s.to_str()).unwrap_or(""));
            sa.partial_cmp(&sb).unwrap_or(std::cmp::Ordering::Equal)
        })
        .cloned()
}

fn load_scores(path: &Path) -> HashMap<String, f64> {
    let doc: serde_json::Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or(serde_json::Value::Null);
    doc.get("providers")
        .and_then(|p| p.as_object())
        .map(|o| {
            o.iter()
                .filter_map(|(k, v)| {
                    v.get("score")
                        .and_then(|s| s.as_f64())
                        .map(|f| (k.clone(), f))
                })
                .collect()
        })
        .unwrap_or_default()
}

fn save_scores(path: &Path, scores: &HashMap<String, f64>) {
    let mut providers = serde_json::Map::new();
    for (k, v) in scores {
        providers.insert(k.clone(), serde_json::json!({ "score": v }));
    }
    let doc = serde_json::json!({ "providers": providers });
    if let Some(p) = path.parent() {
        let _ = std::fs::create_dir_all(p);
    }
    if let Ok(s) = serde_json::to_string_pretty(&doc) {
        let _ = std::fs::write(path, s);
    }
}
