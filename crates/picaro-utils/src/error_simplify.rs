//! Simplify raw error strings into one-line user-friendly messages, mirroring
//! `simplify_error_message` in OrpheusDL.

pub fn simplify_error_message(input: &str) -> String {
    let lower = input.to_lowercase();
    let s = input.trim();

    // Track unavailable / 404
    if lower.contains("track is unavailable")
        || lower.contains("track unavailable")
        || lower == "unavailable"
    {
        return "Not available (404)".to_string();
    }
    if s.contains("\"code\":404") || s.contains("\"code\": 404") {
        return "Not available (404)".to_string();
    }
    if lower.contains("status code 404") || lower.contains("error 404") {
        return "Not available (404)".to_string();
    }
    if lower.contains("total_reco") {
        return "Not available (404)".to_string();
    }

    // Apple Music FFmpeg
    if lower.contains("ffmpeg")
        && (lower.contains("remux")
            || lower.contains("processing")
            || lower.contains("legacy remux"))
    {
        return "Apple Music streaming error (FFmpeg required for processing)".to_string();
    }
    if lower.contains("not authenticated") || lower.contains("cookies.txt") {
        if lower.contains("apple") {
            return "Apple Music authentication error (cookies.txt required)".to_string();
        }
    }
    if lower.contains("apple music") {
        if let Some(rest) = s.split(" - ").last() {
            if (5..200).contains(&rest.len()) {
                if rest.to_lowercase().contains("stopiteration") {
                    return "Apple Music error: Requested quality/codec unavailable".to_string();
                }
                if rest.starts_with("Apple Music:") {
                    return rest.to_string();
                }
                return format!("Apple Music error: {rest}");
            }
        }
        if s.len() < 500 {
            if s.starts_with("Apple Music:") {
                return s.to_string();
            }
            return format!("Apple Music error: {s}");
        }
        return "Apple Music error (see logs for details)".to_string();
    }

    // Shaka Packager
    if lower.contains("shaka packager") && lower.contains("not found") {
        return "Shaka Packager executable not found but is required.\nDownload it at: https://github.com/shaka-project/shaka-packager/releases/latest\nPlace it in the same folder as picaro-rs (packager-win-x64.exe on Windows).".to_string();
    }

    // SoundCloud HLS
    if lower.contains("soundcloud")
        && (lower.contains("hls") || lower.contains("hls_unexpected_error_in_try_block"))
    {
        if lower.contains("ffmpeg") || lower.contains("hls_unexpected_error_in_try_block") {
            return "SoundCloud streaming error (FFmpeg required for HLS streams)".to_string();
        }
        return "SoundCloud streaming error".to_string();
    }

    if lower.contains("ffmpeg")
        && (lower.contains("process failed") || lower.contains("error opening"))
    {
        return "Audio processing error (FFmpeg)".to_string();
    }

    if lower.contains("url")
        || lower.contains("network")
        || lower.contains("connection")
        || lower.contains("timeout")
    {
        return "Network/connection error".to_string();
    }

    if lower.contains("no such file")
        || lower.contains("permission denied")
        || lower.contains("file not found")
    {
        return "File system error".to_string();
    }

    if lower.contains("auth")
        || lower.contains("login")
        || lower.contains("credential")
        || lower.contains("token")
    {
        return "Authentication error".to_string();
    }

    if lower.contains("rate limit") || lower.contains("too many requests") || lower.contains("429")
    {
        return "Rate limited - too many requests".to_string();
    }

    if let Some((_, last)) = s.rsplit_once(':') {
        let t = last.trim();
        if (10..100).contains(&t.len()) {
            return t.to_string();
        }
    }

    if s.len() > 120 {
        format!("{}...", &s[..117])
    } else {
        s.to_string()
    }
}
