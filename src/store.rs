use std::{path::Path, time::Duration};

use anyhow::{ensure, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};

use crate::swap::Chain;

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Quote {
    pub captured_at: chrono::DateTime<chrono::Utc>,
    pub input_mint: String,
    pub output_mint: String,
    pub input_raw: String,
    pub output_raw: String,
}

pub struct Entry {
    pub wallet: String,
    pub signature: String,
    pub submitted_at_ms: Option<i64>,
    pub send_state: Option<String>,
    pub response_ms: Option<i64>,
    pub confirm_ms: Option<i64>,
    pub send_error: Option<String>,
    pub quote: Option<Quote>,
    pub chain: Option<Chain>,
}

pub struct Store(Connection);

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        let db = Connection::open(path)?;
        db.busy_timeout(Duration::from_secs(5))?;
        db.execute_batch(
            "PRAGMA journal_mode = WAL;
            CREATE TABLE IF NOT EXISTS entries (
                wallet TEXT NOT NULL, signature TEXT NOT NULL,
                submitted_at_ms INTEGER, send_state TEXT, response_ms INTEGER, confirm_ms INTEGER,
                send_error TEXT, quote TEXT, chain TEXT, finalized INTEGER NOT NULL DEFAULT 0,
                PRIMARY KEY (wallet, signature)
            );
            CREATE TABLE IF NOT EXISTS import_heads (
                wallet TEXT NOT NULL, programs TEXT NOT NULL, signature TEXT NOT NULL,
                PRIMARY KEY (wallet, programs)
            );",
        )?;
        Ok(Self(db))
    }

    pub fn finalized(&self, wallet: &str, signature: &str) -> Result<bool> {
        Ok(self
            .0
            .query_row(
                "SELECT finalized FROM entries WHERE wallet = ? AND signature = ?",
                params![wallet, signature],
                |row| row.get(0),
            )
            .optional()?
            .unwrap_or(false))
    }

    pub fn save_chain(
        &self,
        wallet: &str,
        signature: &str,
        chain: &Chain,
        finalized: bool,
    ) -> Result<()> {
        self.0.execute("INSERT INTO entries (wallet, signature, chain, finalized) VALUES (?, ?, ?, ?)
            ON CONFLICT (wallet, signature) DO UPDATE SET chain = excluded.chain, finalized = excluded.finalized
            WHERE entries.finalized <= excluded.finalized",
            params![wallet, signature, serde_json::to_string(chain)?, finalized])?;
        Ok(())
    }

    pub fn head(&self, wallet: &str, programs: &str) -> Result<Option<String>> {
        Ok(self
            .0
            .query_row(
                "SELECT signature FROM import_heads WHERE wallet = ? AND programs = ?",
                params![wallet, programs],
                |row| row.get(0),
            )
            .optional()?)
    }

    pub fn advance_head(
        &self,
        wallet: &str,
        programs: &str,
        previous: Option<&str>,
        head: &str,
    ) -> Result<()> {
        match previous {
            Some(previous) => self.0.execute(
                "UPDATE import_heads SET signature = ?
                WHERE wallet = ? AND programs = ? AND signature = ?",
                params![head, wallet, programs, previous],
            )?,
            None => self.0.execute(
                "INSERT OR IGNORE INTO import_heads VALUES (?, ?, ?)",
                params![wallet, programs, head],
            )?,
        };
        Ok(())
    }

    pub fn begin_send(
        &self,
        wallet: &str,
        signature: &str,
        time: i64,
        quote: Option<&Quote>,
    ) -> Result<()> {
        let quote = quote.map(serde_json::to_string).transpose()?;
        // Claim the signature across wallets before any network write.
        let changed = self.0.execute("INSERT INTO entries (wallet, signature, submitted_at_ms, send_state, quote)
            SELECT ?, ?, ?, 'pending', ? WHERE NOT EXISTS (SELECT 1 FROM entries WHERE signature = ?)",
            params![wallet, signature, time, quote, signature])?;
        ensure!(
            changed == 1,
            "signature already in journal; use import --signature to refresh its chain status"
        );
        Ok(())
    }

    pub fn send_response(
        &self,
        wallet: &str,
        signature: &str,
        elapsed: i64,
        error: Option<&str>,
    ) -> Result<()> {
        let state = if error.is_some() {
            "send_error"
        } else {
            "acknowledged"
        };
        self.0.execute(
            "UPDATE entries SET send_state = ?, response_ms = ?, send_error = ?
            WHERE wallet = ? AND signature = ?",
            params![state, elapsed, error, wallet, signature],
        )?;
        Ok(())
    }

    pub fn finish_tracking(
        &self,
        wallet: &str,
        signature: &str,
        state: &str,
        elapsed: Option<i64>,
    ) -> Result<()> {
        self.0.execute(
            "UPDATE entries SET send_state = ?, confirm_ms = ? WHERE wallet = ? AND signature = ?",
            params![state, elapsed, wallet, signature],
        )?;
        Ok(())
    }

    pub fn entries(&self, wallet: Option<&str>) -> Result<Vec<Entry>> {
        let mut stmt = self.0.prepare(
            "SELECT wallet, signature, submitted_at_ms, send_state, response_ms,
            confirm_ms, send_error, quote, chain FROM entries WHERE (?1 IS NULL OR wallet = ?1)",
        )?;
        let mut rows = stmt.query([wallet])?;
        let mut entries = Vec::new();
        while let Some(row) = rows.next()? {
            let parse = |index| row.get::<_, Option<String>>(index);
            entries.push(Entry {
                wallet: row.get(0)?,
                signature: row.get(1)?,
                submitted_at_ms: row.get(2)?,
                send_state: row.get(3)?,
                response_ms: row.get(4)?,
                confirm_ms: row.get(5)?,
                send_error: row.get(6)?,
                quote: parse(7)?.as_deref().map(serde_json::from_str).transpose()?,
                chain: parse(8)?.as_deref().map(serde_json::from_str).transpose()?,
            });
        }
        Ok(entries)
    }
}
