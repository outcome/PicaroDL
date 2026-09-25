//! Lightweight token-overlap text matching for search-result relevance and
//! picking the requested track out of an extracted album.

use std::collections::HashSet;

fn tokens(s: &str) -> HashSet<String> {
    s.to_lowercase()
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

/// Split "artist - title" into (artist, title). Falls back to title-only.
pub fn split_query(q: &str) -> (Option<String>, String) {
    if let Some(idx) = q.find(" - ") {
        let a = q[..idx].trim().to_string();
        let t = q[idx + 3..].trim().to_string();
        (if a.is_empty() { None } else { Some(a) }, t)
    } else {
        (None, q.trim().to_string())
    }
}
