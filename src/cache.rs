use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::api::Place;
use crate::error::{RadioError, Result};

pub struct Cache {
    conn: Connection,
}

/// One Radio Garden channel in the local dedupe index.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexedChannel {
    pub channel_id: String,
    pub title: String,
    pub place_title: String,
    pub country: String,
    pub norm: String,
    pub host: Option<String>,
}

fn cache_err(e: rusqlite::Error) -> RadioError {
    RadioError::Cache(e.to_string())
}

fn row_to_place(row: &Row<'_>) -> rusqlite::Result<Place> {
    Ok(Place {
        id: row.get(0)?,
        title: row.get(1)?,
        country: row.get(2)?,
        geo: vec![row.get::<_, f64>(3)?, row.get::<_, f64>(4)?],
        size: row.get(5)?,
        boost: row.get::<_, i32>(6)? != 0,
        url: row.get(7)?,
    })
}

fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

impl Cache {
    pub fn new(db_path: &Path) -> Result<Self> {
        Self::from_connection(Connection::open(db_path).map_err(cache_err)?)
    }

    #[cfg(test)]
    pub fn in_memory() -> Result<Self> {
        Self::from_connection(Connection::open_in_memory().map_err(cache_err)?)
    }

    fn from_connection(conn: Connection) -> Result<Self> {
        // The UI and the background index builder each hold a connection.
        conn.busy_timeout(Duration::from_secs(10))
            .map_err(cache_err)?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS places (
                id TEXT PRIMARY KEY,
                title TEXT NOT NULL,
                country TEXT NOT NULL,
                geo_lon REAL,
                geo_lat REAL,
                size INTEGER DEFAULT 0,
                boost INTEGER DEFAULT 0,
                url TEXT
            );
            CREATE TABLE IF NOT EXISTS meta (
                key TEXT PRIMARY KEY,
                value INTEGER NOT NULL
            );
            CREATE INDEX IF NOT EXISTS idx_places_country ON places(country);
            CREATE INDEX IF NOT EXISTS idx_places_title ON places(title);
            CREATE TABLE IF NOT EXISTS rg_channels (
                place_id TEXT NOT NULL,
                channel_id TEXT NOT NULL,
                title TEXT NOT NULL,
                place_title TEXT NOT NULL,
                country TEXT NOT NULL,
                norm TEXT NOT NULL,
                host TEXT,
                PRIMARY KEY (place_id, channel_id)
            );
            CREATE TABLE IF NOT EXISTS rg_index_places (
                place_id TEXT PRIMARY KEY,
                fetched_at INTEGER NOT NULL
            );",
        )
        .map_err(cache_err)?;
        Ok(Self { conn })
    }

    /// Replace the cached place index in one transaction.
    pub fn save_places(&self, places: &[Place]) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(cache_err)?;
        tx.execute("DELETE FROM places", []).map_err(cache_err)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO places (id, title, country, geo_lon, geo_lat, size, boost, url)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
                )
                .map_err(cache_err)?;
            for place in places {
                stmt.execute(params![
                    place.id,
                    place.title,
                    place.country,
                    place.geo.first().copied().unwrap_or(0.0),
                    place.geo.get(1).copied().unwrap_or(0.0),
                    place.size,
                    place.boost as i32,
                    place.url,
                ])
                .map_err(cache_err)?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO meta (key, value) VALUES ('places_updated', ?1)",
            params![now_secs()],
        )
        .map_err(cache_err)?;
        tx.commit().map_err(cache_err)
    }

    pub fn get_all_places(&self) -> Result<Vec<Place>> {
        let mut stmt = self
            .conn
            .prepare("SELECT id, title, country, geo_lon, geo_lat, size, boost, url FROM places")
            .map_err(cache_err)?;
        stmt.query_map([], row_to_place)
            .map_err(cache_err)?
            .collect::<rusqlite::Result<Vec<_>>>()
            .map_err(cache_err)
    }

    fn meta_age(&self, key: &str) -> Option<Duration> {
        let saved: i64 = self
            .conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0))
            .optional()
            .ok()??;
        Some(Duration::from_secs(
            now_secs().saturating_sub(saved).max(0) as u64
        ))
    }

    /// How long ago the place index was saved, if ever.
    pub fn places_age(&self) -> Option<Duration> {
        self.meta_age("places_updated")
    }

    // ----- Radio Garden channel index -------------------------------------

    /// Store one place's channels, replacing what was there, and mark the
    /// place as fetched now.
    pub fn save_place_channels(&self, place_id: &str, channels: &[IndexedChannel]) -> Result<()> {
        let tx = self.conn.unchecked_transaction().map_err(cache_err)?;
        tx.execute("DELETE FROM rg_channels WHERE place_id = ?1", [place_id])
            .map_err(cache_err)?;
        {
            let mut stmt = tx
                .prepare(
                    "INSERT OR REPLACE INTO rg_channels
                     (place_id, channel_id, title, place_title, country, norm, host)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                )
                .map_err(cache_err)?;
            for c in channels {
                stmt.execute(params![
                    place_id,
                    c.channel_id,
                    c.title,
                    c.place_title,
                    c.country,
                    c.norm,
                    c.host,
                ])
                .map_err(cache_err)?;
            }
        }
        tx.execute(
            "INSERT OR REPLACE INTO rg_index_places (place_id, fetched_at) VALUES (?1, ?2)",
            params![place_id, now_secs()],
        )
        .map_err(cache_err)?;
        tx.commit().map_err(cache_err)
    }

    /// Ids of places already fetched into the index.
    pub fn indexed_place_ids(&self) -> Result<std::collections::HashSet<String>> {
        let mut stmt = self
            .conn
            .prepare("SELECT place_id FROM rg_index_places")
            .map_err(cache_err)?;
        stmt.query_map([], |r| r.get(0))
            .map_err(cache_err)?
            .collect::<rusqlite::Result<_>>()
            .map_err(cache_err)
    }

    pub fn indexed_channels(&self) -> Result<Vec<IndexedChannel>> {
        let mut stmt = self
            .conn
            .prepare("SELECT channel_id, title, place_title, country, norm, host FROM rg_channels")
            .map_err(cache_err)?;
        stmt.query_map([], |r| {
            Ok(IndexedChannel {
                channel_id: r.get(0)?,
                title: r.get(1)?,
                place_title: r.get(2)?,
                country: r.get(3)?,
                norm: r.get(4)?,
                host: r.get(5)?,
            })
        })
        .map_err(cache_err)?
        .collect::<rusqlite::Result<_>>()
        .map_err(cache_err)
    }

    /// Record that every place has been fetched.
    pub fn mark_index_complete(&self) -> Result<()> {
        self.conn
            .execute(
                "INSERT OR REPLACE INTO meta (key, value) VALUES ('rg_index_complete', ?1)",
                params![now_secs()],
            )
            .map(|_| ())
            .map_err(cache_err)
    }

    /// Age of the last *complete* index build, if there has been one.
    pub fn index_age(&self) -> Option<Duration> {
        self.meta_age("rg_index_complete")
    }

    /// Start a refresh: every place will be fetched again. Channels stay in
    /// place so filtering keeps working while the refresh runs.
    pub fn reset_index_progress(&self) -> Result<()> {
        self.conn
            .execute("DELETE FROM rg_index_places", [])
            .map(|_| ())
            .map_err(cache_err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn place(id: &str, size: u32) -> Place {
        Place {
            id: id.into(),
            title: format!("Title {id}"),
            country: "Country".into(),
            geo: vec![1.5, -2.5],
            size,
            boost: size > 10,
            url: format!("/visit/x/{id}"),
        }
    }

    #[test]
    fn round_trip() {
        let cache = Cache::in_memory().unwrap();
        assert!(cache.places_age().is_none());
        cache.save_places(&[place("a", 1), place("b", 20)]).unwrap();
        let mut got = cache.get_all_places().unwrap();
        got.sort_by(|x, y| x.id.cmp(&y.id));
        assert_eq!(got.len(), 2);
        assert_eq!(got[1].geo, vec![1.5, -2.5]);
        assert!(got[1].boost);
        assert!(cache.places_age().unwrap() < Duration::from_secs(5));
    }

    fn chan(id: &str, title: &str) -> IndexedChannel {
        IndexedChannel {
            channel_id: id.into(),
            title: title.into(),
            place_title: "Seattle WA".into(),
            country: "United States".into(),
            norm: crate::dedupe::normalize(title),
            host: None,
        }
    }

    #[test]
    fn channel_index_round_trip_and_refresh() {
        let cache = Cache::in_memory().unwrap();
        assert!(cache.index_age().is_none());
        cache
            .save_place_channels("p1", &[chan("c1", "KEXP 90.3 FM"), chan("c2", "KUOW")])
            .unwrap();
        cache.save_place_channels("p2", &[]).unwrap();
        assert_eq!(cache.indexed_place_ids().unwrap().len(), 2);
        assert_eq!(cache.indexed_channels().unwrap().len(), 2);

        // Re-fetching a place replaces its channels.
        cache
            .save_place_channels("p1", &[chan("c1", "KEXP 90.3 FM")])
            .unwrap();
        let got = cache.indexed_channels().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].norm, "kexp");

        cache.mark_index_complete().unwrap();
        assert!(cache.index_age().unwrap() < Duration::from_secs(5));

        // A refresh forgets progress but keeps channels for filtering.
        cache.reset_index_progress().unwrap();
        assert!(cache.indexed_place_ids().unwrap().is_empty());
        assert_eq!(cache.indexed_channels().unwrap().len(), 1);
    }

    #[test]
    fn save_replaces_previous_index() {
        let cache = Cache::in_memory().unwrap();
        cache.save_places(&[place("a", 1), place("b", 2)]).unwrap();
        cache.save_places(&[place("c", 3)]).unwrap();
        let got = cache.get_all_places().unwrap();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].id, "c");
    }
}
