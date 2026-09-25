//! Tag a downloaded audio file with the metadata we collected.
//!
//! Uses `lofty` 0.21 to write Vorbis Comments (FLAC/Ogg/Opus), ID3v2 (MP3),
//! and MP4 iTunes atoms (M4A). The 0.21 API exposes typed setters for a
//! small set of common fields; everything else is written via
//! `Tag::insert_text` with a string `ItemKey` or, for special-cased fields,
//! via `TagItem` constructors.

use std::path::Path;

use lofty::config::WriteOptions;
use lofty::file::AudioFile;
use lofty::file::TaggedFileExt;
use lofty::picture::{MimeType, Picture, PictureType};
use lofty::prelude::*;
use lofty::tag::{ItemKey, Tag, TagType};

use picaro_utils::error::{Error, Result};
use picaro_utils::models::{Container, CreditsInfo, TrackInfo};

use image::ImageReader;

/// One-shot tagger that handles a single file.
pub struct Tagger<'a> {
    pub track: &'a TrackInfo,
    pub image_path: Option<&'a Path>,
    pub embedded_lyrics: Option<&'a str>,
    pub credits: &'a [CreditsInfo],
    pub metadata_separator: &'a str,
    pub split_metadata: bool,
    pub container: Container,
    pub file_path: Option<&'a Path>,
}

impl<'a> Tagger<'a> {
    pub fn new(track: &'a TrackInfo, container: Container) -> Self {
        Self {
            track,
            image_path: None,
            embedded_lyrics: None,
            credits: &[],
            metadata_separator: ";",
            split_metadata: true,
            container,
            file_path: None,
        }
    }

    pub fn with_image(mut self, path: &'a Path) -> Self {
        self.image_path = Some(path);
        self
    }

    pub fn with_lyrics(mut self, lyrics: &'a str) -> Self {
        self.embedded_lyrics = Some(lyrics);
        self
    }

    pub fn with_credits(mut self, credits: &'a [CreditsInfo]) -> Self {
        self.credits = credits;
        self
    }

    pub fn with_split(mut self, split: bool) -> Self {
        self.split_metadata = split;
        self
    }

    pub fn with_separator(mut self, sep: &'a str) -> Self {
        self.metadata_separator = sep;
        self
    }

    pub fn with_path(mut self, path: &'a Path) -> Self {
        self.file_path = Some(path);
        self
    }

    /// Write all tags to the file. This is best-effort: failures return
    /// `Err` and the caller may decide to log + keep the file as-is.
    pub fn write(&self) -> Result<()> {
        let path = self
            .file_path
            .ok_or_else(|| Error::TagSavingFailure("Tagger.file_path not set".to_string()))?;
        let vorbis = self.populate_vorbis();
        let id3 = self.populate_id3();
        let (prefer, other) = match self.container {
            Container::Flac | Container::Ogg | Container::Opus => (vorbis, Some(id3)),
            _ => (id3, None),
        };
        write_tags(path, &prefer, other.as_ref(), self.image_path)
    }

    fn populate_vorbis(&self) -> Tag {
        let mut tag = Tag::new(TagType::VorbisComments);
        let _ = self.fill_common(&mut tag, true);
        if let Some(rg) = self.track.tags.replay_gain {
            tag.insert_text(
                ItemKey::Unknown("REPLAYGAIN_TRACK_GAIN".to_string()),
                format!("{rg} dB"),
            );
        }
        if let Some(rp) = self.track.tags.replay_peak {
            tag.insert_text(
                ItemKey::Unknown("REPLAYGAIN_TRACK_PEAK".to_string()),
                format!("{rp}"),
            );
        }
        if let Some(label) = &self.track.tags.label {
            tag.insert_text(ItemKey::Unknown("LABEL".to_string()), label.clone());
            tag.insert_text(ItemKey::Unknown("PUBLISHER".to_string()), label.clone());
            tag.insert_text(ItemKey::Unknown("ORGANIZATION".to_string()), label.clone());
        }
        if let Some(url) = &self.track.tags.track_url {
            tag.insert_text(ItemKey::Unknown("TRACK_URL".to_string()), url.clone());
        }
        tag
    }

    fn populate_id3(&self) -> Tag {
        let mut tag = Tag::new(TagType::Id3v2);
        let _ = self.fill_common(&mut tag, false);
        if let Some(label) = &self.track.tags.label {
            tag.insert_text(ItemKey::Unknown("PUBLISHER".to_string()), label.clone());
        }
        if let Some(url) = &self.track.tags.track_url {
            tag.insert_text(ItemKey::Unknown("URL".to_string()), url.clone());
        }
        if let Some(upc) = &self.track.tags.upc {
            tag.insert_text(ItemKey::Unknown("UPC".to_string()), upc.clone());
        }
        tag
    }

    fn fill_common(&self, tag: &mut Tag, is_vorbis: bool) -> Result<()> {
        tag.set_title(self.track.name.clone());
        if !self.track.album.is_empty() {
            tag.set_album(self.track.album.clone());
        }
        if self.split_metadata {
            for a in &self.track.artists {
                tag.insert_text(ItemKey::TrackArtist, a.clone());
            }
        } else {
            tag.set_artist(self.track.artists.join(self.metadata_separator));
        }
        if let Some(aa) = &self.track.tags.album_artist {
            tag.insert_text(ItemKey::AlbumArtist, aa.clone());
        }
        if let Some(tn) = self.track.tags.track_number {
            tag.set_track(tn);
        }
        if let Some(tt) = self.track.tags.total_tracks {
            tag.set_track_total(tt);
        }
        if let Some(dn) = self.track.tags.disc_number {
            tag.set_disk(dn);
        }
        if let Some(dd) = self.track.tags.total_discs {
            tag.set_disk_total(dd);
        }
        if let Some(rd) = &self.track.tags.release_date {
            tag.set_year(
                rd.chars()
                    .take(4)
                    .collect::<String>()
                    .parse::<u32>()
                    .unwrap_or(0),
            );
        } else {
            tag.set_year(self.track.release_year.max(0) as u32);
        }
        if let Some(c) = &self.track.tags.copyright {
            tag.insert_text(ItemKey::CopyrightMessage, c.clone());
        }
        if let Some(c) = &self.track.tags.composer {
            tag.insert_text(ItemKey::Composer, c.clone());
        }
        if let Some(isrc) = &self.track.tags.isrc {
            tag.insert_text(ItemKey::Unknown("ISRC".to_string()), isrc.clone());
        }
        if let Some(upc) = &self.track.tags.upc {
            tag.insert_text(ItemKey::Unknown("UPC".to_string()), upc.clone());
            tag.insert_text(ItemKey::Unknown("BARCODE".to_string()), upc.clone());
        }
        if let Some(genres) = &self.track.tags.genres {
            tag.remove_key(&ItemKey::Genre);
            if self.split_metadata {
                for g in genres {
                    tag.insert_text(ItemKey::Genre, g.clone());
                }
            } else {
                tag.set_genre(genres.join(self.metadata_separator));
            }
        }
        if let Some(ex) = self.track.explicit {
            tag.insert_text(
                ItemKey::Unknown("RATING".to_string()),
                if ex { "Explicit" } else { "Clean" }.to_string(),
            );
        }
        for (k, v) in &self.track.tags.extra_tags {
            tag.insert_text(ItemKey::Unknown(k.clone()), v.clone());
        }
        for credit in self.credits {
            if credit
                .credit_type
                .replace(['_', '-'], " ")
                .trim()
                .to_lowercase()
                == "music publisher"
            {
                continue;
            }
            tag.remove_key(&ItemKey::Unknown(credit.credit_type.clone()));
            if self.split_metadata {
                for n in &credit.names {
                    tag.insert_text(ItemKey::Unknown(credit.credit_type.clone()), n.clone());
                }
            } else {
                tag.insert_text(
                    ItemKey::Unknown(credit.credit_type.clone()),
                    credit.names.join(self.metadata_separator),
                );
            }
        }
        if let Some(ly) = self.embedded_lyrics {
            tag.insert_text(ItemKey::Lyrics, ly.to_string());
        }
        let _ = is_vorbis;
        Ok(())
    }
}

/// Write the given tags to the file at `path`. The existing primary tag is
/// updated in place.
fn write_tags(
    path: &Path,
    primary: &Tag,
    secondary: Option<&Tag>,
    image_path: Option<&Path>,
) -> Result<()> {
    let mut tagged_file =
        lofty::read_from_path(path).map_err(|e| Error::TagSavingFailure(format!("read: {e}")))?;
    if let Some(tag) = tagged_file.primary_tag_mut() {
        // Replace semantics like mutagen (`tagger[key] = ...` overwrites):
        // drop each affected key once *before* pushing, so re-tagging a file
        // (or tagging original + converted copy) never stacks duplicates.
        let mut seen: Vec<ItemKey> = Vec::new();
        for item in primary
            .items()
            .chain(secondary.iter().flat_map(|s| s.items()))
        {
            if !seen.contains(item.key()) {
                seen.push(item.key().clone());
                tag.remove_key(item.key());
            }
        }
        for item in primary.items() {
            tag.push(item.clone());
        }
        for pic in primary.pictures() {
            tag.push_picture(pic.clone());
        }
        if let Some(secondary) = secondary {
            for item in secondary.items() {
                tag.push(item.clone());
            }
            for pic in secondary.pictures() {
                tag.push_picture(pic.clone());
            }
        }
    } else {
        let t = primary.clone();
        tagged_file.insert_tag(t);
    }
    if let Some(p) = image_path {
        let bytes =
            std::fs::read(p).map_err(|e| Error::TagSavingFailure(format!("read image: {e}")))?;
        let mime = match p.extension().and_then(|e| e.to_str()) {
            Some("png") => MimeType::Png,
            _ => MimeType::Jpeg,
        };
        let picture = Picture::new_unchecked(PictureType::CoverFront, Some(mime), None, bytes);
        if let Some(tag) = tagged_file.primary_tag_mut() {
            tag.push_picture(picture);
        }
    }
    tagged_file
        .save_to_path(path, WriteOptions::default())
        .map_err(|e| Error::TagSavingFailure(format!("save: {e}")))?;
    Ok(())
}

/// Resize an image if it exceeds the limit, mirroring
/// `_resize_image_if_needed`. Returns the (possibly new) path; caller is
/// responsible for cleaning up the temp file.
pub fn resize_cover_if_needed(image_path: &Path, max_bytes: u64) -> Result<std::path::PathBuf> {
    let meta = std::fs::metadata(image_path).map_err(|e| Error::Other(format!("metadata: {e}")))?;
    let size = meta.len();
    if size <= max_bytes {
        return Ok(image_path.to_path_buf());
    }
    let img = ImageReader::open(image_path)
        .map_err(|e| Error::Other(format!("open: {e}")))?
        .with_guessed_format()
        .map_err(|e| Error::Other(format!("format: {e}")))?
        .decode()
        .map_err(|e| Error::TagSavingFailure(format!("decode cover: {e}")))?;
    let img = img.to_rgb8();
    let tmp = tempfile::Builder::new()
        .prefix("picaro_resized_")
        .suffix(".jpg")
        .tempfile()
        .map_err(|e| Error::Other(format!("tempfile: {e}")))?;
    // `NamedTempFile` deletes the file on drop, so persist it: returning
    // `tmp.path()` after drop was a dangling path (the resized cover
    // vanished before the caller could embed it). `keep()` keeps the file.
    let tmp_path = tmp
        .into_temp_path()
        .keep()
        .map_err(|e| Error::Other(format!("persist tmp: {e}")))?;
    let mut out =
        std::fs::File::create(&tmp_path).map_err(|e| Error::Other(format!("create tmp: {e}")))?;
    let dim = img.dimensions();
    let max_dim = 3000u32;
    let scale = (max_dim as f32 / dim.0.max(dim.1) as f32).min(1.0);
    let new_w = ((dim.0 as f32) * scale) as u32;
    let new_h = ((dim.1 as f32) * scale) as u32;
    let resized =
        image::imageops::resize(&img, new_w, new_h, image::imageops::FilterType::Lanczos3);
    use image::ImageEncoder;
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 90);
    encoder
        .write_image(
            resized.as_raw(),
            resized.width(),
            resized.height(),
            image::ExtendedColorType::Rgb8,
        )
        .map_err(|e| Error::TagSavingFailure(format!("encode jpeg: {e}")))?;
    drop(out);
    Ok(tmp_path)
}
