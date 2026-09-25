//! Public API for the tagging crate.

pub mod tags;

pub use tags::{audio_probe, resize_cover_if_needed, AudioProbe, Tagger};
