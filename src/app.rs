//! Application state and key handling. Nothing in here performs I/O: key
//! presses mutate state and may return a [`Command`] that `main` executes
//! (API calls, audio). Results come back through the `on_*` methods.

use std::collections::HashMap;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use nucleo::pattern::{CaseMatching, Normalization, Pattern};
use nucleo::{Config, Matcher, Utf32Str};

use crate::api::{ChannelListItem, Place, RbStation, SearchHit};
use crate::audio::{PlayerEvent, PlayerState};

pub const VOLUME_STEP: f32 = 0.05;
const LOCAL_MATCH_LIMIT: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Screen {
    Places,
    Stations,
    Search,
    NowPlaying,
    /// Radio Browser stations that aren't on Radio Garden.
    Beyond,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SearchFocus {
    Input,
    Results,
}

/// Side effects requested by the UI; executed by `main`.
#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Quit,
    LoadChannels {
        place_id: String,
    },
    Search {
        query: String,
    },
    Play {
        request: u64,
        station: ChannelListItem,
    },
    Stop,
    SetVolume(f32),
    /// Query Radio Browser and filter out Radio Garden stations.
    BeyondSearch {
        query: String,
    },
    /// Rebuild the Radio Garden index now, even if it's fresh.
    RebuildIndex,
}

/// How a Beyond result list was filtered against Radio Garden.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Filtering {
    /// No Radio Garden index yet: may contain Radio Garden stations.
    NotFiltered,
    /// Index still being built: some Radio Garden stations may slip through.
    Partial,
    Filtered,
}

/// One page of Beyond results, already filtered.
#[derive(Debug, Clone, PartialEq)]
pub struct BeyondPage {
    pub stations: Vec<RbStation>,
    pub hidden: usize,
    pub collapsed: usize,
    pub filtering: Filtering,
}

/// State of the background Radio Garden index.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IndexStatus {
    pub channels: usize,
    pub complete: bool,
    /// `(places done, places total)` while a build runs.
    pub progress: Option<(usize, usize)>,
    pub error: Option<String>,
}

impl IndexStatus {
    pub fn filtering(&self) -> Filtering {
        if self.channels == 0 {
            Filtering::NotFiltered
        } else if self.complete {
            Filtering::Filtered
        } else {
            Filtering::Partial
        }
    }

    pub fn percent(&self) -> Option<usize> {
        self.progress
            .map(|(done, total)| done * 100 / total.max(1))
            .filter(|_| !self.complete || self.progress.is_some_and(|(d, t)| d < t))
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Playback {
    Idle,
    Connecting,
    Buffering,
    Playing,
    Stopped,
    Ended,
    Error(String),
}

impl Playback {
    pub fn is_active(&self) -> bool {
        matches!(
            self,
            Playback::Connecting | Playback::Buffering | Playback::Playing
        )
    }
}

/// Latest buffer report from the player.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct BufferInfo {
    pub seconds: f32,
    pub capacity_seconds: f32,
    pub kbps: u32,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlaceSummary {
    pub id: String,
    pub title: String,
    pub subtitle: String,
}

#[derive(Debug, Clone, PartialEq)]
pub enum SearchItem {
    Place(PlaceSummary),
    Station(ChannelListItem),
}

#[derive(Debug, Clone, PartialEq)]
pub enum Notice {
    Info(String),
    Error(String),
}

/// Selection + scroll offset for a list whose visible height is `page`.
#[derive(Debug, Clone, PartialEq)]
pub struct ListState {
    pub offset: usize,
    pub selected: usize,
    pub total: usize,
    pub page: usize,
}

impl Default for ListState {
    fn default() -> Self {
        Self::new(0)
    }
}

impl ListState {
    pub fn new(total: usize) -> Self {
        Self {
            offset: 0,
            selected: 0,
            total,
            page: 20,
        }
    }

    pub fn reset(&mut self, total: usize) {
        self.total = total;
        self.selected = 0;
        self.offset = 0;
    }

    pub fn set_page(&mut self, page: usize) {
        self.page = page.max(1);
        self.clamp_offset();
    }

    fn clamp_offset(&mut self) {
        if self.selected < self.offset {
            self.offset = self.selected;
        }
        if self.selected >= self.offset + self.page {
            self.offset = self.selected + 1 - self.page;
        }
    }

    pub fn next(&mut self) {
        if self.total == 0 {
            return;
        }
        self.selected = (self.selected + 1) % self.total;
        self.clamp_offset();
    }

    pub fn previous(&mut self) {
        if self.total == 0 {
            return;
        }
        self.selected = if self.selected == 0 {
            self.total - 1
        } else {
            self.selected - 1
        };
        self.clamp_offset();
    }

    pub fn page_down(&mut self) {
        if self.total == 0 {
            return;
        }
        self.selected = (self.selected + self.page).min(self.total - 1);
        self.clamp_offset();
    }

    pub fn page_up(&mut self) {
        self.selected = self.selected.saturating_sub(self.page);
        self.clamp_offset();
    }

    pub fn first(&mut self) {
        self.selected = 0;
        self.clamp_offset();
    }

    pub fn last(&mut self) {
        self.selected = self.total.saturating_sub(1);
        self.clamp_offset();
    }

    pub fn select_index(&mut self, i: usize) {
        if i < self.total {
            self.selected = i;
            self.clamp_offset();
        }
    }
}

pub struct App {
    pub screen: Screen,
    /// Where `Esc` from Now Playing returns to.
    pub return_screen: Screen,

    pub places: Vec<Place>,
    /// `(lon, lat)` of every place, for the world map.
    pub place_points: Vec<(f64, f64)>,
    place_index: HashMap<String, usize>,
    pub places_state: ListState,
    pub places_loaded: bool,

    pub current_place: Option<PlaceSummary>,
    pub channels: Vec<ChannelListItem>,
    pub channels_state: ListState,

    pub search_input: String,
    pub search_query: String,
    pub search_focus: SearchFocus,
    pub search_results: Vec<SearchItem>,
    pub search_state: ListState,
    pub search_is_remote: bool,

    pub now_playing: Option<ChannelListItem>,
    pub stream_url: Option<String>,
    pub playback: Playback,
    pub playing_since: Option<Instant>,
    pub buffer: Option<BufferInfo>,
    play_request: u64,
    player_session: Option<u64>,

    pub volume: f32,
    pub loading: Option<String>,
    pub notice: Option<Notice>,
    pub show_help: bool,
    pub tick: u64,
    seed: u64,

    pub index: IndexStatus,
    pub beyond_input: String,
    /// The query the current results are for.
    pub beyond_query: Option<String>,
    pub beyond_focus: SearchFocus,
    pub beyond_results: Vec<RbStation>,
    pub beyond_state: ListState,
    pub beyond_hidden: usize,
    pub beyond_collapsed: usize,
    pub beyond_filtering: Filtering,
}

impl Default for App {
    fn default() -> Self {
        Self::new()
    }
}

impl App {
    pub fn new() -> Self {
        Self {
            screen: Screen::Places,
            return_screen: Screen::Places,
            places: Vec::new(),
            place_points: Vec::new(),
            place_index: HashMap::new(),
            places_state: ListState::default(),
            places_loaded: false,
            current_place: None,
            channels: Vec::new(),
            channels_state: ListState::default(),
            search_input: String::new(),
            search_query: String::new(),
            search_focus: SearchFocus::Input,
            search_results: Vec::new(),
            search_state: ListState::default(),
            search_is_remote: false,
            now_playing: None,
            stream_url: None,
            playback: Playback::Idle,
            playing_since: None,
            buffer: None,
            play_request: 0,
            player_session: None,
            volume: 0.6,
            loading: None,
            notice: None,
            show_help: false,
            tick: 0,
            index: IndexStatus::default(),
            beyond_input: String::new(),
            beyond_query: None,
            beyond_focus: SearchFocus::Input,
            beyond_results: Vec::new(),
            beyond_state: ListState::default(),
            beyond_hidden: 0,
            beyond_collapsed: 0,
            beyond_filtering: Filtering::NotFiltered,
            seed: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0x9E37_79B9_7F4A_7C15),
        }
    }

    // ----- state helpers --------------------------------------------------

    pub fn set_places(&mut self, mut places: Vec<Place>) {
        // Busiest places first: much nicer to browse than API order.
        places.sort_by(|a, b| b.size.cmp(&a.size).then_with(|| a.title.cmp(&b.title)));
        self.places_state.reset(places.len());
        self.place_points = places.iter().filter_map(place_lon_lat).collect();
        self.place_index = places
            .iter()
            .enumerate()
            .map(|(i, p)| (p.id.clone(), i))
            .collect();
        self.places = places;
        self.places_loaded = true;
    }

    /// Visible list height for the main content area, used for paging.
    pub fn set_list_height(&mut self, height: usize) {
        for s in [
            &mut self.places_state,
            &mut self.channels_state,
            &mut self.search_state,
            &mut self.beyond_state,
        ] {
            s.set_page(height);
        }
    }

    fn active_list(&mut self) -> Option<&mut ListState> {
        match self.screen {
            Screen::Places => Some(&mut self.places_state),
            Screen::Stations => Some(&mut self.channels_state),
            Screen::Search if self.search_focus == SearchFocus::Results => {
                Some(&mut self.search_state)
            }
            Screen::Beyond if self.beyond_focus == SearchFocus::Results => {
                Some(&mut self.beyond_state)
            }
            _ => None,
        }
    }

    fn random(&mut self, n: usize) -> usize {
        // xorshift64*: good enough for picking a station.
        self.seed ^= self.seed >> 12;
        self.seed ^= self.seed << 25;
        self.seed ^= self.seed >> 27;
        (self.seed.wrapping_mul(0x2545_F491_4F6C_DD1D) % n.max(1) as u64) as usize
    }

    /// `(lon, lat)` of a place by id, if it's in the index.
    pub fn place_coords(&self, place_id: &str) -> Option<(f64, f64)> {
        let i = *self.place_index.get(place_id)?;
        place_lon_lat(self.places.get(i)?)
    }

    /// Where the tuned station is: its own place, else the open place.
    pub fn now_playing_coords(&self) -> Option<(f64, f64)> {
        self.now_playing
            .as_ref()
            .and_then(|s| s.place.as_ref())
            .and_then(|p| self.place_coords(&p.id))
            .or_else(|| self.place_coords(&self.current_place.as_ref()?.id))
    }

    /// The place under the cursor on the places screen.
    pub fn selected_place_coords(&self) -> Option<(f64, f64)> {
        place_lon_lat(self.places.get(self.places_state.selected)?)
    }

    pub fn elapsed_secs(&self) -> u64 {
        match (&self.playback, self.playing_since) {
            (Playback::Playing, Some(t)) => t.elapsed().as_secs(),
            _ => 0,
        }
    }

    fn volume_by(&mut self, delta: f32) -> Command {
        self.volume = ((self.volume + delta) * 100.0).round().clamp(0.0, 100.0) / 100.0;
        Command::SetVolume(self.volume)
    }

    pub fn volume_up(&mut self) -> Command {
        self.volume_by(VOLUME_STEP)
    }

    pub fn volume_down(&mut self) -> Command {
        self.volume_by(-VOLUME_STEP)
    }

    fn play(&mut self, station: ChannelListItem) -> Command {
        self.play_request += 1;
        self.player_session = None;
        self.now_playing = Some(station.clone());
        self.stream_url = None;
        self.playback = Playback::Connecting;
        self.playing_since = None;
        self.buffer = None;
        self.notice = None;
        if self.screen != Screen::NowPlaying {
            self.return_screen = self.screen;
        }
        self.screen = Screen::NowPlaying;
        Command::Play {
            request: self.play_request,
            station,
        }
    }

    fn toggle_playback(&mut self) -> Option<Command> {
        if self.playback.is_active() {
            self.play_request += 1; // invalidate any pending stream open
            self.player_session = None;
            self.playback = Playback::Stopped;
            self.playing_since = None;
            self.buffer = None;
            Some(Command::Stop)
        } else {
            let station = self.now_playing.clone()?;
            let screen = self.screen;
            let cmd = self.play(station);
            self.screen = screen; // resuming shouldn't jump screens
            Some(cmd)
        }
    }

    fn open_place(&mut self, place: PlaceSummary) -> Command {
        let place_id = place.id.clone();
        if self.current_place.as_ref().map(|p| &p.id) != Some(&place_id) {
            self.channels.clear();
            self.channels_state.reset(0);
        }
        self.current_place = Some(place);
        self.loading = Some("Loading stations".into());
        self.screen = Screen::Stations;
        Command::LoadChannels { place_id }
    }

    fn refresh_local_search(&mut self) {
        self.search_is_remote = false;
        let query = self.search_input.trim();
        self.search_results = if query.is_empty() {
            Vec::new()
        } else {
            fuzzy_places(&self.places, query, LOCAL_MATCH_LIMIT)
                .into_iter()
                .map(|p| SearchItem::Place(place_summary(p)))
                .collect()
        };
        self.search_state.reset(self.search_results.len());
    }

    fn go_back(&mut self) {
        match self.screen {
            Screen::Places => {}
            Screen::Stations => self.screen = Screen::Places,
            Screen::Search => {
                if self.search_focus == SearchFocus::Results {
                    self.search_focus = SearchFocus::Input;
                } else {
                    self.screen = Screen::Places;
                }
            }
            Screen::NowPlaying => {
                self.screen = if self.return_screen == Screen::NowPlaying {
                    Screen::Places
                } else {
                    self.return_screen
                };
            }
            Screen::Beyond => {
                if self.beyond_focus == SearchFocus::Results {
                    self.beyond_focus = SearchFocus::Input;
                } else {
                    self.screen = Screen::Places;
                }
            }
        }
    }

    fn beyond_search(&mut self, query: String) -> Command {
        self.loading = Some(if query.is_empty() {
            "Asking Radio Browser".to_string()
        } else {
            format!("Asking Radio Browser for \"{query}\"")
        });
        self.beyond_query = Some(query.clone());
        Command::BeyondSearch { query }
    }

    fn enter_beyond(&mut self) -> Option<Command> {
        if self.screen != Screen::Beyond {
            self.return_screen = self.screen;
        }
        self.screen = Screen::Beyond;
        if self.beyond_query.is_none() {
            // First visit: show the most-voted stations right away.
            self.beyond_focus = SearchFocus::Input;
            return Some(self.beyond_search(String::new()));
        }
        if !self.beyond_results.is_empty() {
            self.beyond_focus = SearchFocus::Results;
        }
        None
    }

    fn handle_beyond_input(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Esc => {
                self.go_back();
                None
            }
            KeyCode::Enter => {
                let query = self.beyond_input.trim().to_string();
                Some(self.beyond_search(query))
            }
            KeyCode::Down | KeyCode::Tab if !self.beyond_results.is_empty() => {
                self.beyond_focus = SearchFocus::Results;
                None
            }
            KeyCode::Backspace => {
                self.beyond_input.pop();
                None
            }
            KeyCode::Char(c) => {
                self.beyond_input.push(c);
                None
            }
            _ => None,
        }
    }

    // ----- key handling ---------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Option<Command> {
        if key.kind == KeyEventKind::Release {
            return None;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && matches!(key.code, KeyCode::Char('c') | KeyCode::Char('q')) {
            return Some(Command::Quit);
        }

        if self.show_help {
            // Any key closes the help overlay.
            self.show_help = false;
            return None;
        }

        if self.screen == Screen::Search && self.search_focus == SearchFocus::Input {
            return self.handle_search_input(key);
        }
        if self.screen == Screen::Beyond {
            if self.beyond_focus == SearchFocus::Input {
                return self.handle_beyond_input(key);
            }
            match key.code {
                KeyCode::Char('/') => {
                    self.beyond_focus = SearchFocus::Input;
                    return None;
                }
                KeyCode::Char('I') => {
                    self.index.error = None;
                    return Some(Command::RebuildIndex);
                }
                _ => {}
            }
        }

        // Navigation shared by all list screens.
        if let Some(list) = self.active_list() {
            let moved = match key.code {
                KeyCode::Down | KeyCode::Char('j') => {
                    list.next();
                    true
                }
                KeyCode::Up | KeyCode::Char('k') => {
                    list.previous();
                    true
                }
                KeyCode::PageDown => {
                    list.page_down();
                    true
                }
                KeyCode::PageUp => {
                    list.page_up();
                    true
                }
                KeyCode::Home | KeyCode::Char('g') => {
                    list.first();
                    true
                }
                KeyCode::End | KeyCode::Char('G') => {
                    list.last();
                    true
                }
                _ => false,
            };
            if moved {
                return None;
            }
        }

        match key.code {
            KeyCode::Char('q') => Some(Command::Quit),
            KeyCode::Char('?') => {
                self.show_help = true;
                None
            }
            KeyCode::Esc | KeyCode::Backspace | KeyCode::Left | KeyCode::Char('h') => {
                self.go_back();
                None
            }
            KeyCode::Char('/') => {
                self.screen = Screen::Search;
                self.search_focus = SearchFocus::Input;
                None
            }
            KeyCode::Char('b') => self.enter_beyond(),
            KeyCode::Char('n') if self.now_playing.is_some() => {
                if self.screen != Screen::NowPlaying {
                    self.return_screen = self.screen;
                }
                self.screen = Screen::NowPlaying;
                None
            }
            KeyCode::Char(' ') | KeyCode::Char('p') => self.toggle_playback(),
            KeyCode::Char('+') | KeyCode::Char('=') => Some(self.volume_up()),
            KeyCode::Char('-') | KeyCode::Char('_') => Some(self.volume_down()),
            KeyCode::Char('r') => self.random_pick(),
            KeyCode::Enter | KeyCode::Right | KeyCode::Char('l') => self.activate(),
            _ => None,
        }
    }

    fn handle_search_input(&mut self, key: KeyEvent) -> Option<Command> {
        match key.code {
            KeyCode::Esc => {
                self.go_back();
                None
            }
            KeyCode::Enter => {
                let query = self.search_input.trim().to_string();
                if query.is_empty() {
                    return None;
                }
                self.loading = Some(format!("Searching \"{query}\""));
                Some(Command::Search { query })
            }
            KeyCode::Down | KeyCode::Tab if !self.search_results.is_empty() => {
                self.search_focus = SearchFocus::Results;
                None
            }
            KeyCode::Backspace => {
                self.search_input.pop();
                self.refresh_local_search();
                None
            }
            KeyCode::Char(c) => {
                self.search_input.push(c);
                self.refresh_local_search();
                None
            }
            _ => None,
        }
    }

    fn activate(&mut self) -> Option<Command> {
        match self.screen {
            Screen::Places => {
                let place = self.places.get(self.places_state.selected)?;
                let summary = place_summary(place);
                Some(self.open_place(summary))
            }
            Screen::Stations => {
                let station = self.channels.get(self.channels_state.selected)?.clone();
                Some(self.play(station))
            }
            Screen::Search => match self.search_results.get(self.search_state.selected)?.clone() {
                SearchItem::Place(p) => Some(self.open_place(p)),
                SearchItem::Station(s) => Some(self.play(s)),
            },
            Screen::Beyond => {
                let station = self
                    .beyond_results
                    .get(self.beyond_state.selected)?
                    .to_station();
                Some(self.play(station))
            }
            Screen::NowPlaying => None,
        }
    }

    fn random_pick(&mut self) -> Option<Command> {
        match self.screen {
            Screen::Beyond if !self.beyond_results.is_empty() => {
                let i = self.random(self.beyond_results.len());
                self.beyond_state.select_index(i);
                let station = self.beyond_results[i].to_station();
                Some(self.play(station))
            }
            Screen::Stations if !self.channels.is_empty() => {
                let i = self.random(self.channels.len());
                self.channels_state.select_index(i);
                let station = self.channels[i].clone();
                Some(self.play(station))
            }
            Screen::Places | Screen::NowPlaying if !self.places.is_empty() => {
                let i = self.random(self.places.len());
                self.places_state.select_index(i);
                let summary = place_summary(&self.places[i]);
                Some(self.open_place(summary))
            }
            _ => None,
        }
    }

    // ----- async results --------------------------------------------------

    pub fn on_places(&mut self, result: Result<Vec<Place>, String>) {
        self.loading = None;
        match result {
            Ok(places) => self.set_places(places),
            Err(e) => self.notice = Some(Notice::Error(format!("Couldn't load places: {e}"))),
        }
    }

    pub fn on_channels(&mut self, place_id: &str, result: Result<Vec<ChannelListItem>, String>) {
        if self.current_place.as_ref().map(|p| p.id.as_str()) != Some(place_id) {
            return; // user moved on
        }
        self.loading = None;
        match result {
            Ok(channels) => {
                if channels.is_empty() {
                    self.notice = Some(Notice::Info("No stations listed here".into()));
                }
                // Keep the cursor on the station that's playing, if it's here.
                let playing_idx = self
                    .now_playing
                    .as_ref()
                    .and_then(|np| channels.iter().position(|c| c.channel_id == np.channel_id));
                self.channels = channels;
                self.channels_state.reset(self.channels.len());
                if let Some(i) = playing_idx {
                    self.channels_state.select_index(i);
                }
            }
            Err(e) => self.notice = Some(Notice::Error(format!("Couldn't load stations: {e}"))),
        }
    }

    pub fn on_beyond(&mut self, query: &str, result: Result<BeyondPage, String>) {
        if self.beyond_query.as_deref() != Some(query) {
            return; // stale
        }
        self.loading = None;
        match result {
            Ok(page) => {
                self.beyond_results = page.stations;
                self.beyond_hidden = page.hidden;
                self.beyond_collapsed = page.collapsed;
                self.beyond_filtering = page.filtering;
                self.beyond_state.reset(self.beyond_results.len());
                if self.beyond_results.is_empty() {
                    self.notice = Some(Notice::Info("Nothing beyond Radio Garden for that".into()));
                } else if self.screen == Screen::Beyond {
                    self.beyond_focus = SearchFocus::Results;
                }
            }
            Err(e) => self.notice = Some(Notice::Error(format!("Radio Browser: {e}"))),
        }
    }

    pub fn on_index_progress(&mut self, done: usize, total: usize) {
        self.index.progress = Some((done, total));
    }

    /// A new matching snapshot is in use. If the Beyond list on screen was
    /// filtered with less than this, re-run it.
    pub fn on_index_snapshot(&mut self, channels: usize, complete: bool) -> Option<Command> {
        self.index.channels = channels;
        self.index.complete = complete;
        let better = self.index.filtering() == Filtering::Filtered
            && self.beyond_filtering != Filtering::Filtered;
        if better && self.screen == Screen::Beyond && self.loading.is_none() {
            let query = self.beyond_query.clone()?;
            return Some(self.beyond_search(query));
        }
        None
    }

    pub fn on_index_error(&mut self, error: String) {
        self.index.error = Some(error);
    }

    pub fn on_search(&mut self, query: &str, result: Result<Vec<SearchHit>, String>) {
        if self.search_input.trim() != query {
            return; // stale
        }
        self.loading = None;
        match result {
            Ok(hits) => {
                self.search_query = query.to_string();
                self.search_is_remote = true;
                self.search_results = hits.iter().filter_map(search_item).collect();
                self.search_state.reset(self.search_results.len());
                if self.search_results.is_empty() {
                    self.notice = Some(Notice::Info(format!("Nothing found for \"{query}\"")));
                } else if self.screen == Screen::Search {
                    self.search_focus = SearchFocus::Results;
                }
            }
            Err(e) => self.notice = Some(Notice::Error(format!("Search failed: {e}"))),
        }
    }

    /// The stream for `request` was opened (or failed). Returns `true` if it
    /// is still wanted and should be handed to the player.
    pub fn on_stream_opened(&mut self, request: u64, result: Result<String, String>) -> bool {
        if request != self.play_request {
            return false;
        }
        match result {
            Ok(url) => {
                self.stream_url = Some(url);
                self.playback = Playback::Buffering;
                true
            }
            Err(e) => {
                self.playback = Playback::Error(e);
                false
            }
        }
    }

    pub fn on_player_started(&mut self, session: u64) {
        self.player_session = Some(session);
    }

    pub fn on_player_event(&mut self, event: PlayerEvent) {
        if self.player_session != Some(event.session) {
            return;
        }
        self.playback = match event.state {
            PlayerState::Buffer {
                seconds,
                capacity_seconds,
                kbps,
            } => {
                self.buffer = Some(BufferInfo {
                    seconds,
                    capacity_seconds,
                    kbps,
                });
                return;
            }
            PlayerState::Buffering => Playback::Buffering,
            PlayerState::Playing => {
                self.playing_since = Some(Instant::now());
                Playback::Playing
            }
            PlayerState::Ended => {
                self.buffer = None;
                Playback::Ended
            }
            PlayerState::Error(e) => {
                self.buffer = None;
                Playback::Error(e)
            }
        };
    }

    pub fn on_tick(&mut self) {
        self.tick = self.tick.wrapping_add(1);
    }
}

fn place_lon_lat(p: &Place) -> Option<(f64, f64)> {
    match p.geo.as_slice() {
        [lon, lat, ..] if lon.is_finite() && lat.is_finite() => Some((*lon, *lat)),
        _ => None,
    }
}

pub fn place_summary(p: &Place) -> PlaceSummary {
    PlaceSummary {
        id: p.id.clone(),
        title: p.title.clone(),
        subtitle: p.country.clone(),
    }
}

fn search_item(hit: &SearchHit) -> Option<SearchItem> {
    if hit.is_channel() {
        Some(SearchItem::Station(ChannelListItem::from(hit)))
    } else if hit.is_place() {
        Some(SearchItem::Place(PlaceSummary {
            id: hit.id().to_string(),
            title: hit.source.page.title.clone(),
            subtitle: hit.source.page.subtitle.clone().unwrap_or_default(),
        }))
    } else {
        None
    }
}

/// Fuzzy-match places by "title country", best matches first.
pub fn fuzzy_places<'a>(places: &'a [Place], query: &str, limit: usize) -> Vec<&'a Place> {
    let pattern = Pattern::parse(query, CaseMatching::Ignore, Normalization::Smart);
    let mut matcher = Matcher::new(Config::DEFAULT);
    let mut buf = Vec::new();
    let mut scored: Vec<(u32, &Place)> = places
        .iter()
        .filter_map(|p| {
            let hay = format!("{} {}", p.title, p.country);
            let score = pattern.score(Utf32Str::new(&hay, &mut buf), &mut matcher)?;
            Some((score, p))
        })
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| b.1.size.cmp(&a.1.size)));
    scored.into_iter().take(limit).map(|(_, p)| p).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode) -> KeyEvent {
        KeyEvent::new(code, KeyModifiers::NONE)
    }

    fn place(i: usize, title: &str, country: &str, size: u32) -> Place {
        Place {
            id: format!("id{i}"),
            title: title.into(),
            country: country.into(),
            geo: vec![0.0, 0.0],
            size,
            boost: false,
            url: format!("/visit/x/id{i}"),
        }
    }

    fn station(i: usize) -> ChannelListItem {
        ChannelListItem {
            title: format!("Station {i}"),
            channel_id: format!("ch{i}"),
            place: None,
            country: None,
            website: None,
            source: Default::default(),
        }
    }

    fn app_with_places() -> App {
        let mut app = App::new();
        app.set_places(vec![
            place(0, "Austin TX", "United States", 21),
            place(1, "London", "United Kingdom", 374),
            place(2, "Lagos", "Nigeria", 50),
        ]);
        app
    }

    fn app_on_stations(n: usize) -> App {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Enter));
        let id = app.current_place.as_ref().unwrap().id.clone();
        app.on_channels(&id, Ok((0..n).map(station).collect()));
        app
    }

    // ---- ListState ----

    #[test]
    fn list_state_wraps() {
        let mut s = ListState::new(5);
        s.previous();
        assert_eq!(s.selected, 4);
        s.next();
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn list_state_empty_is_noop() {
        let mut s = ListState::new(0);
        s.next();
        s.previous();
        s.page_down();
        s.last();
        assert_eq!(s.selected, 0);
    }

    #[test]
    fn list_state_offset_follows_selection() {
        let mut s = ListState::new(100);
        s.set_page(10);
        for _ in 0..25 {
            s.next();
        }
        assert_eq!(s.selected, 25);
        assert!(s.selected >= s.offset && s.selected < s.offset + 10);
        for _ in 0..25 {
            s.previous();
        }
        assert_eq!((s.selected, s.offset), (0, 0));
    }

    #[test]
    fn list_state_paging() {
        let mut s = ListState::new(30);
        s.set_page(10);
        s.page_down();
        assert_eq!(s.selected, 10);
        s.last();
        assert_eq!(s.selected, 29);
        assert_eq!(s.offset, 20);
        s.page_up();
        assert_eq!(s.selected, 19);
        s.first();
        assert_eq!((s.selected, s.offset), (0, 0));
    }

    // ---- places / navigation ----

    #[test]
    fn places_sorted_by_station_count() {
        let app = app_with_places();
        let titles: Vec<_> = app.places.iter().map(|p| p.title.as_str()).collect();
        assert_eq!(titles, ["London", "Lagos", "Austin TX"]);
    }

    #[test]
    fn enter_on_place_loads_channels() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Down));
        let cmd = app.handle_key(key(KeyCode::Enter));
        assert_eq!(
            cmd,
            Some(Command::LoadChannels {
                place_id: "id2".into()
            })
        );
        assert_eq!(app.screen, Screen::Stations);
        assert!(app.loading.is_some());
    }

    #[test]
    fn stale_channel_results_ignored() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Enter));
        app.on_channels("some-other-place", Ok(vec![station(1)]));
        assert!(app.channels.is_empty());
    }

    #[test]
    fn back_from_stations_keeps_place_selection() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter));
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Places);
        assert_eq!(app.places_state.selected, 1);
    }

    #[test]
    fn q_quits_outside_search_input() {
        let mut app = app_with_places();
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), Some(Command::Quit));
    }

    #[test]
    fn ctrl_c_always_quits() {
        let mut app = App::new();
        app.screen = Screen::Search;
        let k = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
        assert_eq!(app.handle_key(k), Some(Command::Quit));
    }

    #[test]
    fn help_overlay_swallows_next_key() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Char('?')));
        assert!(app.show_help);
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), None);
        assert!(!app.show_help);
    }

    // ---- playback ----

    #[test]
    fn enter_on_station_plays_and_shows_now_playing() {
        let mut app = app_on_stations(3);
        app.handle_key(key(KeyCode::Down));
        let cmd = app.handle_key(key(KeyCode::Enter));
        match cmd {
            Some(Command::Play { request, station }) => {
                assert_eq!(request, 1);
                assert_eq!(station.channel_id, "ch1");
            }
            other => panic!("expected Play, got {other:?}"),
        }
        assert_eq!(app.screen, Screen::NowPlaying);
        assert_eq!(app.playback, Playback::Connecting);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Stations, "back returns to station list");
    }

    #[test]
    fn stale_stream_open_is_dropped() {
        let mut app = app_on_stations(3);
        app.handle_key(key(KeyCode::Enter)); // request 1
        app.handle_key(key(KeyCode::Esc));
        app.handle_key(key(KeyCode::Down));
        app.handle_key(key(KeyCode::Enter)); // request 2
        assert!(!app.on_stream_opened(1, Ok("http://old".into())));
        assert!(app.on_stream_opened(2, Ok("http://new".into())));
        assert_eq!(app.stream_url.as_deref(), Some("http://new"));
    }

    #[test]
    fn player_events_update_state_for_current_session_only() {
        let mut app = app_on_stations(1);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.on_stream_opened(1, Ok("u".into())));
        app.on_player_started(7);
        app.on_player_event(PlayerEvent {
            session: 6,
            state: PlayerState::Error("old".into()),
        });
        assert_eq!(app.playback, Playback::Buffering);
        app.on_player_event(PlayerEvent {
            session: 7,
            state: PlayerState::Playing,
        });
        assert_eq!(app.playback, Playback::Playing);
    }

    #[test]
    fn station_coords_follow_its_place() {
        let mut app = App::new();
        let mut austin = place(0, "Austin TX", "United States", 21);
        austin.geo = vec![-97.74, 30.27];
        let mut lagos = place(1, "Lagos", "Nigeria", 50);
        lagos.geo = vec![3.38, 6.52];
        app.set_places(vec![austin, lagos]);
        assert_eq!(app.place_points.len(), 2);

        let mut s = station(0);
        s.place = Some(crate::api::PlaceRef {
            id: "id1".into(),
            title: "Lagos".into(),
        });
        app.now_playing = Some(s);
        assert_eq!(app.now_playing_coords(), Some((3.38, 6.52)));

        app.now_playing.as_mut().unwrap().place = None;
        assert_eq!(app.now_playing_coords(), None, "no place, none open");
        assert_eq!(
            app.selected_place_coords(),
            Some((3.38, 6.52)),
            "Lagos sorts first"
        );
    }

    #[test]
    fn buffer_reports_dont_change_playback_state() {
        let mut app = app_on_stations(1);
        app.handle_key(key(KeyCode::Enter));
        assert!(app.on_stream_opened(1, Ok("u".into())));
        app.on_player_started(3);
        app.on_player_event(PlayerEvent {
            session: 3,
            state: PlayerState::Playing,
        });
        app.on_player_event(PlayerEvent {
            session: 3,
            state: PlayerState::Buffer {
                seconds: 9.5,
                capacity_seconds: 15.0,
                kbps: 128,
            },
        });
        assert_eq!(app.playback, Playback::Playing);
        assert_eq!(app.buffer.map(|b| b.kbps), Some(128));
        app.handle_key(key(KeyCode::Char(' ')));
        assert!(app.buffer.is_none(), "cleared on stop");
    }

    #[test]
    fn space_toggles_stop_and_resume() {
        let mut app = app_on_stations(2);
        app.handle_key(key(KeyCode::Enter));
        assert_eq!(app.handle_key(key(KeyCode::Char(' '))), Some(Command::Stop));
        assert_eq!(app.playback, Playback::Stopped);
        // A stream opened for the stopped request must not start playing.
        assert!(!app.on_stream_opened(1, Ok("u".into())));
        match app.handle_key(key(KeyCode::Char(' '))) {
            Some(Command::Play { station, .. }) => assert_eq!(station.channel_id, "ch0"),
            other => panic!("expected Play, got {other:?}"),
        }
    }

    #[test]
    fn space_without_station_is_noop() {
        let mut app = app_with_places();
        assert_eq!(app.handle_key(key(KeyCode::Char(' '))), None);
    }

    #[test]
    fn stream_error_is_shown() {
        let mut app = app_on_stations(1);
        app.handle_key(key(KeyCode::Enter));
        assert!(!app.on_stream_opened(1, Err("404".into())));
        assert_eq!(app.playback, Playback::Error("404".into()));
    }

    #[test]
    fn volume_clamps_and_emits_command() {
        let mut app = App::new();
        app.volume = 0.98;
        assert_eq!(
            app.handle_key(key(KeyCode::Char('+'))),
            Some(Command::SetVolume(1.0))
        );
        app.handle_key(key(KeyCode::Char('+')));
        assert!((app.volume - 1.0).abs() < 1e-6);
        app.volume = 0.02;
        app.handle_key(key(KeyCode::Char('-')));
        assert!(app.volume.abs() < 1e-6);
    }

    #[test]
    fn random_station_plays_something() {
        let mut app = app_on_stations(5);
        assert!(matches!(
            app.handle_key(key(KeyCode::Char('r'))),
            Some(Command::Play { .. })
        ));
    }

    // ---- search ----

    #[test]
    fn typing_filters_places_locally() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Char('/')));
        for c in "lgos".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert!(!app.search_is_remote);
        match app.search_results.first() {
            Some(SearchItem::Place(p)) => assert_eq!(p.title, "Lagos"),
            other => panic!("unexpected {other:?}"),
        }
        // 'q' while typing is text, not quit.
        assert_eq!(app.handle_key(key(KeyCode::Char('q'))), None);
        assert!(app.search_input.ends_with('q'));
    }

    #[test]
    fn enter_in_search_runs_remote_search_and_focuses_results() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Char('/')));
        for c in "jazz".chars() {
            app.handle_key(key(KeyCode::Char(c)));
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Command::Search {
                query: "jazz".into()
            })
        );
        let raw = std::fs::read_to_string("tests/fixtures/search.json").unwrap();
        let hits = crate::api::client::parse_search(&serde_json::from_str(&raw).unwrap()).unwrap();
        app.on_search("jazz", Ok(hits));
        assert!(app.search_is_remote);
        assert_eq!(app.search_results.len(), 2, "country hits are skipped");
        assert_eq!(app.search_focus, SearchFocus::Results);

        // Second result is a station: Enter plays it.
        app.handle_key(key(KeyCode::Down));
        assert!(matches!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Command::Play { .. })
        ));
    }

    #[test]
    fn stale_search_results_ignored() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('a')));
        app.on_search("old query", Ok(vec![]));
        assert!(!app.search_is_remote);
    }

    #[test]
    fn esc_in_search_results_returns_to_input_then_leaves() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Char('/')));
        app.handle_key(key(KeyCode::Char('l')));
        app.handle_key(key(KeyCode::Down));
        assert_eq!(app.search_focus, SearchFocus::Results);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.search_focus, SearchFocus::Input);
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Places);
    }

    fn rb_fixture() -> Vec<RbStation> {
        let raw = std::fs::read_to_string("tests/fixtures/rb_stations.json").unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn page(filtering: Filtering) -> BeyondPage {
        BeyondPage {
            stations: rb_fixture(),
            hidden: 3,
            collapsed: 1,
            filtering,
        }
    }

    #[test]
    fn b_opens_beyond_and_fetches_top_stations_once() {
        let mut app = app_with_places();
        assert_eq!(
            app.handle_key(key(KeyCode::Char('b'))),
            Some(Command::BeyondSearch {
                query: String::new()
            })
        );
        assert_eq!(app.screen, Screen::Beyond);
        app.on_beyond("", Ok(page(Filtering::Filtered)));
        assert_eq!(app.beyond_results.len(), 4);
        assert_eq!(app.beyond_focus, SearchFocus::Results);

        app.handle_key(key(KeyCode::Esc)); // results -> input
        app.handle_key(key(KeyCode::Esc)); // input -> places
        assert_eq!(app.screen, Screen::Places);
        assert_eq!(app.handle_key(key(KeyCode::Char('b'))), None, "no refetch");
    }

    #[test]
    fn beyond_typing_and_enter_searches() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Char('b')));
        for c in "jazz cc:NG".chars() {
            assert_eq!(
                app.handle_key(key(KeyCode::Char(c))),
                None,
                "typing is text"
            );
        }
        assert_eq!(
            app.handle_key(key(KeyCode::Enter)),
            Some(Command::BeyondSearch {
                query: "jazz cc:NG".into()
            })
        );
        // The first (empty) query's late answer is ignored.
        app.on_beyond("", Ok(page(Filtering::Filtered)));
        assert!(app.beyond_results.is_empty());
    }

    #[test]
    fn enter_on_beyond_result_plays_radio_browser_stream() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Char('b')));
        app.on_beyond("", Ok(page(Filtering::Filtered)));
        match app.handle_key(key(KeyCode::Enter)) {
            Some(Command::Play { station, .. }) => assert!(matches!(
                station.source,
                crate::api::StationSource::RadioBrowser { .. }
            )),
            other => panic!("expected Play, got {other:?}"),
        }
        app.handle_key(key(KeyCode::Esc));
        assert_eq!(app.screen, Screen::Beyond, "back returns to Beyond");
    }

    #[test]
    fn finished_index_refilters_unfiltered_results() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Char('b')));
        app.on_beyond("", Ok(page(Filtering::NotFiltered)));
        assert_eq!(
            app.on_index_snapshot(500, false),
            None,
            "partial isn't better enough"
        );
        assert_eq!(
            app.on_index_snapshot(40_000, true),
            Some(Command::BeyondSearch {
                query: String::new()
            })
        );
        app.on_beyond("", Ok(page(Filtering::Filtered)));
        assert_eq!(
            app.on_index_snapshot(40_001, true),
            None,
            "already filtered"
        );
    }

    #[test]
    fn index_status_reports_filtering_and_progress() {
        let mut s = IndexStatus::default();
        assert_eq!(s.filtering(), Filtering::NotFiltered);
        s.channels = 10;
        assert_eq!(s.filtering(), Filtering::Partial);
        s.progress = Some((50, 200));
        assert_eq!(s.percent(), Some(25));
        s.complete = true;
        assert_eq!(s.filtering(), Filtering::Filtered);
        s.progress = Some((200, 200));
        assert_eq!(s.percent(), None, "nothing to show once done");
    }

    #[test]
    fn capital_i_requests_index_rebuild_from_results() {
        let mut app = app_with_places();
        app.handle_key(key(KeyCode::Char('b')));
        app.on_beyond("", Ok(page(Filtering::Filtered)));
        assert_eq!(
            app.handle_key(key(KeyCode::Char('I'))),
            Some(Command::RebuildIndex)
        );
    }

    #[test]
    fn fuzzy_places_ranks_best_match_first() {
        let places = vec![
            place(0, "Londrina", "Brazil", 5),
            place(1, "London", "United Kingdom", 374),
            place(2, "Lagos", "Nigeria", 50),
        ];
        let hits = fuzzy_places(&places, "london", 10);
        assert_eq!(hits[0].title, "London");
        assert!(hits.iter().all(|p| p.title != "Lagos"));
    }
}
