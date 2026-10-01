//! Audio fingerprint verification for magnet/torrent downloads.
//!
//! Compares a downloaded audio file against a known-good reference
//! found in the user's Music folder or InnerTune cache. Uses lofty's
//! decoded audio properties (duration / sample rate / channels / bitrate)
//! as a coarse fingerprint; catches obvious mislabeled grabs without
//! needing full PCM waveform analysis.
//!
//! Skips silently when no reference is available - the resolver then
//! falls back to its text-overlap score, same as before.

use std::path::{Path, PathBuf};

use lofty::file::AudioFile;
use lofty::prelude::*;
use lofty::probe::Probe;

use picaro_utils::error::{Error, Result};

/// Delete a rejected download and any sidecar files written next to it
/// (e.g. the synced-lyrics `.lrc`): a rejection must leave nothing
/// behind.
pub fn remove_with_sidecars(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(path.with_extension("lrc"));
}

/// Verify a downloaded file's ACTUAL duration against the expected
/// duration for the requested track. Rejects anything wildly off
/// (<50% or >200%, with 2s slack so interludes/gaps don't false-positive):
/// the "lose the rain" incident served a 177s song for a 28s interlude
/// and nothing caught it. The caller is expected to delete the file on
/// `Err`. Unprobeable files pass (no false blocks on odd containers).
pub fn verify_expected_duration(path: &Path, expected_secs: u64) -> Result<()> {
    if expected_secs == 0 {
        return Ok(());
    }
    let got = match probe_duration_secs(path).filter(|d| *d > 0.5) {
        Some(d) => d,
        // No duration could be determined - do not block on containers
        // we can't read.
        None => return Ok(()),
    };
    let expected = expected_secs as f64;
    if got + 2.0 < expected * 0.5 || got > expected * 2.0 + 2.0 {
        return Err(Error::Download(format!(
            "duration mismatch (expected ~{expected_secs}s, got {}s)",
            got.round()
        )));
    }
    Ok(())
}

/// Post-download artist check: the delivered file's own tags must agree
/// with the requested artist. This catches cover bands /tribute
/// recordings/ mislabeled grabs that share the title and roughly the
/// right duration (mp3tut once served "Re Beatles" covers for a Beatles
/// query). Passes silently when no artist tag or no query artist exists.
pub fn verify_delivered_artist(path: &Path, query: &str) -> Result<()> {
    let (qa, _) = picaro_utils::textmatch::split_query(query);
    let Some(query_artist) = qa else {
        return Ok(());
    };
    let artist = delivered_tag_artist(path);
    let Some(artist) = artist else {
        return Ok(()); // no artist tag -> don't block
    };
    let sim = picaro_utils::textmatch::similarity(&query_artist, &artist);
    let concat = picaro_utils::textmatch::contains_fold(&artist, &query_artist);
    if sim >= 0.4 || concat {
        return Ok(());
    }
    Err(Error::Download(format!(
        "artist mismatch (wanted '{query_artist}', got '{artist}')"
    )))
}

/// The artist tag of a delivered file: lofty first, then a raw ID3v2 /
/// FLAC Vorbis-comment scan. Real-world sources carry malformed frames
/// (e.g. a TDRC "1969-") that make lofty fail the ENTIRE read, and a
/// probe failure would silently disable the artist check - the same
/// class of hole that let wrong files through in the first place.
fn delivered_tag_artist(path: &Path) -> Option<String> {
    if let Ok(tagged) = lofty::read_from_path(path) {
        if let Some(tag) = tagged.primary_tag() {
            if let Some(a) = tag.artist() {
                let a = a.trim();
                if !a.is_empty() {
                    return Some(a.to_string());
                }
            }
        }
    }
    let bytes = std::fs::read(path).ok()?;
    raw_tag_artist(&bytes)
}

/// Artist straight out of the raw bytes: ID3v2 TPE1 frame (2.3/2.4) or
/// FLAC VORBIS_COMMENT ARTIST field.
fn raw_tag_artist(b: &[u8]) -> Option<String> {
    if b.starts_with(b"fLaC") {
        return flac_vorbis_artist(b);
    }
    if b.starts_with(b"ID3") {
        return id3_tpe1_artist(b);
    }
    None
}

fn flac_vorbis_artist(b: &[u8]) -> Option<String> {
    let mut pos = 4;
    for _ in 0..32 {
        if pos + 4 > b.len() {
            return None;
        }
        let block_type = b[pos] & 0x7f;
        let len = ((b[pos + 1] as usize) << 16) | ((b[pos + 2] as usize) << 8) | b[pos + 3] as usize;
        let body = b.get(pos + 4..pos + 4 + len)?;
        if block_type == 4 {
            // vendor string, then comment count, then "KEY=value" entries
            if body.len() < 4 {
                return None;
            }
            let vlen = u32::from_le_bytes([body[0], body[1], body[2], body[3]]) as usize;
            let mut p = 4 + vlen;
            if body.len() < p + 4 {
                return None;
            }
            let count =
                u32::from_le_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]]) as usize;
            p += 4;
            for _ in 0..count {
                if body.len() < p + 4 {
                    return None;
                }
                let clen = u32::from_le_bytes([body[p], body[p + 1], body[p + 2], body[p + 3]])
                    as usize;
                p += 4;
                let entry = body.get(p..p + clen)?;
                p += clen;
                if let Some(rest) = entry.strip_prefix(b"ARTIST=".as_slice()) {
                    let s = String::from_utf8_lossy(rest).trim().to_string();
                    if !s.is_empty() {
                        return Some(s);
                    }
                }
            }
            return None;
        }
        pos += 4 + len;
    }
    None
}

fn id3_tpe1_artist(b: &[u8]) -> Option<String> {
    if b.len() < 10 {
        return None;
    }
    let ver = b[3];
    let tag_size = ((b[6] as usize & 0x7f) << 21)
        | ((b[7] as usize & 0x7f) << 14)
        | ((b[8] as usize & 0x7f) << 7)
        | (b[9] as usize & 0x7f);
    let end = (10 + tag_size).min(b.len());
    let mut pos = 10;
    while pos + 10 <= end {
        let id = &b[pos..pos + 4];
        if &b[pos..pos + 4] == b"\0\0\0\0" {
            break; // padding
        }
        let flen = if ver == 4 {
            ((b[pos + 4] as usize & 0x7f) << 21)
                | ((b[pos + 5] as usize & 0x7f) << 14)
                | ((b[pos + 6] as usize & 0x7f) << 7)
                | (b[pos + 7] as usize & 0x7f)
        } else {
            u32::from_be_bytes([b[pos + 4], b[pos + 5], b[pos + 6], b[pos + 7]]) as usize
        };
        if flen == 0 || flen > end {
            break;
        }
        if id == b"TPE1" {
            let body = b.get(pos + 10..pos + 10 + flen)?;
            return Some(decode_id3_text(body)?);
        }
        pos += 10 + flen;
    }
    None
}

/// Decode an ID3 text frame body: [encoding byte][text bytes].
fn decode_id3_text(body: &[u8]) -> Option<String> {
    let (enc, text) = body.split_first()?;
    match enc {
        0 => Some(String::from_utf8_lossy(text).trim().to_string()), // latin1 ~= utf8 lossy for tags
        3 => Some(String::from_utf8_lossy(text).trim().to_string()),
        1 | 2 => {
            // UTF-16 with BOM; keep it simple and only handle the common cases
            let bytes: Vec<u8> = text.to_vec();
            let s = if bytes.starts_with(&[0xFF, 0xFE]) {
                String::from_utf16_lossy(
                    &bytes[2..]
                        .chunks(2)
                        .filter(|c| c.len() == 2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect::<Vec<u16>>(),
                )
            } else if bytes.starts_with(&[0xFE, 0xFF]) {
                String::from_utf16_lossy(
                    &bytes[2..]
                        .chunks(2)
                        .filter(|c| c.len() == 2)
                        .map(|c| u16::from_be_bytes([c[0], c[1]]))
                        .collect::<Vec<u16>>(),
                )
            } else {
                String::from_utf16_lossy(
                    &bytes
                        .chunks(2)
                        .filter(|c| c.len() == 2)
                        .map(|c| u16::from_le_bytes([c[0], c[1]]))
                        .collect::<Vec<u16>>(),
                )
            };
            Some(s.trim().to_string())
        }
        _ => None,
    }
    .filter(|s| !s.is_empty())
    .map(|s| s.split('\u{0}').next().unwrap_or("").trim().to_string())
    .filter(|s| !s.is_empty())
}

/// Best-effort duration: lofty first, then a raw container parse for the
/// two formats that matter (FLAC / MP3). Lofty chokes on some real-world
/// files (e.g. ID3v2 timestamp frames like "2001-"), and a silent probe
/// failure would silently disable the duration guard - so the fallback
/// parses the container headers directly.
pub fn probe_duration_secs(path: &Path) -> Option<f64> {
    if let Some(probe) = picaro_tagging::audio_probe(path) {
        if probe.duration_secs > 0.5 {
            return Some(probe.duration_secs);
        }
    }
    let bytes = std::fs::read(path).ok()?;
    if bytes.starts_with(b"fLaC") {
        return flac_duration_secs(&bytes);
    }
    if bytes.starts_with(b"ID3") || (bytes.first() == Some(&0xFF) && bytes.get(1).map_or(false, |b| b & 0xE0 == 0xE0)) {
        return mp3_duration_secs(&bytes);
    }
    None
}

/// FLAC: total samples and sample rate from the STREAMINFO metadata block.
fn flac_duration_secs(b: &[u8]) -> Option<f64> {
    // "fLaC" then blocks: [flags(1)] [len(3)] [body]; type 0 = STREAMINFO.
    if b.len() < 4 + 4 + 34 {
        return None;
    }
    let mut pos = 4;
    for _ in 0..16 {
        if pos + 4 > b.len() {
            return None;
        }
        let block_type = b[pos] & 0x7f;
        let len = ((b[pos + 1] as usize) << 16) | ((b[pos + 2] as usize) << 8) | b[pos + 3] as usize;
        let body = b.get(pos + 4..pos + 4 + len)?;
        if block_type == 0 && len >= 18 {
            // body[0..2]=min block, [2..4]=max block, [4..7]+[7..10]=frame sizes,
            // then 8 bytes: 20 bits sample rate, 3 bits channels, 5 bits bps,
            // 36 bits total samples.
            let v = u64::from_be_bytes([
                0,
                body[10],
                body[11],
                body[12],
                body[13],
                body[14],
                body[15],
                body[16],
            ]);
            let sample_rate = (v >> 44) & 0xFFFFF;
            let total_samples = v & 0xFFFFFFFFF;
            if sample_rate > 0 && total_samples > 0 {
                return Some(total_samples as f64 / sample_rate as f64);
            }
            return None;
        }
        pos += 4 + len;
    }
    None
}

/// MP3: skip ID3v2, find the first frame, prefer the Xing/Info frame
/// count, else estimate from the first frame's bitrate and the data size.
fn mp3_duration_secs(b: &[u8]) -> Option<f64> {
    let mut pos = 0usize;
    if b.starts_with(b"ID3") && b.len() > 10 {
        let size = ((b[6] as usize & 0x7f) << 21)
            | ((b[7] as usize & 0x7f) << 14)
            | ((b[8] as usize & 0x7f) << 7)
            | (b[9] as usize & 0x7f);
        pos = 10 + size;
    }
    // Find a valid frame sync (Layer III).
    let mut frame = None;
    let scan_end = b.len().min(pos + 200_000);
    while pos + 4 <= scan_end {
        if b[pos] == 0xFF && (b[pos + 1] & 0xE0) == 0xE0 && ((b[pos + 1] >> 1) & 0x03) == 0x01 {
            frame = Some(pos);
            break;
        }
        pos += 1;
    }
    let f = frame?;
    let version_bits = (b[f + 1] >> 3) & 0x03; // 0=MPEG2.5, 2=MPEG2, 3=MPEG1
    let mpeg1 = version_bits == 3;
    let bitrate_idx = (b[f + 2] >> 4) as usize;
    let sr_idx = ((b[f + 2] & 0x0C) >> 2) as usize;
    if bitrate_idx == 0 || bitrate_idx == 15 || sr_idx == 3 {
        return None;
    }
    let bitrate = match if mpeg1 {
        [0, 32, 40, 48, 56, 64, 80, 96, 112, 128, 160, 192, 224, 256, 320, 0]
    } else {
        [0, 8, 16, 24, 32, 40, 48, 56, 64, 80, 96, 112, 128, 144, 160, 0]
    }
    .get(bitrate_idx)
    {
        Some(k) if *k > 0 => *k as f64,
        _ => return None,
    };
    let sample_rate = match version_bits {
        3 => [44100.0, 48000.0, 32000.0][sr_idx], // MPEG1
        2 => [22050.0, 24000.0, 16000.0][sr_idx], // MPEG2
        0 => [11025.0, 12000.0, 8000.0][sr_idx],  // MPEG2.5
        _ => return None,
    };
    let samples_per_frame = if mpeg1 { 1152.0 } else { 576.0 };

    // Xing/Info header: inside the first frame, after the side info.
    let side_info = if mpeg1 {
        if (b[f + 3] & 0xC0) >> 6 == 3 {
            32
        } else {
            17
        }
    } else if (b[f + 3] & 0xC0) >> 6 == 3 {
        17
    } else {
        9
    };
    let tag_pos = f + 4 + side_info;
    for marker in [b"Xing", b"Info"] {
        if b.get(tag_pos..tag_pos + 4).map_or(false, |w| w == marker) {
            let flags = b
                .get(tag_pos + 4..tag_pos + 8)
                .map(|w| u32::from_be_bytes([w[0], w[1], w[2], w[3]]))
                .unwrap_or(0);
            if flags & 0x1 != 0 {
                if let Some(w) = b.get(tag_pos + 8..tag_pos + 12) {
                    let frames = u32::from_be_bytes([w[0], w[1], w[2], w[3]]) as f64;
                    if frames > 0.0 {
                        return Some(frames * samples_per_frame / sample_rate);
                    }
                }
            }
            break;
        }
    }
    // CBR estimate from the audio data size.
    let data = (b.len().saturating_sub(f)) as f64;
    Some(data * 8.0 / (bitrate * 1000.0))
}

/// Directories that may contain a known-good reference copy of a track:
/// the user's Music folder and InnerTune's cache (if mounted from Android
/// or copied over). Add more as needed.
pub fn default_search_dirs() -> Vec<PathBuf> {
 let mut out = Vec::new();
 if let Ok(home) = std::env::var("USERPROFILE") {
 out.push(PathBuf::from(&home).join("Music"));
 out.push(PathBuf::from(&home).join("Music").join("InnerTune"));
 out.push(PathBuf::from(&home).join("InnerTune"));
 out.push(PathBuf::from(&home).join("Downloads").join("InnerTune"));
 }
 if let Ok(local) = std::env::var("LOCALAPPDATA") {
 out.push(PathBuf::from(&local).join("InnerTune").join("cache"));
 }
 out
}

const AUDIO_EXTS: &[&str] = &["flac", "mp3", "m4a", "aac", "ogg", "oga", "opus", "wav"];

fn is_audio_name(name: &str) -> bool {
 let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
 AUDIO_EXTS.contains(&ext.as_str())
}

#[derive(Debug, Clone, Copy, Default)]
struct Fingerprint {
 duration_ms: u64,
 sample_rate: u32,
 channels: u32,
 bitrate_kbps: u32,
}

fn audio_fingerprint(path: &Path) -> Result<Fingerprint> {
 let tagged = Probe::open(path)
 .map_err(|e| Error::Other(format!("fp open {}: {e}", path.display())))?
 .guess_file_type()
 .map_err(|e| Error::Other(format!("fp guess: {e}")))?
 .read()
 .map_err(|e| Error::Other(format!("fp read: {e}")))?;
 let p = tagged.properties();
 Ok(Fingerprint {
 duration_ms: p.duration().as_millis() as u64,
 sample_rate: p.sample_rate().unwrap_or(0),
 channels: p.channels().unwrap_or(0) as u32,
 bitrate_kbps: p.audio_bitrate().map(|b| b as u32).unwrap_or(0),
 })
}

fn find_reference(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
 let tokens: Vec<String> = name
 .split_whitespace()
 .filter(|s| !s.is_empty())
 .map(|s| s.to_ascii_lowercase())
 .collect();
 if tokens.is_empty() {
 return None;
 }
 let mut best: Option<(usize, PathBuf)> = None;
 for d in dirs {
 let walker = match std::fs::read_dir(d) {
 Ok(e) => e,
 Err(_) => continue,
 };
 for entry in walker.flatten() {
 let path = entry.path();
 if !path.is_file() {
 continue;
 }
 let Some(name_str) = path.file_name().and_then(|s| s.to_str()) else {
 continue;
 };
 if !is_audio_name(name_str) {
 continue;
 }
 let stem = path
 .file_stem()
 .and_then(|s| s.to_str())
 .unwrap_or("")
 .to_ascii_lowercase();
 let hits = tokens.iter().filter(|t| stem.contains(t.as_str())).count();
 if hits == 0 {
 continue;
 }
 if best.as_ref().map_or(true, |(h, _)| hits > *h) {
 best = Some((hits, path));
 }
 }
 }
 best.map(|(_, p)| p)
}

/// Compare a downloaded magnet/torrent file against a known-good reference.
///
/// Returns `Ok(())` when no reference exists (skip), or when fingerprints
/// match (duration within +-2s, sample rate equal, channels equal, bitrate
/// within +-25%). Returns `Err` when they diverge enough to suggest a wrong
/// grab. The caller is expected to delete the downloaded file on `Err`.
pub fn verify_against_reference(
 downloaded: &Path,
 track_name: &str,
 dirs: &[PathBuf],
) -> Result<()> {
 let Some(reference) = find_reference(track_name, dirs) else {
 return Ok(());
 };
 let dl = audio_fingerprint(downloaded)?;
 let rf = audio_fingerprint(&reference)?;
 if (dl.duration_ms as i64 - rf.duration_ms as i64).abs() > 2000 {
 return Err(Error::Download(format!(
 "fingerprint mismatch: duration {}ms vs reference {}ms ({})",
 dl.duration_ms,
 rf.duration_ms,
 reference.display()
 )));
 }
 if dl.sample_rate != 0 && rf.sample_rate != 0 && dl.sample_rate != rf.sample_rate {
 return Err(Error::Download(format!(
 "fingerprint mismatch: sample rate {}Hz vs reference {}Hz ({})",
 dl.sample_rate,
 rf.sample_rate,
 reference.display()
 )));
 }
 if dl.channels != 0 && rf.channels != 0 && dl.channels != rf.channels {
 return Err(Error::Download(format!(
 "fingerprint mismatch: channels {} vs reference {} ({})",
 dl.channels,
 rf.channels,
 reference.display()
 )));
 }
 if dl.bitrate_kbps != 0 && rf.bitrate_kbps != 0 {
 let diff = (dl.bitrate_kbps as i32 - rf.bitrate_kbps as i32).abs();
 let max = (rf.bitrate_kbps as f32 * 0.25) as i32 + 1;
 if diff > max {
 return Err(Error::Download(format!(
 "fingerprint mismatch: bitrate {}kbps vs reference {}kbps ({})",
 dl.bitrate_kbps,
 rf.bitrate_kbps,
 reference.display()
 )));
 }
 }
 Ok(())
}
