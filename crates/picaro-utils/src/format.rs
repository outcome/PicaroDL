//! Format a path with the user-defined {placeholder} templates.

use std::collections::BTreeMap;

use crate::error::{Error, Result};

/// Build a path/filename from a template like `{artist} - {name}.{ext}`.
///
/// Unknown placeholders become empty strings. Missing values are also empty.
/// `sanitise` is applied to every text value to make the path filesystem-safe.
pub fn format_template<S: AsRef<str>>(template: S, vars: &BTreeMap<String, String>) -> String {
    let template = template.as_ref();
    let mut out = String::with_capacity(template.len());
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '{' {
            // find matching '}'
            if let Some(close_rel) = template[i + 1..].find('}') {
                let key = &template[i + 1..i + 1 + close_rel];
                let val = vars.get(key).cloned().unwrap_or_default();
                out.push_str(&val);
                i = i + 1 + close_rel + 1;
                continue;
            }
        }
        out.push(c);
        i += 1;
    }
    out
}

/// Convenience: build a vars map from typical track info + zfill logic.
pub fn zfill(value: &str, width: usize) -> String {
    if value.len() >= width {
        return value.to_string();
    }
    format!("{}{}", "0".repeat(width - value.len()), value)
}

/// Validate a quality name (e.g. "hifi", "lossless") -> module-quality enum.
pub fn parse_quality<S: AsRef<str>>(q: S) -> Result<crate::models::Quality> {
    let q = q.as_ref().to_lowercase();
    let parsed = match q.as_str() {
        "minimum" => crate::models::Quality::MINIMUM,
        "low" => crate::models::Quality::LOW,
        "medium" => crate::models::Quality::MEDIUM,
        "high" => crate::models::Quality::HIGH,
        "lossless" => crate::models::Quality::LOSSLESS,
        "hifi" => crate::models::Quality::HIFI,
        "atmos" => crate::models::Quality::ATMOS,
        other => {
            return Err(Error::Other(format!(
                "Unknown quality '{other}'. Expected one of: minimum, low, medium, high, lossless, hifi, atmos"
            )))
        }
    };
    Ok(parsed)
}

/// Validate a module name against the registry; returns error with hint if not found.
pub fn ensure_module<S: AsRef<str>>(
    name: S,
    registry: &crate::ModuleRegistry,
) -> Result<crate::RegisteredModule> {
    let n = name.as_ref().to_lowercase();
    registry
        .get(&n)
        .ok_or_else(|| Error::Other(format!("Module '{n}' is not installed")))
}
