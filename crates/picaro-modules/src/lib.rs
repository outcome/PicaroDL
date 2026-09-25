//! All bundled modules. Each module gets its own file.

pub mod blogspot;
pub mod butterboy;
pub mod ccmixter;
pub mod coreradio;
pub mod dance_music;
pub mod deezer_preview;
pub mod dle_blog;
pub mod ektoplazm;
pub mod freemp3cloud;
pub mod globaldjmix;
pub mod grimearchive;
pub mod lrclib;
pub mod lyrics_ovh;
pub mod lyrist;
pub mod musixmatch;
pub mod punkcata;
pub mod registry;
pub mod soulseek;
pub mod soundcloud;
pub mod stubs;
pub mod tancpol;
pub mod wordpress_blog;
pub mod youtube;
pub mod zvu4it;

pub fn md5_hex(input: &[u8]) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(input);
    format!("{:x}", h.finalize())
}
