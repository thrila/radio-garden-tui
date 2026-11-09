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

use api::{ApiClient, RadioBrowserClient, RbQuery, StationSource};
use app::{App, BeyondPage, Command, Filtering, Notice};
use audio::{NullPlayer, Player, PlayerEvent, RodioPlayer, StreamSource};
use cache::Cache;
use dedupe::DedupeIndex;
use event::AppEvent;
use rg_index::{IndexEvent, IndexOptions};

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
    match args.first().map(String::as_str) {
        Some("--probe") => {
            let channel_id = args.get(1).ok_or_else(|| {
                anyhow::anyhow!("usage: --probe <channel_id | stream_url> [seconds]")
            })?;
            let secs = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(8);
            return probe(&api, channel_id, secs).await;
        }
        Some("--build-index") => {
            let force = args.iter().any(|a| a == "--force");
            return build_index_cli(api, force).await;
        }
        Some("--beyond") => return beyond_cli(&args[1..].join(" ")).await,
        _ => {}
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

    let mut services = Services {
        api,
        rb: Arc::new(RadioBrowserClient::new()?),
        tx: tx.clone(),
        index: None,
        index_complete: false,
        index_job: None,
    };
    if app.places_loaded {
        services.start_index(&app, false);
    }

    event::spawn_input(tx.clone(), TICK_RATE);

    let mut terminal = ratatui::init();
    let result = run(
        &mut terminal,
        &mut app,
        &mut rx,
        &mut services,
        &mut *player,
        cache.as_ref(),
    )
    .await;
    player.stop();
    if let Some(job) = services.index_job.take() {
        job.abort();
    }
    ratatui::restore();
    result
}

/// Long-lived handles the event loop and commands need.
struct Services {
    api: Arc<ApiClient>,
    rb: Arc<RadioBrowserClient>,
    tx: mpsc::UnboundedSender<AppEvent>,
    /// Latest Radio Garden matching snapshot.
    index: Option<Arc<DedupeIndex>>,
    index_complete: bool,
    index_job: Option<tokio::task::JoinHandle<()>>,
}

impl Services {
    /// Build/resume/refresh the Radio Garden index in the background.
    fn start_index(&mut self, app: &App, force: bool) {
        if let Some(job) = self.index_job.take() {
            job.abort();
        }
        let places = app.places.iter().map(rg_index::PlaceRef::from).collect();
        let (index_tx, mut index_rx) = mpsc::unbounded_channel();
        let tx = self.tx.clone();
        tokio::spawn(async move {
            while let Some(ev) = index_rx.recv().await {
                if tx.send(AppEvent::Index(ev)).is_err() {
                    break;
                }
            }
        });
        self.index_job = Some(tokio::spawn(rg_index::run(
            self.api.clone(),
            cache_path(),
            places,
            IndexOptions::from_env(force),
            index_tx,
        )));
    }

    fn filtering(&self) -> Filtering {
        match (&self.index, self.index_complete) {
            (None, _) => Filtering::NotFiltered,
            (Some(_), false) => Filtering::Partial,
            (Some(_), true) => Filtering::Filtered,
        }
    }
}

async fn run(
    terminal: &mut DefaultTerminal,
    app: &mut App,
    rx: &mut mpsc::UnboundedReceiver<AppEvent>,
    services: &mut Services,
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
        let follow_up = match ev {
            AppEvent::Key(key) => app.handle_key(key),
            AppEvent::Resize => None,
            AppEvent::Tick => {
                app.on_tick();
                None
            }
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
                    if app.places_loaded && services.index_job.is_none() {
                        services.start_index(app, false);
                    }
                }
                None
            }
            AppEvent::Channels { place_id, result } => {
                app.on_channels(&place_id, result);
                None
            }
            AppEvent::Search { query, result } => {
                app.on_search(&query, result);
                None
            }
            AppEvent::StreamOpened { request, result } => {
                match result {
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
                }
                None
            }
            AppEvent::Player(ev) => {
                app.on_player_event(ev);
                None
            }
            AppEvent::Beyond { query, result } => {
                app.on_beyond(&query, result);
                None
            }
            AppEvent::Index(ev) => match ev {
                IndexEvent::Progress { done, total } => {
                    app.on_index_progress(done, total);
                    None
                }
                IndexEvent::Snapshot { index, complete } => {
                    let channels = index.channel_count();
                    services.index = Some(index);
                    services.index_complete = complete;
                    app.on_index_snapshot(channels, complete)
                }
                IndexEvent::Failed(e) => {
                    app.on_index_error(e);
                    None
                }
            },
        };
        if let Some(cmd) = follow_up
            && !execute(cmd, app, services, player)
        {
            return Ok(());
        }
    }
}

/// Run a command. Returns `false` when the app should exit.
fn execute(cmd: Command, app: &App, services: &mut Services, player: &mut dyn Player) -> bool {
    let (api, tx) = (services.api.clone(), services.tx.clone());
    match cmd {
        Command::Quit => return false,
        Command::LoadChannels { place_id } => {
            tokio::spawn(async move {
                let result = api
                    .get_place_channels(&place_id)
                    .await
                    .map_err(|e| e.to_string());
                let _ = tx.send(AppEvent::Channels { place_id, result });
            });
        }
        Command::Search { query } => {
            tokio::spawn(async move {
                let result = api.search(&query).await.map_err(|e| e.to_string());
                let _ = tx.send(AppEvent::Search { query, result });
            });
        }
        Command::Play { request, station } => {
            player.stop();
            let rb = services.rb.clone();
            tokio::spawn(async move {
                let result = match &station.source {
                    StationSource::RadioGarden => api.open_stream(&station.channel_id).await,
                    StationSource::RadioBrowser { uuid, stream_url } => {
                        // Radio Browser's click counter: once per play.
                        let (rb, uuid) = (rb.clone(), uuid.clone());
                        tokio::spawn(async move {
                            let _ = rb.count_click(&uuid).await;
                        });
                        api.open_url(stream_url).await
                    }
                };
                let result = result.map_err(|e| e.to_string());
                let _ = tx.send(AppEvent::StreamOpened { request, result });
            });
        }
        Command::Stop => player.stop(),
        Command::SetVolume(v) => player.set_volume(v),
        Command::BeyondSearch { query } => {
            let rb = services.rb.clone();
            let index = services.index.clone();
            let filtering = services.filtering();
            tokio::spawn(async move {
                let result = match rb.search(&RbQuery::parse(&query)).await {
                    Ok(raw) => tokio::task::spawn_blocking(move || {
                        let r = beyond::filter(raw, index.as_deref());
                        BeyondPage {
                            stations: r.stations,
                            hidden: r.hidden,
                            collapsed: r.collapsed,
                            filtering,
                        }
                    })
                    .await
                    .map_err(|e| e.to_string()),
                    Err(e) => Err(e.to_string()),
                };
                let _ = tx.send(AppEvent::Beyond { query, result });
            });
        }
        Command::RebuildIndex => services.start_index(app, true),
    }
    true
}

/// `radio_tui --probe <channel_id | stream_url> [seconds]`: resolve and play
/// a station without the TUI, printing each step. Handy for diagnosing a
/// station that won't play.
async fn probe(api: &ApiClient, channel_id: &str, secs: u64) -> anyhow::Result<()> {
    // A full URL (e.g. a Radio Browser url_resolved) is played directly.
    let stream = if channel_id.starts_with("http://") || channel_id.starts_with("https://") {
        println!("[probe] stream url: {channel_id}");
        api.open_url(channel_id).await?
    } else {
        println!("[probe] listen url: {}", ApiClient::listen_url(channel_id));
        api.open_stream(channel_id).await?
    };
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

/// Places from the cache, downloading the list first if it's empty.
async fn places_for_index(
    api: &ApiClient,
    cache: &Cache,
) -> anyhow::Result<Vec<rg_index::PlaceRef>> {
    let mut places = cache.get_all_places()?;
    if places.is_empty() {
        places = api.get_all_places().await?;
        cache.save_places(&places)?;
    }
    Ok(places.iter().map(rg_index::PlaceRef::from).collect())
}

/// `radio_tui --build-index [--force]`: build the Radio Garden channel index
/// without the TUI, printing progress.
async fn build_index_cli(api: Arc<ApiClient>, force: bool) -> anyhow::Result<()> {
    let path = cache_path();
    let places = places_for_index(&api, &Cache::new(&path)?).await?;
    let (tx, mut rx) = mpsc::unbounded_channel();
    let options = rg_index::IndexOptions::from_env(force);
    println!(
        "[index] {} places, {} workers, host resolution {}",
        places.len(),
        rg_index::WORKERS,
        if options.resolve_hosts { "on" } else { "off" }
    );
    let job = tokio::spawn(rg_index::run(api, path, places, options, tx));
    let mut last_pct = usize::MAX;
    while let Some(ev) = rx.recv().await {
        match ev {
            rg_index::IndexEvent::Progress { done, total } => {
                let pct = done * 100 / total.max(1);
                if pct != last_pct {
                    last_pct = pct;
                    println!("[index] {done}/{total} places ({pct}%)");
                }
            }
            rg_index::IndexEvent::Snapshot { index, complete } => println!(
                "[index] snapshot: {} channels{}",
                index.channel_count(),
                if complete { " (complete)" } else { "" }
            ),
            rg_index::IndexEvent::Failed(e) => println!("[index] {e}"),
        }
    }
    job.await?;
    Ok(())
}

/// `radio_tui --beyond <query>`: Radio Browser results minus Radio Garden,
/// using whatever index is cached. Same query syntax as the Beyond screen.
async fn beyond_cli(query: &str) -> anyhow::Result<()> {
    let cache = Cache::new(&cache_path())?;
    let channels = cache.indexed_channels()?;
    let index = (!channels.is_empty()).then(|| rg_index::snapshot(&channels));
    let rb = api::RadioBrowserClient::new()?;
    let raw = rb.search(&api::RbQuery::parse(query)).await?;
    let fetched = raw.len();
    let result = beyond::filter(raw, index.as_ref());
    println!(
        "[beyond] {fetched} from Radio Browser, {} collapsed as duplicates, {} hidden (on Radio Garden), {} shown; index: {}",
        result.collapsed,
        result.hidden,
        result.stations.len(),
        match (&index, cache.index_age()) {
            (None, _) => "none yet (NOT FILTERED)".to_string(),
            (Some(i), Some(_)) => format!("{} channels, complete", i.channel_count()),
            (Some(i), None) => format!("{} channels, partial", i.channel_count()),
        }
    );
    for s in &result.stations {
        println!(
            "  [RB] {:<40} {:<3} {:>5} {:>4}k  votes {}",
            s.name.chars().take(40).collect::<String>(),
            s.countrycode,
            s.codec,
            s.bitrate,
            s.votes
        );
    }
    Ok(())
}
