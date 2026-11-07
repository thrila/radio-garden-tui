use std::time::{Duration, Instant};

use reqwest::{Client, StatusCode, redirect};
use serde_json::Value;
use tokio::sync::{Mutex, mpsc};

use super::types::*;
use crate::error::{RadioError, Result};

const BASE_URL: &str = "https://radio.garden/api";
const USER_AGENT: &str = "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/137.0.0.0 Safari/537.36";

/// Radio Garden (and the stream hosts it points at) can chain several
/// redirects: listen endpoint -> station CDN -> edge node -> ...
const MAX_REDIRECTS: usize = 10;
/// Playlists (.pls/.m3u) can point at other playlists; cap the nesting.
const MAX_PLAYLIST_HOPS: usize = 4;
/// Minimum spacing between requests to radio.garden so we never hammer it.
const MIN_REQUEST_INTERVAL: Duration = Duration::from_millis(250);
const JSON_TIMEOUT: Duration = Duration::from_secs(20);
const MAX_RETRIES: u32 = 3;
/// Number of network chunks buffered between the HTTP reader and the decoder.
const STREAM_CHANNEL_CAPACITY: usize = 128;
const MAX_PLAYLIST_BYTES: usize = 64 * 1024;

/// An open, live audio stream. Bytes arrive on `chunks` until the server
/// closes the connection or the receiver is dropped (which cancels the read).
pub struct LiveStream {
    /// Final URL after following every redirect / playlist hop.
    pub url: String,
    pub content_type: Option<String>,
    pub chunks: mpsc::Receiver<Vec<u8>>,
}

pub struct ApiClient {
    client: Client,
    last_request: Mutex<Option<Instant>>,
}

impl ApiClient {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .redirect(redirect::Policy::limited(MAX_REDIRECTS))
            .connect_timeout(Duration::from_secs(10))
            // Per-read timeout (not total), so long-lived streams keep going
            // but a stalled server is detected.
            .read_timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            client,
            last_request: Mutex::new(None),
        })
    }

    /// Wait so consecutive radio.garden requests are spaced out.
    async fn pace(&self) {
        let mut last = self.last_request.lock().await;
        if let Some(prev) = *last {
            let elapsed = prev.elapsed();
            if elapsed < MIN_REQUEST_INTERVAL {
                tokio::time::sleep(MIN_REQUEST_INTERVAL - elapsed).await;
            }
        }
        *last = Some(Instant::now());
    }

    async fn get_json(&self, path: &str, query: &[(&str, &str)]) -> Result<Value> {
        let url = format!("{BASE_URL}/{path}");
        let mut attempt = 0;
        loop {
            self.pace().await;
            let result = self
                .client
                .get(&url)
                .query(query)
                .timeout(JSON_TIMEOUT)
                .send()
                .await;

            let retryable = match &result {
                Ok(resp) => {
                    resp.status() == StatusCode::TOO_MANY_REQUESTS
                        || resp.status().is_server_error()
                }
                Err(e) => e.is_timeout() || e.is_connect(),
            };
            if retryable && attempt + 1 < MAX_RETRIES {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
                continue;
            }

            let resp = result?.error_for_status()?;
            return Ok(resp.json().await?);
        }
    }

    pub async fn get_all_places(&self) -> Result<Vec<Place>> {
        let raw = self.get_json("ara/content/places", &[]).await?;
        parse_places(&raw)
    }

    pub async fn get_place_channels(&self, place_id: &str) -> Result<Vec<ChannelListItem>> {
        let raw = self
            .get_json(&format!("ara/content/page/{place_id}/channels"), &[])
            .await?;
        Ok(parse_channel_list(&raw["data"]))
    }

    pub async fn search(&self, query: &str) -> Result<Vec<SearchHit>> {
        let raw = self.get_json("search", &[("q", query)]).await?;
        parse_search(&raw)
    }

    pub fn listen_url(channel_id: &str) -> String {
        format!("{BASE_URL}/ara/content/listen/{channel_id}/channel.mp3")
    }

    /// Open the live stream for a channel, following the whole redirect
    /// chain (and any .pls/.m3u playlist indirection) to the actual audio.
    pub async fn open_stream(&self, channel_id: &str) -> Result<LiveStream> {
        let mut url = Self::listen_url(channel_id);
        self.pace().await;

        for _ in 0..MAX_PLAYLIST_HOPS {
            let resp = self.client.get(&url).send().await?;
            if resp.status() == StatusCode::NOT_FOUND {
                return Err(RadioError::StreamNotFound {
                    channel_id: channel_id.to_string(),
                });
            }
            let mut resp = resp.error_for_status()?;
            let final_url = resp.url().to_string();
            let content_type = resp
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|v| v.to_str().ok())
                .map(|s| s.split(';').next().unwrap_or(s).trim().to_ascii_lowercase());

            match classify(content_type.as_deref(), &final_url) {
                StreamKind::Playlist => {
                    let mut body = Vec::new();
                    while let Some(chunk) = resp.chunk().await? {
                        body.extend_from_slice(&chunk);
                        if body.len() > MAX_PLAYLIST_BYTES {
                            break;
                        }
                    }
                    let text = String::from_utf8_lossy(&body);
                    if text.contains("#EXT-X-") {
                        return Err(RadioError::UnsupportedStream("HLS playlist".into()));
                    }
                    url = parse_playlist(&text)
                        .ok_or_else(|| RadioError::UnsupportedStream("empty playlist".into()))?;
                    continue;
                }
                StreamKind::Html => {
                    return Err(RadioError::UnsupportedStream(
                        "station links to a web page, not audio".into(),
                    ));
                }
                StreamKind::Audio => {
                    let (tx, rx) = mpsc::channel(STREAM_CHANNEL_CAPACITY);
                    tokio::spawn(async move {
                        // Ends when the server closes, a read fails, or the
                        // player drops the receiver (stop / station change).
                        while let Ok(Some(chunk)) = resp.chunk().await {
                            if tx.send(chunk.to_vec()).await.is_err() {
                                break;
                            }
                        }
                    });
                    return Ok(LiveStream {
                        url: final_url,
                        content_type,
                        chunks: rx,
                    });
                }
            }
        }

        Err(RadioError::UnsupportedStream(
            "too many nested playlists".into(),
        ))
    }
}

#[derive(Debug, PartialEq)]
enum StreamKind {
    Audio,
    Playlist,
    Html,
}

fn classify(content_type: Option<&str>, url: &str) -> StreamKind {
    let path = url
        .split(['?', '#'])
        .next()
        .unwrap_or(url)
        .to_ascii_lowercase();
    match content_type {
        Some(
            "audio/x-scpls"
            | "audio/x-mpegurl"
            | "audio/mpegurl"
            | "application/x-mpegurl"
            | "application/vnd.apple.mpegurl"
            | "application/pls+xml",
        ) => StreamKind::Playlist,
        Some("text/html") => StreamKind::Html,
        _ if path.ends_with(".pls") || path.ends_with(".m3u") || path.ends_with(".m3u8") => {
            StreamKind::Playlist
        }
        _ => StreamKind::Audio,
    }
}

/// Pull the first stream URL out of a PLS or (non-HLS) M3U playlist.
pub(crate) fn parse_playlist(body: &str) -> Option<String> {
    body.lines()
        .map(str::trim)
        .filter_map(|line| {
            if line.starts_with('#') || line.starts_with('[') {
                return None;
            }
            // PLS: File1=http://...
            let candidate = match line.split_once('=') {
                Some((key, value)) if key.to_ascii_lowercase().starts_with("file") => value,
                _ => line,
            };
            let candidate = candidate.trim();
            (candidate.starts_with("http://") || candidate.starts_with("https://"))
                .then(|| candidate.to_string())
        })
        .next()
}

pub(crate) fn parse_places(raw: &Value) -> Result<Vec<Place>> {
    let list = raw["data"]["list"]
        .as_array()
        .ok_or_else(|| RadioError::Api {
            message: "Invalid places response".into(),
        })?;
    // Skip individual malformed entries instead of failing the whole list.
    Ok(list
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect())
}

pub(crate) fn parse_channel_list(data: &Value) -> Vec<ChannelListItem> {
    let Some(sections) = data["content"].as_array() else {
        return Vec::new();
    };
    sections
        .iter()
        .filter_map(|s| s["items"].as_array())
        .flatten()
        .filter_map(|item| {
            let page = &item["page"];
            let title = page["title"].as_str()?;
            let url = page["url"].as_str()?;
            let place = serde_json::from_value(page["place"].clone()).ok();
            let country = serde_json::from_value(page["country"].clone()).ok();
            Some(ChannelListItem {
                title: title.to_string(),
                channel_id: id_from_url(url).to_string(),
                place,
                country,
                website: page["website"].as_str().map(str::to_string),
            })
        })
        .collect()
}

pub(crate) fn parse_search(raw: &Value) -> Result<Vec<SearchHit>> {
    let hits = raw["hits"]["hits"]
        .as_array()
        .ok_or_else(|| RadioError::Api {
            message: "Invalid search response".into(),
        })?;
    Ok(hits
        .iter()
        .filter_map(|v| serde_json::from_value(v.clone()).ok())
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn playlist_pls() {
        let body = "[playlist]\nNumberOfEntries=1\nFile1=http://example.com:8000/live\nTitle1=X\n";
        assert_eq!(
            parse_playlist(body).as_deref(),
            Some("http://example.com:8000/live")
        );
    }

    #[test]
    fn playlist_m3u() {
        let body = "#EXTM3U\n#EXTINF:-1,Station\nhttps://example.com/stream.mp3\n";
        assert_eq!(
            parse_playlist(body).as_deref(),
            Some("https://example.com/stream.mp3")
        );
    }

    #[test]
    fn playlist_empty() {
        assert_eq!(parse_playlist("#EXTM3U\n"), None);
    }

    #[test]
    fn classify_streams() {
        assert_eq!(
            classify(Some("audio/mpeg"), "https://a/b"),
            StreamKind::Audio
        );
        assert_eq!(
            classify(Some("audio/aac"), "https://a/b"),
            StreamKind::Audio
        );
        assert_eq!(classify(None, "https://a/b.pls"), StreamKind::Playlist);
        assert_eq!(
            classify(Some("audio/x-mpegurl"), "https://a/b"),
            StreamKind::Playlist
        );
        assert_eq!(
            classify(Some("application/octet-stream"), "https://a/live.m3u?x=1"),
            StreamKind::Playlist
        );
        assert_eq!(classify(Some("text/html"), "https://a/"), StreamKind::Html);
    }
}
