//! `utils/utils.py` equivalents: name sanitisation, byte-limit, ffmpeg/path
//! resolution, etc.

use std::path::{Component, Path, PathBuf};
use std::process::Command;

use md5::{Digest, Md5};
use regex::Regex;

/// Strip filesystem-unsafe characters from a name and normalise the spacing.
/// Mirrors `sanitise_name` in OrpheusDL.
pub fn sanitise_name<S: AsRef<str>>(name: S) -> String {
    let raw = name.as_ref();
    if raw.is_empty() {
        return String::new();
    }

    let mut s: String = if raw.contains(',') {
        raw.split(',').map(str::trim).collect::<Vec<_>>().join(", ")
    } else {
        raw.to_string()
    };

    s = s.trim().to_string();
    s = s.replace('\u{0}', "");
    // Control characters 0x00-0x1F and 0x7F
    let mut buf = String::with_capacity(s.len());
    for ch in s.chars() {
        let b = ch as u32;
        if b <= 0x1F || b == 0x7F {
            continue;
        }
        buf.push(ch);
    }
    s = buf;

    s = s
        .replace('\\', "")
        .replace('/', "")
        .replace('*', "")
        .replace('?', "")
        .replace('"', "")
        .replace('<', "")
        .replace('>', "")
        .replace('|', "")
        .replace('$', "");

    // Windows-illegal ':' -> ' · '
    let re_colon = Regex::new(r"\s*:\s*").unwrap();
    s = re_colon.replace_all(&s, " \u{00B7} ").to_string();

    let re_dash = Regex::new(r"\s+-\s+").unwrap();
    s = re_dash.replace_all(&s, " \u{00B7} ").to_string();

    s = s.split_whitespace().collect::<Vec<_>>().join(" ");
    s.trim().to_string()
}

/// Pick the first artist from a list/string.
pub fn primary_artist(artist: &str) -> String {
    if artist.is_empty() {
        return String::new();
    }
    // high-confidence separators only
    let parts: Vec<&str> = Regex::new(r"(?i) / | feat\. | ft\. ")
        .unwrap()
        .split(artist)
        .collect();
    parts.first().copied().unwrap_or(artist).trim().to_string()
}

/// Primary artist from a `MultiArtist` (string or list).
pub fn primary_from_multi(m: Option<&crate::models::MultiArtist>) -> String {
    match m {
        None => String::new(),
        Some(ma) => ma
            .to_vec()
            .first()
            .cloned()
            .map(|s| primary_artist(&s))
            .unwrap_or_default(),
    }
}

/// Truncate a string to at most `max_bytes` UTF-8 bytes, without splitting a
/// multi-byte sequence.
pub fn truncate_utf8_bytes(value: &str, max_bytes: usize) -> String {
    if max_bytes == 0 {
        return String::new();
    }
    let encoded = value.as_bytes();
    if encoded.len() <= max_bytes {
        return value.to_string();
    }
    // Find a valid UTF-8 boundary at or below max_bytes.
    let mut end = max_bytes;
    while end > 0 && (encoded[end] & 0b1100_0000) == 0b1000_0000 {
        end -= 1;
    }
    String::from_utf8_lossy(&encoded[..end]).to_string()
}

/// Trim a generated file path so the filename portion is at most `byte_limit`
/// bytes (preserving the extension). On Windows we also enforce a 220-byte
/// overall path limit (the well-known MAX_PATH - headroom for shell/Explorer).
pub fn fix_byte_limit<P: AsRef<Path>>(path: P, byte_limit: usize) -> PathBuf {
    let p = path.as_ref();
    let normalised = normalise_path(p);
    let (dir, filename) = split_dir_file(&normalised);
    let filename = match filename {
        Some(f) => f,
        None => return normalised,
    };

    let (stem, ext) = match filename.rfind('.') {
        Some(idx) => (filename[..idx].to_string(), filename[idx..].to_string()),
        None => (filename.clone(), String::new()),
    };

    let ext_bytes = ext.len();
    let max_stem_bytes = byte_limit.saturating_sub(ext_bytes).max(1);
    let mut stem = truncate_utf8_bytes(&stem, max_stem_bytes);
    let mut fixed = format!("{stem}{ext}");

    let mut candidate = match &dir {
        Some(d) => d.join(&fixed),
        None => PathBuf::from(&fixed),
    };

    #[cfg(windows)]
    {
        let limit = 220usize;
        while absolute_path_length(&candidate) > limit && stem.chars().count() > 1 {
            stem = truncate_utf8_bytes(&stem, stem.len().saturating_sub(1));
            fixed = format!("{stem}{ext}");
            candidate = match &dir {
                Some(d) => d.join(&fixed),
                None => PathBuf::from(&fixed),
            };
        }
    }

    candidate
}

fn split_dir_file(p: &Path) -> (Option<PathBuf>, Option<String>) {
    let mut comps = p.components().peekable();
    let file = match comps.next_back() {
        Some(Component::Normal(s)) => s.to_string_lossy().to_string(),
        _ => return (Some(p.to_path_buf()), None),
    };
    let dir: PathBuf = comps.collect();
    let dir = if dir.as_os_str().is_empty() {
        None
    } else {
        Some(dir)
    };
    (dir, Some(file))
}

fn normalise_path(p: &Path) -> PathBuf {
    p.components().collect()
}

#[cfg(windows)]
fn absolute_path_length(p: &Path) -> usize {
    // `canonicalize` fails for paths that do not exist yet (the common case
    // when we are about to create the file), which silently fell back to the
    // *relative* length and defeated the limit. Mirror Python's
    // `os.path.abspath` instead: join with cwd when relative.
    // NOTE: no I/O, purely lexical.
    let abs = if p.is_absolute() {
        p.to_path_buf()
    } else {
        std::env::current_dir()
            .map(|c| c.join(p))
            .unwrap_or_else(|_| p.to_path_buf())
    };
    abs.as_os_str().len()
}

/// Try to resolve an ffmpeg binary. Mirrors `locate_ffmpeg()` in OrpheusDL.
pub fn locate_ffmpeg(preferred: Option<&str>) -> Option<PathBuf> {
    let ffmpeg_name = if cfg!(windows) {
        "ffmpeg.exe"
    } else {
        "ffmpeg"
    };

    if let Some(p) = preferred
        .map(str::trim)
        .filter(|s| !s.is_empty() && s.to_lowercase() != "ffmpeg")
    {
        let path = PathBuf::from(p);
        if path.is_file() {
            return Some(path);
        }
    }

    // 1. project root
    if let Ok(cwd) = std::env::current_dir() {
        let cand = cwd.join(ffmpeg_name);
        if cand.is_file() {
            return Some(cand);
        }
    }

    // 2. PATH
    which::which(ffmpeg_name).ok()
}

/// True when the error string looks like a missing executable.
pub fn is_missing_executable_error(msg: &str) -> bool {
    let l = msg.to_lowercase();
    l.contains("winerror 2")
        || l.contains("errno 2")
        || l.contains("cannot find the file specified")
        || l.contains("no such file or directory")
        || l.contains("het systeem kan het opgegeven bestand niet vinden")
}

/// Pretty-format a number of seconds: 61 -> "1:01", 3725 -> "1:02:05".
pub fn beauty_format_seconds(seconds: u32) -> String {
    let h = seconds / 3600;
    let m = (seconds % 3600) / 60;
    let s = seconds % 60;
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Cheap MD5 helper used by Qobuz / Deezer.
pub fn md5_hex(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.update(input.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Cheap MD5 of bytes -> hex.
pub fn md5_hex_bytes(input: &[u8]) -> String {
    let mut hasher = Md5::new();
    hasher.update(input);
    format!("{:x}", hasher.finalize())
}

/// Truncate a UTF-8 string to at most N bytes (same as truncate_utf8_bytes but
/// matches the Python name `fix_byte_limit` callers sometimes use).
pub fn fix_string_len(value: &str, byte_limit: usize) -> String {
    truncate_utf8_bytes(value, byte_limit)
}

/// Run an external command, hiding its window on Windows.
pub fn run_command(mut cmd: Command) -> std::io::Result<std::process::Output> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x08000000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    cmd.output()
}
