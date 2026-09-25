//! All bundled modules. Each module gets its own file.

pub mod alterportal;
pub mod archive_org;
pub mod beatport;
pub mod beatsource;
pub mod blogspot;
pub mod butterboy;
pub mod ccmixter;
pub mod coreradio;
pub mod deadpulpit;
pub mod deezer;
pub mod discogc;
pub mod discografias;
pub mod dle_blog;
pub mod exystence;
pub mod ezhevika;
pub mod flacmusic;
pub mod glorybeats;
pub mod intmusic;
pub mod iplusfree;
pub mod losslessalbums;
pub mod losslessmusic;
pub mod lrclib;
pub mod mp3db;
pub mod musicrider;
pub mod musify;
pub mod musixmatch;
pub mod newalbumreleases;
pub mod primitiveofferings;
pub mod punkcata;
pub mod qobuz;
pub mod registry;
pub mod soundclick;
pub mod soundcloud;
pub mod spotify;
pub mod stubs;
pub mod tancpol;
pub mod themfire;
pub mod tidal;
pub mod wordpress_blog;
pub mod youtube;
pub mod zvu4it;

pub fn md5_hex(input: &[u8]) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(input);
    format!("{:x}", h.finalize())
}
