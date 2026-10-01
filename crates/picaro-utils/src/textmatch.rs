//! Lightweight token-overlap text matching for search-result relevance and
//! picking the requested track out of an extracted album.

use std::collections::HashSet;

/// Strip HTML entities (e.g. `&#8211;`, `&amp;`) so they don't pollute tokens.
fn strip_entities(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'&' {
            if let Some(semi) = s[i..].find(';').filter(|&p| p <= 10) {
                i += semi + 1;
                out.push(' ');
                continue;
            }
        }
        let ch = s[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    out
}

/// Normalise dashes so " – " / " — " act like " - ".
fn normalise_dashes(s: &str) -> String {
    s.replace('\u{2013}', "-").replace('\u{2014}', "-")
}

fn tokens(s: &str) -> HashSet<String> {
    strip_entities(&s.to_lowercase())
        .split(|c: char| !c.is_alphanumeric())
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}

/// Jaccard token overlap in [0.0, 1.0].
pub fn similarity(a: &str, b: &str) -> f64 {
    let ta = tokens(a);
    let tb = tokens(b);
    if ta.is_empty() || tb.is_empty() {
        return 0.0;
    }
    let inter = ta.intersection(&tb).count() as f64;
    let union = ta.union(&tb).count() as f64;
    inter / union
}

/// Fraction of `a`'s tokens that appear somewhere in `b` (0.0 when `a`
/// has no tokens). Used as a hard presence gate: a candidate whose title
/// shares ZERO tokens with the query title is not a match, no matter how
/// well the artist lines up.
pub fn token_presence(a: &str, b: &str) -> f64 {
    let ta = tokens(a);
    if ta.is_empty() {
        return 0.0;
    }
    let tb = tokens(b);
    if tb.is_empty() {
        return 0.0;
    }
    let present = ta.intersection(&tb).count() as f64;
    present / ta.len() as f64
}

/// Lowercased alphanumeric-only form of a string ("The Beatles" ->
/// "thebeatles"), for matching sources that concatenate names.
pub fn concat_key(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

/// Concatenated-substring match: "TheBeatles" contains "thebeatles".
/// Catches sources that strip the spaces from artist names.
pub fn contains_fold(haystack: &str, needle: &str) -> bool {
    let h = concat_key(haystack);
    let n = concat_key(needle);
    !n.is_empty() && h.contains(&n)
}

/// Split "artist - title" into (artist, title). Handles hyphen, en-dash and
/// em-dash separators; falls back to title-only.
pub fn split_query(q: &str) -> (Option<String>, String) {
    let norm = normalise_dashes(q);
    if let Some(idx) = norm.find(" - ") {
        let a = norm[..idx].trim().to_string();
        let t = norm[idx + 3..].trim().to_string();
        (if a.is_empty() { None } else { Some(a) }, t)
    } else {
        (None, norm.trim().to_string())
    }
}
