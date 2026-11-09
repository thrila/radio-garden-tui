//! Client for Radio Browser (radio-browser.info), the second station source.
//!
//! Etiquette Radio Browser asks for, all handled here:
//! - discover servers via `all.api.radio-browser.info` and spread load
//!   across them at random,
//! - send a descriptive User-Agent,
//! - register a click (`/json/url/{uuid}`) when a station is played.

use std::time::Duration;

use reqwest::Client;
use serde::Deserialize;
use tokio::sync::Mutex;

use super::types::{ChannelListItem, CountryRef, StationSource};
use crate::error::{RadioError, Result};

const USER_AGENT: &str = concat!(
    "radio_tui/",
    env!("CARGO_PKG_VERSION"),
    " (+https://github.com/thrila/radio-garden-tui)"
);
const DISCOVERY_HOST: &str = "all.api.radio-browser.info";
const FALLBACK_SERVER: &str = "de1.api.radio-browser.info";
/// Servers tried per request before giving up.
const MAX_SERVER_ATTEMPTS: usize = 3;
pub const PAGE_SIZE: u32 = 100;

#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct RbStation {
    pub stationuuid: String,
    pub name: String,
    #[serde(default)]
    pub url_resolved: String,
    #[serde(default)]
    pub homepage: String,
    #[serde(default)]
    pub favicon: String,
    #[serde(default)]
    pub country: String,
    #[serde(default)]
    pub countrycode: String,
    #[serde(default)]
    pub state: String,
    #[serde(default)]
    pub tags: String,
    #[serde(default)]
    pub codec: String,
    #[serde(default)]
    pub bitrate: u32,
    #[serde(default)]
    pub votes: i64,
    #[serde(default)]
    pub lastcheckok: u8,
    #[serde(default)]
    pub hls: u8,
}

impl RbStation {
    /// Our player handles plain HTTP audio and .pls/.m3u, but not HLS.
    pub fn is_playable(&self) -> bool {
        let url = self.url_resolved.to_ascii_lowercase();
        !url.is_empty()
            && (url.starts_with("http://") || url.starts_with("https://"))
            && self.hls == 0
            && !url.split('?').next().unwrap_or(&url).ends_with(".m3u8")
    }

    pub fn tag_list(&self) -> impl Iterator<Item = &str> {
        self.tags
            .split(',')
            .map(str::trim)
            .filter(|t| !t.is_empty())
    }

    pub fn to_station(&self) -> ChannelListItem {
        ChannelListItem {
            title: self.name.trim().to_string(),
            channel_id: self.stationuuid.clone(),
            place: None,
            country: (!self.country.is_empty()).then(|| CountryRef {
                id: self.countrycode.clone(),
                title: self.country.clone(),
            }),
            website: (!self.homepage.is_empty()).then(|| self.homepage.clone()),
            source: StationSource::RadioBrowser {
                uuid: self.stationuuid.clone(),
                stream_url: self.url_resolved.clone(),
            },
        }
    }
}

/// Filters for `/json/stations/search`.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RbQuery {
    pub name: Option<String>,
    pub tag: Option<String>,
    pub countrycode: Option<String>,
    pub codec: Option<String>,
    pub bitrate_min: Option<u32>,
    pub limit: u32,
    pub offset: u32,
}

impl RbQuery {
    /// Parse the Beyond-screen search box: free text is the name, plus
    /// `tag:jazz`, `cc:NG` / `country:NG`, `codec:mp3`, `br:128`.
    pub fn parse(input: &str) -> Self {
        let mut q = RbQuery {
            limit: PAGE_SIZE,
            ..Default::default()
        };
        let mut name = Vec::new();
        for word in input.split_whitespace() {
            let (key, value) = word.split_once(':').unwrap_or(("", word));
            let value = value.trim();
            match key.to_ascii_lowercase().as_str() {
                "tag" if !value.is_empty() => q.tag = Some(value.replace('_', " ")),
                "cc" | "country" if value.len() == 2 => {
                    q.countrycode = Some(value.to_ascii_uppercase())
                }
                "codec" if !value.is_empty() => q.codec = Some(value.to_ascii_lowercase()),
                "br" | "bitrate" => q.bitrate_min = value.parse().ok(),
                _ => name.push(word),
            }
        }
        if !name.is_empty() {
            q.name = Some(name.join(" "));
        }
        q
    }

    fn params(&self) -> Vec<(&'static str, String)> {
        let mut p = vec![
            ("hidebroken", "true".to_string()),
            ("order", "votes".to_string()),
            ("reverse", "true".to_string()),
            ("limit", self.limit.to_string()),
            ("offset", self.offset.to_string()),
        ];
        if let Some(v) = &self.name {
            p.push(("name", v.clone()));
        }
        if let Some(v) = &self.tag {
            p.push(("tag", v.clone()));
        }
        if let Some(v) = &self.countrycode {
            p.push(("countrycode", v.clone()));
        }
        if let Some(v) = &self.codec {
            p.push(("codec", v.clone()));
        }
        if let Some(v) = self.bitrate_min {
            p.push(("bitrateMin", v.to_string()));
        }
        p
    }
}

#[derive(Deserialize)]
struct ServerEntry {
    name: String,
}

/// Server names from `/json/servers`, deduplicated (it lists one entry per
/// IP, so IPv4 + IPv6 of the same host appear twice).
fn parse_servers(raw: &[u8]) -> Vec<String> {
    let entries: Vec<ServerEntry> = serde_json::from_slice(raw).unwrap_or_default();
    let mut names: Vec<String> = entries
        .into_iter()
        .map(|e| e.name.trim().to_ascii_lowercase())
        .filter(|n| n.ends_with(".radio-browser.info"))
        .collect();
    names.sort();
    names.dedup();
    names
}

fn shuffle<T>(items: &mut [T]) {
    let mut seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0x9E37_79B9_7F4A_7C15)
        | 1;
    for i in (1..items.len()).rev() {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        items.swap(i, (seed % (i as u64 + 1)) as usize);
    }
}

pub struct RadioBrowserClient {
    client: Client,
    servers: Mutex<Vec<String>>,
}

impl RadioBrowserClient {
    pub fn new() -> Result<Self> {
        let client = Client::builder()
            .user_agent(USER_AGENT)
            .connect_timeout(Duration::from_secs(8))
            .timeout(Duration::from_secs(20))
            .build()?;
        Ok(Self {
            client,
            servers: Mutex::new(Vec::new()),
        })
    }

    /// Server hostnames in random order. Resolves `all.api.radio-browser.info`
    /// and asks it for `/json/servers` (hostnames are needed for HTTPS, and
    /// there's no reverse DNS in std/tokio). Falls back to `de1`.
    async fn servers(&self) -> Vec<String> {
        let mut cached = self.servers.lock().await;
        if cached.is_empty() {
            let resolved = tokio::net::lookup_host((DISCOVERY_HOST, 443))
                .await
                .map(|addrs| addrs.count() > 0)
                .unwrap_or(false);
            let mut names = Vec::new();
            if resolved
                && let Ok(resp) = self
                    .client
                    .get(format!("https://{DISCOVERY_HOST}/json/servers"))
                    .send()
                    .await
                && let Ok(body) = resp.bytes().await
            {
                names = parse_servers(&body);
            }
            if names.is_empty() {
                names.push(FALLBACK_SERVER.to_string());
            }
            *cached = names;
        }
        let mut order = cached.clone();
        shuffle(&mut order);
        order
    }

    /// GET `path` on a random server, retrying on others if one fails.
    async fn get(&self, path: &str, params: &[(&str, String)]) -> Result<reqwest::Response> {
        let mut last_err = None;
        for server in self.servers().await.into_iter().take(MAX_SERVER_ATTEMPTS) {
            let result = self
                .client
                .get(format!("https://{server}{path}"))
                .query(params)
                .send()
                .await
                .and_then(|r| r.error_for_status());
            match result {
                Ok(resp) => return Ok(resp),
                Err(e) => last_err = Some(e),
            }
        }
        Err(match last_err {
            Some(e) => RadioError::Http(e),
            None => RadioError::Api {
                message: "no Radio Browser server reachable".into(),
            },
        })
    }

    pub async fn search(&self, query: &RbQuery) -> Result<Vec<RbStation>> {
        let resp = self.get("/json/stations/search", &query.params()).await?;
        Ok(resp.json().await?)
    }

    /// Radio Browser's click counter: call once when a station is played.
    pub async fn count_click(&self, uuid: &str) -> Result<()> {
        self.get(&format!("/json/url/{uuid}"), &[]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> Vec<RbStation> {
        let raw = std::fs::read_to_string("tests/fixtures/rb_stations.json").unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn deserializes_live_shape() {
        let stations = fixture();
        assert_eq!(stations.len(), 4);
        let kexp = &stations[0];
        assert_eq!(kexp.name, "KEXP 90.3 Seattle, WA");
        assert_eq!(kexp.countrycode, "US");
        assert!(kexp.url_resolved.starts_with("http"));
        assert!(kexp.votes > 0);
        assert_eq!(kexp.lastcheckok, 1);
        assert!(kexp.tag_list().any(|t| t == "indie"));
        assert!(kexp.is_playable());
    }

    #[test]
    fn converts_to_playable_station() {
        let s = fixture()[0].to_station();
        assert_eq!(s.title, "KEXP 90.3 Seattle, WA");
        assert_eq!(s.location(), "The United States Of America");
        match s.source {
            StationSource::RadioBrowser { uuid, stream_url } => {
                assert_eq!(uuid, s.channel_id);
                assert!(stream_url.starts_with("http"));
            }
            other => panic!("unexpected source {other:?}"),
        }
    }

    #[test]
    fn hls_and_empty_urls_are_not_playable() {
        let mut s = fixture()[0].clone();
        s.hls = 1;
        assert!(!s.is_playable());
        s.hls = 0;
        s.url_resolved = "https://x/live.m3u8?token=1".into();
        assert!(!s.is_playable());
        s.url_resolved = String::new();
        assert!(!s.is_playable());
    }

    #[test]
    fn parses_query_box() {
        let q = RbQuery::parse("jazz cafe tag:smooth_jazz cc:ng codec:MP3 br:128");
        assert_eq!(q.name.as_deref(), Some("jazz cafe"));
        assert_eq!(q.tag.as_deref(), Some("smooth jazz"));
        assert_eq!(q.countrycode.as_deref(), Some("NG"));
        assert_eq!(q.codec.as_deref(), Some("mp3"));
        assert_eq!(q.bitrate_min, Some(128));
        assert_eq!(q.limit, PAGE_SIZE);

        let empty = RbQuery::parse("   ");
        assert_eq!(empty.name, None);
        // Malformed filters are treated as plain text, never dropped silently.
        let odd = RbQuery::parse("cc:nigeria");
        assert_eq!(odd.countrycode, None);
        assert_eq!(odd.name.as_deref(), Some("cc:nigeria"));
    }

    #[test]
    fn query_params_include_required_flags() {
        let p = RbQuery::parse("kexp br:64").params();
        let has = |k: &str, v: &str| p.iter().any(|(pk, pv)| *pk == k && pv == v);
        assert!(has("hidebroken", "true"));
        assert!(has("order", "votes"));
        assert!(has("reverse", "true"));
        assert!(has("name", "kexp"));
        assert!(has("bitrateMin", "64"));
    }

    #[test]
    fn parses_server_list() {
        let raw = std::fs::read("tests/fixtures/rb_servers.json").unwrap();
        assert_eq!(parse_servers(&raw), vec!["de1.api.radio-browser.info"]);
        assert!(parse_servers(b"not json").is_empty());
        assert!(parse_servers(br#"[{"ip":"1.2.3.4","name":"evil.example.com"}]"#).is_empty());
    }

    #[test]
    fn user_agent_names_app_version_and_repo() {
        assert!(USER_AGENT.starts_with("radio_tui/"));
        assert!(USER_AGENT.contains("github.com/thrila/radio-garden-tui"));
    }

    #[test]
    fn shuffle_keeps_items() {
        let mut v: Vec<u32> = (0..20).collect();
        shuffle(&mut v);
        let mut sorted = v.clone();
        sorted.sort();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>());
    }
}
