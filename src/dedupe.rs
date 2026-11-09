//! Pure, synchronous duplicate detection between Radio Browser stations and
//! the local Radio Garden channel index.
//!
//! Design rule: prefer false negatives (letting a duplicate through) over
//! false positives (hiding a station that isn't on Radio Garden). Every
//! signal below is deliberately conservative.

use std::collections::{HashMap, HashSet};

/// Jaro-Winkler similarity at or above which two normalized names in the same
/// country count as the same station.
pub const FUZZY_THRESHOLD: f64 = 0.93;
/// Fuzzy matching is skipped for short names: call signs like "kexp" and
/// "kexq" score ~0.93 but are different stations.
pub const FUZZY_MIN_LEN: usize = 6;
pub const FUZZY_ENABLED: bool = true;
/// Longest place name (in words) looked for inside a station name.
const MAX_PLACE_WORDS: usize = 4;

const FILLER: &[&str] = &["radio", "fm", "am", "the", "online", "live"];

/// Lowercase, drop frequencies (`98.7`, `fm 101`, `96.9fm`), strip
/// punctuation, drop filler words and collapse whitespace.
///
/// `"Cool FM 96.9 Lagos"` -> `"cool lagos"`, `"KEXP 90.3 FM"` -> `"kexp"`.
pub fn normalize(name: &str) -> String {
    tokens(name).join(" ")
}

fn tokens(name: &str) -> Vec<String> {
    let lower = name.to_lowercase();
    // Keep '.' and ',' for now so "98.7" / "96,9" survive as one token.
    let spaced: String = lower
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '.' || c == ',' {
                c
            } else {
                ' '
            }
        })
        .collect();

    spaced
        .split_whitespace()
        .filter_map(|raw| {
            let tok = raw.trim_matches(|c| c == '.' || c == ',');
            if tok.is_empty() || is_frequency(tok) {
                return None;
            }
            // "k.e.x.p" -> "kexp", "rock,pop" -> "rockpop"
            let tok: String = tok.chars().filter(|c| c.is_alphanumeric()).collect();
            if tok.is_empty() || FILLER.contains(&tok.as_str()) {
                None
            } else {
                Some(tok)
            }
        })
        .collect()
}

/// `98.7`, `101`, `96,9`, `96.9fm`, `fm101`, `1080am`.
fn is_frequency(tok: &str) -> bool {
    let core = tok
        .strip_suffix("fm")
        .or_else(|| tok.strip_suffix("am"))
        .or_else(|| tok.strip_suffix("mhz"))
        .or_else(|| tok.strip_suffix("khz"))
        .unwrap_or(tok);
    let core = core
        .strip_prefix("fm")
        .or_else(|| core.strip_prefix("am"))
        .unwrap_or(core);
    let mut seps = 0;
    !core.is_empty()
        && core.chars().any(|c| c.is_ascii_digit())
        && core.chars().all(|c| {
            if c == '.' || c == ',' {
                seps += 1;
                seps <= 1
            } else {
                c.is_ascii_digit()
            }
        })
}

/// Remove the first occurrence of `phrase` (as whole words) from `name`.
/// Returns `None` if it isn't there or nothing would be left.
fn strip_phrase(name: &[String], phrase: &[String]) -> Option<Vec<String>> {
    if phrase.is_empty() || phrase.len() >= name.len() {
        return None;
    }
    let at = name.windows(phrase.len()).position(|w| w == phrase)?;
    let mut out = name[..at].to_vec();
    out.extend_from_slice(&name[at + phrase.len()..]);
    Some(out)
}

/// Hostname of a stream URL, lowercased, without port, path or `www.`.
pub fn stream_host(url: &str) -> Option<String> {
    let rest = url.split_once("://").map_or(url, |(_, r)| r);
    let authority = rest.split(['/', '?', '#']).next()?;
    let host = authority.rsplit_once('@').map_or(authority, |(_, h)| h);
    let host = if host.starts_with('[') {
        host.split(']').next()?.trim_start_matches('[')
    } else {
        host.split(':').next()?
    };
    let host = host.trim_end_matches('.').to_ascii_lowercase();
    let host = host.strip_prefix("www.").unwrap_or(&host).to_string();
    (!host.is_empty()).then_some(host)
}

// ---------------------------------------------------------------------------
// Index
// ---------------------------------------------------------------------------

/// One Radio Garden channel, as stored in the local index.
pub struct IndexEntry<'a> {
    pub title: &'a str,
    pub place_title: &'a str,
    /// ISO 3166-1 alpha-2, if the Radio Garden country name is known.
    pub country_code: Option<&'a str>,
    /// Resolved stream host, if host resolution was enabled.
    pub host: Option<&'a str>,
}

#[derive(Default)]
struct CountryNames {
    /// Normalized name (with and without its own place) -> places it's in.
    names: HashMap<String, HashSet<String>>,
    /// Normalized place titles in this country.
    places: HashSet<Vec<String>>,
}

/// Lookup structure over the Radio Garden index.
#[derive(Default)]
pub struct DedupeIndex {
    countries: HashMap<String, CountryNames>,
    /// host -> number of Radio Garden channels on it.
    hosts: HashMap<String, u32>,
    channels: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchReason {
    StreamHost,
    Name,
    Fuzzy,
}

/// A Radio Browser station reduced to what matching needs.
pub struct Candidate<'a> {
    pub name: &'a str,
    pub country_code: &'a str,
    pub stream_url: &'a str,
}

impl DedupeIndex {
    pub fn build<'a>(entries: impl IntoIterator<Item = IndexEntry<'a>>) -> Self {
        let mut idx = DedupeIndex::default();
        for e in entries {
            idx.channels += 1;
            if let Some(h) = e.host {
                *idx.hosts.entry(h.to_ascii_lowercase()).or_default() += 1;
            }
            let Some(cc) = e.country_code else { continue };
            let name = tokens(e.title);
            if name.is_empty() {
                continue;
            }
            let place = tokens(e.place_title);
            let country = idx.countries.entry(cc.to_ascii_uppercase()).or_default();
            let place_key = place.join(" ");
            country
                .names
                .entry(name.join(" "))
                .or_default()
                .insert(place_key.clone());
            if let Some(base) = strip_phrase(&name, &place) {
                country
                    .names
                    .entry(base.join(" "))
                    .or_default()
                    .insert(place_key.clone());
            }
            if !place.is_empty() {
                country.places.insert(place);
            }
        }
        idx
    }

    pub fn channel_count(&self) -> usize {
        self.channels
    }

    /// Why `c` is (probably) already on Radio Garden, or `None` if it isn't.
    pub fn find(&self, c: &Candidate<'_>) -> Option<MatchReason> {
        // 1. Stream host, but only hosts used by exactly one Radio Garden
        //    channel: shared hosts (radio.co, zeno.fm, ...) carry many
        //    unrelated stations.
        if let Some(host) = stream_host(c.stream_url)
            && self.hosts.get(&host) == Some(&1)
        {
            return Some(MatchReason::StreamHost);
        }

        let country = self.countries.get(&c.country_code.to_ascii_uppercase())?;
        let name = tokens(c.name);
        if name.is_empty() {
            return None;
        }

        // 2a. Same normalized name anywhere in the country
        //     ("Cool FM" vs Radio Garden's "Cool FM 96.9").
        let full = name.join(" ");
        if country.names.contains_key(&full) {
            return Some(MatchReason::Name);
        }

        // 2b. The Radio Browser name carries a city that is a Radio Garden
        //     place: strip it and require the channel to be in *that* place
        //     ("KEXP 90.3 Seattle, WA" vs "KEXP 90.3 FM" in "Seattle WA").
        let mut variants = vec![full];
        for len in (1..=MAX_PLACE_WORDS.min(name.len().saturating_sub(1))).rev() {
            for window in name.windows(len) {
                if !country.places.contains(window) {
                    continue;
                }
                let place_key = window.join(" ");
                if let Some(rest) = strip_phrase(&name, window) {
                    let rest = rest.join(" ");
                    if country
                        .names
                        .get(&rest)
                        .is_some_and(|places| places.contains(&place_key))
                    {
                        return Some(MatchReason::Name);
                    }
                    variants.push(rest);
                }
            }
        }

        // 3. Fuzzy, same country, long names only.
        if FUZZY_ENABLED {
            for v in variants
                .iter()
                .filter(|v| v.chars().count() >= FUZZY_MIN_LEN)
            {
                let first = v.chars().next();
                let hit = country.names.keys().any(|known| {
                    known.chars().count() >= FUZZY_MIN_LEN
                        && known.chars().next() == first
                        && strsim::jaro_winkler(v, known) >= FUZZY_THRESHOLD
                });
                if hit {
                    return Some(MatchReason::Fuzzy);
                }
            }
        }
        None
    }
}

/// Collapse items that share a key, keeping the best-ranked one (highest
/// `rank`) at the position of the first occurrence.
pub fn collapse<T, K, R>(items: Vec<T>, key: impl Fn(&T) -> K, rank: impl Fn(&T) -> R) -> Vec<T>
where
    K: std::hash::Hash + Eq,
    R: Ord,
{
    let mut slot: HashMap<K, usize> = HashMap::new();
    let mut out: Vec<T> = Vec::with_capacity(items.len());
    for item in items {
        match slot.get(&key(&item)) {
            Some(&i) => {
                if rank(&item) > rank(&out[i]) {
                    out[i] = item;
                }
            }
            None => {
                slot.insert(key(&item), out.len());
                out.push(item);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry<'a>(title: &'a str, place: &'a str, cc: &'a str) -> IndexEntry<'a> {
        IndexEntry {
            title,
            place_title: place,
            country_code: Some(cc),
            host: None,
        }
    }

    fn cand<'a>(name: &'a str, cc: &'a str) -> Candidate<'a> {
        Candidate {
            name,
            country_code: cc,
            stream_url: "https://unique.example/stream",
        }
    }

    fn sample_index() -> DedupeIndex {
        DedupeIndex::build([
            entry("KEXP 90.3 FM", "Seattle WA", "US"),
            entry("Cool FM 96.9 Lagos", "Lagos", "NG"),
            entry("Cool FM Kano 96.9", "Kano", "NG"),
            entry("Kiss FM", "Berlin", "DE"),
            entry("Radio Nova", "Paris", "FR"),
            entry("Deutschlandfunk Kultur", "Berlin", "DE"),
        ])
    }

    #[test]
    fn normalizes_names() {
        assert_eq!(normalize("Cool FM 96.9 Lagos"), "cool lagos");
        assert_eq!(normalize("KEXP 90.3 FM"), "kexp");
        assert_eq!(normalize("KEXP 90.3 Seattle, WA"), "kexp seattle wa");
        assert_eq!(normalize("The Online Radio: FM 101"), "");
        assert_eq!(normalize("96.9FM Rock-Pop!"), "rock pop");
        assert_eq!(normalize("K.E.X.P."), "kexp");
        assert_eq!(normalize("Radio  Nova  (Paris)"), "nova paris");
        assert_eq!(normalize("Энергия 104,2"), "энергия");
    }

    #[test]
    fn frequencies() {
        for f in [
            "98.7", "101", "96,9", "96.9fm", "fm101", "1080am", "88.5mhz",
        ] {
            assert!(is_frequency(f), "{f}");
        }
        for f in ["kexp", "1.2.3", "fm", "", "24/7"] {
            assert!(!is_frequency(f), "{f}");
        }
    }

    #[test]
    fn cool_fm_lagos_matches_cool_fm() {
        let idx = sample_index();
        assert_eq!(idx.find(&cand("Cool FM", "NG")), Some(MatchReason::Name));
        assert_eq!(
            idx.find(&cand("Cool FM 96.9 Lagos", "NG")),
            Some(MatchReason::Name)
        );
    }

    #[test]
    fn kexp_with_frequency_and_city_matches() {
        let idx = sample_index();
        assert_eq!(idx.find(&cand("KEXP", "US")), Some(MatchReason::Name));
        assert_eq!(
            idx.find(&cand("KEXP 90.3 Seattle, WA", "US")),
            Some(MatchReason::Name)
        );
        assert_eq!(
            idx.find(&cand("KEXP 90.3 FM Seattle, WA", "us")),
            Some(MatchReason::Name)
        );
    }

    #[test]
    fn near_misses_do_not_match() {
        let idx = sample_index();
        // Different call sign, same country.
        assert_eq!(idx.find(&cand("KEXQ 90.3", "US")), None);
        // Same name, different country.
        assert_eq!(idx.find(&cand("KEXP", "CA")), None);
        // Same brand, a city Radio Garden doesn't list it in.
        assert_eq!(idx.find(&cand("Kiss FM Munich", "DE")), None);
        // A different station under a shared brand.
        assert_eq!(idx.find(&cand("Cool FM - Goldies", "NG")), None);
        // Only filler + frequency: nothing to match on.
        assert_eq!(idx.find(&cand("Radio FM 101", "US")), None);
    }

    #[test]
    fn city_must_be_the_channels_own_place() {
        let idx = sample_index();
        // "Kiss FM" is a Berlin channel; "Kiss FM Berlin" is the same one.
        assert_eq!(
            idx.find(&cand("Kiss FM Berlin", "DE")),
            Some(MatchReason::Name)
        );
        // "Kano" is a Nigerian place, but Kano's channel is "Cool FM Kano",
        // whose base "cool" is in Kano -> match; while "Wazobia Kano" isn't.
        assert_eq!(
            idx.find(&cand("Cool FM Kano", "NG")),
            Some(MatchReason::Name)
        );
        assert_eq!(idx.find(&cand("Wazobia FM Kano", "NG")), None);
    }

    #[test]
    fn fuzzy_catches_small_spelling_differences_in_long_names() {
        let idx = sample_index();
        assert_eq!(
            idx.find(&cand("Deutschlandfunk Kultr", "DE")),
            Some(MatchReason::Fuzzy)
        );
        // Short names never go through fuzzy matching.
        assert_eq!(idx.find(&cand("Nava", "FR")), None);
    }

    #[test]
    fn unknown_country_never_matches_by_name() {
        let idx = DedupeIndex::build([IndexEntry {
            title: "KEXP",
            place_title: "Seattle WA",
            country_code: None,
            host: None,
        }]);
        assert_eq!(idx.find(&cand("KEXP", "US")), None);
        assert_eq!(idx.channel_count(), 1);
    }

    #[test]
    fn stream_host_match_ignores_port_and_path_but_not_shared_hosts() {
        let idx = DedupeIndex::build([
            IndexEntry {
                host: Some("kexp-mp3-128.streamguys1.com"),
                ..entry("KEXP 90.3 FM", "Seattle WA", "US")
            },
            IndexEntry {
                host: Some("s4.radio.co"),
                ..entry("Station A", "Austin TX", "US")
            },
            IndexEntry {
                host: Some("s4.radio.co"),
                ..entry("Station B", "London", "GB")
            },
        ]);
        let by_url = |url: &'static str| Candidate {
            name: "Something Else Entirely",
            country_code: "ZZ",
            stream_url: url,
        };
        assert_eq!(
            idx.find(&by_url(
                "https://kexp-mp3-128.streamguys1.com:443/kexp128.mp3?x=1"
            )),
            Some(MatchReason::StreamHost)
        );
        assert_eq!(idx.find(&by_url("https://s4.radio.co/abc/listen")), None);
    }

    #[test]
    fn hosts_parse() {
        assert_eq!(
            stream_host("http://user@WWW.Example.com:8000/live").as_deref(),
            Some("example.com")
        );
        assert_eq!(stream_host("https://[::1]:80/x").as_deref(), Some("::1"));
        assert_eq!(stream_host("not a url").as_deref(), Some("not a url"));
        assert_eq!(stream_host("").as_deref(), None);
    }

    #[test]
    fn collapse_keeps_best_ranked_in_first_position() {
        let items = vec![("a", 5), ("b", 1), ("a", 9), ("c", 2), ("b", 0)];
        let out = collapse(items, |i| i.0, |i| i.1);
        assert_eq!(out, vec![("a", 9), ("b", 1), ("c", 2)]);
    }
}
