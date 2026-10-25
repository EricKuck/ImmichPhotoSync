use std::collections::HashMap;
use std::path::Path;
use std::sync::Mutex;

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

pub struct StateStore {
    conn: Mutex<Connection>,
}

#[derive(Debug, Clone)]
pub struct AssetRow {
    pub modification_ts: f64,
    pub has_adjustments: bool,
    pub is_favorite: bool,
    pub favorite_synced: bool,
}

#[derive(Debug, Clone)]
pub struct VariantRow {
    pub variant: String,
    pub signature: String,
    pub immich_id: String,
}

#[derive(Debug, Clone)]
pub struct AlbumRow {
    pub title: String,
    pub immich_album_id: String,
}

#[derive(Debug, Clone, Default)]
pub struct Counts {
    pub assets: u64,
    pub variants: u64,
    pub albums: u64,
    pub album_members: u64,
    pub failures: u64,
}

const SCHEMA: &str = "
PRAGMA journal_mode=WAL;
PRAGMA synchronous=NORMAL;
CREATE TABLE IF NOT EXISTS assets (
  local_id TEXT PRIMARY KEY,
  media_type INTEGER NOT NULL,
  creation_ts REAL,
  modification_ts REAL NOT NULL,
  has_adjustments INTEGER NOT NULL,
  is_favorite INTEGER NOT NULL,
  favorite_synced INTEGER NOT NULL DEFAULT 0,
  last_seen_ts REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS variants (
  local_id TEXT NOT NULL,
  variant TEXT NOT NULL,
  signature TEXT NOT NULL,
  immich_id TEXT NOT NULL,
  sha1 TEXT,
  uploaded_ts REAL NOT NULL,
  PRIMARY KEY (local_id, variant)
);
CREATE TABLE IF NOT EXISTS failures (
  local_id TEXT PRIMARY KEY,
  attempts INTEGER NOT NULL,
  last_error TEXT NOT NULL,
  last_attempt_ts REAL NOT NULL
);
CREATE TABLE IF NOT EXISTS albums (
  local_id TEXT PRIMARY KEY,
  title TEXT NOT NULL,
  immich_album_id TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS album_members (
  album_local_id TEXT NOT NULL,
  immich_asset_id TEXT NOT NULL,
  PRIMARY KEY (album_local_id, immich_asset_id)
);
CREATE TABLE IF NOT EXISTS meta (
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);
";

fn now_ts() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

impl StateStore {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("opening state db {}", path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        conn.execute_batch(SCHEMA).context("initializing state db schema")?;
        Ok(Self { conn: Mutex::new(conn) })
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn get_asset(&self, local_id: &str) -> Result<Option<AssetRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT modification_ts, has_adjustments, is_favorite, favorite_synced FROM assets WHERE local_id = ?1",
            params![local_id],
            |row| {
                Ok(AssetRow {
                    modification_ts: row.get(0)?,
                    has_adjustments: row.get::<_, i64>(1)? != 0,
                    is_favorite: row.get::<_, i64>(2)? != 0,
                    favorite_synced: row.get::<_, i64>(3)? != 0,
                })
            },
        )
        .optional()
        .context("reading asset row")
    }

    #[allow(clippy::too_many_arguments)]
    pub fn upsert_asset(
        &self,
        local_id: &str,
        media_type: i64,
        creation_ts: Option<f64>,
        modification_ts: f64,
        has_adjustments: bool,
        is_favorite: bool,
        favorite_synced: bool,
    ) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO assets (local_id, media_type, creation_ts, modification_ts, has_adjustments, is_favorite, favorite_synced, last_seen_ts)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(local_id) DO UPDATE SET
               media_type = excluded.media_type,
               creation_ts = excluded.creation_ts,
               modification_ts = excluded.modification_ts,
               has_adjustments = excluded.has_adjustments,
               is_favorite = excluded.is_favorite,
               favorite_synced = excluded.favorite_synced,
               last_seen_ts = excluded.last_seen_ts",
            params![local_id, media_type, creation_ts, modification_ts, has_adjustments as i64, is_favorite as i64, favorite_synced as i64, now_ts()],
        )?;
        Ok(())
    }

    pub fn variants_for(&self, local_id: &str) -> Result<Vec<VariantRow>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached("SELECT variant, signature, immich_id FROM variants WHERE local_id = ?1")?;
        let rows = stmt.query_map(params![local_id], |row| {
            Ok(VariantRow { variant: row.get(0)?, signature: row.get(1)?, immich_id: row.get(2)? })
        })?;
        rows.collect::<std::result::Result<Vec<_>, _>>().context("reading variants")
    }

    pub fn upsert_variant(&self, local_id: &str, variant: &str, signature: &str, immich_id: &str, sha1: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO variants (local_id, variant, signature, immich_id, sha1, uploaded_ts) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(local_id, variant) DO UPDATE SET
               signature = excluded.signature, immich_id = excluded.immich_id, sha1 = excluded.sha1, uploaded_ts = excluded.uploaded_ts",
            params![local_id, variant, signature, immich_id, sha1, now_ts()],
        )?;
        Ok(())
    }

    // Immich ids of every uploaded variant for the given assets, excluding Live Photo video halves.
    pub fn album_candidate_ids(&self, local_ids: &[String]) -> Result<HashMap<String, Vec<String>>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached(
            "SELECT variant, immich_id FROM variants WHERE local_id = ?1 AND variant NOT LIKE '%PairedVideo'",
        )?;
        let mut out: HashMap<String, Vec<String>> = HashMap::new();
        for id in local_ids {
            let ids = stmt
                .query_map(params![id], |row| row.get::<_, String>(1))?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if !ids.is_empty() {
                out.insert(id.clone(), ids);
            }
        }
        Ok(out)
    }

    pub fn record_failure(&self, local_id: &str, error: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO failures (local_id, attempts, last_error, last_attempt_ts) VALUES (?1, 1, ?2, ?3)
             ON CONFLICT(local_id) DO UPDATE SET attempts = attempts + 1, last_error = excluded.last_error, last_attempt_ts = excluded.last_attempt_ts",
            params![local_id, error, now_ts()],
        )?;
        Ok(())
    }

    pub fn clear_failure(&self, local_id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute("DELETE FROM failures WHERE local_id = ?1", params![local_id])?;
        Ok(())
    }

    pub fn failure_attempts(&self, local_id: &str) -> Result<Option<(i64, f64)>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT attempts, last_attempt_ts FROM failures WHERE local_id = ?1",
            params![local_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .context("reading failure row")
    }

    pub fn get_album(&self, local_id: &str) -> Result<Option<AlbumRow>> {
        let conn = self.lock();
        conn.query_row(
            "SELECT title, immich_album_id FROM albums WHERE local_id = ?1",
            params![local_id],
            |row| Ok(AlbumRow { title: row.get(0)?, immich_album_id: row.get(1)? }),
        )
        .optional()
        .context("reading album row")
    }

    pub fn upsert_album(&self, local_id: &str, title: &str, immich_album_id: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO albums (local_id, title, immich_album_id) VALUES (?1, ?2, ?3)
             ON CONFLICT(local_id) DO UPDATE SET title = excluded.title, immich_album_id = excluded.immich_album_id",
            params![local_id, title, immich_album_id],
        )?;
        Ok(())
    }

    pub fn album_members(&self, album_local_id: &str) -> Result<Vec<String>> {
        let conn = self.lock();
        let mut stmt = conn.prepare_cached("SELECT immich_asset_id FROM album_members WHERE album_local_id = ?1")?;
        let rows = stmt.query_map(params![album_local_id], |row| row.get::<_, String>(0))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().context("reading album members")
    }

    pub fn add_album_members(&self, album_local_id: &str, immich_asset_ids: &[String]) -> Result<()> {
        let mut conn = self.lock();
        let tx = conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached("INSERT OR IGNORE INTO album_members (album_local_id, immich_asset_id) VALUES (?1, ?2)")?;
            for id in immich_asset_ids {
                stmt.execute(params![album_local_id, id])?;
            }
        }
        tx.commit()?;
        Ok(())
    }

    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let conn = self.lock();
        conn.query_row("SELECT value FROM meta WHERE key = ?1", params![key], |row| row.get(0))
            .optional()
            .context("reading meta")
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.lock();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2) ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn counts(&self) -> Result<Counts> {
        let conn = self.lock();
        let count = |sql: &str| -> Result<u64> { Ok(conn.query_row(sql, [], |row| row.get::<_, i64>(0))? as u64) };
        Ok(Counts {
            assets: count("SELECT COUNT(*) FROM assets")?,
            variants: count("SELECT COUNT(*) FROM variants")?,
            albums: count("SELECT COUNT(*) FROM albums")?,
            album_members: count("SELECT COUNT(*) FROM album_members")?,
            failures: count("SELECT COUNT(*) FROM failures")?,
        })
    }

    pub fn recent_failures(&self, limit: usize) -> Result<Vec<(String, i64, String)>> {
        let conn = self.lock();
        let mut stmt = conn.prepare("SELECT local_id, attempts, last_error FROM failures ORDER BY last_attempt_ts DESC LIMIT ?1")?;
        let rows = stmt.query_map(params![limit as i64], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?;
        rows.collect::<std::result::Result<Vec<_>, _>>().context("reading failures")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, StateStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = StateStore::open(&dir.path().join("nested").join("state.sqlite")).unwrap();
        (dir, store)
    }

    #[test]
    fn asset_round_trip() {
        let (_dir, store) = store();
        assert!(store.get_asset("a").unwrap().is_none());
        store.upsert_asset("a", 1, Some(1.0), 2.5, true, false, true).unwrap();
        let row = store.get_asset("a").unwrap().unwrap();
        assert_eq!(row.modification_ts, 2.5);
        assert!(row.has_adjustments);
        assert!(!row.is_favorite);
        assert!(row.favorite_synced);
        store.upsert_asset("a", 1, Some(1.0), 3.0, true, true, false).unwrap();
        let row = store.get_asset("a").unwrap().unwrap();
        assert_eq!(row.modification_ts, 3.0);
        assert!(row.is_favorite);
        assert!(!row.favorite_synced);
        assert_eq!(store.counts().unwrap().assets, 1);
    }

    #[test]
    fn variants_and_album_candidates_skip_paired_videos() {
        let (_dir, store) = store();
        store.upsert_variant("a", "pairedVideo", "original", "vid-1", "00").unwrap();
        store.upsert_variant("a", "original", "original", "img-1", "11").unwrap();
        store.upsert_variant("a", "edited", "mod:1", "img-1e", "22").unwrap();
        store.upsert_variant("a", "editedPairedVideo", "mod:1", "vid-1e", "33").unwrap();
        store.upsert_variant("b", "original", "original", "img-2", "44").unwrap();
        store.upsert_variant("a", "edited", "mod:2", "img-1f", "55").unwrap();

        let variants = store.variants_for("a").unwrap();
        assert_eq!(variants.len(), 4);
        let edited = variants.iter().find(|v| v.variant == "edited").unwrap();
        assert_eq!((edited.signature.as_str(), edited.immich_id.as_str()), ("mod:2", "img-1f"));

        let candidates = store.album_candidate_ids(&["a".into(), "b".into(), "missing".into()]).unwrap();
        let mut a = candidates["a"].clone();
        a.sort();
        assert_eq!(a, vec!["img-1".to_string(), "img-1f".to_string()]);
        assert_eq!(candidates["b"], vec!["img-2".to_string()]);
        assert!(!candidates.contains_key("missing"));
    }

    #[test]
    fn albums_and_members() {
        let (_dir, store) = store();
        assert!(store.get_album("alb").unwrap().is_none());
        store.upsert_album("alb", "Trip", "imm-alb").unwrap();
        store.upsert_album("alb", "Trip 2024", "imm-alb").unwrap();
        let row = store.get_album("alb").unwrap().unwrap();
        assert_eq!((row.title.as_str(), row.immich_album_id.as_str()), ("Trip 2024", "imm-alb"));

        store.add_album_members("alb", &["x".into(), "y".into()]).unwrap();
        store.add_album_members("alb", &["y".into(), "z".into()]).unwrap();
        let mut members = store.album_members("alb").unwrap();
        members.sort();
        assert_eq!(members, vec!["x", "y", "z"]);
        assert_eq!(store.counts().unwrap().album_members, 3);
    }

    #[test]
    fn failures_and_meta() {
        let (_dir, store) = store();
        assert!(store.failure_attempts("a").unwrap().is_none());
        store.record_failure("a", "boom").unwrap();
        store.record_failure("a", "boom again").unwrap();
        let (attempts, ts) = store.failure_attempts("a").unwrap().unwrap();
        assert_eq!(attempts, 2);
        assert!(ts > 0.0);
        assert_eq!(store.recent_failures(5).unwrap()[0].2, "boom again");
        store.clear_failure("a").unwrap();
        assert!(store.failure_attempts("a").unwrap().is_none());

        assert!(store.get_meta("k").unwrap().is_none());
        store.set_meta("k", "v1").unwrap();
        store.set_meta("k", "v2").unwrap();
        assert_eq!(store.get_meta("k").unwrap().as_deref(), Some("v2"));
    }
}
