//! The keyless lyrics providers BitChord lists (README "Lyrics providers"),
//! as Picaro lyrics modules: Unison, BetterLyrics, LyricsPlus, BiniLyrics,
//! lrc.red and KuGou. Each answers with TTML / LRC / plain text that
//! `lyrics_kit` turns into `LyricsInfo` (synced LRC when the source is timed).
//!
//! Not ported (checked live 2026-11-15): SimpMusic and Megalobiz answer 403
//! (BitChord hides both by default), Genius answers 403 to rustls' TLS
//! fingerprint (plain text only anyway), PaxSenix needs an Apple Music token
//! or an API key, and the YouTube caption sources need a YouTube video id.

use async_trait::async_trait;
use base64::Engine;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use picaro_utils::models::LyricsInfo;

use crate::lyrics_kit::*;

fn s<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|x| x.as_str()).unwrap_or("")
}

fn dur_s(q: &LyricsQuery) -> String {
    q.duration.map(|d| d.to_string()).unwrap_or_default()
}

// ---------------------------------------------------------------------------
//  Unison - community word-synced TTML / LRC
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct Unison;

#[async_trait]
impl LyricsProvider for Unison {
    const SERVICE: &'static str = "Unison";
    const HOST: &'static str = "unison.boidu.dev";

    async fn fetch(&self, q: &LyricsQuery) -> Option<LyricsInfo> {
        let d = dur_s(q);
        let mut query = vec![("song", q.title.as_str()), ("artist", q.artist.as_str())];
        if !q.album.is_empty() {
            query.push(("album", q.album.as_str()));
        }
        if !d.is_empty() {
            query.push(("duration", d.as_str()));
        }
        let v = get_json("https://unison.boidu.dev/lyrics", &query).await?;
        if !v.get("success").and_then(|x| x.as_bool()).unwrap_or(false) {
            return None;
        }
        let data = v.get("data")?;
        if !same_song(q, s(data, "song"), s(data, "artist")) {
            return None;
        }
        lyrics_from_text(s(data, "lyrics"))
    }
}

// ---------------------------------------------------------------------------
//  BetterLyrics - Apple TTML behind a JSON envelope
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct BetterLyrics;

#[async_trait]
impl LyricsProvider for BetterLyrics {
    const SERVICE: &'static str = "BetterLyrics";
    const HOST: &'static str = "lyrics-api.boidu.dev";

    async fn fetch(&self, q: &LyricsQuery) -> Option<LyricsInfo> {
        let d = dur_s(q);
        let mut query = vec![("s", q.title.as_str()), ("a", q.artist.as_str())];
        if !d.is_empty() {
            query.push(("d", d.as_str()));
        }
        if !q.album.is_empty() {
            query.push(("al", q.album.as_str()));
        }
        // 403s the default reqwest agent; the shared browser UA is accepted.
        let v = get_json("https://lyrics-api.boidu.dev/getLyrics", &query).await?;
        lyrics_from_text(s(&v, "ttml"))
    }
}

// ---------------------------------------------------------------------------
//  LyricsPlus (YouLyPlus) - JSON lines with millisecond times
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct LyricsPlus;

/// Mirrors in BitChord's order; dead ones (402 / 404 / bad certificate on
/// 2026-11-15) just fall through to the next.
const LYRICSPLUS_MIRRORS: [&str; 3] = [
    "https://lyricsplus.prjktla.my.id",
    "https://lyricsplus.atomix.one",
    "https://lyricsplus.binimum.org",
];

/// `{"lyrics":[{"time":17657,"text":"..."}]}` -> LRC.
pub fn lyricsplus_to_lrc(v: &Value) -> Option<String> {
    let lines: Vec<String> = v
        .get("lyrics")?
        .as_array()?
        .iter()
        .filter_map(|l| {
            let text = l.get("text")?.as_str()?.trim();
            let ms = l.get("time")?.as_f64()?;
            (!text.is_empty()).then(|| format!("{}{text}", stamp(ms / 1000.0)))
        })
        .collect();
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[async_trait]
impl LyricsProvider for LyricsPlus {
    const SERVICE: &'static str = "LyricsPlus";
    const HOST: &'static str = "lyricsplus.prjktla.my.id";

    async fn fetch(&self, q: &LyricsQuery) -> Option<LyricsInfo> {
        let d = dur_s(q);
        let mut query = vec![("title", q.title.as_str()), ("artist", q.artist.as_str())];
        if !d.is_empty() {
            query.push(("duration", d.as_str()));
        }
        if !q.album.is_empty() {
            query.push(("album", q.album.as_str()));
        }
        for host in LYRICSPLUS_MIRRORS {
            let Some(v) = get_json(&format!("{host}/v2/lyrics/get"), &query).await else {
                continue;
            };
            if let Some(lrc) = lyricsplus_to_lrc(&v) {
                return Some(LyricsInfo {
                    embedded: Some(strip_stamps(&lrc)),
                    synced: Some(lrc),
                });
            }
        }
        None
    }
}

// ---------------------------------------------------------------------------
//  BiniLyrics / lrc.red - ISRC-keyed TTML
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct BiniLyrics;

#[async_trait]
impl LyricsProvider for BiniLyrics {
    const SERVICE: &'static str = "BiniLyrics";
    const HOST: &'static str = "lyrics-api.binimum.org";

    async fn fetch(&self, q: &LyricsQuery) -> Option<LyricsInfo> {
        let d = dur_s(q);
        let mut query = vec![("track", q.title.as_str()), ("artist", q.artist.as_str())];
        if !q.album.is_empty() {
            query.push(("album", q.album.as_str()));
        }
        if !d.is_empty() {
            query.push(("duration", d.as_str()));
        }
        let v = get_json("https://lyrics-api.binimum.org/", &query).await?;
        let hit = v.get("results")?.as_array()?.iter().find(|r| {
            same_song(q, s(r, "track_name"), s(r, "artist_name"))
                && dur_ok(q.duration, r.get("duration").and_then(|x| x.as_f64()), 3.0)
        })?;
        let url = s(hit, "lyricsUrl");
        // The URL comes from a third-party reply: https only.
        if !url.starts_with("https://") {
            return None;
        }
        lyrics_from_text(&get_text(url, &[], None).await?)
    }
}

#[derive(Debug, Default)]
pub struct LrcRed;

#[async_trait]
impl LyricsProvider for LrcRed {
    const SERVICE: &'static str = "LrcRed";
    const HOST: &'static str = "lrc.red";

    async fn fetch(&self, q: &LyricsQuery) -> Option<LyricsInfo> {
        let text = q.text();
        let v = get_json("https://lrc.red/search.json", &[("q", text.as_str())]).await?;
        let hit = v.get("hits")?.as_array()?.iter().find(|h| {
            same_song(q, s(h, "title"), s(h, "artist"))
                && dur_ok(q.duration, h.get("duration").and_then(|x| x.as_f64()), 3.0)
        })?;
        let isrc = s(hit, "isrc");
        // The ISRC is spliced into a URL path: letters and digits only.
        if isrc.is_empty() || !isrc.chars().all(|c| c.is_ascii_alphanumeric()) {
            return None;
        }
        lyrics_from_text(&get_text(&format!("https://lrc.red/s/{isrc}.ttml"), &[], None).await?)
    }
}

// ---------------------------------------------------------------------------
//  KuGou - three unauthenticated calls, base64 LRC
// ---------------------------------------------------------------------------

#[derive(Debug, Default)]
pub struct KuGou;

/// Decode KuGou's `content` and drop its `[id:..]` / `[ti:..]` header tags.
pub fn kugou_decode(b64: &str) -> Option<String> {
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64.trim()).ok()?;
    let text = String::from_utf8(bytes).ok()?;
    let lrc: Vec<&str> = text
        .lines()
        .map(|l| l.trim_end_matches('\r'))
        .filter(|l| !l.trim().is_empty() && !is_meta_line(l))
        .collect();
    (!lrc.is_empty()).then(|| lrc.join("\n"))
}

fn is_meta_line(l: &str) -> bool {
    static META: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*\[[a-zA-Z]{1,8}:[^\]]*\]\s*$").unwrap());
    META.is_match(l)
}

#[async_trait]
impl LyricsProvider for KuGou {
    const SERVICE: &'static str = "KuGou";
    const HOST: &'static str = "lyrics.kugou.com";

    async fn fetch(&self, q: &LyricsQuery) -> Option<LyricsInfo> {
        let keyword = format!("{} - {}", q.artist, q.title);
        // KuGou ranks "+"-encoded spaces (what `.query()` sends) differently, so spell them %20.
        let kw = percent_encoding::utf8_percent_encode(&keyword, percent_encoding::NON_ALPHANUMERIC);
        let v = get_json(
            &format!("https://mobileservice.kugou.com/api/v3/search/song?format=json&page=1&pagesize=8&keyword={kw}"),
            &[],
        )
        .await?;
        let song = v.pointer("/data/info")?.as_array()?.iter().find(|i| {
            same_song(q, s(i, "songname"), s(i, "singername"))
                && dur_ok(q.duration, i.get("duration").and_then(|x| x.as_f64()), 5.0)
        })?;
        let hash = s(song, "hash");
        if hash.is_empty() {
            return None;
        }
        let v = get_json(
            "https://lyrics.kugou.com/search",
            &[("ver", "1"), ("man", "yes"), ("client", "pc"), ("hash", hash)],
        )
        .await?;
        let cand = v.get("candidates")?.as_array()?.first()?;
        let (id, key) = (s(cand, "id"), s(cand, "accesskey"));
        if id.is_empty() || key.is_empty() {
            return None;
        }
        let v = get_json(
            "https://lyrics.kugou.com/download",
            &[("fmt", "lrc"), ("charset", "utf8"), ("client", "pc"), ("ver", "1"), ("id", id), ("accesskey", key)],
        )
        .await?;
        lyrics_from_text(&kugou_decode(s(&v, "content"))?)
    }
}

pub fn register_all(registry: &picaro_utils::ModuleRegistry) {
    register_provider::<Unison>(registry);
    register_provider::<BetterLyrics>(registry);
    register_provider::<LyricsPlus>(registry);
    register_provider::<BiniLyrics>(registry);
    register_provider::<LrcRed>(registry);
    register_provider::<KuGou>(registry);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lyricsplus_lines() {
        let v: Value = serde_json::from_str(
            r#"{"type":"Line","lyrics":[{"time":17657,"text":"It starts with one"},{"time":19000,"text":" "},{"time":23467,"text":"Keep that in mind"}]}"#,
        )
        .unwrap();
        assert_eq!(
            lyricsplus_to_lrc(&v).unwrap(),
            "[00:17.66]It starts with one\n[00:23.47]Keep that in mind"
        );
    }

    /// Live network check: `cargo test -p picaro-modules live_ -- --ignored --nocapture`.
    #[tokio::test]
    #[ignore]
    async fn live_kugou() {
        let q = LyricsQuery {
            artist: "Linkin Park".into(),
            title: "In The End".into(),
            ..Default::default()
        };
        let l = KuGou.fetch(&q).await.expect("kugou found nothing");
        assert!(l.synced.is_some());
    }

    #[test]
    fn kugou_content_drops_header_tags() {
        let raw = "[id:$00000000]\r\n[ti:In The End]\r\n[00:00.00]In The End - Linkin Park\r\n[00:12.10]It starts with one\r\n";
        let b64 = base64::engine::general_purpose::STANDARD.encode(raw);
        assert_eq!(
            kugou_decode(&b64).unwrap(),
            "[00:00.00]In The End - Linkin Park\n[00:12.10]It starts with one"
        );
        assert!(kugou_decode("!!!").is_none());
    }

}
