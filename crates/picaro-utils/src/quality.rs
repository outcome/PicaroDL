//! Quality tiers for the multi-provider resolver.

use serde::{Deserialize, Serialize};

use crate::models::Quality;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum QualityTier {
    Lossless,
    High,
    Medium,
    Low,
}

impl QualityTier {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_lowercase().as_str() {
            "lossless" | "flac" | "alac" | "ape" | "wav" | "hifi" | "hi-res" | "hires" => {
                Some(Self::Lossless)
            }
            "high" | "320" | "mp3_320" => Some(Self::High),
            "medium" | "256" | "192" | "mp3_192" => Some(Self::Medium),
            "low" | "128" | "minimum" | "min" => Some(Self::Low),
            _ => None,
        }
    }

    pub fn rank(self) -> u8 {
        match self {
            Self::Lossless => 3,
            Self::High => 2,
            Self::Medium => 1,
            Self::Low => 0,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Lossless => "lossless",
            Self::High => "high",
            Self::Medium => "medium",
            Self::Low => "low",
        }
    }

    /// Requested tier first, then higher tiers, then lower tiers.
    pub fn fallback_order(self) -> [QualityTier; 4] {
        use QualityTier::*;
        match self {
            Lossless => [Lossless, High, Medium, Low],
            High => [High, Lossless, Medium, Low],
            Medium => [Medium, High, Lossless, Low],
            Low => [Low, Medium, High, Lossless],
        }
    }

    pub fn to_quality(self) -> Quality {
        match self {
            Self::Lossless => Quality::LOSSLESS,
            Self::High => Quality::HIGH,
            Self::Medium => Quality::MEDIUM,
            Self::Low => Quality::LOW,
        }
    }

    pub fn from_quality(q: Quality) -> Self {
        if q.contains(Quality::LOSSLESS) || q.contains(Quality::HIFI) || q.contains(Quality::ATMOS)
        {
            Self::Lossless
        } else if q.contains(Quality::HIGH) {
            Self::High
        } else if q.contains(Quality::MEDIUM) {
            Self::Medium
        } else {
            Self::Low
        }
    }
}
