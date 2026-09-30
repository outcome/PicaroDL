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

10 songs, full-chain resolve, Sept 2026, debug build on Windows, residential
connection. Two tiers tell the whole story:

**`--quality high` (the "just get me the song" mode): 100.4s total, avg
10.0s/song, 10/10 fresh downloads.**

| # | Song | Time | Source | Codec | Size |
|---|---|---|---|---|---|
| 1 | Queen — Bohemian Rhapsody | 11.0 s | Tancpol | MP3 | 14.1 MB |
| 2 | Michael Jackson — Billie Jean | 7.3 s | Zvu4it | MP3 | 11.9 MB |
| 3 | The Beatles — Hey Jude | 7.0 s | Mp3Tut | MP3 | 7.3 MB |
| 4 | Daft Punk — One More Time | 20.4 s | Tancpol | MP3 | 9.8 MB |
| 5 | Nirvana — Come As You Are | 9.3 s | Tancpol | MP3 | 8.9 MB |
| 6 | Radiohead — Paranoid Android | 7.0 s | Zvu4it | MP3 | 8.5 MB |
| 7 | Aphex Twin — Xtal | 16.7 s | Zvu4it | MP3 | 13.9 MB |
| 8 | Burial — Archangel | 8.8 s | Zvu4it | MP3 | 3.0 MB |
| 9 | Boards of Canada — Roygbiv | 6.9 s | Zvu4it | MP3 | 6.0 MB |
| 10 | Z LEAF — Hidden Temple | 6.0 s | Mp3Tut | MP3 | 7.4 MB |

**`--quality lossless` (P2P gets its shot first, per the FLAC-first
policy):** the same songs averaged ~106s — when FLAC genuinely exists
Soulseek delivers it in 25–90s; when it doesn't, the bounded P2P budget
(120s hard cap) is honestly spent before a direct MP3 fallback wins. True
lossless landed for 1–3 of the 10 depending on the hour (swarm health
varies); every resolve succeeded.

How a resolve works: **one search wave** queries every source at once (no
tier-by-tier re-searching). For lossless requests candidates run:
direct-lossless → P2P (one hard 120s budget, at most two attempts, a swarm
under 70% at 120s abandoned, dead magnets cut at 30s) → direct MP3 → Opus.
For lossy requests P2P moves **after** the direct sources — a fast MP3
must never wait behind Soulseek's 20s search window. Lossless requests
also filter lossy torrent files before downloading a single byte.

- **When lossless exists, it arrives via Soulseek or Khinsider** in
  seconds-to-minutes.
- **Lossy fallback is seconds away** at `--quality high`.
- A lossless request that ends in MP3 is by design (fallback); the
  resolver log reports which actually landed.

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
| Khinsider | FLAC / MP3 | downloads.khinsider.com (game soundtracks; needs --features impersonate) |
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
# download a whole album
./target/release/picaro get "Shapeshifter" -t album --quality lossless --only coreradio

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
| `p2p.min_speed_kbps` | `128` | a P2P transfer averaging below this counts as a strike |
| `p2p.slow_strikes` | `3` | strikes in a row benches the service (skipped, including its search) |
| `p2p.bench_minutes` | `10` | how long a bench lasts — recovery is wall-clock, then full standing |
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
takes longer than HTTP) and the whole P2P phase lives under one hard 120s
budget.

**Self-benching:** every P2P transfer is timed. One averaging under
`p2p.min_speed_kbps` (or failing outright) counts as a strike;
`p2p.slow_strikes` strikes in a row **benches** the service — it's skipped
entirely, search included, so resolves stay as fast as the direct pool —
for `p2p.bench_minutes`, after which it returns at full standing on wall-clock
time (no probe counters to satisfy). State persists in
`config/p2p-health.json`. Benchmarks: the same song that took 142.6s through
a stalling peer resolved in **9.9s** with the slow service benched.

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
PicaroDL; `PICARO_FLARESOLVERR` can point at an optional remote solver (plain
HTTP, so it can live on any machine on your LAN). Neither is required.

### Cloudflare-gated sources

PicaroDL ships **no browser**, but it doesn't need one for the source that
matters:

**Khinsider (game soundtracks, FLAC/MP3)** gates its album/track pages with
a Cloudflare WAF rule on the **TLS fingerprint** — no cookie can ever pass
it. Building with

```bash
cargo build --release --features impersonate
```

adds **Chrome TLS-fingerprint emulation** (the `wreq` client, BoringSSL):
plain HTTP with a real Chrome ClientHello + HTTP/2 fingerprint. Verified:
album and track pages that 403 every library TLS return 200 with the full
track table — **no browser, no cookies, no webview, works on every platform
BoringSSL builds** (Android NDK included). E2E: single track FLAC in
**4–5s**, whole 45-track OST in **109s**.

Build toolchain for `--features impersonate`: `cmake`, `nasm`, and
`libclang.dll` (for bindgen) on PATH / `LIBCLANG_PATH`. Default builds
(neither desktop nor Switch/Android) compile no BoringSSL at all and simply
skip the module.

For the classic **JS-challenge** sites (where a cookie IS the answer), the
clearance is just data — solve once anywhere, every platform uses it:

`config/cf-cookies.json` holds solved sessions per domain:

```json
{
  "flacmania.biz": {
    "cookies": "cf_clearance=...; other=...",
    "user_agent": "Mozilla/5.0 ... (must match the browser that solved it)",
    "solved_at": 1790694712
  }
}
```

- `PICARO_CF_COOKIE` injects a raw `Cookie` header as a one-off alternative.
- `PICARO_CF_COOKIES` points at a custom cookie-store path.
- `PICARO_FLARESOLVERR` delegates solving to a [FlareSolverr](https://github.com/FlareSolverr/FlareSolverr)
  instance (it bundles a browser, so the device running it needs a display or
  xvfb — that's why it stays a separate service).

| Platform | Path to gated sources |
|---|---|
| Windows / Linux / Mac desktop | `--features impersonate` (khinsider); FlareSolverr or cookie-copy for challenge sites |
| Android | `--features impersonate` via NDK (BoringSSL builds there); or copy `cf-cookies.json` |
| Switch homebrew | copy `cf-cookies.json` for challenge sites; impersonate builds if the devkit toolchain has cmake+nasm |

Cookies are bound to the solving IP + user agent; if the site still blocks,
re-solve from the same network.

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
