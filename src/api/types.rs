#![allow(dead_code)]

use serde::Deserialize;

#[derive(Debug, Clone, Deserialize)]
pub struct Place {
    pub id: String,
    pub geo: Vec<f64>,
    pub url: String,
    pub size: u32,
    #[serde(default)]
    pub boost: bool,
    pub title: String,
    pub country: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PlacePage {
    pub title: String,
    #[serde(default)]
    pub subtitle: String,
    pub url: String,
    #[serde(default)]
    pub map: String,
    #[serde(default)]
    pub count: u32,
    #[serde(default)]
    pub utc_offset: Option<i32>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Channel {
    pub id: String,
    pub title: String,
    pub url: String,
    #[serde(default)]
    pub website: Option<String>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub place: Option<PlaceRef>,
    #[serde(default)]
    pub country: Option<CountryRef>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PlaceRef {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CountryRef {
    pub id: String,
    pub title: String,
}

/// A single hit from `GET /search?q=...`.
#[derive(Debug, Clone, Deserialize)]
pub struct SearchHit {
    #[serde(rename = "_score", default)]
    pub score: f64,
    #[serde(rename = "_source")]
    pub source: SearchSource,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchSource {
    #[serde(default)]
    pub code: String,
    /// `"channel"`, `"place"`, `"country"`, ...
    #[serde(rename = "type")]
    pub hit_type: String,
    pub page: SearchPage,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SearchPage {
    pub url: String,
    pub title: String,
    #[serde(default)]
    pub subtitle: Option<String>,
    #[serde(default)]
    pub map: Option<String>,
    #[serde(default)]
    pub count: Option<u32>,
    #[serde(default)]
    pub place: Option<PlaceRef>,
    #[serde(default)]
    pub country: Option<CountryRef>,
}

impl SearchHit {
    pub fn is_channel(&self) -> bool {
        self.source.hit_type == "channel"
    }

    pub fn is_place(&self) -> bool {
        self.source.hit_type == "place"
    }

    /// The trailing id segment of the hit's url, e.g. `/listen/lbc/e8CURqFY` -> `e8CURqFY`.
    pub fn id(&self) -> &str {
        id_from_url(&self.source.page.url)
    }
}

/// Extract the trailing id from a Radio Garden page url
/// (`/listen/kutx-fm-98-9/vbFsCngB` -> `vbFsCngB`).
pub fn id_from_url(url: &str) -> &str {
    url.trim_end_matches('/').rsplit('/').next().unwrap_or(url)
}

/// Where a station comes from, and so how to play it.
#[derive(Debug, Clone, PartialEq, Default)]
pub enum StationSource {
    /// Played via Radio Garden's listen endpoint using `channel_id`.
    #[default]
    RadioGarden,
    /// Played directly from Radio Browser's resolved stream URL.
    RadioBrowser { uuid: String, stream_url: String },
}

impl StationSource {
    pub fn label(&self) -> &'static str {
        match self {
            StationSource::RadioGarden => "radio garden",
            StationSource::RadioBrowser { .. } => "radio browser",
        }
    }

    pub fn short(&self) -> &'static str {
        match self {
            StationSource::RadioGarden => "RG",
            StationSource::RadioBrowser { .. } => "RB",
        }
    }
}

/// A station as shown in lists (place channel lists, search results) and
/// handed to the player.
#[derive(Debug, Clone, PartialEq)]
pub struct ChannelListItem {
    pub title: String,
    pub channel_id: String,
    pub place: Option<PlaceRef>,
    pub country: Option<CountryRef>,
    pub website: Option<String>,
    pub source: StationSource,
}

impl ChannelListItem {
    pub fn location(&self) -> String {
        match (&self.place, &self.country) {
            (Some(p), Some(c)) => format!("{}, {}", p.title, c.title),
            (Some(p), None) => p.title.clone(),
            (None, Some(c)) => c.title.clone(),
            (None, None) => String::new(),
        }
    }
}

impl From<&SearchHit> for ChannelListItem {
    fn from(hit: &SearchHit) -> Self {
        Self {
            title: hit.source.page.title.clone(),
            channel_id: hit.id().to_string(),
            place: hit.source.page.place.clone(),
            country: hit.source.page.country.clone(),
            website: None,
            source: StationSource::RadioGarden,
        }
    }
}
