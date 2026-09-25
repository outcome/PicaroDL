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

- **No sign-in anywhere in the default path.** The 82 bundled providers need no
  accounts.
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
- **TUI and CLI.** A terminal browser for casual use, and a full CLI for scripts.
- **Portable.** Pure Rust with `reqwest`, so it builds for desktop and
  cross-compiles to Android.

## Providers

82 modules are bundled and usable without an account. Whether a music source can
actually *download* depends on the third-party file host it links to — some hosts
are dead, paywalled, or captcha-gated. The tables are honest about it:

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

### Search only (host-dependent)

| Provider | Format | Source |
|---|---|---|
| AlterPortal | FLAC | alterportal.net |
| Exystence | FLAC | exystence.net |
| FlacMusic | FLAC | flacmusic.info |
| LosslessAlbums | FLAC | losslessalbums.club |
| MusicRider | FLAC / MP3 | musicrider.org |
| ThemFire | FLAC | themfire.pro |
| Mp3db | MP3 | mp3db.pro |
| DeadPulpit | MP3 | deadpulpit.com |
| Ezhevika | MP3 | ezhevika.blogspot.com |
| Butterboy | MP3 | butterboycompilations.blogspot.com |
| Primitive Offerings | MP3 | primitiveofferings.blogspot.com |
| iPlusfree | M4A (256 kbps AAC) | iplusfree.org |

### Lyrics

| Provider | Source |
|---|---|
| LRCLIB | lrclib.net |
| Lyrics.ovh | lyrics.ovh |
| Lyrist | lyrist.vercel.app |
| Musixmatch | musixmatch.com |

> The "search only" sources work whenever their file hosts are up and
> auto-resolvable. MediaFire and Yandex Disk links resolve automatically;
> captcha-gated hosts (nitroflare, turbobit, hotlink, filecrypt) do not.

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

## Toggles

Three optional steps can be turned off independently in `config/settings.json`.
All default to on.

| Setting | Default | Effect |
|---|---|---|
| `metadata.fetch_lyrics` | `true` | fetch and embed lyrics from the lyrics providers |
| `metadata.fetch_cover` | `true` | download and embed album art |
| `metadata.fill_misc` | `true` | fill missing artist / album / title / cover from free metadata services |

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

## License

MIT. See [LICENSE](LICENSE).
