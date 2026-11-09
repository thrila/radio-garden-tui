mod api;
mod app;
mod audio;
mod beyond;
mod budget;
mod cache;
mod countries;
mod dedupe;
mod error;
mod event;
mod rg_index;
mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use ratatui::DefaultTerminal;
use tokio::sync::mpsc;

use api::ApiClient;
use app::{App, Command, Notice};
use audio::{NullPlayer, Player, PlayerEvent, RodioPlayer, StreamSource};
use cache::Cache;
use event::AppEvent;

const TICK_RATE: Duration = Duration::from_millis(80);
/// Re-download the (large) place index at most this often.
const PLACES_MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);

fn cache_path() -> PathBuf {
    std::env::var_os("RADIO_TUI_CACHE")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("radio_cache.db"))
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let api = Arc::new(ApiClient::new()?);

    let args: Vec<String> = std::env::args().skip(1).collect();
    if let [flag, channel_id, ..] = args.as_slice()
        && flag == "--probe"
    {
        let secs = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
        return probe(&api, channel_id, secs).await;
    }

    let mut app = App::new();

    let (tx, mut rx) = mpsc::unbounded_channel::<AppEvent>();
    let (player_tx, mut player_rx) = mpsc::unbounded_channel::<PlayerEvent>();

    // Open audio before entering the alternate screen so any driver noise on
    // stderr doesn't garble the UI. A missing device is not fatal.
    let mut player: Box<dyn Player> = match RodioPlayer::new(player_tx.clone(), app.volume) {
        Ok(p) => Box::new(p),
        Err(e) => {
            app.notice = Some(Notice::Error(format!("{e} — playback disabled")));
            Box::new(NullPlayer::new(e.to_string(), player_tx))
        }
    };
    {
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = player_rx.recv().await {
                if tx.send(AppEvent::Player(ev)).is_err() {
                    break;
                }
            }
        });
    }

    let cache = match Cache::new(&cache_path()) {
        Ok(c) => Some(c),
        Err(e) => {
            app.notice = Some(Notice::Error(format!("{e} — running without cache")));
            None
        }
    };
    let cached = cache
        .as_ref()
        .and_then(|c| c.get_all_places().ok())
        .unwrap_or_default();
    let fresh = cache
        .as_ref()
        .is_some_and(|c| c.places_age().is_some_and(|age| age < PLACES_MAX_AGE));
    if !cached.is_empty() {
        app.set_places(cached);
    }
    if !app.places_loaded || !fresh {
        if !app.places_loaded {
            app.loading = Some("Downloading place index".into());
        }
        let api = api.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            let result = api.get_all_places().await.map_err(|e| e.to_string());
            let _ = tx.send(AppEvent::Places(result));
        });
    }

    event::spawn_input(tx.clone(), TICK_RATE);

    let mut terminal = ratatui::init();
    let result = run(
        &mut terminal,
        &mut app,
        &mut rx,
        &tx,
        &api,
        &mut *player,
        cache.as_ref(),
    )
    .await;
    player.stop();
    ratatui::restore();
    result
}

async fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    tx: &mpsc::UnboundedSender<AppEvent>,
    api: &Arc<ApiClient>,
    player: &mut dyn Player,
    cache: Option<&Cache>,
) -> anyhow::Result<()> {
    loop {
        let size = terminal.size()?;
        app.set_list_height(ui::list_rows(size.height, app.screen));
        terminal.draw(|frame| ui::render(frame, app))?;

        let Some(ev) = rx.recv().await else {
            return Ok(());
        };
        match ev {
            AppEvent::Key(key) => {
                if let Some(cmd) = app.handle_key(key)
                    && !execute(cmd, tx, api, player)
                {
                    return Ok(());
                }
            }
            AppEvent::Resize => {}
            AppEvent::Tick => app.on_tick(),
            AppEvent::Places(result) => {
                if let (Ok(places), Some(cache)) = (&result, cache)
                    && let Err(e) = cache.save_places(places)
                {
                    app.notice = Some(Notice::Error(e.to_string()));
                }
                // A background refresh shouldn't yank the list from under
                // the user; only apply it when nothing was shown yet.
                if !app.places_loaded || result.is_err() {
                    app.on_places(result);
                }
            }
            AppEvent::Channels { place_id, result } => app.on_channels(&place_id, result),
            AppEvent::Search { query, result } => app.on_search(&query, result),
            AppEvent::StreamOpened { request, result } => match result {
                Ok(stream) => {
                    // A dropped `stream` closes the connection, so stale
                    // streams clean themselves up.
                    if app.on_stream_opened(request, Ok(stream.url.clone())) {
                        let session = player.play(StreamSource {
                            content_type: stream.content_type,
                            bitrate_kbps: stream.bitrate_kbps,
                            chunks: stream.chunks,
                            budget: stream.budget,
                        });
                        app.on_player_started(session);
                    }
                }
                Err(e) => {
                    app.on_stream_opened(request, Err(e));
                }
            },
            AppEvent::Player(ev) => app.on_player_event(ev),
        }
    }
}

/// Run a command. Returns `false` when the app should exit.
fn execute(
    cmd: Command,
    tx: &mpsc::UnboundedSender<AppEvent>,
    api: &Arc<ApiClient>,
    player: &mut dyn Player,
) -> bool {
    match cmd {
        Command::Quit => return false,
        Command::LoadChannels { place_id } => {
            let (api, tx) = (api.clone(), tx.clone());
            tokio::spawn(async move {
                let result = api
                    .get_place_channels(&place_id)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx.send(AppEvent::Channels { place_id, result });
            });
        }
        Command::Search { query } => {
            let (api, tx) = (api.clone(), tx.clone());
            tokio::spawn(async move {
                let result = api.search(&query).await.map_err(|e| e.to_string());
                let _ = tx.send(AppEvent::Search { query, result });
            });
        }
        Command::Play { request, station } => {
            player.stop();
            let (api, tx) = (api.clone(), tx.clone());
            tokio::spawn(async move {
                let result = api
                    .open_stream(&station.channel_id)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx.send(AppEvent::StreamOpened { request, result });
            });
        }
        Command::Stop => player.stop(),
        Command::SetVolume(v) => player.set_volume(v),
    }
    true
}

/// `radio_tui --probe <channel_id> [seconds]`: resolve and play a station
/// without the TUI, printing each step. Handy for diagnosing a station that
/// won't play.
async fn probe(api: &ApiClient, channel_id: &str, secs: u64) -> anyhow::Result<()> {
    println!("[probe] listen url: {}", ApiClient::listen_url(channel_id));
    let stream = api.open_stream(channel_id).await?;
    println!(
        "[probe] resolved:   {} ({})",
        stream.url,
        stream.content_type.as_deref().unwrap_or("no content-type")
    );
    match stream.bitrate_kbps {
        Some(kbps) => println!("[probe] bitrate:    {kbps} kbps (advertised)"),
        None => println!("[probe] bitrate:    not advertised, will measure"),
    }

    let (tx, mut rx) = mpsc::unbounded_channel();
    let mut player = RodioPlayer::new(tx, 0.4)?;
    player.play(StreamSource {
        content_type: stream.content_type,
        bitrate_kbps: stream.bitrate_kbps,
        chunks: stream.chunks,
        budget: stream.budget,
    });

    let deadline = tokio::time::sleep(Duration::from_secs(secs));
    tokio::pin!(deadline);
    let mut reports = 0u32;
    loop {
        tokio::select! {
            _ = &mut deadline => {
                println!("[probe] still playing after {secs}s: OK");
                break;
            }
            ev = rx.recv() => match ev {
                Some(ev) => {
                    if let audio::PlayerState::Buffer { seconds, capacity_seconds, kbps } = ev.state {
                        reports += 1;
                        if reports.is_multiple_of(4) {
                            println!(
                                "[probe] buffer:     {seconds:>4.1}s / {capacity_seconds:.0}s @ {kbps} kbps"
                            );
                        }
                        continue;
                    }
                    println!("[probe] player:     {:?}", ev.state);
                    if matches!(ev.state, audio::PlayerState::Error(_) | audio::PlayerState::Ended) {
                        anyhow::bail!("playback failed");
                    }
                }
                None => break,
            },
        }
    }
    player.stop();
    Ok(())
}
