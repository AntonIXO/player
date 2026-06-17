//! The persisted key/value store (the `meta` table): app settings and the
//! last-session snapshot (saved queue, current index, position). Split out of
//! `lib.rs` to keep that a thin facade — these methods live on `Library` and use
//! its private `conn` directly (a child module may touch the parent's fields).

use rusqlite::{params, OptionalExtension};

use crate::{Library, Result};

impl Library {
    /// Read a persisted value from the `meta` table.
    pub fn get_meta(&self, key: &str) -> Result<Option<String>> {
        let v = self
            .conn
            .query_row("SELECT v FROM meta WHERE k = ?1", params![key], |r| {
                r.get::<_, String>(0)
            })
            .optional()?;
        Ok(v)
    }

    /// Write (upsert) a persisted value into the `meta` table.
    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute(
            "INSERT INTO meta(k, v) VALUES(?1, ?2) \
             ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            params![key, value],
        )?;
        Ok(())
    }

    /// Upsert several `meta` values in a single transaction — one WAL commit
    /// instead of N. Used by session save on window close so the write does not
    /// stall the closing window with a commit per key.
    pub fn set_meta_many(&self, kv: &[(&str, &str)]) -> Result<()> {
        let tx = self.conn.unchecked_transaction()?;
        {
            let mut stmt = tx.prepare(
                "INSERT INTO meta(k, v) VALUES(?1, ?2) \
                 ON CONFLICT(k) DO UPDATE SET v = excluded.v",
            )?;
            for (k, v) in kv {
                stmt.execute(params![k, v])?;
            }
        }
        tx.commit()?;
        Ok(())
    }
}
