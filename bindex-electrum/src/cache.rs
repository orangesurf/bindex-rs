use std::path::Path;

use rusqlite::{params, Connection, OptionalExtension};

use crate::protocol::{scripthash_status, ElectrumScripthash, HistoryEntry};

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CacheTip {
    pub height: usize,
    pub block_hash: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Utxo {
    pub tx_hash: String,
    pub tx_pos: u32,
    pub height: i64,
    pub value: u64,
    pub script_pubkey: Vec<u8>,
}

pub struct ElectrumCache {
    conn: Connection,
}

impl ElectrumCache {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, Error> {
        let conn = Connection::open(path)?;
        let cache = Self { conn };
        cache.init()?;
        Ok(cache)
    }

    pub fn open_memory() -> Result<Self, Error> {
        let conn = Connection::open_in_memory()?;
        let cache = Self { conn };
        cache.init()?;
        Ok(cache)
    }

    fn init(&self) -> Result<(), Error> {
        self.conn.execute_batch(
            "
            PRAGMA journal_mode=WAL;
            PRAGMA foreign_keys=ON;

            CREATE TABLE IF NOT EXISTS cache_tip (
                id INTEGER PRIMARY KEY CHECK (id = 0),
                height INTEGER NOT NULL,
                block_hash TEXT NOT NULL
            );

            CREATE TABLE IF NOT EXISTS history (
                scripthash TEXT NOT NULL,
                tx_hash TEXT NOT NULL,
                height INTEGER NOT NULL,
                position INTEGER NOT NULL,
                fee INTEGER,
                PRIMARY KEY (scripthash, tx_hash)
            ) WITHOUT ROWID;

            CREATE INDEX IF NOT EXISTS history_scripthash_height_position
                ON history (scripthash, height, position);

            CREATE TABLE IF NOT EXISTS utxo (
                scripthash TEXT NOT NULL,
                tx_hash TEXT NOT NULL,
                tx_pos INTEGER NOT NULL,
                height INTEGER NOT NULL,
                value INTEGER NOT NULL,
                script_pubkey BLOB NOT NULL,
                spent_by_tx_hash TEXT,
                PRIMARY KEY (tx_hash, tx_pos)
            ) WITHOUT ROWID;

            CREATE INDEX IF NOT EXISTS utxo_scripthash_unspent
                ON utxo (scripthash, spent_by_tx_hash, height);

            CREATE TABLE IF NOT EXISTS outpoint_spends (
                prev_tx_hash TEXT NOT NULL,
                prev_tx_pos INTEGER NOT NULL,
                spending_tx_hash TEXT NOT NULL,
                spending_height INTEGER NOT NULL,
                spending_position INTEGER NOT NULL,
                PRIMARY KEY (prev_tx_hash, prev_tx_pos, spending_tx_hash)
            ) WITHOUT ROWID;

            CREATE TABLE IF NOT EXISTS status (
                scripthash TEXT PRIMARY KEY,
                status_hash TEXT
            ) WITHOUT ROWID;
            ",
        )?;
        Ok(())
    }

    pub fn tip(&self) -> Result<Option<CacheTip>, Error> {
        self.conn
            .query_row(
                "SELECT height, block_hash FROM cache_tip WHERE id = 0",
                [],
                |row| {
                    Ok(CacheTip {
                        height: row.get::<_, i64>(0)? as usize,
                        block_hash: row.get(1)?,
                    })
                },
            )
            .optional()
            .map_err(Error::Sqlite)
    }

    pub fn set_tip(&self, tip: &CacheTip) -> Result<(), Error> {
        self.conn.execute(
            "INSERT INTO cache_tip (id, height, block_hash) VALUES (0, ?1, ?2)
             ON CONFLICT(id) DO UPDATE SET height = excluded.height, block_hash = excluded.block_hash",
            params![tip.height as i64, tip.block_hash],
        )?;
        Ok(())
    }

    pub fn invalidate_from_height(&self, height: usize) -> Result<(), Error> {
        let tx = self.conn.unchecked_transaction()?;
        tx.execute(
            "DELETE FROM history WHERE height >= ?1",
            params![height as i64],
        )?;
        tx.execute(
            "DELETE FROM utxo WHERE height >= ?1",
            params![height as i64],
        )?;
        tx.execute(
            "DELETE FROM outpoint_spends WHERE spending_height >= ?1",
            params![height as i64],
        )?;
        tx.execute(
            "DELETE FROM status WHERE scripthash NOT IN (SELECT DISTINCT scripthash FROM history)",
            [],
        )?;
        tx.commit()?;
        Ok(())
    }

    pub fn history(&self, scripthash: ElectrumScripthash) -> Result<Vec<HistoryEntry>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT tx_hash, height, fee
             FROM history
             WHERE scripthash = ?1
             ORDER BY height, position",
        )?;
        let rows = stmt
            .query_map(params![scripthash.to_string()], |row| {
                Ok(HistoryEntry {
                    tx_hash: row.get(0)?,
                    height: row.get(1)?,
                    fee: row.get(2)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    pub fn replace_history(
        &mut self,
        scripthash: ElectrumScripthash,
        history: &[HistoryEntry],
    ) -> Result<Option<String>, Error> {
        let tx = self.conn.transaction()?;
        let key = scripthash.to_string();
        tx.execute("DELETE FROM history WHERE scripthash = ?1", params![key])?;
        for (position, item) in history.iter().enumerate() {
            tx.execute(
                "INSERT INTO history (scripthash, tx_hash, height, position, fee)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![key, item.tx_hash, item.height, position as i64, item.fee],
            )?;
        }
        let status = scripthash_status(history);
        tx.execute(
            "INSERT INTO status (scripthash, status_hash) VALUES (?1, ?2)
             ON CONFLICT(scripthash) DO UPDATE SET status_hash = excluded.status_hash",
            params![key, status],
        )?;
        tx.commit()?;
        Ok(status)
    }

    pub fn listunspent(&self, scripthash: ElectrumScripthash) -> Result<Vec<Utxo>, Error> {
        let mut stmt = self.conn.prepare(
            "SELECT tx_hash, tx_pos, height, value, script_pubkey
             FROM utxo
             WHERE scripthash = ?1 AND spent_by_tx_hash IS NULL
             ORDER BY height, tx_hash, tx_pos",
        )?;
        let rows = stmt
            .query_map(params![scripthash.to_string()], |row| {
                Ok(Utxo {
                    tx_hash: row.get(0)?,
                    tx_pos: row.get::<_, i64>(1)? as u32,
                    height: row.get(2)?,
                    value: row.get(3)?,
                    script_pubkey: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_replaces_history_and_status() {
        let mut cache = ElectrumCache::open_memory().unwrap();
        let sh = ElectrumScripthash::parse(&"11".repeat(32)).unwrap();
        let history = vec![HistoryEntry {
            tx_hash: "aa".repeat(32),
            height: 1,
            fee: None,
        }];
        let status = cache.replace_history(sh, &history).unwrap();
        assert!(status.is_some());
        assert_eq!(cache.history(sh).unwrap(), history);
    }

    #[test]
    fn cache_tip_round_trips() {
        let cache = ElectrumCache::open_memory().unwrap();
        let tip = CacheTip {
            height: 42,
            block_hash: "bb".repeat(32),
        };
        cache.set_tip(&tip).unwrap();
        assert_eq!(cache.tip().unwrap(), Some(tip));
    }
}
