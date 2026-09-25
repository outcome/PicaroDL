//! Deezer Blowfish decryption for the legacy MP3 download format.
//!
//! This is a faithful port of `dl_track` in `modules/deezer/dzapi.py`.

use std::io::Write;

use blowfish::cipher::{block_padding::NoPadding, BlockDecryptMut, KeyIvInit};
use cbc::Decryptor;
use md5::{Digest, Md5};

type BlowfishCbcDec = Decryptor<blowfish::Blowfish>;

/// Decrypt a Deezer BF_CBC_STRIPE stream.
///
/// `bf_secret` is the `bf_secret` configured for the module
/// (default `g4el58wc0zvf9na1`). `track_id` is the numeric Deezer SNG_ID.
///
/// `reader` is the (possibly already-buffered) encrypted bytes; we read the
/// input in 2048-byte chunks and decrypt every *third* chunk using a fresh
/// cipher (matching the original implementation's quirky DRM).
pub fn decrypt_to_writer<R: std::io::Read, W: std::io::Write + Write>(
    mut reader: R,
    writer: &mut W,
    track_id: &str,
    bf_secret: &str,
) -> std::io::Result<()> {
    let key = compute_blowfish_key(track_id, bf_secret);
    let iv = [0u8; 8];
    let mut buf = vec![0u8; 2048];
    let mut index: usize = 0;
    loop {
        let n = reader.read(&mut buf)?;
        if n == 0 {
            break;
        }
        let chunk = if n == 2048 && index % 3 == 0 {
            let cipher = BlowfishCbcDec::new_from_slices(&key, &iv).expect("blowfish key/iv");
            cipher
                .decrypt_padded_mut::<NoPadding>(&mut buf[..n])
                .map(|p| p.to_vec())
                .unwrap_or_else(|_| buf[..n].to_vec())
        } else {
            buf[..n].to_vec()
        };
        writer.write_all(&chunk)?;
        index += 1;
    }
    Ok(())
}

fn compute_blowfish_key(track_id: &str, bf_secret: &str) -> Vec<u8> {
    let mut hasher = Md5::new();
    hasher.update(track_id.as_bytes());
    let digest = hasher.finalize();
    let hex = format!("{:x}", digest);
    let bytes = hex.as_bytes();
    let secret = bf_secret.as_bytes();
    let mut key = vec![0u8; 16];
    for i in 0..16 {
        key[i] = bytes[i] ^ bytes[i + 16] ^ secret[i];
    }
    key
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_blowfish_key() {
        let k = compute_blowfish_key("123", "g4el58wc0zvf9na1");
        assert_eq!(k.len(), 16);
    }
}
