//! Path / formatting helpers for downloads. Mirrors the formatting block of
//! `_create_album_location` and `_create_track_location` in OrpheusDL.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use picaro_utils::error::Result;
use picaro_utils::format::format_template;
use picaro_utils::models::{AlbumInfo, PlaylistInfo, TrackInfo, TrackRef};
use picaro_utils::util::{fix_byte_limit, primary_artist, primary_from_multi, sanitise_name};

use crate::globals::GlobalSettings;

/// Build the album folder path under `root`.
pub fn build_album_path(
    settings: &GlobalSettings,
    root: &Path,
    album: &AlbumInfo,
) -> Result<PathBuf> {
    let mut vars = BTreeMap::new();
    vars.insert("artist".to_string(), sanitise_name(&album.artist));
    // Keep album_artist compact: primary artist only. Providers may return
    // long multi-artist strings ("A / B", "A feat. B"); `first()` alone
    // would keep the whole string and blow Windows path limits.
    let primary_aa = primary_from_multi(album.album_artist.as_ref());
    if primary_aa.is_empty() {
        vars.insert("album_artist".to_string(), sanitise_name(&album.artist));
    } else {
        vars.insert("album_artist".to_string(), sanitise_name(&primary_aa));
    }
    vars.insert("name".to_string(), sanitise_name(&album.name));
    vars.insert("id".to_string(), album.id.clone().unwrap_or_default());
    vars.insert("year".to_string(), album.release_year.to_string());
    if let Some(q) = &album.quality {
        // Python replaces '/' with '·' before sanitising (which strips '/'),
        // so "24B/96kHz" stays readable instead of collapsing to "24B96kHz".
        vars.insert(
            "quality".to_string(),
            sanitise_name(&format!(" [{}]", q.replace('/', "\u{00B7}"))),
        );
    } else {
        vars.insert("quality".to_string(), String::new());
    }
    vars.insert(
        "explicit".to_string(),
        if album.explicit.unwrap_or(false) {
            " \u{1F174}".to_string()
        } else {
            String::new()
        },
    );
    vars.insert(
        "label".to_string(),
        album.label.clone().map(sanitise_name).unwrap_or_default(),
    );
    vars.insert(
        "catalog_number".to_string(),
        album
            .catalog_number
            .clone()
            .map(sanitise_name)
            .unwrap_or_default(),
    );

    let template = settings
        .formatting()
        .get("album_format")
        .and_then(|v| v.as_str())
        .unwrap_or("{artist}/{name}")
        .to_string();
    let name = format_template(&template, &vars);
    let candidate = root.join(name);
    let final_path = fix_byte_limit(candidate, 250);
    Ok(final_path)
}

/// Build a playlist folder path under `root`.
pub fn build_playlist_path(
    settings: &GlobalSettings,
    root: &Path,
    playlist: &PlaylistInfo,
) -> Result<PathBuf> {
    let mut vars = BTreeMap::new();
    vars.insert("name".to_string(), sanitise_name(&playlist.name));
    vars.insert("creator".to_string(), sanitise_name(&playlist.creator));
    vars.insert("year".to_string(), playlist.release_year.to_string());
    if let Some(id) = &playlist.id {
        vars.insert("id".to_string(), id.clone());
    } else {
        vars.insert("id".to_string(), String::new());
    }
    vars.insert(
        "explicit".to_string(),
        if playlist.explicit.unwrap_or(false) {
            " \u{1F174}".to_string()
        } else {
            String::new()
        },
    );
    let template = settings
        .formatting()
        .get("playlist_format")
        .and_then(|v| v.as_str())
        .unwrap_or("{name}")
        .to_string();
    let name = format_template(&template, &vars);
    let candidate = root.join(name);
    let final_path = fix_byte_limit(candidate, 250);
    Ok(final_path)
}

/// Build a track filename + (optional) sub-path.
pub fn build_track_filename(
    settings: &GlobalSettings,
    track: &TrackInfo,
    codec_extension: &str,
    is_single: bool,
) -> Result<PathBuf> {
    let sep = settings
        .formatting()
        .get("metadata_separator")
        .and_then(|v| v.as_str())
        .unwrap_or(";")
        .to_string();
    let template = if is_single {
        settings
            .formatting()
            .get("single_full_path_format")
            .and_then(|v| v.as_str())
            .unwrap_or("{artist} - {name}")
            .to_string()
    } else {
        settings
            .formatting()
            .get("track_filename_format")
            .and_then(|v| v.as_str())
            .unwrap_or("{artist} - {name}")
            .to_string()
    };

    let mut vars = BTreeMap::new();
    let artist_str = if track.artists.is_empty() {
        String::new()
    } else {
        track
            .artists
            .iter()
            .map(|a| sanitise_name(a))
            .collect::<Vec<_>>()
            .join(&sep)
    };
    vars.insert("artist".to_string(), artist_str.clone());
    // Album artist: primary only (mirrors `get_primary_artist`), falling
    // back to the joined track artists like Python does.
    let album_artist_str = track
        .tags
        .album_artist
        .as_deref()
        .map(primary_artist)
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| artist_str.clone());
    vars.insert("album_artist".to_string(), sanitise_name(&album_artist_str));
    vars.insert(
        "name".to_string(),
        compact_path_tag(&sanitise_name(&track.name)),
    );
    // Aliases required by default format strings.
    vars.insert(
        "track_name".to_string(),
        compact_path_tag(&sanitise_name(&track.name)),
    );
    vars.insert("track_artist".to_string(), artist_str.clone());
    vars.insert("album".to_string(), sanitise_name(&track.album));
    vars.insert("id".to_string(), track.id.clone().unwrap_or_default());
    vars.insert("year".to_string(), track.release_year.to_string());
    // Align `{release_year}` with canonical release_date metadata when
    // present, so folder names don't show a reissue year while embedded
    // tags show the original date.
    let release_year = track
        .tags
        .release_date
        .as_deref()
        .and_then(|d| {
            d.trim()
                .chars()
                .take(4)
                .collect::<String>()
                .parse::<i32>()
                .ok()
        })
        .filter(|y| (1000..=9999).contains(y))
        .unwrap_or(track.release_year);
    vars.insert("release_year".to_string(), release_year.to_string());
    let zfill = settings
        .formatting()
        .get("enable_zfill")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let mut track_no = track
        .tags
        .track_number
        .map(|n| n.to_string())
        .unwrap_or_default();
    let mut disc_no = track
        .tags
        .disc_number
        .map(|n| n.to_string())
        .unwrap_or_default();
    if zfill {
        if let (Some(tn), Some(tt)) = (track.tags.track_number, track.tags.total_tracks) {
            let w = tt.to_string().len();
            track_no = format!("{tn:0>w$}", w = w);
        }
        if let (Some(dn), Some(dt)) = (track.tags.disc_number, track.tags.total_discs) {
            let w = dt.to_string().len();
            disc_no = format!("{dn:0>w$}", w = w);
        }
    }
    vars.insert("track_number".to_string(), track_no);
    vars.insert("disc_number".to_string(), disc_no);
    vars.insert(
        "isrc".to_string(),
        track.tags.isrc.clone().unwrap_or_default(),
    );
    vars.insert(
        "upc".to_string(),
        track.tags.upc.clone().unwrap_or_default(),
    );
    if let Some(bit_depth) = track.bit_depth {
        vars.insert("bit_depth".to_string(), bit_depth.to_string());
    } else {
        vars.insert("bit_depth".to_string(), String::new());
    }
    if let Some(sr) = track.sample_rate {
        vars.insert("sample_rate".to_string(), (sr / 1000.0).round().to_string());
    } else {
        vars.insert("sample_rate".to_string(), String::new());
    }
    if let Some(br) = track.bitrate {
        vars.insert("bitrate".to_string(), br.to_string());
    } else {
        vars.insert("bitrate".to_string(), String::new());
    }
    if let Some(rd) = &track.tags.release_date {
        vars.insert("release_date".to_string(), rd.clone());
    } else {
        vars.insert("release_date".to_string(), String::new());
    }
    vars.insert(
        "explicit".to_string(),
        if track.explicit.unwrap_or(false) {
            " \u{1F174}".to_string()
        } else {
            String::new()
        },
    );

    let mut name = format_template(&template, &vars);
    if !name
        .to_lowercase()
        .ends_with(&format!(".{codec_extension}"))
    {
        name.push('.');
        name.push_str(codec_extension);
    }
    let final_path = fix_byte_limit(name, 250);
    Ok(final_path)
}

pub fn track_id(track: &TrackRef) -> &str {
    track.id()
}

/// Compact very long path values while keeping them human-readable.
/// Mirrors `_compact_path_tag`: drops "(feat. ...)" / "[ft ...]" segments
/// and caps length so long titles don't blow path limits.
pub fn compact_path_tag(value: &str) -> String {
    if value.is_empty() {
        return String::new();
    }
    let mut compact = value.to_string();
    // Remove bracketed feat/ft segments: "(feat. ...)" or "[ft ...]".
    let mut out = String::with_capacity(compact.len());
    let bytes = compact.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'(' || bytes[i] == b'[' {
            let close = if bytes[i] == b'(' { b')' } else { b']' };
            if let Some(end) = bytes[i..].iter().position(|&b| b == close) {
                let inner = &compact[i + 1..i + end];
                let low = inner.to_lowercase();
                let trimmed = low.trim_start();
                if trimmed.starts_with("feat") || trimmed.starts_with("ft") {
                    i += end + 1;
                    out.push(' ');
                    continue;
                }
            }
        }
        // `compact` is valid UTF-8; copy one char at a time.
        let ch = compact[i..].chars().next().unwrap();
        out.push(ch);
        i += ch.len_utf8();
    }
    compact = out;
    // Remove trailing inline feat/ft segments (" - feat. X" / " feat X").
    for marker in [" feat. ", " feat ", " ft. ", " ft "] {
        if let Some(pos) = compact.to_lowercase().rfind(marker) {
            compact.truncate(pos);
        }
    }
    compact = compact
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .trim_matches(|c| c == ' ' || c == '.' || c == '-' || c == '_')
        .to_string();
    if compact.is_empty() {
        compact = value.trim().to_string();
    }
    const MAX_LEN: usize = 100;
    if compact.len() > MAX_LEN {
        compact.truncate(MAX_LEN);
        compact = compact
            .trim_end_matches(|c| c == ' ' || c == '.' || c == '-' || c == '_')
            .to_string();
    }
    compact
}
