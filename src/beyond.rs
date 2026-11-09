//! "Beyond Radio Garden": Radio Browser results with stations that are
//! already on Radio Garden removed. Pure and synchronous.

use crate::api::RbStation;
use crate::dedupe::{Candidate, DedupeIndex, collapse, normalize};

#[derive(Debug, Default)]
pub struct BeyondResult {
    pub stations: Vec<RbStation>,
    /// Removed because they're on Radio Garden.
    pub hidden: usize,
    /// Removed as Radio Browser duplicates of each other.
    pub collapsed: usize,
}

/// Drop unplayable entries, collapse Radio Browser's own duplicates (keeping
/// a working entry with the most votes), then remove stations found in the
/// Radio Garden index (if there is one yet).
pub fn filter(raw: Vec<RbStation>, index: Option<&DedupeIndex>) -> BeyondResult {
    let playable: Vec<RbStation> = raw.into_iter().filter(RbStation::is_playable).collect();
    let before = playable.len();

    let rank = |s: &RbStation| (s.lastcheckok == 1, s.votes);
    // Same stream URL is the same station whatever its name...
    let by_url = collapse(
        playable,
        |s| s.url_resolved.trim_end_matches('/').to_string(),
        rank,
    );
    // ...and the same normalized name in the same country usually is too.
    let unique = collapse(
        by_url,
        |s| {
            let norm = normalize(&s.name);
            if norm.is_empty() {
                // Nothing to compare on; keep it distinct.
                (String::new(), s.stationuuid.clone())
            } else {
                (s.countrycode.to_ascii_uppercase(), norm)
            }
        },
        rank,
    );
    let collapsed = before - unique.len();

    let Some(index) = index else {
        return BeyondResult {
            stations: unique,
            hidden: 0,
            collapsed,
        };
    };
    let total = unique.len();
    let stations: Vec<RbStation> = unique
        .into_iter()
        .filter(|s| {
            index
                .find(&Candidate {
                    name: &s.name,
                    country_code: &s.countrycode,
                    stream_url: &s.url_resolved,
                })
                .is_none()
        })
        .collect();
    BeyondResult {
        hidden: total - stations.len(),
        stations,
        collapsed,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dedupe::IndexEntry;

    fn fixture() -> Vec<RbStation> {
        let raw = std::fs::read_to_string("tests/fixtures/rb_stations.json").unwrap();
        serde_json::from_str(&raw).unwrap()
    }

    fn station(name: &str, cc: &str, url: &str, votes: i64, ok: u8) -> RbStation {
        let mut s = fixture()[0].clone();
        s.stationuuid = format!("{name}-{votes}");
        s.name = name.into();
        s.countrycode = cc.into();
        s.url_resolved = url.into();
        s.votes = votes;
        s.lastcheckok = ok;
        s
    }

    #[test]
    fn collapses_radio_browser_duplicates() {
        // Fixture: "KEXP 90.3 Seattle, WA", "KEXP 90.3 FM Seattle, WA",
        // "KEXP", "KEXP" -> two distinct normalized names.
        let r = filter(fixture(), None);
        assert_eq!(r.stations.len(), 2);
        assert_eq!(r.collapsed, 2);
        assert_eq!(r.hidden, 0);
        // The higher-voted entry of each pair survives.
        assert_eq!(r.stations[0].name, "KEXP 90.3 Seattle, WA");
    }

    #[test]
    fn prefers_working_entry_over_votes() {
        let r = filter(
            vec![
                station("Jazz Cafe", "NG", "http://a/1", 900, 0),
                station("Jazz Cafe", "NG", "http://a/2", 10, 1),
            ],
            None,
        );
        assert_eq!(r.stations.len(), 1);
        assert_eq!(r.stations[0].url_resolved, "http://a/2");
    }

    #[test]
    fn hides_kexp_when_radio_garden_has_it() {
        let idx = DedupeIndex::build([IndexEntry {
            title: "KEXP 90.3 FM",
            place_title: "Seattle WA",
            country_code: Some("US"),
            host: None,
        }]);
        let r = filter(fixture(), Some(&idx));
        assert!(r.stations.is_empty(), "{:?}", r.stations);
        assert_eq!(r.hidden, 2);
    }

    #[test]
    fn keeps_stations_radio_garden_lacks() {
        let idx = DedupeIndex::build([IndexEntry {
            title: "Cool FM 96.9 Lagos",
            place_title: "Lagos",
            country_code: Some("NG"),
            host: None,
        }]);
        let r = filter(
            vec![
                station("Cool FM", "NG", "https://cool/lagos", 32, 1),
                station("Naija Afrobeats Hits", "NG", "https://naija/hits", 5, 1),
            ],
            Some(&idx),
        );
        assert_eq!(r.hidden, 1);
        assert_eq!(r.stations.len(), 1);
        assert_eq!(r.stations[0].name, "Naija Afrobeats Hits");
    }

    #[test]
    fn drops_unplayable() {
        let mut hls = station("Some HLS", "US", "https://x/live.m3u8", 1, 1);
        hls.hls = 1;
        assert!(filter(vec![hls], None).stations.is_empty());
    }
}
