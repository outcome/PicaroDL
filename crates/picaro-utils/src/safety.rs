//! Lightweight safety checks for downloaded files.
//!
//! Guards against mislabeled binaries and HTML error pages, and against
//! archives dropping unexpected (potentially executable) payloads. Files are
//! only ever written or deleted here - nothing is executed.

use std::path::{Path, PathBuf};

pub const AUDIO_EXTENSIONS: &[&str] = &[
    "flac", "mp3", "m4a", "mp4", "aac", "ogg", "oga", "opus", "wav", "aiff", "aif", "ape", "wv",
    "alac", "dsf", "dff", "wma", "mka", "webm",
];

pub fn is_audio_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| AUDIO_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Check a file's leading bytes against known audio container magic numbers.
pub fn looks_like_audio(path: &Path) -> bool {
    let Ok(bytes) = std::fs::read(path) else {
        return false;
    };
    if bytes.len() < 4 {
        return false;
    }
    let b = &bytes[..bytes.len().min(16)];
    if b.starts_with(b"fLaC") {
        return true; // FLAC
    }
    if b.starts_with(b"ID3") {
        return true; // MP3 with ID3 tag
    }
    if b[0] == 0xFF && (b[1] & 0xE0) == 0xE0 {
        return true; // MP3 frame sync
    }
    if b.starts_with(b"OggS") {
        return true; // Ogg / Opus / Vorbis
    }
    if b.starts_with(b"RIFF") {
        return true; // WAV / RIFF
    }
    if b.starts_with(b"FORM") {
        return true; // AIFF
    }
    if b.len() >= 8 && &b[4..8] == b"ftyp" {
        return true; // MP4 / M4A / ALAC
    }
    if b.starts_with(b"MAC ") {
        return true; // Monkey's Audio
    }
    if b.starts_with(b"wvpk") {
        return true; // WavPack
    }
    if b.starts_with(b"DSD ") {
        return true; // DSD
    }
    if b[0] == 0x1A && b[1] == 0x45 && b[2] == 0xDF && b[3] == 0xA3 {
        return true; // Matroska / WebM (EBML)
    }
    false
}

/// Delete every non-audio file under `dir` (recursively). Returns removed paths.
/// Used after extracting archives so unexpected payloads (e.g. .exe/.scr/.js)
/// never remain. Nothing is executed.
pub fn purge_non_audio(dir: &Path) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if !is_audio_extension(&path) && std::fs::remove_file(&path).is_ok() {
                removed.push(path);
            }
        }
    }
    removed
}

// ---------------------------------------------------------------------------
//  Malware / executable safety
// ---------------------------------------------------------------------------

use std::io::Read;

/// Extensions that are executable or script-like: never expected in audio.
pub const DANGEROUS_EXTENSIONS: &[&str] = &[
    "exe", "dll", "scr", "com", "bat", "cmd", "msi", "msix", "js", "jse", "vbs", "vbe", "wsf",
    "wsh", "ps1", "psm1", "psd1", "lnk", "url", "hta", "cpl", "reg", "jar", "sh", "bash", "zsh",
    "app", "pif", "gadget", "inf", "sys", "drv", "msc", "apk", "dmg", "pkg", "deb", "rpm", "iso",
    "img", "vhd", "vbs", "ws", "scf", "desktop",
];

pub fn is_dangerous_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| DANGEROUS_EXTENSIONS.contains(&e.to_lowercase().as_str()))
        .unwrap_or(false)
}

/// Detect executable/script *content* by magic bytes (PE, ELF, Mach-O, shebang).
pub fn looks_like_executable(path: &Path) -> bool {
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let mut b = [0u8; 4];
    let n = f.read(&mut b).unwrap_or(0);
    if n >= 2 && &b[..2] == b"MZ" {
        return true; // Windows PE (exe/dll)
    }
    if n >= 4 && &b == b"\x7fELF" {
        return true; // Linux ELF
    }
    if n >= 4
        && matches!(
            b,
            [0xFE, 0xED, 0xFA, 0xCE]
                | [0xFE, 0xED, 0xFA, 0xCF]
                | [0xCE, 0xFA, 0xED, 0xFE]
                | [0xCF, 0xFA, 0xED, 0xFE]
        )
    {
        return true; // Mach-O
    }
    if n >= 2 && &b[..2] == b"#!" {
        return true; // shebang script
    }
    false
}

pub fn is_dangerous(path: &Path) -> bool {
    is_dangerous_extension(path) || looks_like_executable(path)
}

/// Recursively find dangerous files under `dir`.
pub fn scan_dangerous(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&d) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if is_dangerous(&path) {
                found.push(path);
            }
        }
    }
    found
}

/// Reason a file is unsafe (executable/script content).
pub fn danger_reason(path: &Path) -> Option<String> {
    if is_dangerous(path) {
        return Some("executable/script content".into());
    }
    None
}
