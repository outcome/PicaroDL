# PicaroDL

A music downloader that doesn't ask you to sign in.

PicaroDL started as a Rust port of [OrpheusDL](https://github.com/OrpheusDL/OrpheusDL).
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
dead host, captcha, whatever — it moves to the next one. If the quality you asked
for isn't there, it steps down to the next best instead of failing outright.

Nothing waits on a slow source. Nothing asks you to log in.

## What's in it

- **No sign-in anywhere.** Every bundled source works without an account, and
  Soulseek P2P is on by default (opt out with one setting).
- **Multi-source resolver.** Sources are raced in parallel, scored for relevance,
  and tried in a sensible order that updates itself as you use it.
- **Quality tiers** — `lossless`, `high`, `medium`, `low` — with automatic
  fallback (requested tier → higher → lower).
- **Metadata repair.** Sources like YouTube only give you an uploader name and a
  video frame; PicaroDL replaces those with the real artist, title, album, and
  cover art, using free services that need no keys.
- **Safety checks.** Downloads are checked against their file signature before
  tagging, and anything extracted from an archive that isn't audio gets removed.
  Nothing is ever executed.
- **Quality guard.** Downloads are probed for container and bitrate; a lossy
  file masquerading as lossless (a "fake FLAC") is rejected and another source is
  tried automatically.
- **TUI and CLI.** A terminal browser for casual use, and a full CLI for scripts.
- **Portable.** Pure Rust with `reqwest`, so it builds for desktop and
  cross-compiles to Android.

## Providers

The bundled sources are the ones that actually download. Many FLAC blogs link
third-party file hosts that are dead or captcha-gated, so those sources are
**disabled** (kept out of the build) rather than shipped broken. What's enabled is
verified end-to-end, and includes keyless **MEGA** support and **Soulseek (P2P)**.

- **Download verified** — PicaroDL fetched a real file end-to-end in testing.
- **Search only** — search and metadata resolve, but the source's file host could
  not be downloaded automatically.

### Download verified

| Provider | Format | Source |
|---|---|---|
| YouTube | Opus (stream) | youtube.com |
| SoundCloud | Opus (stream) | soundcloud.com |
| CoreRadio | FLAC | coreradio.online |
| ccMixter | MP3 | ccmixter.org |
| Zvu4it | MP3 | zvu4it.org |
| Tancpol | MP3 | tancpol.net |
| Punkcata | MP3 | punkcata.blogspot.com |
| GlobalDJMix | MP3 | globaldjmix.com |
| Grime Archive | MP3 | grimearchive.org |
| Ektoplazm | MP3 / FLAC | ektoplazm.com |
| Ezhevika | MP3 | ezhevika.blogspot.com (MEGA) |
| Soulseek | FLAC / MP3 | slsknet.org (P2P, opt-in) |
| Deezer | MP3 (30s preview) | deezer.com (signed-out) |
| FreeMP3Cloud | MP3 | freemp3cloud.com |
| DanceMusic | MP3 | dance-music.org |
| FondSound | M4A | fondsound.com (MEGA) |

### Lyrics

| Provider | Source |
|---|---|
| LRCLIB | lrclib.net |
| Lyrics.ovh | lyrics.ovh |
| Lyrist | lyrist.vercel.app |
| Musixmatch | musixmatch.com |

> **MEGA** (`mega.nz` file/folder links), **MediaFire**, **Yandex Disk**,
> **pixeldrain**, **Google Drive**, **Dropbox** and **gofile** are resolved and
> downloaded automatically — no login, no captcha. Captcha-gated hosts are not,
> and won't be faked.

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

# search one provider directly
./target/release/picaro search --service flacmusic -t album "radiohead"

# time and compare every source
./target/release/picaro benchmark

# list loaded modules, or show settings
./target/release/picaro modules
./target/release/picaro settings
```

`--quality` takes `lossless`, `high`, `medium`, or `low`.

## Settings & toggles

Everything lives in `config/settings.json`. The defaults are safe — nothing
unexpected happens out of the box.

| Setting | Default | Effect |
|---|---|---|
| `metadata.fetch_lyrics` | `true` | fetch + embed lyrics from the lyrics providers |
| `metadata.fetch_cover` | `true` | download + embed album art |
| `metadata.fill_misc` | `true` | fill missing artist / album / title / cover from free metadata services |
| `resolver.allow_mixed_sources` | `false` | let one album's tracks come from different providers |
| `resolver.allow_mixed_quality` | `false` | let the resolver fall back to a different quality tier |
| `p2p.enabled` | `true` | Soulseek peer-to-peer (set `false` or `PICARO_ENABLE_P2P=0` to opt out) |

The **quality guard** only applies to **lossless** requests: if a "FLAC" is really
a re-encoded lossy file (wrong container, or under ~500 kbps) it is rejected and
another source is tried. Lossy tiers are taken as-is.

**Mixed album assembly is opt-in.** By default an album is accepted only from a
source at the quality you asked for — if a matching source would need a different
quality or a different provider, it's skipped unless you enable
`resolver.allow_mixed_quality` / `resolver.allow_mixed_sources`. Individual-track
requests always keep their resilient multi-source fallback.

### P2P / Soulseek

Soulseek gives near-universal coverage (FLAC included) with no account — the
login is generated and stored locally on first use, and regenerated automatically
if the network ever rejects it. It is **enabled by default**; if peer-to-peer
traffic is a problem on your connection, set `p2p.enabled = false` or
`PICARO_ENABLE_P2P=0`.

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
  files — a `.exe`/`.js`/`.scr` can never be dropped on you, and nothing is ever
  executed.
- A **quality guard** rejects fake lossless (see above).
- **MEGA** decryption happens in-process; **Soulseek** is opt-in.

## Settings and privacy

Settings and any saved logins live in `config/settings.json`, inside the project
folder. That path is git-ignored, as are `downloads/`, `cache/`, and `temp/`, so
none of it is ever committed. There's no telemetry and nothing phones home.

## Android

The code is plain Rust and `reqwest`, so it cross-compiles to Android; the
intended UI is Slint. Sources behind Cloudflare are skipped by default. If you
want them, a WebView can solve the challenge once and hand the cookie to PicaroDL
through the `PICARO_CF_COOKIE` environment variable; `PICARO_FLARESOLVERR` can
point at an optional remote solver. Neither is required.

## Disclaimer

PicaroDL hosts nothing and does not defeat authentication. It's meant for
personal and educational use. What you download and whether it's legal where you
live is on you — support the artists you like.

## Credits

PicaroDL is a Rust rewrite of, and heavily inspired by,
**[OrpheusDL](https://github.com/OrpheusDL/OrpheusDL)** by the OrpheusDL
contributors. The module contract, download flow, tagging and formatting logic
all follow OrpheusDL's design — the credit for that architecture belongs to that
project. Thanks also to the maintainers of the crates this leans on: `reqwest`,
`lofty`, `sevenz-rust`, `zip`, `mega`, `soulseek-rs-lib`, and `ratatui`.

## License

MIT. See [LICENSE](LICENSE).
