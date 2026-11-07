use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, Row, params};

use crate::api::Place;
use crate::error::{RadioError, Result};

pub struct Cache {
    conn: Connection,
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
            CREATE INDEX IF NOT EXISTS idx_places_title ON places(title);",
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

    /// How long ago the place index was saved, if ever.
    pub fn places_age(&self) -> Option<Duration> {
        let saved: i64 = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = 'places_updated'",
                [],
                |r| r.get(0),
            )
            .optional()
            .ok()??;
        Some(Duration::from_secs(
            now_secs().saturating_sub(saved).max(0) as u64
        ))
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
