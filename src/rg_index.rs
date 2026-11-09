//! Background builder for the local Radio Garden channel index used to hide
//! Radio Garden stations from Radio Browser results.
//!
//! Radio Garden has no "all stations" endpoint, so this fetches the channel
//! list of every place (~13k requests). It runs as a background task with a
//! small worker pool and a delay between requests, stores progress per place
//! in SQLite (so an interrupted build resumes), and refreshes after a week.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tokio::sync::mpsc;

use crate::api::{ApiClient, Place};
use crate::cache::{Cache, IndexedChannel};
use crate::countries::country_code;
use crate::dedupe::{DedupeIndex, IndexEntry, normalize};
use crate::error::RadioError;

/// Requests in flight at once.
pub const WORKERS: usize = 6;
/// Pause each worker takes after every request.
pub const REQUEST_DELAY: Duration = Duration::from_millis(400);
/// Rebuild the index when the last complete build is older than this.
pub const MAX_AGE: Duration = Duration::from_secs(7 * 24 * 60 * 60);
/// Publish a fresh matching snapshot every this many places.
const SNAPSHOT_EVERY: usize = 1000;
const PROGRESS_EVERY: usize = 10;

#[derive(Debug, Clone)]
pub struct IndexOptions {
    /// Also resolve each channel's stream host (one extra request per
    /// channel, roughly doubling the work). Off by default.
    pub resolve_hosts: bool,
    /// Rebuild even if the index is fresh.
    pub force: bool,
}

impl IndexOptions {
    pub fn from_env(force: bool) -> Self {
        Self {
            resolve_hosts: std::env::var("RADIO_TUI_RESOLVE_HOSTS")
                .is_ok_and(|v| v == "1" || v.eq_ignore_ascii_case("true")),
            force,
        }
    }
}

pub enum IndexEvent {
    Progress {
        done: usize,
        total: usize,
    },
    /// A matcher built from everything indexed so far. `complete` means every
    /// place has been fetched (now or in the last build).
    Snapshot {
        index: Arc<DedupeIndex>,
        complete: bool,
    },
    Failed(String),
}

/// The subset of a place the builder needs.
#[derive(Debug, Clone)]
pub struct PlaceRef {
    pub id: String,
    pub title: String,
    pub country: String,
}

impl From<&Place> for PlaceRef {
    fn from(p: &Place) -> Self {
        Self {
            id: p.id.clone(),
            title: p.title.clone(),
            country: p.country.clone(),
        }
    }
}

/// Build a matcher from stored channels.
pub fn snapshot(channels: &[IndexedChannel]) -> DedupeIndex {
    DedupeIndex::build(channels.iter().map(|c| IndexEntry {
        title: &c.title,
        place_title: &c.place_title,
        country_code: country_code(&c.country),
        host: c.host.as_deref(),
    }))
}

fn load_snapshot(cache: &Cache) -> Option<Arc<DedupeIndex>> {
    let channels = cache.indexed_channels().ok()?;
    (!channels.is_empty()).then(|| Arc::new(snapshot(&channels)))
}

type FetchResult = (PlaceRef, Result<Vec<IndexedChannel>, String>);

fn is_not_found(e: &RadioError) -> bool {
    matches!(e, RadioError::Http(err) if err.status() == Some(reqwest::StatusCode::NOT_FOUND))
}

async fn fetch_place(
    api: &ApiClient,
    place: &PlaceRef,
    resolve_hosts: bool,
) -> Result<Vec<IndexedChannel>, String> {
    let channels = match api.get_place_channels_for_index(&place.id).await {
        Ok(c) => c,
        // The place was removed from Radio Garden since our place list was
        // saved: it has no channels, which is a result, not a failure.
        Err(e) if is_not_found(&e) => Vec::new(),
        Err(e) => return Err(e.to_string()),
    };
    let mut out = Vec::with_capacity(channels.len());
    for c in channels {
        let host = if resolve_hosts {
            tokio::time::sleep(REQUEST_DELAY).await;
            api.stream_host(&c.channel_id).await.ok().flatten()
        } else {
            None
        };
        out.push(IndexedChannel {
            norm: normalize(&c.title),
            channel_id: c.channel_id,
            title: c.title,
            place_title: place.title.clone(),
            country: place.country.clone(),
            host,
        });
    }
    Ok(out)
}

/// Build (or resume, or refresh) the index. Sends progress and snapshots on
/// `events`; returns when done or when `events` is closed.
pub async fn run(
    api: Arc<ApiClient>,
    cache_path: PathBuf,
    places: Vec<PlaceRef>,
    options: IndexOptions,
    events: mpsc::UnboundedSender<IndexEvent>,
) {
    let cache = match Cache::new(&cache_path) {
        Ok(c) => c,
        Err(e) => {
            let _ = events.send(IndexEvent::Failed(e.to_string()));
            return;
        }
    };

    let fresh = cache.index_age().is_some_and(|age| age < MAX_AGE);
    let had_complete = cache.index_age().is_some();
    if (options.force || (had_complete && !fresh))
        && let Err(e) = cache.reset_index_progress()
    {
        let _ = events.send(IndexEvent::Failed(e.to_string()));
        return;
    }

    // Whatever is already indexed is usable right away.
    if let Some(index) = tokio::task::block_in_place(|| load_snapshot(&cache)) {
        let complete = had_complete;
        let _ = events.send(IndexEvent::Snapshot { index, complete });
    }
    if fresh && !options.force {
        return;
    }

    let done_ids = cache.indexed_place_ids().unwrap_or_default();
    let total = places.len();
    let pending: VecDeque<PlaceRef> = places
        .into_iter()
        .filter(|p| !done_ids.contains(&p.id))
        .collect();
    let mut done = total - pending.len();
    let _ = events.send(IndexEvent::Progress { done, total });

    let queue = Arc::new(Mutex::new(pending));
    let (results_tx, mut results) = mpsc::channel::<FetchResult>(WORKERS * 2);
    for _ in 0..WORKERS {
        let (api, queue, tx) = (api.clone(), queue.clone(), results_tx.clone());
        let resolve_hosts = options.resolve_hosts;
        tokio::spawn(async move {
            loop {
                let next = queue.lock().ok().and_then(|mut q| q.pop_front());
                let Some(place) = next else { break };
                let result = fetch_place(&api, &place, resolve_hosts).await;
                if tx.send((place, result)).await.is_err() {
                    break; // builder stopped
                }
                tokio::time::sleep(REQUEST_DELAY).await;
            }
        });
    }
    drop(results_tx);

    let mut failures = 0usize;
    let mut since_snapshot = 0usize;
    while let Some((place, result)) = results.recv().await {
        match result {
            Ok(channels) => {
                if let Err(e) = cache.save_place_channels(&place.id, &channels) {
                    let _ = events.send(IndexEvent::Failed(e.to_string()));
                    return;
                }
                done += 1;
                since_snapshot += 1;
            }
            // Left unmarked: retried on the next run.
            Err(_) => failures += 1,
        }
        if events.is_closed() {
            return;
        }
        if done.is_multiple_of(PROGRESS_EVERY) {
            let _ = events.send(IndexEvent::Progress { done, total });
        }
        if since_snapshot >= SNAPSHOT_EVERY {
            since_snapshot = 0;
            if let Some(index) = tokio::task::block_in_place(|| load_snapshot(&cache)) {
                let complete = had_complete;
                let _ = events.send(IndexEvent::Snapshot { index, complete });
            }
        }
    }

    let _ = events.send(IndexEvent::Progress { done, total });
    let complete = failures == 0;
    if complete {
        let _ = cache.mark_index_complete();
    }
    if let Some(index) = tokio::task::block_in_place(|| load_snapshot(&cache)) {
        let complete = complete || had_complete;
        let _ = events.send(IndexEvent::Snapshot { index, complete });
    }
    if failures > 0 {
        let _ = events.send(IndexEvent::Failed(format!(
            "{failures} places failed to index; they'll be retried next run"
        )));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn snapshot_maps_country_names_to_codes() {
        let chans = vec![IndexedChannel {
            channel_id: "2m0QRgcW".into(),
            title: "KEXP 90.3 FM".into(),
            place_title: "Seattle WA".into(),
            country: "United States".into(),
            norm: "kexp".into(),
            host: None,
        }];
        let idx = snapshot(&chans);
        assert_eq!(idx.channel_count(), 1);
        let hit = idx.find(&crate::dedupe::Candidate {
            name: "KEXP 90.3 Seattle, WA",
            country_code: "US",
            stream_url: "http://live-aacplus-64.kexp.org/kexp64.aac",
        });
        assert!(hit.is_some());
    }

    #[test]
    fn host_resolution_is_off_unless_asked() {
        // Only meaningful when the variable is not set in the environment.
        if std::env::var_os("RADIO_TUI_RESOLVE_HOSTS").is_none() {
            assert!(!IndexOptions::from_env(false).resolve_hosts);
        }
    }
}
