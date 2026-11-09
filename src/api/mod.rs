pub mod client;
pub mod radio_browser;
pub mod types;

pub use client::{ApiClient, LiveStream};
pub use radio_browser::{RadioBrowserClient, RbQuery, RbStation};
pub use types::*;

#[cfg(test)]
mod tests {
    use super::client::{parse_channel_list, parse_places, parse_search};
    use super::types::*;
    use serde_json::Value;

    fn fixture(name: &str) -> Value {
        let path = format!("tests/fixtures/{}", name);
        let raw = std::fs::read_to_string(&path)
            .unwrap_or_else(|e| panic!("Failed to read {}: {}", path, e));
        serde_json::from_str(&raw).unwrap()
    }

    #[test]
    fn deserialize_places() {
        let places = parse_places(&fixture("places.json")).unwrap();
        assert_eq!(places.len(), 3);

        let austin = &places[0];
        assert_eq!(austin.id, "Aq7xeIiB");
        assert_eq!(austin.title, "Austin TX");
        assert_eq!(austin.country, "United States");
        assert_eq!(austin.size, 21);
        assert!(austin.boost);
        assert_eq!(austin.geo.len(), 2);
    }

    #[test]
    fn parse_channels_from_page() {
        let json = fixture("channels.json");
        let channels = parse_channel_list(&json["data"]);
        assert_eq!(channels.len(), 3);
        assert_eq!(channels[0].title, "KUTX FM 98.9");
        assert_eq!(channels[0].channel_id, "vbFsCngB");
        assert_eq!(channels[0].website.as_deref(), Some("https://kutx.org"));
    }

    #[test]
    fn parse_channels_missing_content_is_empty() {
        let v: Value = serde_json::json!({"title": "x"});
        assert!(parse_channel_list(&v).is_empty());
    }

    #[test]
    fn extract_channel_id_from_href() {
        assert_eq!(id_from_url("/listen/kutx-fm-98-9/vbFsCngB"), "vbFsCngB");
        assert_eq!(id_from_url("/listen/kut-90-5-fm/mfnMeV9N/"), "mfnMeV9N");
        assert_eq!(id_from_url("bare_id"), "bare_id");
    }

    #[test]
    fn deserialize_channel_detail() {
        let json = fixture("channel.json");
        let channel: Channel = serde_json::from_value(json["data"].clone()).unwrap();
        assert_eq!(channel.id, "vbFsCngB");
        assert_eq!(channel.title, "KUTX FM 98.9");
        assert!(channel.secure);

        let place = channel.place.as_ref().unwrap();
        assert_eq!(place.id, "Aq7xeIiB");
        assert_eq!(place.title, "Austin TX");

        let country = channel.country.as_ref().unwrap();
        assert_eq!(country.id, "GhDXw4EW");
        assert_eq!(country.title, "United States");
    }

    #[test]
    fn deserialize_search_hits() {
        let hits = parse_search(&fixture("search.json")).unwrap();
        assert_eq!(hits.len(), 3);

        assert!(hits[0].is_place());
        assert_eq!(hits[0].id(), "0eZoYyEW");
        assert_eq!(hits[0].source.page.title, "London");

        assert!(hits[1].is_channel());
        assert_eq!(hits[1].id(), "e8CURqFY");
        let item = ChannelListItem::from(&hits[1]);
        assert_eq!(item.title, "LBC London");
        assert_eq!(item.location(), "London, United Kingdom");

        assert!(!hits[2].is_channel() && !hits[2].is_place());
    }

    #[test]
    fn listen_url_construction() {
        assert_eq!(
            super::ApiClient::listen_url("vbFsCngB"),
            "https://radio.garden/api/ara/content/listen/vbFsCngB/channel.mp3"
        );
    }
}
