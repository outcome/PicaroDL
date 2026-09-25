//! Keyless metadata backfill for tracks with missing name/album/artist/cover.
//!
//! All sources are public, need no API key and no sign-in:
//!   1. Deezer      - api.deezer.com/search          (title/artist/album/cover_xl)
//!   2. iTunes      - itunes.apple.com/search        (trackName/artistName/collectionName/artworkUrl100)
//!   3. MusicBrainz - musicbrainz.org/ws/2/recording (canonical names)
//!   4. Cover Art Archive - coverartarchive.org/release/{mbid}
//!
//! Fields are only filled when currently empty, so existing module metadata is
//! never overwritten.

use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use serde_json::Value;

use crate::models::TrackInfo;
use crate::textmatch;

const UA: &str = "PicaroDL/0.1 (+https://example.invalid)";

fn enc(q: &str) -> String {
    utf8_percent_encode(q, NON_ALPHANUMERIC).to_string()
}

fn missing(s: &str) -> bool {
    let t = s.trim();
    t.is_empty() || t.eq_ignore_ascii_case("unknown") || t.eq_ignore_ascii_case("unknown artist")
}

fn strip_quality_suffix(s: &str) -> String {
    s.replace("(FLAC)", "")
        .replace("(MP3)", "")
        .replace("(M4A)", "")
        .replace("(WAV)", "")
        .trim()
        .to_string()
}

async fn get_json(client: &reqwest::Client, url: &str) -> Option<Value> {
    let resp = client
        .get(url)
        .header("User-Agent", UA)
        .header("Accept", "application/json")
        .timeout(std::time::Duration::from_secs(10))
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.json::<Value>().await.ok()
}

async fn deezer_lookup(client: &reqwest::Client, query: &str) -> Option<Value> {
    let url = format!("https://api.deezer.com/search?q={}&limit=5", enc(query));
    let v = get_json(client, &url).await?;
    v.get("data")?.as_array()?.first().cloned()
}

/// Deezer search that scores every candidate against the wanted title and
/// returns the best match, penalising live/remix/instrumental/karaoke variants.
async fn deezer_lookup_best(client: &reqwest::Client, query: &str, want: &str) -> Option<Value> {
    let url = format!("https://api.deezer.com/search?q={}&limit=10", enc(query));
    let v = get_json(client, &url).await?;
    let arr = v.get("data")?.as_array()?;
    let bad = [
        "(live",
        "live at",
        "live from",
        "remix",
        "instrumental",
        "karaoke",
        "cover",
        "(edit",
        "radio edit",
        "(demo",
        "demo)",
        "rehearsal",
        "acoustic",
        "session",
    ];
    let mut best: Option<(f64, Value)> = None;
    for it in arr {
        let t = it.get("title").and_then(|x| x.as_str()).unwrap_or("");
        let mut s = textmatch::similarity(want, t);
        let lt = t.to_lowercase();
        if bad.iter().any(|b| lt.contains(b)) {
            s -= 0.35;
        }
        if best.as_ref().map_or(true, |(bs, _)| s > *bs) {
            best = Some((s, it.clone()));
        }
    }
    best.map(|(_, v)| v)
}

async fn itunes_lookup(client: &reqwest::Client, query: &str) -> Option<Value> {
    let url = format!(
        "https://itunes.apple.com/search?term={}&media=music&limit=5",
        enc(query)
    );
    let v = get_json(client, &url).await?;
    v.get("results")?.as_array()?.first().cloned()
}

async fn musicbrainz_lookup(client: &reqwest::Client, artist: &str, title: &str) -> Option<Value> {
    let q = if artist.is_empty() {
        format!("recording:\"{title}\"")
    } else {
        format!("recording:\"{title}\" AND artist:\"{artist}\"")
    };
    let url = format!(
        "https://musicbrainz.org/ws/2/recording?query={}&fmt=json&limit=5",
        enc(&q)
    );
    let v = get_json(client, &url).await?;
    v.get("recordings")?.as_array()?.first().cloned()
}

/// Fill empty `name`/`album`/`artists`/`cover_url` on `info` from keyless
/// sources. Never overwrites a non-empty field. Returns the filled field names.
pub async fn fill_track_metadata(
    client: &reqwest::Client,
    info: &mut TrackInfo,
) -> Vec<&'static str> {
    let mut filled: Vec<&'static str> = Vec::new();

    let mut artist = info.artists.first().cloned().unwrap_or_default();
    let name_clean = strip_quality_suffix(&info.name);
    let mut title = if info.album.trim().is_empty() {
        name_clean.clone()
    } else {
        info.album.clone()
    };
    if artist.trim().is_empty() {
        if let Some(idx) = name_clean.find(" - ") {
            artist = name_clean[..idx].trim().to_string();
            title = name_clean[idx + 3..].trim().to_string();
        }
    }
    let query = format!("{artist} {title}").trim().to_string();
    if query.is_empty() {
        return filled;
    }

    let need_name = missing(&info.name);
    let need_album = missing(&info.album);
    let need_artist = info.artists.iter().all(|a| a.trim().is_empty());
    let need_cover = info.cover_url.trim().is_empty();

    // 1. Deezer (single call covers name/artist/album + high-res cover).
    if need_name || need_album || need_artist || need_cover {
        if let Some(hit) = deezer_lookup_best(client, &query, &title).await {
            if need_name {
                if let Some(t) = hit.get("title").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        info.name = t.to_string();
                        filled.push("name");
                    }
                }
            }
            if need_artist {
                if let Some(a) = hit.pointer("/artist/name").and_then(|v| v.as_str()) {
                    if !a.is_empty() {
                        info.artists = vec![a.to_string()];
                        filled.push("artist");
                    }
                }
            }
            if need_album {
                if let Some(al) = hit.pointer("/album/title").and_then(|v| v.as_str()) {
                    if !al.is_empty() {
                        info.album = al.to_string();
                        filled.push("album");
                    }
                }
            }
            if need_cover {
                if let Some(c) = hit.pointer("/album/cover_xl").and_then(|v| v.as_str()) {
                    if !c.is_empty() {
                        info.cover_url = c.to_string();
                        filled.push("cover");
                    }
                }
            }
        }
    }

    // 2. iTunes fallback for anything still missing (esp. cover).
    let still = missing(&info.name)
        || missing(&info.album)
        || info.artists.iter().all(|a| a.trim().is_empty())
        || info.cover_url.trim().is_empty();
    if still {
        if let Some(hit) = itunes_lookup(client, &query).await {
            if missing(&info.name) {
                if let Some(t) = hit.get("trackName").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        info.name = t.to_string();
                        filled.push("name");
                    }
                }
            }
            if info.artists.iter().all(|a| a.trim().is_empty()) {
                if let Some(a) = hit.get("artistName").and_then(|v| v.as_str()) {
                    if !a.is_empty() {
                        info.artists = vec![a.to_string()];
                        filled.push("artist");
                    }
                }
            }
            if missing(&info.album) {
                if let Some(al) = hit.get("collectionName").and_then(|v| v.as_str()) {
                    if !al.is_empty() {
                        info.album = al.to_string();
                        filled.push("album");
                    }
                }
            }
            if info.cover_url.trim().is_empty() {
                if let Some(c) = hit.get("artworkUrl100").and_then(|v| v.as_str()) {
                    if !c.is_empty() {
                        info.cover_url = c.replace("100x100bb", "1000x1000bb");
                        filled.push("cover");
                    }
                }
            }
        }
    }

    // 3. MusicBrainz (canonical names) + Cover Art Archive for the cover.
    let still = missing(&info.name)
        || missing(&info.album)
        || info.artists.iter().all(|a| a.trim().is_empty())
        || info.cover_url.trim().is_empty();
    if still {
        if let Some(rec) = musicbrainz_lookup(client, &artist, &title).await {
            if missing(&info.name) {
                if let Some(t) = rec.get("title").and_then(|v| v.as_str()) {
                    if !t.is_empty() {
                        info.name = t.to_string();
                        filled.push("name");
                    }
                }
            }
            if info.artists.iter().all(|a| a.trim().is_empty()) {
                if let Some(a) = rec
                    .pointer("/artist-credit/0/name")
                    .and_then(|v| v.as_str())
                {
                    if !a.is_empty() {
                        info.artists = vec![a.to_string()];
                        filled.push("artist");
                    }
                }
            }
            if info.cover_url.trim().is_empty() {
                if let Some(mbid) = rec.pointer("/releases/0/id").and_then(|v| v.as_str()) {
                    if !mbid.is_empty() {
                        info.cover_url =
                            format!("https://coverartarchive.org/release/{mbid}/front-500");
                        filled.push("cover");
                    }
                }
            }
        }
    }

    filled.sort_unstable();
    filled.dedup();
    filled
}

/// Overwrite name/album/artists/cover from Deezer when a confident match is
/// found for the current artist/title. Used for sources whose own metadata is
/// unreliable (e.g. YouTube uploader names and music-video thumbnails).
pub async fn fill_track_metadata_force(
    client: &reqwest::Client,
    info: &mut TrackInfo,
) -> Vec<&'static str> {
    let mut filled: Vec<&'static str> = Vec::new();
    let artist0 = info.artists.first().cloned().unwrap_or_default();
    // Strip uploader noise ("Official", "VEVO", "Records"...) so the query
    // targets the real artist rather than a YouTube channel name.
    let artist_q = artist0
        .split_whitespace()
        .filter(|w| {
            !matches!(
                w.to_lowercase().as_str(),
                "official" | "vevo" | "records" | "music" | "tv" | "hq" | "hd" | "-"
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    let title_full = strip_quality_suffix(&info.name);
    // Query the parenthetical-less core title first: YouTube titles often carry
    // "(Official Video)", "(Live)" etc. which hurt the match.
    let title_core = title_full
        .split(['(', '['])
        .next()
        .unwrap_or(&title_full)
        .trim()
        .to_string();

    for t in [title_core.as_str(), title_full.as_str()] {
        if t.is_empty() {
            continue;
        }
        let query = format!("{artist_q} {t}").trim().to_string();
        if query.is_empty() {
            continue;
        }
        let Some(hit) = deezer_lookup_best(client, &query, t).await else {
            continue;
        };
        let h_title = hit.get("title").and_then(|v| v.as_str()).unwrap_or("");
        let h_artist = hit
            .pointer("/artist/name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let title_ok = textmatch::similarity(t, h_title) >= 0.5;
        let artist_ok =
            artist_q.trim().is_empty() || textmatch::similarity(&artist_q, h_artist) >= 0.5;
        if !(title_ok && artist_ok) {
            continue;
        }
        if !h_title.is_empty() {
            let h_core = h_title.split(['(', '[']).next().unwrap_or(h_title).trim();
            info.name = if !h_core.is_empty() && textmatch::similarity(t, h_core) >= 0.5 {
                h_core.to_string()
            } else {
                h_title.to_string()
            };
            filled.push("name");
        }
        if !h_artist.is_empty() {
            info.artists = vec![h_artist.to_string()];
            filled.push("artist");
        }
        if let Some(al) = hit.pointer("/album/title").and_then(|v| v.as_str()) {
            if !al.is_empty() {
                info.album = al.to_string();
                filled.push("album");
            }
        }
        if let Some(c) = hit.pointer("/album/cover_xl").and_then(|v| v.as_str()) {
            if !c.is_empty() {
                info.cover_url = c.to_string();
                filled.push("cover");
            }
        }
        break;
    }
    filled.sort_unstable();
    filled.dedup();
    filled
}
