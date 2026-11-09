# radio_tui

```
██████╗  █████╗ ██████╗ ██╗ ██████╗  ██████╗  █████╗ ██████╗ ██████╗ ███████╗███╗   ██╗
██╔══██╗██╔══██╗██╔══██╗██║██╔═══██╗██╔════╝ ██╔══██╗██╔══██╗██╔══██╗██╔════╝████╗  ██║
██████╔╝███████║██║  ██║██║██║   ██║██║  ███╗███████║██████╔╝██║  ██║█████╗  ██╔██╗ ██║
██╔══██╗██╔══██║██║  ██║██║██║   ██║██║   ██║██╔══██║██╔══██╗██║  ██║██╔══╝  ██║╚██╗██║
██║  ██║██║  ██║██████╔╝██║╚██████╔╝╚██████╔╝██║  ██║██║  ██║██████╔╝███████╗██║ ╚████║
╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝ ╚═╝ ╚═════╝  ╚═════╝ ╚═╝  ╚═╝╚═╝  ╚═╝╚═════╝ ╚══════╝╚═╝  ╚═══╝
```

Live radio from around the planet, in your terminal. A TUI client for
[Radio Garden](https://radio.garden) built with `ratatui` and `rodio`.

- Browse ~13,000 places, busiest first.
- Fuzzy-grep places locally as you type, or search stations on radio.garden.
- Stream MP3 / AAC / OGG / FLAC, including stations behind redirect chains and `.pls`/`.m3u` playlists.
- Phosphor-green hacker UI with an ANSI Shadow logo, a world map that marks where the station is, a tuner bar and a spectrum display.
- Holds at most 15 seconds of audio in memory, sized from the station's bitrate.
- **Beyond Radio Garden:** stations from [Radio Browser](https://www.radio-browser.info)
  that Radio Garden doesn't have, with Radio Browser's own duplicates merged.

## Install & run

You need a stable Rust toolchain. On Linux you also need the ALSA headers
(`libasound2-dev` on Debian/Ubuntu, `alsa-lib-devel` on Fedora).

```bash
cargo run --release
```

The first launch downloads the place index (a few MB) and caches it in
`radio_cache.db` in the current directory. The cache is refreshed after
7 days. To store it somewhere else, set `RADIO_TUI_CACHE`:

```bash
RADIO_TUI_CACHE=~/.cache/radio_tui.db cargo run --release
```

If no audio device is available the UI still runs. Playback reports an
error instead of crashing.

## Keys

| Key | Action |
|---|---|
| `j`/`k`, `↑`/`↓` | move |
| `PgUp`/`PgDn`, `g`/`G` | page, first / last |
| `Enter`, `l`, `→` | open place / tune station |
| `Esc`, `h`, `←`, `Backspace` | back |
| `/` | search: typing filters places locally, `Enter` queries radio.garden, `↓`/`Tab` jumps into results |
| `Space`, `p` | stop / resume |
| `+` / `-` | volume |
| `r` | random place (on places) or random station (on a station list) |
| `n` | jump to the tuner (now playing) |
| `b` | Beyond Radio Garden (Radio Browser stations not on Radio Garden) |
| `I` | on Beyond results: rebuild the Radio Garden index now |
| `?` | help |
| `q`, `Ctrl+C` | quit |

## Beyond Radio Garden

Press `b` for stations from Radio Browser, a community station directory,
that aren't on Radio Garden. Each one is marked `[RB]`, and the tuner shows
the source of whatever is playing.

The query box takes a name plus optional filters:

```
jazz cafe tag:smooth_jazz cc:NG codec:mp3 br:128
```

(`cc:` is a two-letter country code, `br:` a minimum bitrate. Use `_` for
spaces inside a tag.)

To know which stations Radio Garden already has, the app builds a local index
of every Radio Garden channel. Radio Garden has no "all stations" endpoint, so
this means one request per place, about 13,000 requests in total. It runs in
the background (6 at a time, with a pause between requests), takes roughly
half an hour, resumes if interrupted, and refreshes after 7 days. Until it's
done, the Beyond list is marked **NOT YET FILTERED** or **PARTLY FILTERED**,
and it re-filters itself once the index is complete.

A Radio Browser station counts as already on Radio Garden when, in the same
country, its name matches after dropping frequencies, punctuation, filler
words (`radio`, `fm`, ...), its city and any tagline after `:` or `|`. Very
close long names also count. The matching errs on the side of showing a
station: a duplicate slipping through is better than hiding one that isn't on
Radio Garden.

Headless helpers:

```bash
cargo run --release -- --build-index           # build/resume the index, print progress
cargo run --release -- --build-index --force   # rebuild from scratch
cargo run --release -- --beyond "kexp"         # filtered Radio Browser results
```

Set `RADIO_TUI_RESOLVE_HOSTS=1` to also record each Radio Garden stream's
host, which adds a host match. It roughly doubles the index build, so it's off
by default.

## Troubleshooting a station

If a station won't play, probe it without the UI:

```bash
cargo run -- --probe <channel_id> [seconds]
```

The channel id is the last segment of a station URL, e.g. `vbFsCngB` in
`radio.garden/listen/kutx-fm-98-9/vbFsCngB`. The probe prints the final
stream URL after all redirects, the content type and each player state, then
plays for 8 seconds (or the number you give), printing the buffer level every
2 seconds.

HLS (`.m3u8` segment) streams are not supported yet.

## Development

```bash
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

Tests never touch the network. API parsing is tested against recorded JSON in
`tests/fixtures/` (Radio Garden and Radio Browser), and audio logic is tested
against the `Player` trait without a sound device.

Layout:

```
src/
  main.rs       event loop, command execution, headless modes
  app.rs        state + key handling (pure, unit tested)
  event.rs      terminal input + tick timer
  api/          Radio Garden client (client.rs) and Radio Browser client (radio_browser.rs)
  audio/        Player trait, rodio implementation, streaming reader
  ui/           ratatui rendering (pure functions of &App)
  cache.rs      SQLite cache: place list and Radio Garden channel index
  rg_index.rs   background builder for the channel index
  dedupe.rs     name normalization and matching (pure, unit tested)
  beyond.rs     Radio Browser results minus Radio Garden (pure)
  countries.rs  Radio Garden country names -> ISO codes
```

Radio Garden has no official API. The endpoints used here are unofficial and
may change. They are all isolated in `src/api/client.rs`. Radio Browser's
API is public; its client sends a User-Agent naming this app and registers a
click when you play one of its stations, as Radio Browser asks.
