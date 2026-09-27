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
use lofty::probe::Probe;

use picaro_utils::error::{Error, Result};

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
