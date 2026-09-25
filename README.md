# PicaroDL

A music downloader that doesn't ask you to sign in.

PicaroDL started as a Rust port of [OrpheusDL](https://github.com/OrpheusDL/OrpheusDL).
OrpheusDL is a good program, but it's built around logging into streaming
services — you bring an account, sometimes a paid one, and it does the rest.
Somewhere along the way this project stopped being a port and turned into
something else.

The account-based modules are still here (Qobuz, Tidal, Deezer, Spotify,
Beatport, Beatsource), but they're turned off by default, because none of them
can actually download without credentials. What's enabled instead works with
nothing: no accounts, no tokens, no API keys. You give it a song and it pulls
from public sources, picking whichever one answers best and fastest.

## How it behaves

Ask for `"Radiohead - Creep"` and PicaroDL queries every suitable source at the
same time. It takes the first result that actually matches the artist and title,
ignoring wrong matches, and downloads it. If that source dies partway through —
dead host, captcha, whatever — it moves to the next one. If the quality you asked
for isn't there, it steps down to the next best instead of failing outright.

Nothing waits on a slow source. Nothing asks you to log in.

## What's in it

- **No sign-in anywhere in the default path.** The 21 enabled providers need no
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

## Providers (enabled by default)

Everything below works without an account.

**Lossless — FLAC**

| Provider | Source |
|---|---|
| CoreRadio | coreradio.online |
| AlterPortal | alterportal.net |
| Exystence | exystence.net |
| ThemFire | themfire.pro |
| FlacMusic | flacmusic.info |
| LosslessAlbums | losslessalbums.club |
| MusicRider | musicrider.org (FLAC / MP3) |

**MP3**

| Provider | Source |
|---|---|
| ccMixter | ccmixter.org (320 kbps) |
| Mp3db | mp3db.pro |
| Zvu4it | zvu4it.org |
| Tancpol | tancpol.net |
| DeadPulpit | deadpulpit.com |
| Punkcata | punkcata.blogspot.com |
| Ezhevika | ezhevika.blogspot.com |
| Butterboy | butterboycompilations.blogspot.com |
| Primitive Offerings | primitiveofferings.blogspot.com |

**Other formats**

| Provider | Format | Source |
|---|---|---|
| iPlusfree | M4A (256 kbps AAC) | iplusfree.org |
| YouTube | Opus (stream) | youtube.com |
| SoundCloud | Opus (stream) | soundcloud.com |

**Lyrics**

| Provider | Source |
|---|---|
| LRCLIB | lrclib.net |
| Musixmatch | musixmatch.com |

The account-only providers (Qobuz, Tidal, Deezer, Spotify, Beatport, Beatsource)
are present in the source tree but disabled, and the resolver skips them.

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
