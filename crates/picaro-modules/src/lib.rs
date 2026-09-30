//! All bundled modules. Each module gets its own file.

pub mod blogspot;
pub mod butterboy;
pub mod ccmixter;
pub mod certifiedmixtapez;
pub mod coreradio;
pub mod dance_music;
pub mod darktorrent;
pub mod dle_blog;
pub mod ektoplazm;
pub mod ezhevika;
pub mod fma;
pub mod fondsound;
pub mod freemp3cloud;
pub mod globaldjmix;
pub mod grimearchive;
pub mod impersonate;
pub mod internetarchive;

#[cfg(feature = "cf-impersonate")]
pub mod khinsider;
pub mod lrclib;
pub mod lyrics_ovh;
pub mod lyrist;
pub mod mixtapemonkey;
pub mod musixmatch;
pub mod mp3tut;
pub mod mp3zona;
pub mod onetrance;
pub mod piratebay;
pub mod punkcata;
pub mod registry;
pub mod relisten;
pub mod soulseek;
pub mod soundcloud;
pub mod sor;
pub mod tancpol;
pub mod technicaldeathmetal;
pub mod testpressing;
pub mod tomlehrer;
pub mod wordpress_blog;
pub mod youtube;
pub mod zvu4it;

pub fn md5_hex(input: &[u8]) -> String {
    use md5::Digest;
    let mut h = md5::Md5::new();
    h.update(input);
    format!("{:x}", h.finalize())
}
