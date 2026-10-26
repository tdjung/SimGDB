//! Bookmarks and the tool-call log (ARCHITECTURE §2.7, §2.8), in
//! `annotations.sqlite` next to the index unless placed elsewhere. People
//! (GDB, VS Code) and AI clients share it; the index itself stays read-only.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use crate::StoreError;

pub struct Annotations {
    db: rusqlite::Connection,
    pub path: PathBuf,
}

#[derive(Clone, Debug, Serialize, PartialEq, Eq)]
pub struct Bookmark {
    pub id: i64,
    pub index: u64,
    pub note: String,
    pub author: String,
    /// Seconds since the Unix epoch.
    pub created: u64,
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Annotations {
    /// Open (creating if needed) the annotations database.
    pub fn open(path: &Path) -> Result<Annotations, StoreError> {
        let db = rusqlite::Connection::open(path)?;
        db.busy_timeout(std::time::Duration::from_secs(5))?;
        db.execute_batch(
            "CREATE TABLE IF NOT EXISTS bookmarks (
                 id INTEGER PRIMARY KEY, idx INTEGER NOT NULL, note TEXT NOT NULL,
                 author TEXT NOT NULL, created INTEGER NOT NULL);
             CREATE TABLE IF NOT EXISTS call_log (
                 id INTEGER PRIMARY KEY, time INTEGER NOT NULL, tool TEXT NOT NULL,
                 args TEXT NOT NULL, ok INTEGER NOT NULL, summary TEXT NOT NULL, ms REAL NOT NULL);",
        )?;
        Ok(Annotations { db, path: path.to_path_buf() })
    }

    pub fn add_bookmark(&self, index: u64, note: &str, author: &str) -> Result<Bookmark, StoreError> {
        let created = now();
        self.db.execute(
            "INSERT INTO bookmarks (idx, note, author, created) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![index as i64, note, author, created as i64],
        )?;
        Ok(Bookmark { id: self.db.last_insert_rowid(), index, note: note.into(), author: author.into(), created })
    }

    /// Bookmarks in point order, optionally within `from..=to`.
    pub fn bookmarks(&self, from: u64, to: u64) -> Result<Vec<Bookmark>, StoreError> {
        let mut st = self.db.prepare(
            "SELECT id, idx, note, author, created FROM bookmarks WHERE idx >= ?1 AND idx <= ?2 ORDER BY idx, id",
        )?;
        let rows = st.query_map(rusqlite::params![from as i64, to.min(i64::MAX as u64) as i64], |r| {
            Ok(Bookmark {
                id: r.get(0)?,
                index: r.get::<_, i64>(1)? as u64,
                note: r.get(2)?,
                author: r.get(3)?,
                created: r.get::<_, i64>(4)? as u64,
            })
        })?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    pub fn log_call(&self, tool: &str, args: &str, ok: bool, summary: &str, ms: f64) -> Result<(), StoreError> {
        self.db.execute(
            "INSERT INTO call_log (time, tool, args, ok, summary, ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            rusqlite::params![now() as i64, tool, args, ok, summary, ms],
        )?;
        Ok(())
    }

    /// Calls logged, oldest first: (tool, args, ok, summary).
    pub fn calls(&self) -> Result<Vec<(String, String, bool, String)>, StoreError> {
        let mut st = self.db.prepare("SELECT tool, args, ok, summary FROM call_log ORDER BY id")?;
        let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }
}
