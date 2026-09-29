# PicaroDL

A music downloader that doesn't ask you to sign in.

PicaroDL started as a Rust port of [OrpheusDL](https://github.com/OrfiTeam/OrpheusDL).
OrpheusDL needs a login — and usually a paid subscription — to reach
high-quality audio, and its downloads stop the moment the account or token does.
PicaroDL took a different route. It no longer depends on accounts at all: it
pulls from public sources and needs no login, no tokens, and no API keys. Hand it
a song and it queries the sources at once, keeps the best answer, and downloads
it.

## How it behaves

Ask for `"Radiohead - Creep"` and PicaroDL queries every suitable source at the
same time. It takes the first result that actually matches the artist and title,
ignoring wrong matches, and downloads it. If that source dies partway through —
dead host, dead torrent swarm, captcha, whatever — it moves to the next one. If
the quality you asked for isn't there, it steps down to the next best instead of
failing outright.

**Direct downloads always come first.** HTTP sources are tried ahead of every
P2P source (Soulseek, torrents), no matter how well any source performed before:
a dead swarm or hung transfer must never sit in front of a working server. P2P
sources are also gated for viability before they're tried — torrents below
`min_seeders` are never picked, swarms that transfer nothing for 90s are
abandoned, and a magnet that can't produce metadata in 60s is treated as dead.

Nothing waits on a slow source. Nothing asks you to log in.

## What's in it

- **No sign-in anywhere.** Every bundled source works without an account, and
  Soulseek P2P is on by default (opt out with one setting).
- **Multi-source resolver.** Sources are raced in parallel, scored for relevance,
  and tried in a self-updating order — with P2P pinned to the back.
- **Quality tiers** — `lossless`, `high`, `medium`, `low` — with automatic
  fallback (requested tier → higher → lower).
- **BitTorrent built in.** Magnets and `.torrent` files both work, with a
  librqbit engine that merges public HTTP/UDP trackers (so peer discovery
  survives networks where UDP is blocked). Song requests torrent **only the
  matching file** out of a release, not the whole album.
- **Metadata repair.** Sources like YouTube only give you an uploader name and a
  video frame; PicaroDL replaces those with the real artist, title, album, and
  cover art, using free services that need no keys.
- **Normalize filenames.** Every track lands as
  `<SongName> - <Artist> - (<CODEC>).<ext>` — e.g.
  `Billie Jean - Michael Jackson - (FLAC).flac` — no matter which of the 30+
  sources produced it (disable via `formatting.normalize_filename`).
- **Safety checks.** Downloads are checked against their file signature before
  tagging, and anything extracted from an archive that isn't audio gets removed.
  Nothing is ever executed. Interrupted downloads stage as `.part` files, so a
  killed transfer can never masquerade as a complete one.
- **Quality guard.** Downloads are probed for container and bitrate; a lossy
  file masquerading as lossless (a "fake FLAC") is rejected and another source is
  tried automatically. Downloads are additionally fingerprint-checked against
  your existing music library when a reference track is found.
- **TUI and CLI.** A terminal UI (parallel search across all modules, live
  result merging, downloads, logs, and a settings editor) and a full CLI for
  scripts.
- **Portable.** Pure Rust with `reqwest`, so it builds for desktop and
  cross-compiles to Android.

## Benchmark

10 songs run at `--quality lossless` (fallback allowed), full-chain resolve,
Sept 2026, debug build on Windows, residential connection. Total: ~19.7 min,
10/10 resolved, 2/10 in true lossless.

| # | Song | Time | Source | Resolved | Size |
|---|---|---|---|---|---|
| 1 | Queen — Bohemian Rhapsody | 24.5 s | PirateBay | FLAC (lossless) | 33.0 MB |
| 2 | Michael Jackson — Billie Jean | 91.5 s | Tancpol | MP3 (high) | 8.1 MB |
| 3 | The Beatles — Hey Jude | 99.5 s | Mp3Tut | MP3 (high) | 7.0 MB |
| 4 | Daft Punk — One More Time | 87.5 s | Soulseek | FLAC (lossless) | 38.9 MB |
| 5 | Nirvana — Come As You Are | 92.3 s | Tancpol | MP3 (high) | 8.5 MB |
| 6 | Radiohead — Paranoid Android | 93.4 s | Tancpol | MP3 (high) | 5.3 MB |
| 7 | Aphex Twin — Xtal | 36.2 s | Zvu4it | MP3 (high) | 13.3 MB |
| 8 | Burial — Archangel | 280.6 s | Zvu4it | MP3 (high) | 2.8 MB |
| 9 | Boards of Canada — Roygbiv | 100.0 s | Zvu4it | MP3 (high) | 5.7 MB |
| 10 | Z LEAF — Hidden Temple | 277.2 s | OneTrance | MP3 (high) | 2.1 MB |

Reading the results:

- **Popular songs resolve fast** when a seeded torrent or direct source exists
  (Bohemian Rhapsody: single-file FLAC torrent in 24.5s).
- **The 90–100s cluster** is mostly a dead torrent fast-failing (60s metadata
  cap) before a direct MP3 source wins — the price of trying lossless first
  with torrents enabled.
- **The ~280s runs** were dominated by a Soulseek peer that accepted the
  connection but never delivered; its 240s transfer cap fires and the chain
  moves on. Set `PICARO_SOULSEEK_DOWNLOAD_TIMEOUT` lower if you prefer
  snappier fallbacks.
- True lossless for popular catalogue mostly comes from torrents and Soulseek;
  the rest of the pool is MP3 320-adjacent, so `--quality lossless` often
  settles at `high` by design.

## Providers

The bundled sources are the ones that actually download. Many FLAC sites route
through third-party file hosts that are dead or captcha-gated — 40+ candidates
were tested and rejected for exactly that. What ships below is verified
end-to-end. `MEGA`, `MediaFire`, `Yandex Disk`, `pixeldrain`, `Google Drive`,
`Dropbox` and `gofile` links are resolved automatically — no login, no captcha.
Captcha-gated hosts (filecrypt, filecat, rapidgator, hotlink…) are not, and
won't be faked.

### Download verified

| Provider | Format | Source |
|---|---|---|
| YouTube Music | Opus (stream) | music.youtube.com (InnerTune-style InnerTube API for search; yt-dlp for delivery) |
| InternetArchive | FLAC / MP3 | archive.org |
| Soulseek | FLAC / MP3 | slsknet.org (P2P, on by default) |
| PirateBay | FLAC / MP3 | thepiratebay.org via apibay.org (BitTorrent magnet; opt-in) |
| DarkTorrent | FLAC / MP3 | darktorrent.org (`.torrent` files; opt-in) |
| CoreRadio | FLAC | coreradio.online |
| Ektoplazm | MP3 / FLAC | ektoplazm.com |
| TechnicalDeathMetal | FLAC | technicaldeathmetal.org (VK doc archives) |
| Systems of Romance | MP3 | systemsofromance.com (same-domain zip) |
| Relisten | MP3 / FLAC | relisten.net → archive.org live shows |
| Free Music Archive | MP3 | freemusicarchive.org |
| ccMixter | MP3 | ccmixter.org |
| Zvu4it | MP3 | zvu4it.org |
| Tancpol | MP3 | tancpol.net |
| GlobalDJMix | MP3 | globaldjmix.com |
| Grime Archive | MP3 | grimearchive.org |
| TestPressing | MP3 | testpressing.org (DJ mixes) |
| OneTrance | MP3 | 1trance.org |
| Mp3Zona | MP3 | mp3zona.net |
| Mp3Tut | MP3 | mp3-tut.click |
| Tom Lehrer Songs | MP3 | tomlehrersongs.com |
| MixtapeMonkey | MP3 | mixtapemonkey.com |
| Certified Mixtapez | MP3 | certifiedmixtapez.com |
| Ezhevika | MP3 | ezhevika.blogspot.com (MEGA) |
| Butterboy | MP3 | butterboycompilations.blogspot.com (pixeldrain) |
| FondSound | M4A | fondsound.com (MEGA) |
| Punkcata | MP3 | punkcata.blogspot.com (post availability varies) |
| DanceMusic | MP3 | dance-music.org |
| FreeMP3Cloud | MP3 | freemp3cloud.com |

SoundCloud search works (keyless client-ID scrape) but their CDN increasingly
404s anonymous stream URLs for popular tracks — the module is bundled but the
resolver's quality guard will fail those loudly rather than save junk.

### Lyrics

| Provider | Source |
|---|---|
| LRCLIB | lrclib.net |
| Lyrics.ovh | lyrics.ovh |
| Lyrist | lyrist.vercel.app |
| Musixmatch | musixmatch.com |

## Build

```bash
cargo build --release
# binary: target/release/picaro
```

## Use

```bash
# open the TUI
./target/release/picaro

# download a track, letting it choose the best source and quality to fall back to
./target/release/picaro get "Radiohead - Creep" --quality high

# see which source would win, without downloading
./target/release/picaro get "Radiohead - Creep" --quality lossless --resolve-only

# restrict to one provider (testing / forcing)
./target/release/picaro get "Z LEAF - Hidden Temple" --only onetrance

# search one provider directly
./target/release/picaro search --service fma "steady love"

# time and compare every source
./target/release/picaro benchmark

# hand it a magnet or a direct URL
./target/release/picaro url "magnet:?xt=urn:btih:..."

# list loaded modules, or show settings
./target/release/picaro modules
./target/release/picaro settings
```

`--quality` takes `lossless`, `high`, `medium`, or `low`. Downloads land in
`downloads/` anchored to the project root (not wherever the binary was started
from), laid out as `Artist/Album/` for releases.

## Settings & toggles

Everything lives in `config/settings.json` — browsable and editable from the
TUI's Settings tab (`c`). The defaults are safe: nothing unexpected happens out
of the box.

| Setting | Default | Effect |
|---|---|---|
| `metadata.fetch_lyrics` | `true` | fetch + embed lyrics (`.lrc` sidecars too) |
| `metadata.fetch_cover` | `true` | download + embed album art |
| `metadata.fill_misc` | `true` | fill missing artist / album / title / cover from free metadata services |
| `formatting.normalize_filename` | `true` | uniform `<SongName> - <Artist> - (<CODEC>).<ext>` filenames across all sources |
| `resolver.allow_mixed_sources` | `false` | let one album's tracks come from different providers |
| `resolver.allow_mixed_quality` | `true` | let the resolver fall back to a different quality tier |
| `resolver.probe_timeout_secs` | `4` | per-source search budget (Soulseek gets ≥25s automatically) |
| `resolver.download_timeout_secs` | `360` | hard cap on any single download attempt |
| `p2p.enabled` | `true` | Soulseek peer-to-peer (set `false` or `PICARO_ENABLE_P2P=0` to opt out) |
| `torrent.enabled` | `false` | BitTorrent downloads (PirateBay magnets, DarkTorrent `.torrent`s); opt in to allow P2P torrent traffic |
| `torrent.min_seeders` | `5` | torrents below this seeder count are never picked |
| `torrent.max_size_gb` | `8` | refuse oversized releases (the selected file, for song requests) |

The **quality guard** applies to **lossless** requests: if a "FLAC" is really
a re-encoded lossy file (wrong container, or under ~500 kbps) it is rejected and
another source is tried. Lossy tiers are taken as-is.

### P2P / Soulseek

Soulseek gives near-universal coverage (FLAC included) with no account — the
login is generated and stored locally on first use, and regenerated
automatically if the network ever rejects it. It is **enabled by default**, but
always tried **after** direct sources: its search runs on a ≥25s window (P2P
takes longer than HTTP) and a transfer that goes nowhere is capped at 240s
(`PICARO_SOULSEEK_DOWNLOAD_TIMEOUT`) before the chain moves on.

### BitTorrent / torrents

Torrent support is **off by default** (`torrent.enabled = false`) because it is
peer-to-peer traffic; enable it to let PicaroDL use PirateBay and DarkTorrent
results. When enabled:

- magnets **and** `.torrent` URLs both work (DarkTorrent serves `.torrent`
  files whose HTTP trackers keep peer discovery alive even where UDP/DHT is
  blocked — hotel wifi, campus NAT);
- a release's files are listed first, and a song request downloads **only the
  file matching the song**, not the album;
- swarms that produce nothing for 90s, or no metadata within 60s, fail fast and
  the resolver moves on — no infinite hangs;
- public HTTP + UDP trackers are merged into every add, so a dead tracker in
  the magnet doesn't kill the download.

### Progress for host apps

When driven as a subprocess, PicaroDL prints one parseable line per event on
stdout, so a host can show live progress:

```text
picaro started <service> <context>
picaro track-start <name>
picaro progress <bytes> <total|-> <name>
picaro ok <name> <path>
picaro skip <name> <path>
picaro fail <name> <reason>
picaro finished <ok> <skipped> <failed>
picaro error <message>
```

## Safety

- Downloads are **magic-byte validated** and archives are purged of non-audio
  files (and macOS `__MACOSX` junk) — a `.exe`/`.js`/`.scr` can never be dropped
  on you, and nothing is ever executed.
- A **quality guard** rejects fake lossless, and downloads are
  **fingerprint-verified** against your existing library when a reference
  exists.
- **MEGA** decryption happens in-process; **Soulseek** is on by default (see
  above); **torrents** are opt-in.

## Settings and privacy

Settings and any saved logins live in `config/settings.json`, inside the project
folder. That path is git-ignored, as are `downloads/`, `cache/`, and `temp/`, so
none of it is ever committed. There's no telemetry and nothing phones home.

## Android

The code is plain Rust and `reqwest`, so it cross-compiles to Android; the
intended UI is Slint. Sources behind Cloudflare are skipped by default. If you
want them, a WebView can solve the challenge once and hand the cookie to
PicaroDL through the `PICARO_CF_COOKIE` environment variable; `PICARO_FLARESOLVERR`
can point at an optional remote solver. Neither is required.

## Disclaimer

PicaroDL hosts nothing and does not defeat authentication. It's meant for
personal and educational use. What you download and whether it's legal where you
live is on you — support the artists you like.

## Credits

PicaroDL is a Rust rewrite of, and heavily inspired by,
**[OrpheusDL](https://github.com/OrfiTeam/OrpheusDL)** by the OrpheusDL
contributors. The module contract, download flow, tagging and formatting logic
all follow OrpheusDL's design — the credit for that architecture belongs to that
project. Thanks also to the maintainers of the crates this leans on: `reqwest`,
`lofty`, `librqbit`, `sevenz-rust`, `zip`, `mega`, `soulseek-rs-lib`, and
`ratatui`.

## License

MIT. See [LICENSE](LICENSE).
