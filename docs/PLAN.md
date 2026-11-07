# Radio TUI - Build Plan

## Overview

A Rust TUI client for [Radio Garden](https://radio.garden) that lets users browse global radio stations and stream live audio from the terminal.

## Architecture

```
src/
  main.rs          # Entry point, event loop, terminal setup
  app.rs           # App state, screen enum, navigation logic
  error.rs         # Project-wide error types (thiserror)
  event.rs         # crossterm input + tick event handling
  cache.rs         # SQLite persistence via rusqlite
  api/
    mod.rs         # Public re-exports
    types.rs       # Deserialize structs matching Radio Garden JSON
    client.rs      # HTTP client, redirect chains, playlists
  audio/
    mod.rs         # Player trait + rodio implementation
  ui/
    mod.rs         # ratatui rendering (pure functions, no I/O)
```

## Dependencies

| Crate | Purpose | Notes |
|-------|---------|-------|
| `ratatui` + `crossterm` | Terminal UI | |
| `rodio` | Audio playback | Runs on separate thread |
| `tokio` | Async runtime | `features = ["full"]` |
| `reqwest` | HTTP client | `features = ["json", "query"]`, follows redirect chains (limit 10) |
| `nucleo` | Fuzzy search | For filtering places/stations |
| `rusqlite` | Local cache | `features = ["bundled"]` |
| `thiserror` | Error derive | |
| `anyhow` | Main error handling | |

## API Endpoints

All endpoints are under `https://radio.garden/api`:

| Endpoint | Returns | Used For |
|----------|---------|----------|
| `GET /ara/content/places` | All places with stations | Initial load, cache |
| `GET /ara/content/page/{id}` | Place details + curated content | Place detail screen |
| `GET /ara/content/page/{id}/channels` | Flat station list for a place | Channel list |
| `GET /ara/content/channel/{id}` | Single station details | Channel detail |
| `GET /ara/content/listen/{id}/channel.mp3` | **302 redirect** to stream URL | Stream resolution |
| `GET /search?q={query}` | Search hits | Fuzzy search |

### Stream Resolution Flow

```
1. Client -> GET /ara/content/listen/{channelId}/channel.mp3
2. Server -> 302 to the station host, which may redirect again
   (CDN -> edge node -> ...). reqwest follows up to 10 hops.
3. If the final response is a .pls/.m3u playlist, read the first URL from it and repeat (max 4 times).
4. The audio response body is pumped into a channel, which the decoder
   reads on its own thread.
```

HEAD requests are not used: many Icecast/Shoutcast hosts reject or hang on
them. The stream is a single GET, so there's no second connection.

## Screens

| Screen | Key Bindings | Description |
|--------|-------------|-------------|
| `Places` | `j/k` navigate, `Enter` select, `/` search, `q` quit | Browse all places |
| `PlaceDetail` | `j/k` navigate, `Enter` play station, `Backspace` back | Channels in a place |
| `ChannelDetail` | `p` toggle playback, `+/-` volume, `Backspace` back | Station info |
| `Search` | `j/k` navigate, `Enter` select, `Esc` back | Search results |
| `NowPlaying` | `p` toggle playback, `+/-` volume, `Backspace` back | Active playback |

## Conventions

- **No unwrap/expect** in network or audio I/O paths
- **Pure UI functions**: take `&App` + `Frame`, no mutation or I/O
- **API layer isolated**: no `ratatui`/`rodio` types in `api/`
- **Audio behind trait**: `Player` trait for testability
- **Error handling**: `thiserror` enum in `error.rs`, `anyhow` in `main.rs`

## CI Pipeline

`.github/workflows/ci.yml` runs on push/PR to master:

1. **fmt** - `cargo fmt --all -- --check`
2. **clippy** - `cargo clippy --all-targets --all-features -- -D warnings`
3. **build** - `cargo build --release` on ubuntu/windows/macos
4. **test** - `cargo test --all-features`

## Build & Run

```bash
cargo build
cargo run
cargo test
cargo clippy -- -D warnings
cargo fmt --check
```

## Future Work

- [ ] Wire up `nucleo` for fuzzy search across places/stations
- [ ] Add volume control to `RodioPlayer` via `Sink::set_volume`
- [ ] Implement `is_playing()` properly by tracking sink state
- [ ] Add keyboard shortcut to jump to a random station
- [ ] Cache stream URLs with short TTL to avoid repeated 302 hops
- [ ] Add a loading spinner / progress indicator during API calls
- [ ] Support filtering by country
- [ ] Add help screen overlay
