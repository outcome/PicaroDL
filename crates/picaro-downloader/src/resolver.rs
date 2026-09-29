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
    /// Rolling P2P transfer health (speed strikes + benching), persisted
    /// next to the scores so slow/dead P2P services get skipped outright.
    health: HashMap<String, P2PHealth>,
    health_path: PathBuf,
 timeout: Duration,
 timeout_lossless: Duration,
 max_parallel: usize,
    min_match: f64,
    allow_mixed_sources: bool,
    allow_mixed_quality: bool,
    allow: Option<Vec<String>>,
    tier_order: HashMap<QualityTier, Vec<String>>,
}

/// Speed/failure history for one P2P service.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
struct P2PHealth {
    strikes: u32,
    benched: bool,
    last_speed_kbps: f64,
    /// Resolves passed since the service was benched; a probe is allowed
    /// every `p2p.probe_interval` resolves.
    since_probe: u32,
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
        let allow_mixed_sources = g.get_bool_or("resolver", "allow_mixed_sources", false);
        let allow_mixed_quality = g.get_bool_or("resolver", "allow_mixed_quality", true);
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
        let health_path = scores_path.with_file_name("p2p-health.json");
        let health = std::fs::read_to_string(&health_path)
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default();
        Self {
            picaro,
            scores,
            scores_path,
            health,
            health_path,
 timeout,
 timeout_lossless,
 max_parallel,
            min_match,
            allow_mixed_sources,
            allow_mixed_quality,
            allow,
            tier_order,
        }
    }

    /// Benchmark-derived default provider order for a tier (all no-signin).
    fn default_chain(tier: QualityTier) -> Vec<&'static str> {
        match tier {
            QualityTier::Lossless => vec![
                // Direct-download sources first; P2P (soulseek) last: a
                // hung transfer or dead swarm must never sit in front of
                // a working HTTP source. Torrents (piratebay/darktorrent)
                // are appended after this list when [torrent] enabled.
                "technicaldeathmetal",
                "coreradio",
                "ektoplazm",
                "relisten",
                "soulseek",
            ],
            QualityTier::High => vec![
                "grimearchive",
                "globaldjmix",
                "zvu4it",
                "tancpol",
                "ccmixter",
                "freemp3cloud",
                "iplusfree",
                "mp3db",
                "ezhevika",
                "butterboy",
                "punkcata",
                "primitiveofferings",
                "deadpulpit",
                "fma",
                "tomlehrer",
                "testpressing",
                "mp3zona",
                "mp3tut",
                "onetrance",
                "systemsofromance",
                "relisten",
                "mixtapemonkey",
                "certifiedmixtapez",
                "soundcloud",
                "soulseek",
                "youtube",
            ],
            QualityTier::Medium => vec![
                "zvu4it",
                "tancpol",
                "iplusfree",
                "mp3db",
                "ccmixter",
                "butterboy",
                "punkcata",
                "ezhevika",
                "primitiveofferings",
                "deadpulpit",
                "fma",
                "tomlehrer",
                "testpressing",
                "mp3zona",
                "mp3tut",
                "onetrance",
                "systemsofromance",
                "relisten",
                "mixtapemonkey",
                "certifiedmixtapez",
                "soundcloud",
                "soulseek",
                "youtube",
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

    /// Restrict the resolver to a single provider (forced use / testing).
    pub fn set_only(&mut self, service: Option<String>) {
        self.allow = service.map(|s| vec![s.to_lowercase()]);
    }

    fn chain_for(&self, tier: QualityTier) -> Vec<String> {
        if let Some(a) = &self.allow {
            return a.clone();
        }
        let configured = self.tier_order.get(&tier);
        let list: Vec<String> = match configured {
            Some(v) if !v.is_empty() => v.clone(),
            _ => Self::default_chain(tier)
                .into_iter()
                .map(|s| s.to_string())
                .collect(),
        };
        let torrents_on = self
            .picaro
            .merged_globals
            .get("torrent")
            .and_then(|v| v.get("enabled"))
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let mut list = list;
        for torrent_module in ["piratebay", "darktorrent"] {
            if torrents_on && !list.iter().any(|s| s == torrent_module) {
                list.push(torrent_module.to_string());
            }
        }
        let mut out: Vec<String> = list
            .into_iter()
            .filter(|s| self.registered(s))
            .filter(|s| torrents_on || (s != "piratebay" && s != "darktorrent"))
            .collect();
        // Deprioritise Opus-only providers so a native codec wins when the
        // source offers one. P2P (soulseek, torrents) stays pinned behind
        // every direct source no matter how well it scored: a dead swarm
        // or hung transfer in front is the worst failure mode. Within the
        // remaining groups the self-tuning score still decides.
        out.sort_by(|a, b| {
            let p2p = |s: &str| matches!(s, "soulseek" | "piratebay" | "darktorrent");
            let oa = is_opus_provider(a) || p2p(a);
            let ob = is_opus_provider(b) || p2p(b);
            oa.cmp(&ob).then_with(|| {
                self.score_of(b)
                    .partial_cmp(&self.score_of(a))
                    .unwrap_or(std::cmp::Ordering::Equal)
            })
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
            // Soulseek is P2P: its own search waits ~15s for peers to
            // answer, so the 4s probe would kill it before any results.
            let to = if service == "soulseek" {
                to.max(std::time::Duration::from_secs(25))
            } else {
                to
            };
            let min_match = self.min_match;
            // Torrent viability gate: a magnet with no seeders is a dead
            // end that would hang the engine - never pick one below
            // [torrent] min_seeders. PirateBay carries the count in
            // `extra_kwargs.seeders`.
            let min_seeders: u64 = self
                .picaro
                .merged_globals
                .get("torrent")
                .and_then(|v| v.get("min_seeders"))
                .and_then(|v| v.as_u64())
                .unwrap_or(5);
            futs.push(async move {
                let start = Instant::now();
                let res = tokio::time::timeout(to, async {
                    let m = picaro.load_module(&service).await.ok()?;
                    // 25, not 8: weak-search modules (blogspot labels,
                    // DLE recency sidebars) push real matches deep into
                    // the result list; search returns metadata only, so
                    // a full page is cheap.
                    let results = m.search(DownloadType::track, &q, None, 25).await.ok()?;
                    let mut best: Option<(f64, SearchResult)> = None;
                    for r in results {
                        let seeders = r
                            .extra_kwargs
                            .get("seeders")
                            .and_then(|v| v.as_u64())
                            .unwrap_or(u64::MAX);
                        if seeders < min_seeders {
                            continue;
                        }
                        let s = score_result(&q, &r);
                        if best.as_ref().map_or(true, |(bs, _)| s > *bs) {
                            best = Some((s, r));
                        }
                    }
                    // Many sites phrase-match the " - " separator and
                    // return nothing for "artist - title" queries; retry
                    // once with plain words (scored against the original
                    // query) before giving up on this service.
                    let weak = best
                        .as_ref()
                        .map_or(true, |(s, _)| *s < min_match);
                    if weak && q.contains(" - ") {
                        let alt = q.replace(" - ", " ");
                        if let Ok(more) = m.search(DownloadType::track, &alt, None, 25).await {
                            for r in more {
                                let s = score_result(&q, &r);
                                if best.as_ref().map_or(true, |(bs, _)| s > *bs) {
                                    best = Some((s, r));
                                }
                            }
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
    ///
    /// ONE search wave queries every source at once — no tier-by-tier
    /// sweeps that re-search the same sites four times. Candidates then
    /// run in priority order:
    ///
    ///   1. direct lossless sources (fast HTTP FLAC)
    ///   2. seeded P2P (torrents / Soulseek) — the torrent engine drops
    ///      a swarm that is still under 70% after 120s, so P2P never
    ///      holds the resolve hostage
    ///   3. direct MP3 sources
    ///   4. Opus stream providers
    ///
    /// The requested tier is a ceiling: sources that only deliver
    /// higher-tier content sit out when a lower tier was asked for.
    pub async fn resolve_and_download(
        &mut self,
        downloader: &Downloader,
        query: &str,
        tier: QualityTier,
    ) -> Result<PathBuf> {
        let mut last_err: Option<Error> = None;
        let min_match = self.min_match;

        let is_p2p = |s: &str| matches!(s, "soulseek" | "piratebay" | "darktorrent");
        let lossless_src = |s: &str| {
            matches!(
                s,
                "technicaldeathmetal" | "coreradio" | "ektoplazm" | "relisten"
            )
        };

        // Service pool: --only, or the union of every tier's chain.
        let mut services: Vec<String> = match &self.allow {
            Some(a) => a.clone(),
            None => {
                let mut v: Vec<String> = Vec::new();
                for t in [
                    QualityTier::Lossless,
                    QualityTier::High,
                    QualityTier::Medium,
                    QualityTier::Low,
                ] {
                    for s in Self::default_chain(t) {
                        if !v.iter().any(|x| x == s) {
                            v.push(s.to_string());
                        }
                    }
                }
                let torrents_on = self
                    .picaro
                    .merged_globals
                    .get("torrent")
                    .and_then(|v| v.get("enabled"))
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                for m in ["piratebay", "darktorrent"] {
                    if torrents_on && !v.iter().any(|x| x == m) {
                        v.push(m.to_string());
                    }
                }
                v.into_iter().filter(|s| self.registered(s)).collect()
            }
        };
        // Ceiling: a FLAC-only source doesn't join when a lower tier
        // was requested (the request is never upgraded).
        if tier != QualityTier::Lossless {
            services.retain(|s| !lossless_src(s));
        }
        // Bench the slow: a P2P service with a recent record of slow or
        // failed transfers sits the whole resolve out (its search, too -
        // Soulseek's 15s window is the fixed cost we most want to skip),
        // until its probe interval comes due.
        let g = GlobalSettings::from_merged(&self.picaro.merged_globals);
        let probe_interval = g.get_int_or("p2p", "probe_interval", 10).max(1) as u32;
        let min_speed_kbps =
            g.get("p2p", "min_speed_kbps").and_then(|v| v.as_f64()).unwrap_or(128.0);
        let slow_strikes = g.get_int_or("p2p", "slow_strikes", 3).max(1) as u32;
        services.retain(|s| !is_p2p(s) || self.p2p_allowed_in_wave(s, probe_interval));
        services.dedup();
        if services.is_empty() {
            return Err(Error::Other(format!("resolver: no source has '{query}'")));
        }
        info!("resolver: wave [{}] -> {}", tier.as_str(), services.join(", "));

        // Fire every search simultaneously. P2P windows are longer:
        // Soulseek's own search waits ~15s for peers to answer.
        let min_seeders: u64 = self
            .picaro
            .merged_globals
            .get("torrent")
            .and_then(|v| v.get("min_seeders"))
            .and_then(|v| v.as_u64())
            .unwrap_or(5);
        let mut direct_futs = FuturesUnordered::new();
        let mut p2p_futs = FuturesUnordered::new();
        for s in services {
            let p2p = is_p2p(&s);
            let to = if p2p {
                self.timeout.max(Duration::from_secs(25))
            } else {
                self.timeout
            };
            let picaro = self.picaro.clone();
            let q = query.to_string();
            let fut = async move {
                let start = Instant::now();
                let hits = search_service_hits(&picaro, &s, &q, min_match, min_seeders, to).await;
                (s, start.elapsed().as_secs_f64(), hits)
            };
            if p2p {
                p2p_futs.push(fut);
            } else {
                direct_futs.push(fut);
            }
        }

        // Collect direct hits (all bounded by the probe timeout; a short
        // grace covers module loading).
        let mut direct_hits: Vec<(String, f64, SearchResult)> = Vec::new();
        let direct_deadline =
            tokio::time::Instant::from_std(Instant::now() + self.timeout + Duration::from_secs(4));
        while let Some((s, dt, hits)) = tokio::time::timeout_at(direct_deadline, direct_futs.next())
            .await
            .ok()
            .flatten()
        {
            let found = hits.as_ref().map_or(false, |h| !h.is_empty());
            self.record(&s, dt, found);
            if let Some(h) = hits {
                for (score, r) in h {
                    direct_hits.push((s.clone(), score, r));
                }
            }
        }

        // Group 1: direct lossless sources. Fast HTTP FLAC beats P2P at
        // equal quality.
        let mut g_lossless: Vec<_> = direct_hits
            .iter()
            .filter(|(s, _, _)| lossless_src(s))
            .cloned()
            .collect::<Vec<(String, f64, SearchResult)>>();
        g_lossless.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (service, _, r) in &g_lossless {
            match self
                .attempt(downloader, query, service, r, QualityTier::Lossless, tier)
                .await
            {
                Ok(p) => return Ok(self.finish(service, query, p)),
                Err(e) => {
                    self.record(service, 5.0, false);
                    warn!("resolver: {} matched '{}' but failed: {}", service, query, e);
                    last_err = Some(e);
                }
            }
        }

        // Group 2: P2P. Only seeded torrents survive the search filter;
        // the torrent engine enforces the 120s / 70% rule and Soulseek
        // caps transfers at 120s. The phase itself gives up after ~100s
        // of wall time so a dead first swarm doesn't stack a second.
        let mut p2p_hits: Vec<(String, f64, SearchResult)> = Vec::new();
        let p2p_deadline =
            tokio::time::Instant::from_std(Instant::now() + Duration::from_secs(30));
        while let Some((s, dt, hits)) = tokio::time::timeout_at(p2p_deadline, p2p_futs.next())
            .await
            .ok()
            .flatten()
        {
            let found = hits.as_ref().map_or(false, |h| !h.is_empty());
            self.record(&s, dt, found);
            if let Some(h) = hits {
                for (score, r) in h {
                    p2p_hits.push((s.clone(), score, r));
                }
            }
        }
        p2p_hits.sort_by(|a, b| {
            let seeders = |r: &SearchResult| {
                r.extra_kwargs
                    .get("seeders")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0)
            };
            seeders(&b.2)
                .cmp(&seeders(&a.2))
                .then(b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal))
        });
        // P2P contract, verbatim: "check for song, if seeded, download.
        // If it's absurdly slow (2min+), find another source." The PHASE
        // owns one hard 120s budget shared by at most two attempts, and
        // each attempt is capped to the remaining time — P2P can never
        // cost more than two minutes before the resolve moves on.
        let phase_start = Instant::now();
        let phase_budget = Duration::from_secs(120);
        let mut p2p_tries = 0u32;
        for (service, _, r) in &p2p_hits {
            if p2p_tries >= 2 {
                break;
            }
            let Some(remaining) = phase_budget.checked_sub(phase_start.elapsed()) else {
                break;
            };
            if remaining < Duration::from_secs(10) {
                break; // not enough left to be worth starting
            }
            p2p_tries += 1;
            let t0 = Instant::now();
            let attempt_fut = self.attempt(downloader, query, service, r, tier, tier);
            let outcome = match tokio::time::timeout(remaining, attempt_fut).await {
                Ok(res) => res,
                Err(_) => {
                    warn!("resolver: {service} blew the P2P time budget; moving on");
                    Err(Error::Download(format!(
                        "{service} exceeded the 120s P2P budget"
                    )))
                }
            };
            match outcome {
                Ok(p) => {
                    // Measure the delivered transfer speed: sustained
                    // slowness benches the service (settings: p2p.*).
                    let dt = t0.elapsed().as_secs_f64().max(0.1);
                    let kb = tokio::fs::metadata(&p)
                        .await
                        .map(|m| m.len() as f64)
                        .unwrap_or(0.0)
                        / 1024.0;
                    let speed = kb / dt;
                    self.p2p_report(service, true, speed, min_speed_kbps, slow_strikes);
                    return Ok(self.finish(service, query, p));
                }
                Err(e) => {
                    self.p2p_report(service, false, 0.0, min_speed_kbps, slow_strikes);
                    self.record(service, 5.0, false);
                    warn!("resolver: {} matched '{}' but failed: {}", service, query, e);
                    last_err = Some(e);
                }
            }
        }

        // Group 3: direct MP3 sources (best-scored first), then group 4:
        // Opus stream providers.
        let expected_direct = if tier == QualityTier::Lossless {
            QualityTier::High
        } else {
            tier
        };
        let mut g_direct: Vec<_> = direct_hits
            .iter()
            .filter(|(s, _, _)| !lossless_src(s) && !is_opus_provider(s))
            .cloned()
            .collect::<Vec<(String, f64, SearchResult)>>();
        g_direct.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (service, _, r) in g_direct {
            match self
                .attempt(downloader, query, &service, &r, expected_direct, tier)
                .await
            {
                Ok(p) => return Ok(self.finish(&service, query, p)),
                Err(e) => {
                    self.record(&service, 5.0, false);
                    warn!("resolver: {} matched '{}' but failed: {}", service, query, e);
                    last_err = Some(e);
                }
            }
        }
        let mut g_opus: Vec<_> = direct_hits
            .iter()
            .filter(|(s, _, _)| is_opus_provider(s))
            .cloned()
            .collect::<Vec<(String, f64, SearchResult)>>();
        g_opus.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (service, _, r) in g_opus {
            // Opus streams are the last-resort tier; quality-check them
            // laxly (a lossless request accepting an opus fallback is
            // the designed behavior, not a guard violation).
            let expected = if tier == QualityTier::Lossless {
                QualityTier::High
            } else {
                tier
            };
            match self
                .attempt(downloader, query, &service, &r, expected, tier)
                .await
            {
                Ok(p) => return Ok(self.finish(&service, query, p)),
                Err(e) => {
                    self.record(&service, 5.0, false);
                    warn!("resolver: {} matched '{}' but failed: {}", service, query, e);
                    last_err = Some(e);
                }
            }
        }

        save_scores(&self.scores_path, &self.scores);
        Err(last_err.unwrap_or_else(|| Error::Other(format!("resolver: no source has '{query}'"))))
    }

    /// Download one candidate (direct track or album container), verify
    /// quality against `expected`, and hand back the produced path.
    async fn attempt(
        &self,
        downloader: &Downloader,
        query: &str,
        service: &str,
        hit: &SearchResult,
        expected: QualityTier,
        _ceiling: QualityTier,
    ) -> Result<PathBuf> {
        let name = hit.name.clone().unwrap_or_default();
        let result_id = hit.result_id.clone();
        let artists = hit.artists.clone().unwrap_or_default();
        let cap_secs = self
            .picaro
            .merged_globals
            .get("resolver")
            .and_then(|v| v.get("download_timeout_secs"))
            .and_then(|v| v.as_u64())
            .unwrap_or(360)
            .max(60);
        let cap = Duration::from_secs(cap_secs);
        let result = tokio::time::timeout(cap, async {
            if is_direct_track(service) {
                let mut data = HashMap::new();
                data.insert(
                    "__track_name__".to_string(),
                    serde_json::Value::String(name.clone()),
                );
                if let Some(a) = artists.first() {
                    data.insert("__artist__".to_string(), serde_json::Value::String(a.clone()));
                }
                downloader
                    .download_track_with_data(service, &result_id, data)
                    .await
            } else {
                downloader
                    .download_album(service, &result_id)
                    .await
                    .and_then(|files| {
                        pick_track(&files, query)
                            .or_else(|| files.into_iter().next())
                            .ok_or_else(|| Error::Download("album produced no files".into()))
                    })
            }
        })
        .await
        .unwrap_or_else(|_| {
            warn!("resolver: {service} download exceeded {cap_secs}s; moving on");
            Err(Error::Download(format!(
                "{service} download timed out after {cap_secs}s"
            )))
        });
        let p = result?;
        if !quality_ok(&p, expected) {
            let _ = std::fs::remove_file(&p);
            return Err(Error::Download(format!(
                "{service} returned a file that doesn't match {expected:?} quality (wrong container / likely fake)"
            )));
        }
        Ok(p)
    }

    fn finish(&mut self, service: &str, query: &str, p: PathBuf) -> PathBuf {
        self.record(service, 0.5, true);
        save_scores(&self.scores_path, &self.scores);
        // Report what actually landed on disk, not what was requested.
        let delivered = if quality_ok(&p, QualityTier::Lossless) {
            "lossless"
        } else {
            "lossy"
        };
        info!("resolver: '{}' -> {} [{}]", query, service, delivered);
        p
    }

    fn save_health(&self) {
        if let Ok(s) = serde_json::to_string_pretty(&self.health) {
            let _ = std::fs::write(&self.health_path, s);
        }
    }

    /// May this P2P service join the search wave? Benched services sit
    /// out until `p2p.probe_interval` resolves pass, then rejoin for one
    /// probe attempt.
    fn p2p_allowed_in_wave(&mut self, service: &str, probe_interval: u32) -> bool {
        let h = self.health.entry(service.to_string()).or_default();
        if !h.benched {
            return true;
        }
        h.since_probe += 1;
        if h.since_probe >= probe_interval {
            h.since_probe = 0;
            info!("resolver: {service} benched for slowness - probe attempt");
            true
        } else {
            false
        }
    }

    /// Record a P2P attempt outcome: successes below `min_speed_kbps` and
    /// failures count as strikes; `slow_strikes` in a row benches the
    /// service.
    fn p2p_report(
        &mut self,
        service: &str,
        ok: bool,
        speed_kbps: f64,
        min_speed_kbps: f64,
        slow_strikes: u32,
    ) {
        let h = self.health.entry(service.to_string()).or_default();
        if ok {
            h.last_speed_kbps = speed_kbps;
            if speed_kbps >= min_speed_kbps {
                if h.benched {
                    info!("resolver: {service} unbenched (delivered at {speed_kbps:.0} kbps)");
                }
                h.strikes = 0;
                h.benched = false;
                self.save_health();
                return;
            }
        }
        h.strikes += 1;
        if h.strikes >= slow_strikes && !h.benched {
            h.benched = true;
            h.since_probe = 0;
            warn!(
                "resolver: benching {service} - consistently slow/failing P2P \
                 (last speed {:.0} kbps, min {min_speed_kbps:.0}); skipping it for now",
                h.last_speed_kbps
            );
        }
        self.save_health();
    }
}

/// Search one service and return every result that clears `min_match`
/// (plus the ` - `-stripped retry when the first pass finds nothing).
/// Torrent results below `min_seeders` are dropped before they can be
/// picked — a dead swarm must never be attempted.
async fn search_service_hits(
    picaro: &std::sync::Arc<Picaro>,
    service: &str,
    q: &str,
    min_match: f64,
    min_seeders: u64,
    to: Duration,
) -> Option<Vec<(f64, SearchResult)>> {
    tokio::time::timeout(to, async {
        let m = picaro.load_module(service).await.ok()?;
        // 25, not 8: weak-search modules (blogspot labels, DLE recency
        // sidebars) push real matches deep into the result list.
        let mut hits: Vec<(f64, SearchResult)> = Vec::new();
        let mut scan = |results: Vec<SearchResult>, hits: &mut Vec<(f64, SearchResult)>| {
            for r in results {
                let seeders = r
                    .extra_kwargs
                    .get("seeders")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(u64::MAX);
                if seeders < min_seeders {
                    continue;
                }
                let s = score_result(q, &r);
                if s >= min_match {
                    hits.push((s, r));
                }
            }
        };
        match m.search(DownloadType::track, q, None, 25).await {
            Ok(results) => scan(results, &mut hits),
            Err(_) => {}
        }
        // Many sites phrase-match the " - " separator and return nothing
        // for "artist - title" queries; retry with plain words (scored
        // against the original query).
        if hits.is_empty() && q.contains(" - ") {
            let alt = q.replace(" - ", " ");
            if let Ok(more) = m.search(DownloadType::track, &alt, None, 25).await {
                scan(more, &mut hits);
            }
        }
        Some(hits)
    })
    .await
    .ok()
    .flatten()
}

/// Providers that resolve a single track URL directly (vs album/archive pages).
fn is_direct_track(service: &str) -> bool {
    matches!(
        service,
        "zvu4it"
            | "tancpol"
            | "ccmixter"
            | "soundcloud"
            | "youtube"
            | "grimearchive"
            | "globaldjmix"
            | "freemp3cloud"
            | "onetrance"
            | "fma"
            | "tomlehrer"
            | "testpressing"
            | "mp3zona"
            | "mp3tut"
            | "soulseek"
    )
}

/// Providers that only ever deliver Opus (no native MP3/AAC/M4A alternative).
/// The resolver deprioritises these so a native codec is chosen when available.
fn is_opus_provider(service: &str) -> bool {
    matches!(service, "youtube" | "soundcloud")
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
    let s_artist = textmatch::similarity(&qt, &artists);
    let s_title = textmatch::similarity(&qt, name);
    match &qa {
        Some(a) => {
            if artists.trim().is_empty() {
                // Result carries no artist metadata: score by requiring the
                // FULL query (artist + title tokens) to overlap the result
                // name, so a same-title/different-artist hit still scores low.
                s_full
            } else {
                let a_match =
                    textmatch::similarity(a, &artists).max(textmatch::similarity(a, &combined));
                0.5 * a_match + 0.5 * s_full.max(s_title)
            }
        }
        // No explicit artist: accept if the query matches the artist, the
        // title, or the combined string well.
        None => s_full.max(s_artist).max(s_title),
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

/// Heuristic quality check: reject output that clearly doesn't match the tier
/// (wrong container for lossless, or a lossless request that is really a
/// low-bitrate transcode / "fake FLAC"). Returns true when acceptable.
fn quality_ok(path: &Path, tier: QualityTier) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    let lossless_ext = matches!(
        ext.as_str(),
        "flac" | "wav" | "aiff" | "aif" | "ape" | "wv" | "alac"
    );
    if tier == QualityTier::Lossless && !lossless_ext {
        return false;
    }
    let Some(probe) = picaro_tagging::audio_probe(path) else {
        return true;
    };
    if probe.duration_secs <= 5.0 {
        return true;
    }
    let size = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) as f64;
    let kbps = size * 8.0 / probe.duration_secs / 1000.0;
    // Only enforce a bitrate floor for lossless (to catch fake/transcoded
    // FLACs); mp3/opus/m4a are accepted at any bitrate.
    if tier == QualityTier::Lossless {
        kbps >= 500.0
    } else {
        true
    }
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
