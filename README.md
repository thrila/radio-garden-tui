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
| `?` | help |
| `q`, `Ctrl+C` | quit |

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
`tests/fixtures/`, and audio logic is tested against the `Player` trait
without a sound device.

Layout:

```
src/
  main.rs      event loop, command execution, --probe
  app.rs       state + key handling (pure, unit tested)
  event.rs     terminal input + tick timer
  api/         Radio Garden client: places, channels, search, stream resolution
  audio/       Player trait, rodio implementation, streaming reader
  ui/          ratatui rendering (pure functions of &App)
  cache.rs     SQLite cache of the place index
```

Radio Garden has no official API. The endpoints used here are unofficial and
may change. They are all isolated in `src/api/client.rs`.
