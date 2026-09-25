//! Public API for the downloader crate.

pub mod downloader;
pub mod globals;
pub mod hosters;
pub mod http;
pub mod mega;
pub mod paths;
pub mod resolver;

pub use downloader::{DownloadEvent, Downloader, LogLevel};
pub use globals::GlobalSettings;
