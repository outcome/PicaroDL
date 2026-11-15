# PicaroDL source + lyrics notes (agent working notes, 2026-11-15)

Kept in the repo so the backup carries them. Facts were checked live on 2026-11-15
unless marked "read only".

## What was added this round (commits bcbff71, 76a8a52)

Lyrics providers in `crates/picaro-modules/src/lyrics_providers.rs` (plumbing in
`lyrics_kit.rs`): Unison, BetterLyrics, LyricsPlus, BiniLyrics, lrc.red, KuGou.
All are queried at once by `Downloader::fetch_lyrics_from_providers`; a synced
answer beats a plain one, ranked by `ORDER` there.

### Provider quirks (hard-won)
- BetterLyrics / LyricsPlus answer 403 to a "BitChord" User-Agent; a browser UA works.
- KuGou ranks `+`-encoded spaces differently from `%20`; reqwest `.query()` sends `+`,
  so the search URL is built by hand with percent-encoding. (This is why KuGou
  looked EMPTY in the first live test.)
- Genius answers 403 to rustls' TLS fingerprint (plain HTTP/1.1 and h2, any UA) even
  though Python's OpenSSL passes. Removed; plain text only anyway. Revisit only with
  the `cf-impersonate` feature (khinsider uses it).
- Not ported: SimpMusic + Megalobiz (403/410), PaxSenix (token/API key), YouTube
  caption sources (need a video id).

## BitChord (Kotlin, GPLv3) research
- Song sources: YouTube + JioSaavn + user-installed HTTP addons (manifest/search/stream).
  No named default addons. Lyrics providers are the portable part.
- "Upgrading quality" in BitChord = it starts on YouTube Opus, searches the other
  sources with no time limit in the background, and swaps (short audio break) only if
  the same recording is genuinely better (lossless, or 320k vs Opus 160k).
  Source: `app/src/main/java/com/music/bitchord/playback/QualityUpgrade.kt`
  in github.com/kushagrasinghx/BitChord.

## Bugs found and fixed
- YouTube Music search (`youtube.rs`): plain "Artist Title" returns the artist's other
  songs; the real track appears with the Songs filter / title-first query. The artist
  column was mis-parsed ("Song" label). Now three searches run together and merge.
- mp3tut (`mp3tut.rs`): `parse_data_id` preferred the `/media/music/...mp3` links,
  which are the page's unrelated "more tracks" list, over the first `data-id` (the
  requested song) -> wrong audio under the right name. Result names also echoed the
  query, so scoring accepted anything. Now: first data-id, real "Artist - Title" from
  the page, artist kept separate. Known: the real CDN link
  (`cdn.mp3-tut.click/?h=...\`) currently returns 400/500, so mp3tut fails honestly.
- Resolver note: Opus providers (YouTube) are only used for a high request when
  `allow_mixed_quality` is on; otherwise "wanted high, no source".

## Monochrome (implemented: `crates/picaro-modules/src/monochrome.rs`)
- Working API host: `https://tracks.monochrome.st` (`/health`, `/search/tracks?q=`,
  `/search/releases?q=`, `/releases/<id>`, `/track/<id>` = the FLAC itself, Range works).
  Other public instances were dead when checked; `HOSTS` is a list so more can be added.
- Track ids carry their metadata after `#`:
  `<trackId>#<releaseId>|<ms>|<isrc>|<enc title>|<enc artists joined by U+001F>`, so
  `get_track_info` needs no extra lookup beyond a best-effort `/releases/<rid>`.
- API quirks: `trackNumber` is unreliable (use array order per disc); a search for
  "Artist Title" ranks other songs, so the module drops hits whose normalised title
  differs from the requested one; results are deduped by ISRC.
- Rate limit: HEAD on `/track/<id>` answers 429 under load. 429 is NOT "no file":
  `get_track_download` retries with backoff (a 4xx other than 429 is a real miss).
  A swallowed 429 once let the resolver fall through to coreradio and deliver a
  different recording ("In the End (Demo)").
- The stream is sometimes cut mid-file; `http::download_to_path` now resumes with
  `Range` (up to 3 times, only if the host answers 206).
- Resolver: first in the Lossless chain; `lossless_src`/`is_direct_track`/`album_served_tier`
  include it. Verified: `picaro get "Linkin Park - In The End" -q lossless` -> FLAC,
  album "Hybrid Theory (Bonus Edition)", track 8/15, ISRC/synced lyrics tagged.
- Testing tip: the CLI bin is the `picaro` package (`cargo build -p picaro`), NOT
  `picaro-tui`; building the wrong one leaves a stale `target/debug/picaro.exe`.

## Source audit 2026-11-15 (all sources re-tested with `get --only <svc>`)
Test sequentially: parallel runs trigger 429s/timeouts that look like breakage.
Fixed this round: SoundCloud (client_id rotated -> constant + runtime scrape from a-v2.sndcdn.com
JS; only HLS now: segments concatenated + ffmpeg remux; `snipped` previews skipped),
FreeMp3Cloud (host moved a2 -> b2), FMA (search param `quicksearch=`), Khinsider (search
behind Cloudflare TLS check -> impersonate fetch with 429 retry; track queries open the best
matching albums and return per-TRACK results; CDN host is now bare `vgmtreasurechest.com`;
added to `lossless_src`), systemsofromance (`&#038;` in names), multi-artist tags
(`push` not `insert_text`), resolver title gate (wrong song by same artist no longer matches),
artist-less downloads no longer get a title-only Deezer/iTunes/MB lookup (wrong artist).
Working: monochrome, youtube, soundcloud, freemp3cloud, zvu4it, tancpol, ccmixter,
certifiedmixtapez, mixtapemonkey (some tapes removed upstream), fma, globaldjmix, tomlehrer,
onetrance, ezhevika, testpressing, soulseek, ektoplazm, khinsider (FLAC when the album has it;
MP3-only albums are rejected for lossless), dancemusicorg, fondsound, systemsofromance, butterboy,
mp3zona (search only finds some titles). Lyrics: Unison, BetterLyrics, LyricsPlus, BiniLyrics,
LrcRed, KuGou OK; Lyrist 403 (dead), Musixmatch empty.
Broken upstream: punkcata (archive.org links 404), mp3tut (CDN link 400), grimearchive (search
OK, mix S3 files 404 for the ones tried), coreradio (some VK files empty), relisten/
technicaldeathmetal (MP3 only / no direct archive), internetarchive (poor search).

## Candidate sources to add next (from FMHY, live-tested)
Ranked; none implemented yet.

| Name | Offers | Search | File | Feasibility |
|---|---|---|---|---|
| NetEase (music.163.com) lyrics | synced LRC | `GET https://music.163.com/api/search/get?s=<q>&type=1&limit=3`, then `/api/song/lyric?id=<id>&lv=1&tv=-1` | LRC in JSON | High, keyless, exact hit |
| QQ Music lyrics | synced LRC | `https://c.y.qq.com/soso/fcgi-bin/client_search_cp?w=<q>&format=json&n=1`, then `.../fcg_query_lyric_new.fcg?songmid=<mid>&format=json&nobase64=1` with `Referer: https://y.qq.com/` | LRC | High |
| musify.club | songs + albums, MP3 up to 320k | `GET /search?searchText=<q>` -> `/track/<slug>-<id>` links | `/track/pl/<id>/<slug>.mp3` returned 200 audio/mpeg with a Referer (`/track/dl/` returned HTML) | High; Russian host, rate limits unknown |
| JioSaavn (community API, e.g. a self-hosted `sumitkolhe/jiosaavn-api`) | songs, AAC up to 320k | `/api/search/songs?query=<q>` | `downloadUrl[]` -> `aac.saavncdn.com/...320.mp4` | High access, low instance stability; BitChord also uses JioSaavn |
| Audius | indie / covers | `https://api.audius.co/v1/tracks/search?query=<q>&app_name=picaro` | `/v1/tracks/<id>/stream?app_name=picaro` (audio/mpeg) | High, but poor hit rate for mainstream |
| Deezer public API | metadata + 30 s preview | `https://api.deezer.com/search?q=<q>` | preview only | metadata/cover only |
| Monochrome | FLAC | see "Monochrome (implemented)" below | direct FLAC | DONE (round 2) |
| Megalobiz API (champagne.pages.dev) | synced LRC, keyless, ~30 req/min | not implemented | LRC | High |
| Kuwo | MP3 128k | `search.kuwo.cn/r.s?...` then `antiserver.kuwo.cn/anti.s?type=convert_url&rid=MUSIC_<id>&format=mp3&response=url` | direct URL | Medium (low quality) |
| Others on champagne.pages.dev | mixcloud, bandcamp, JioSaavn `api.php` (needs Referer), Audius | - | - | Not tried |

Skipped/blocked: squid.wtf / lucida / doubledouble (DRM ripping, undocumented), mixupload
(login), xprm.net / z3.fm / lyricsify (403), Jamendo (needs client_id), Vagalume (503),
Zophar (chip music, KHInsider covers games), songslover / nodata / mp3db (file-host
links, covered by the generic blog scrapers).
