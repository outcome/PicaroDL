//! Strongly-typed view over the merged global settings.

use std::collections::HashMap;

use serde_json::Value;

#[derive(Debug, Default, Clone)]
pub struct GlobalSettings {
    pub raw: serde_json::Map<String, Value>,
}

impl GlobalSettings {
    pub fn from_merged(merged: &serde_json::Map<String, Value>) -> Self {
        Self {
            raw: merged.clone(),
        }
    }

    pub fn section(&self, name: &str) -> HashMap<String, Value> {
        self.raw
            .get(name)
            .and_then(|v| v.as_object())
            .map(|o| o.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }

    pub fn get<'a>(&self, section: &'a str, key: &'a str) -> Option<&Value> {
        self.raw
            .get(section)
            .and_then(|v| v.as_object())
            .and_then(|o| o.get(key))
    }

    pub fn get_str(&self, section: &str, key: &str) -> Option<String> {
        self.get(section, key)
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    }

    pub fn get_bool(&self, section: &str, key: &str) -> Option<bool> {
        self.get(section, key).and_then(|v| v.as_bool())
    }

    pub fn get_int(&self, section: &str, key: &str) -> Option<i64> {
        self.get(section, key).and_then(|v| v.as_i64())
    }

    pub fn get_str_or<'a>(&'a self, section: &'a str, key: &'a str, default: &'a str) -> String {
        self.get_str(section, key)
            .unwrap_or_else(|| default.to_string())
    }

    pub fn get_bool_or(&self, section: &str, key: &str, default: bool) -> bool {
        self.get_bool(section, key).unwrap_or(default)
    }

    pub fn get_int_or(&self, section: &str, key: &str, default: i64) -> i64 {
        self.get_int(section, key).unwrap_or(default)
    }

    pub fn formatting(&self) -> HashMap<String, Value> {
        self.section("formatting")
    }

    pub fn covers(&self) -> HashMap<String, Value> {
        self.section("covers")
    }

    pub fn advanced(&self) -> HashMap<String, Value> {
        self.section("advanced")
    }

    pub fn general(&self) -> HashMap<String, Value> {
        self.section("general")
    }

    pub fn lyrics(&self) -> HashMap<String, Value> {
        self.section("lyrics")
    }

    pub fn playlist(&self) -> HashMap<String, Value> {
        self.section("playlist")
    }

    pub fn codecs(&self) -> HashMap<String, Value> {
        self.section("codecs")
    }
}
