//! Shared plumbing for the keyless lyrics providers (Unison, BetterLyrics,
//! LyricsPlus, lrc.red, BiniLyrics, KuGou): one trait per provider
//! instead of ~150 lines of `ModuleInterface` stubs each, plus the parsing
//! they share (TTML -> LRC, song matching, entity decoding).

use std::collections::HashMap;
use std::marker::PhantomData;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use once_cell::sync::Lazy;
use regex::Regex;
use serde_json::Value;

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::lyrics_ovh::artist_title;

pub const CHROME_UA: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 \
(KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/// What the downloader knows about the track; only artist + title are
/// guaranteed, `album` / `duration` sharpen the match when present.
#[derive(Debug, Clone, Default)]
pub struct LyricsQuery {
    pub artist: String,
    pub title: String,
    pub album: String,
    pub duration: Option<u32>,
}

impl LyricsQuery {
    pub fn from_data(track_id: &str, data: &HashMap<String, Value>) -> Option<Self> {
        let (artist, title) = artist_title(track_id, data)?;
        let album = data
            .get("__album__")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        let duration = data
            .get("__duration__")
            .and_then(|v| v.as_f64())
            .filter(|d| d.is_finite() && *d > 0.0)
            .map(|d| d.round() as u32);
        Some(Self {
            artist,
            title,
            album,
            duration,
        })
    }

    /// "artist title" for the search boxes of providers that take one string.
    pub fn text(&self) -> String {
        format!("{} {}", self.artist, self.title)
    }
}

#[async_trait]
pub trait LyricsProvider: Send + Sync + std::fmt::Debug + Default + 'static {
    /// Service name (the registry key is its lowercase form).
    const SERVICE: &'static str;
    /// Host shown as the module's netlocation.
    const HOST: &'static str;
    /// Best lyrics for the track, `None` on a miss (never an error: a dead
    /// provider just falls through to the next one).
    async fn fetch(&self, q: &LyricsQuery) -> Option<LyricsInfo>;
}

pub fn information<P: LyricsProvider>() -> ModuleInformation {
    ModuleInformation {
        service_name: P::SERVICE.to_string(),
        module_supported_modes: ModuleModes::lyrics,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single(P::HOST.to_string()),
        url_constants: indexmap::IndexMap::new(),
        test_url: None,
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn register_provider<P: LyricsProvider>(registry: &picaro_utils::ModuleRegistry) {
    crate::registry::register(registry, information::<P>(), Arc::new(Ctor::<P>(PhantomData)));
}

struct Ctor<P>(PhantomData<fn() -> P>);

impl<P> std::fmt::Debug for Ctor<P> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LyricsCtor")
    }
}

impl<P: LyricsProvider> ModuleConstructor for Ctor<P> {
    fn construct(&self, _c: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(ProviderModule(P::default())))
    }
}

#[derive(Debug)]
struct ProviderModule<P>(P);

fn unsupported<T>(service: &str, ability: &str) -> Result<T> {
    Err(Error::ModuleDoesNotSupportAbility {
        module: service.into(),
        ability: ability.into(),
    })
}

#[async_trait]
impl<P: LyricsProvider> picaro_utils::module::ModuleInterface for ProviderModule<P> {
    fn name(&self) -> &str {
        P::SERVICE
    }

    async fn get_track_info(
        &self,
        _id: &str,
        _q: Quality,
        _c: &CodecOptions,
        _d: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        unsupported(P::SERVICE, "track_info")
    }

    async fn get_track_download(
        &self,
        _id: &str,
        _q: Quality,
        _c: &CodecOptions,
        _d: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        unsupported(P::SERVICE, "download")
    }

    async fn get_album_info(&self, _id: &str, _d: HashMap<String, Value>) -> Result<AlbumInfo> {
        unsupported(P::SERVICE, "album")
    }

    async fn get_playlist_info(
        &self,
        _id: &str,
        _d: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        unsupported(P::SERVICE, "playlist")
    }

    async fn get_artist_info(
        &self,
        _id: &str,
        _g: bool,
        _n: Option<&str>,
        _d: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        unsupported(P::SERVICE, "artist")
    }

    async fn get_track_credits(
        &self,
        _id: &str,
        _d: HashMap<String, Value>,
    ) -> Result<Vec<CreditsInfo>> {
        Ok(vec![])
    }

    async fn get_track_cover(
        &self,
        _id: &str,
        _c: &CoverOptions,
        _d: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        unsupported(P::SERVICE, "cover")
    }

    async fn get_track_lyrics(
        &self,
        track_id: &str,
        data: HashMap<String, Value>,
    ) -> Result<LyricsInfo> {
        let Some(q) = LyricsQuery::from_data(track_id, &data) else {
            return Ok(LyricsInfo::default());
        };
        Ok(self.0.fetch(&q).await.unwrap_or_default())
    }

    async fn search(
        &self,
        _qt: DownloadType,
        _q: &str,
        _ti: Option<&TrackInfo>,
        _l: u32,
    ) -> Result<Vec<SearchResult>> {
        Ok(vec![])
    }
}

// ---------------------------------------------------------------------------
//  HTTP
// ---------------------------------------------------------------------------

static HTTP: Lazy<reqwest::Client> = Lazy::new(|| {
    reqwest::Client::builder()
        .user_agent(CHROME_UA)
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap_or_default()
});

/// 2xx body as text; any failure (network, status, body) is a miss.
pub async fn get_text(url: &str, query: &[(&str, &str)], ua: Option<&str>) -> Option<String> {
    let mut req = HTTP.get(url).query(query);
    if let Some(ua) = ua {
        req = req.header(reqwest::header::USER_AGENT, ua);
    }
    let resp = req.send().await.ok()?;
    if !resp.status().is_success() {
        return None;
    }
    resp.text().await.ok()
}

pub async fn get_json(url: &str, query: &[(&str, &str)]) -> Option<Value> {
    serde_json::from_str(&get_text(url, query, None).await?).ok()
}

// ---------------------------------------------------------------------------
//  Matching
// ---------------------------------------------------------------------------

/// Lowercase alphanumerics with "(...)" / "[...]" asides dropped, so
/// "In the End (Remastered)" and "In The End" compare equal.
pub fn norm(s: &str) -> String {
    let mut out = String::new();
    let mut depth = 0u32;
    for c in s.chars() {
        match c {
            '(' | '[' => depth += 1,
            ')' | ']' => depth = depth.saturating_sub(1),
            _ if depth > 0 => {}
            c if c.is_alphanumeric() => out.extend(c.to_lowercase()),
            _ => {
                if !out.ends_with(' ') {
                    out.push(' ');
                }
            }
        }
    }
    out.trim().to_string()
}

/// Whether a candidate's (title, artist) is the wanted song. Titles must be
/// equal after `norm` (or one contains the other, 4+ chars); artists must
/// overlap (containment or a shared 3+ letter word) when both are known.
pub fn same_song(q: &LyricsQuery, title: &str, artist: &str) -> bool {
    let (qt, ct) = (norm(&q.title), norm(title));
    if qt.is_empty() || ct.is_empty() {
        return false;
    }
    let title_ok = qt == ct
        || (qt.len() >= 4 && ct.contains(&qt))
        || (ct.len() >= 4 && qt.contains(&ct));
    if !title_ok {
        return false;
    }
    let (qa, ca) = (norm(&q.artist), norm(artist));
    if qa.is_empty() || ca.is_empty() {
        return true;
    }
    qa.contains(&ca)
        || ca.contains(&qa)
        || qa
            .split(' ')
            .filter(|w| w.chars().count() >= 3)
            .any(|w| ca.split(' ').any(|x| x == w))
}

/// Duration within `tol` seconds, or no duration known on either side.
pub fn dur_ok(want: Option<u32>, got_secs: Option<f64>, tol: f64) -> bool {
    match (want, got_secs) {
        (Some(w), Some(g)) if g > 0.0 => (w as f64 - g).abs() <= tol,
        _ => true,
    }
}

// ---------------------------------------------------------------------------
//  Text -> LRC
// ---------------------------------------------------------------------------

static STAMP_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"\[\d{1,3}:\d{2}(?:[.:]\d{1,3})?\]").unwrap());
static META_TAG_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"^\s*\[[a-zA-Z]{1,8}:[^\]]*\]\s*$").unwrap());
static P_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"(?s)<p\b([^>]*?)(/?)>(.*?)</p>").unwrap());
static BEGIN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r#"\bbegin\s*=\s*"([^"]+)""#).unwrap());
static TOKEN_RE: Lazy<Regex> = Lazy::new(|| Regex::new(r"<[^>]*>|[^<]+").unwrap());
static SKIP_ROLE_RE: Lazy<Regex> =
    Lazy::new(|| Regex::new(r#"role\s*=\s*"x-(?:translation|roman|bg)""#).unwrap());

/// `[mm:ss.xx]` for a time in seconds.
pub fn stamp(secs: f64) -> String {
    let cs = (secs.max(0.0) * 100.0).round() as u64;
    format!("[{:02}:{:02}.{:02}]", cs / 6000, (cs / 100) % 60, cs % 100)
}

pub fn is_timed(text: &str) -> bool {
    text.lines().any(|l| STAMP_RE.find(l.trim_start()).is_some_and(|m| m.start() == 0))
}

/// LRC -> plain lines: stamps and `[ti:..]`-style tags removed.
pub fn strip_stamps(lrc: &str) -> String {
    lrc.lines()
        .filter(|l| !META_TAG_RE.is_match(l))
        .map(|l| STAMP_RE.replace_all(l, "").trim().to_string())
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

/// TTML clock value ("0:16.712", "16.7s", "1234ms", "1:02:03.5") -> seconds.
pub fn ttml_time(s: &str) -> Option<f64> {
    let s = s.trim();
    if let Some(ms) = s.strip_suffix("ms") {
        return ms.trim().parse::<f64>().ok().map(|v| v / 1000.0);
    }
    if let Some(sec) = s.strip_suffix('s') {
        return sec.trim().parse::<f64>().ok();
    }
    let mut total = 0.0;
    for part in s.split(':') {
        total = total * 60.0 + part.trim().parse::<f64>().ok()?;
    }
    Some(total)
}

pub fn unescape(s: &str) -> String {
    static ENT: Lazy<Regex> = Lazy::new(|| Regex::new(r"&(#x[0-9a-fA-F]+|#\d+|[a-zA-Z]+);").unwrap());
    ENT.replace_all(s, |c: &regex::Captures| {
        let e = &c[1];
        let ch = match e {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            "nbsp" => Some(' '),
            _ => e
                .strip_prefix("#x")
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| e.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        ch.map(|c| c.to_string()).unwrap_or_else(|| c[0].to_string())
    })
    .into_owned()
}

/// The sung text of a `<p>`'s inner XML: every tag dropped, but spans that are
/// translations, romanisations or background vocals skipped with their
/// children. Whitespace is taken as the file has it (word spans are
/// separated by text nodes; syllable spans are not), then collapsed.
fn ttml_inner_text(inner: &str) -> String {
    let mut out = String::new();
    let mut depth = 0usize;
    let mut skip_at: Option<usize> = None;
    for m in TOKEN_RE.find_iter(inner) {
        let t = m.as_str();
        if t.starts_with("</span") {
            depth = depth.saturating_sub(1);
            if skip_at == Some(depth) {
                skip_at = None;
            }
        } else if t.starts_with("<span") && !t.ends_with("/>") {
            if skip_at.is_none() && SKIP_ROLE_RE.is_match(t) {
                skip_at = Some(depth);
            }
            depth += 1;
        } else if t.starts_with("<br") {
            if skip_at.is_none() {
                out.push(' ');
            }
        } else if !t.starts_with('<') && skip_at.is_none() {
            out.push_str(t);
        }
    }
    unescape(&out).split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Line-synced LRC from a TTML document (Apple-style, word- or line-timed).
pub fn ttml_to_lrc(ttml: &str) -> Option<String> {
    let mut lines: Vec<(f64, String)> = Vec::new();
    for c in P_RE.captures_iter(ttml) {
        let (attrs, inner) = (&c[1], &c[3]);
        let begin = BEGIN_RE
            .captures(attrs)
            .or_else(|| BEGIN_RE.captures(inner))
            .and_then(|b| ttml_time(&b[1]));
        let (Some(t), text) = (begin, ttml_inner_text(inner)) else {
            continue;
        };
        if !text.is_empty() {
            lines.push((t, text));
        }
    }
    if lines.is_empty() {
        return None;
    }
    lines.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));
    lines.dedup_by(|b, a| a.0 == b.0 && a.1 == b.1);
    Some(
        lines
            .iter()
            .map(|(t, s)| format!("{}{s}", stamp(*t)))
            .collect::<Vec<_>>()
            .join("\n"),
    )
}

/// Whatever a provider returned (TTML, LRC or plain text) as `LyricsInfo`:
/// `synced` is set only when real timestamps exist.
pub fn lyrics_from_text(raw: &str) -> Option<LyricsInfo> {
    let t = raw.trim_start_matches('\u{feff}').trim();
    if t.is_empty() {
        return None;
    }
    if t.starts_with('<') {
        let lrc = ttml_to_lrc(t)?;
        return Some(LyricsInfo {
            embedded: Some(strip_stamps(&lrc)),
            synced: Some(lrc),
        });
    }
    if is_timed(t) {
        let lrc = t.replace("\r\n", "\n");
        let plain = strip_stamps(&lrc);
        if plain.is_empty() {
            return None;
        }
        return Some(LyricsInfo {
            embedded: Some(plain),
            synced: Some(lrc),
        });
    }
    Some(LyricsInfo {
        embedded: Some(t.replace("\r\n", "\n")),
        synced: None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(artist: &str, title: &str) -> LyricsQuery {
        LyricsQuery {
            artist: artist.into(),
            title: title.into(),
            ..Default::default()
        }
    }

    #[test]
    fn ttml_word_timed_becomes_line_lrc_without_bg_or_translation() {
        let ttml = r#"<tt xmlns="http://www.w3.org/ns/ttml"><body><div>
<p begin="0:16.712" end="0:19.112" ttm:agent="v1"><span begin="0:16.712" end="0:17.016">It</span> <span begin="0:17.016" end="0:17.6">starts</span> <span begin="0:17.6" end="0:19.112">one</span><span ttm:role="x-bg" begin="0:18" end="0:19"><span begin="0:18" end="0:19">(ooh)</span></span><span ttm:role="x-translation">Es beginnt</span></p>
<p begin="1:02.5" end="1:05"><span begin="1:02.5" end="1:03">Lo</span><span begin="1:03" end="1:04">ve &amp; hate</span></p>
</div></body></tt>"#;
        let lrc = ttml_to_lrc(ttml).unwrap();
        assert_eq!(lrc, "[00:16.71]It starts one\n[01:02.50]Love & hate");
    }

    #[test]
    fn line_timed_p_without_spans_and_span_only_begin() {
        let ttml = r#"<tt><body><p begin="5.5s">Hello</p><p><span begin="7s">World</span></p></body></tt>"#;
        assert_eq!(ttml_to_lrc(ttml).unwrap(), "[00:05.50]Hello\n[00:07.00]World");
    }

    #[test]
    fn times_parse() {
        assert_eq!(ttml_time("0:16.712"), Some(16.712));
        assert_eq!(ttml_time("1:02:03.5"), Some(3723.5));
        assert_eq!(ttml_time("1234ms"), Some(1.234));
        assert_eq!(ttml_time("7s"), Some(7.0));
        assert_eq!(ttml_time("x"), None);
    }

    #[test]
    fn text_forms() {
        let t = lyrics_from_text("[ti:x]\n[00:01.00]one\n[00:02.50]two").unwrap();
        assert_eq!(t.embedded.as_deref(), Some("one\ntwo"));
        assert!(t.synced.is_some());
        let p = lyrics_from_text("just words\nmore").unwrap();
        assert!(p.synced.is_none());
        assert!(lyrics_from_text("  ").is_none());
        assert!(lyrics_from_text("<html></html>").is_none());
    }

    #[test]
    fn song_matching() {
        assert!(same_song(&q("Linkin Park", "In The End"), "In the End (Remastered)", "LINKIN PARK"));
        assert!(same_song(&q("Jiinzo", "Silver Spoon"), "Silver Spoon", "Jiinzo, Someone"));
        assert!(!same_song(&q("Linkin Park", "In The End"), "In the End: Live", "Someone Else"));
        assert!(!same_song(&q("Linkin Park", "Numb"), "Crawling", "Linkin Park"));
        assert!(dur_ok(Some(216), Some(216.3), 3.0));
        assert!(!dur_ok(Some(216), Some(240.0), 3.0));
        assert!(dur_ok(None, Some(240.0), 3.0));
    }

    #[test]
    fn entities() {
        assert_eq!(unescape("a &amp; b &#39;c&#39; &lt;3 &#x41;"), "a & b 'c' <3 A");
    }
}
