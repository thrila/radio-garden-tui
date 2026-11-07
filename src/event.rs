use std::time::Duration;

use crossterm::event::{self, Event, KeyEvent};
use tokio::sync::mpsc;

use crate::api::{ChannelListItem, LiveStream, Place, SearchHit};
use crate::audio::PlayerEvent;

/// Everything the main loop reacts to.
pub enum AppEvent {
    Key(KeyEvent),
    Resize,
    Tick,
    Places(Result<Vec<Place>, String>),
    Channels {
        place_id: String,
        result: Result<Vec<ChannelListItem>, String>,
    },
    Search {
        query: String,
        result: Result<Vec<SearchHit>, String>,
    },
    StreamOpened {
        request: u64,
        result: Result<LiveStream, String>,
    },
    Player(PlayerEvent),
}

/// Spawn the terminal input reader on its own OS thread (crossterm polling
/// is blocking) and a tick timer for animations.
pub fn spawn_input(tx: mpsc::UnboundedSender<AppEvent>, tick_rate: Duration) {
    let input_tx = tx.clone();
    std::thread::spawn(move || {
        loop {
            match event::poll(Duration::from_millis(250)) {
                Ok(true) => {
                    let ev = match event::read() {
                        Ok(Event::Key(key)) => AppEvent::Key(key),
                        Ok(Event::Resize(_, _)) => AppEvent::Resize,
                        Ok(_) => continue,
                        Err(_) => break,
                    };
                    if input_tx.send(ev).is_err() {
                        break;
                    }
                }
                Ok(false) => {
                    if input_tx.is_closed() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(tick_rate);
        loop {
            interval.tick().await;
            if tx.send(AppEvent::Tick).is_err() {
                break;
            }
        }
    });
}
