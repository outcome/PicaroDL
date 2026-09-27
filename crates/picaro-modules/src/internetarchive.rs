//! Internet Archive / Live Music Archive (etree) module.
//!
//! Fully keyless and legal: archive.org exposes a public JSON search API and
//! per-item metadata, and serves lossless audio (FLAC / 24-bit FLAC) directly
//! with no login and no captcha. Each item is treated as an album (usually a
//! live show), and its FLAC files as tracks.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine;
use percent_encoding::{utf8_percent_encode, NON_ALPHANUMERIC};
use regex::Regex;
use serde_json::{json, Value};

use picaro_utils::error::{Error, Result};
use picaro_utils::models::*;
use picaro_utils::module::CodecOptions;
use picaro_utils::{ModuleConstructor, ModuleInterfacePtr};

use crate::registry::register;

const SERVICE: &str = "InternetArchive";
const UA: &str = "PicaroDL/0.1 (+https://github.com/outcome/PicaroDL)";
const SEARCH: &str = "https://archive.org/advancedsearch.php";
const METADATA: &str = "https://archive.org/metadata";
const DOWNLOAD: &str = "https://archive.org/download";
const IMG: &str = "https://archive.org/services/img";

pub fn module_information() -> ModuleInformation {
    let mut url_constants = indexmap::IndexMap::new();
    url_constants.insert("track".to_string(), DownloadType::track);
    url_constants.insert("album".to_string(), DownloadType::album);
    ModuleInformation {
        service_name: SERVICE.to_string(),
        module_supported_modes: ModuleModes::download | ModuleModes::covers,
        global_settings: indexmap::IndexMap::new(),
        global_storage_variables: vec![],
        session_settings: indexmap::IndexMap::new(),
        session_storage_variables: vec![],
        flags: ModuleFlags::empty(),
        netlocation_constant: NetlocConstants::Single("archive.org".to_string()),
        url_constants,
        test_url: Some("https://archive.org/details/ween2022-03-19".to_string()),
        url_decoding: ManualEnum::Manual,
        login_behaviour: ManualEnum::Manual,
    }
}

pub fn constructor() -> Arc<dyn ModuleConstructor> {
    Arc::new(Ctor)
}

#[derive(Debug)]
struct Ctor;

impl ModuleConstructor for Ctor {
    fn construct(&self, _c: ModuleController) -> Result<ModuleInterfacePtr> {
        Ok(Arc::new(Mod {
            client: picaro_utils::http::build_client_with_user_agent(None, UA),
        }))
    }
}

#[derive(Debug)]
struct Mod {
    client: reqwest::Client,
}

fn enc(s: &str) -> String {
    utf8_percent_encode(s, NON_ALPHANUMERIC).to_string()
}

fn enc_path(s: &str) -> String {
    s.split('/').map(enc).collect::<Vec<_>>().join("/")
}

/// Track ids pack everything we need so `get_track_info` / `get_track_download`
/// don't have to re-fetch the item metadata for every file.
fn encode_ref(id: &str, file: &str, title: &str, artist: &str, album: &str, year: i32) -> String {
    let v = json!({
        "i": id, "f": file,
        "t": title, "ar": artist, "al": album, "y": year,
    });
    format!(
        "ia:{}",
        base64::engine::general_purpose::STANDARD.encode(v.to_string())
    )
}

fn decode_ref(track_id: &str) -> Result<Value> {
    let b64 = track_id
        .strip_prefix("ia:")
        .ok_or_else(|| Error::Other(format!("internetarchive: bad id '{track_id}'")))?;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| Error::Other(format!("internetarchive: id decode: {e}")))?;
    serde_json::from_slice(&raw).map_err(|e| Error::Other(format!("internetarchive: id json: {e}")))
}

fn clean_title(stem: &str) -> String {
    let re = Regex::new(r"(?i)^\s*(?:d\d+)?t?\d{1,3}[\s._-]+").unwrap();
    let s = re.replace(stem, "");
    s.replace('_', " ")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn ext_of(name: &str) -> String {
    name.rsplit_once('.')
        .map(|(_, e)| e.to_ascii_lowercase())
        .unwrap_or_default()
}

fn codec_for_ext(ext: &str) -> CodecFlags {
    match ext {
        "flac" => CodecFlags::FLAC,
        "m4a" | "alac" => CodecFlags::ALAC,
        "wav" => CodecFlags::WAV,
        "ogg" | "oga" => CodecFlags::VORBIS,
        "opus" => CodecFlags::OPUS,
        "aac" => CodecFlags::AAC,
        _ => CodecFlags::MP3,
    }
}

fn is_audio_name(name: &str) -> bool {
    let base = name.rsplit('/').next().unwrap_or(name);
    if base.starts_with("__") || base.to_ascii_lowercase().contains("spectrogram") {
        return false;
    }
    matches!(ext_of(name).as_str(), "flac" | "mp3" | "m4a" | "ogg" | "oga" | "opus" | "wav")
}

struct ItemMeta {
    title: String,
    artist: String,
    year: i32,
    /// (filename, codec) preferred lossless first.
    files: Vec<(String, CodecFlags)>,
}

async fn fetch_item(client: &reqwest::Client, id: &str) -> Result<ItemMeta> {
    let v = client
        .get(format!("{METADATA}/{id}"))
        .send()
        .await
        .map_err(|e| Error::Other(format!("internetarchive metadata: {e}")))?
        .json::<Value>()
        .await
        .map_err(|e| Error::Other(format!("internetarchive metadata json: {e}")))?;

    let md = v.get("metadata").cloned().unwrap_or(Value::Null);
    let title = md
        .get("title")
        .and_then(|x| x.as_str())
        .unwrap_or(id)
        .to_string();
    let artist = md
        .get("creator")
        .map(|c| match c {
            Value::Array(a) => a.first().and_then(|x| x.as_str()).unwrap_or("").to_string(),
            Value::String(s) => s.clone(),
            _ => String::new(),
        })
        .unwrap_or_default();
    let year = md
        .get("year")
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<i32>().ok())
        .or_else(|| {
            md.get("date")
                .and_then(|x| x.as_str())
                .and_then(|s| s.get(..4))
                .and_then(|s| s.parse::<i32>().ok())
        })
        .unwrap_or(0);

    // Prefer one format tier so items that ship both FLAC and WAV (the
    // original) don't download twice.
    let mut flac: Vec<(String, CodecFlags)> = Vec::new();
    let mut alac: Vec<(String, CodecFlags)> = Vec::new();
    let mut wav: Vec<(String, CodecFlags)> = Vec::new();
    let mut lossy: Vec<(String, CodecFlags)> = Vec::new();
    if let Some(files) = v.get("files").and_then(|x| x.as_array()) {
        for f in files {
            let Some(name) = f.get("name").and_then(|x| x.as_str()) else {
                continue;
            };
            if !is_audio_name(name) {
                continue;
            }
            let codec = codec_for_ext(&ext_of(name));
            match codec {
                CodecFlags::FLAC => flac.push((name.to_string(), codec)),
                CodecFlags::ALAC => alac.push((name.to_string(), codec)),
                CodecFlags::WAV => wav.push((name.to_string(), codec)),
                _ => lossy.push((name.to_string(), codec)),
            }
        }
    }
    for bucket in [&mut flac, &mut alac, &mut wav, &mut lossy] {
        bucket.sort_by(|a, b| a.0.cmp(&b.0));
    }
    let files = if !flac.is_empty() {
        flac
    } else if !alac.is_empty() {
        alac
    } else if !wav.is_empty() {
        wav
    } else {
        lossy
    };
    if files.is_empty() {
        return Err(Error::Other(format!(
            "internetarchive: no audio files in item {id}"
        )));
    }
    Ok(ItemMeta {
        title,
        artist,
        year,
        files,
    })
}

#[async_trait]
impl picaro_utils::module::ModuleInterface for Mod {
    fn name(&self) -> &str {
        SERVICE
    }

    fn is_authenticated(&self) -> bool {
        true
    }

    async fn get_track_info(
        &self,
        track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackInfo> {
        let v = decode_ref(track_id)?;
        let get = |k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
        let name = get("t");
        let album = get("al");
        let artist = get("ar");
        let id = get("i");
        Ok(TrackInfo {
            name,
            album: album.clone(),
            album_id: id.clone(),
            artists: if artist.is_empty() {
                vec![]
            } else {
                vec![artist]
            },
            tags: Tags {
                track_url: Some(format!("https://archive.org/details/{id}")),
                ..Default::default()
            },
            codec: codec_for_ext(&ext_of(&get("f"))),
            cover_url: format!("{IMG}/{id}"),
            release_year: v.get("y").and_then(|x| x.as_i64()).unwrap_or(0) as i32,
            id: Some(track_id.to_string()),
            ..Default::default()
        })
    }

    async fn get_track_download(
        &self,
        track_id: &str,
        _quality: Quality,
        _codec: &CodecOptions,
        _data: HashMap<String, Value>,
    ) -> Result<TrackDownloadInfo> {
        let v = decode_ref(track_id)?;
        let id = v.get("i").and_then(|x| x.as_str()).unwrap_or("");
        let file = v.get("f").and_then(|x| x.as_str()).unwrap_or("");
        if id.is_empty() || file.is_empty() {
            return Err(Error::Other(format!(
                "internetarchive: incomplete ref {track_id}"
            )));
        }
        Ok(TrackDownloadInfo {
            download_type: DownloadSource::Url,
            file_url: Some(format!("{DOWNLOAD}/{id}/{}", enc_path(file))),
            file_url_headers: serde_json::Map::new(),
            temp_file_path: None,
            different_codec: Some(codec_for_ext(&ext_of(file))),
        })
    }

    async fn get_album_info(
        &self,
        album_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<AlbumInfo> {
        let id = album_id
            .rsplit('/')
            .next()
            .unwrap_or(album_id)
            .trim()
            .to_string();
        let meta = fetch_item(&self.client, &id).await?;
        let tracks = meta
            .files
            .iter()
            .map(|(f, _)| {
                let title = clean_title(
                    f.rsplit_once('.')
                        .map(|(s, _)| s)
                        .unwrap_or(f.as_str()),
                );
                TrackRef::Id(encode_ref(
                    &id,
                    f,
                    &title,
                    &meta.artist,
                    &meta.title,
                    meta.year,
                ))
            })
            .collect();
        Ok(AlbumInfo {
            name: meta.title,
            artist: meta.artist.clone(),
            tracks,
            release_year: meta.year,
            artist_id: None,
            id: Some(id.clone()),
            quality: Some("FLAC".to_string()),
            cover_url: Some(format!("{IMG}/{id}")),
            cover_type: Some(ImageFileType::Jpg),
            ..Default::default()
        })
    }

    async fn get_playlist_info(
        &self,
        _playlist_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<PlaylistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.to_string(),
            ability: "playlist".to_string(),
        })
    }

    async fn get_artist_info(
        &self,
        _artist_id: &str,
        _get_credited_albums: bool,
        _artist_name: Option<&str>,
        _data: HashMap<String, Value>,
    ) -> Result<ArtistInfo> {
        Err(Error::ModuleDoesNotSupportAbility {
            module: SERVICE.to_string(),
            ability: "artist".to_string(),
        })
    }

    async fn get_track_credits(
        &self,
        _track_id: &str,
        _data: HashMap<String, Value>,
    ) -> Result<Vec<CreditsInfo>> {
        Ok(Vec::new())
    }

    async fn get_track_cover(
        &self,
        track_id: &str,
        _cover: &CoverOptions,
        _data: HashMap<String, Value>,
    ) -> Result<CoverInfo> {
        let v = decode_ref(track_id).map_err(|_| Error::Other("internetarchive: bad id".into()))?;
        let id = v.get("i").and_then(|x| x.as_str()).unwrap_or("");
        Ok(CoverInfo {
            url: format!("{IMG}/{id}"),
            file_type: ImageFileType::Jpg,
        })
    }

    async fn search(
        &self,
        _query_type: DownloadType,
        query: &str,
        _track_info: Option<&TrackInfo>,
        limit: u32,
    ) -> Result<Vec<SearchResult>> {
        let rows = limit.clamp(1, 50);
        let q = format!("({query}) AND (mediatype:audio OR mediatype:etree)");
        let url = format!(
            "{SEARCH}?q={}&fl%5B%5D=identifier&fl%5B%5D=title&fl%5B%5D=creator&fl%5B%5D=year&rows={rows}&page=1&output=json",
            enc(&q)
        );
        let v = self
            .client
            .get(&url)
            .send()
            .await
            .map_err(|e| Error::Other(format!("internetarchive search: {e}")))?
            .json::<Value>()
            .await
            .map_err(|e| Error::Other(format!("internetarchive search json: {e}")))?;
        let mut out = Vec::new();
        if let Some(docs) = v.pointer("/response/docs").and_then(|x| x.as_array()) {
            for d in docs {
                let id = d.get("identifier").and_then(|x| x.as_str()).unwrap_or("");
                if id.is_empty() {
                    continue;
                }
                let title = d.get("title").and_then(|x| x.as_str()).unwrap_or(id);
                let creator = d.get("creator").map(|c| match c {
                    Value::Array(a) => a
                        .first()
                        .and_then(|x| x.as_str())
                        .unwrap_or("")
                        .to_string(),
                    Value::String(s) => s.clone(),
                    _ => String::new(),
                });
                out.push(SearchResult {
                    result_id: id.to_string(),
                    name: Some(title.to_string()),
                    artists: creator.filter(|c| !c.is_empty()).map(|c| vec![c]),
                    year: d.get("year").and_then(|x| x.as_str()).map(|s| s.to_string()),
                    ..Default::default()
                });
            }
        }
        Ok(out)
    }
}

pub fn register_module(registry: &picaro_utils::ModuleRegistry) {
    register(registry, module_information(), constructor());
}
