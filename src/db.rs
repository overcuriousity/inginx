use anyhow::Result;
use rusqlite::Connection;
use std::{
    path::Path,
    sync::{Arc, Mutex},
};

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS backends (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, base_url TEXT NOT NULL,
  upstream_api_key TEXT, enabled INTEGER NOT NULL DEFAULT 1, is_default INTEGER NOT NULL DEFAULT 0);
CREATE TABLE IF NOT EXISTS models (
  id INTEGER PRIMARY KEY, backend_id INTEGER NOT NULL REFERENCES backends(id) ON DELETE CASCADE,
  upstream_name TEXT NOT NULL, alias TEXT NOT NULL UNIQUE, enabled INTEGER NOT NULL DEFAULT 1);
CREATE TABLE IF NOT EXISTS api_keys (
  id INTEGER PRIMARY KEY, name TEXT NOT NULL, key_hash TEXT NOT NULL UNIQUE, key_prefix TEXT NOT NULL,
  all_models INTEGER NOT NULL DEFAULT 0, created_at INTEGER NOT NULL,
  revoked_at INTEGER, expires_at INTEGER);
CREATE TABLE IF NOT EXISTS key_models (
  key_id INTEGER NOT NULL REFERENCES api_keys(id) ON DELETE CASCADE,
  model_id INTEGER NOT NULL REFERENCES models(id) ON DELETE CASCADE,
  PRIMARY KEY (key_id, model_id));
CREATE TABLE IF NOT EXISTS requests (
  id TEXT PRIMARY KEY, ts INTEGER NOT NULL, key_id INTEGER, model TEXT, endpoint TEXT NOT NULL,
  status INTEGER NOT NULL, latency_ms INTEGER NOT NULL, ttfb_ms INTEGER,
  prompt_tokens INTEGER NOT NULL DEFAULT 0, completion_tokens INTEGER NOT NULL DEFAULT 0,
  usage_source TEXT NOT NULL, error TEXT, req_headers TEXT, resp_headers TEXT,
  req_body BLOB, resp_body BLOB);
CREATE INDEX IF NOT EXISTS idx_requests_ts ON requests(ts);
CREATE INDEX IF NOT EXISTS idx_requests_key ON requests(key_id, ts);
CREATE TABLE IF NOT EXISTS usage_hourly (
  key_id INTEGER NOT NULL, model TEXT NOT NULL, hour INTEGER NOT NULL,
  n_requests INTEGER NOT NULL DEFAULT 0, prompt_tokens INTEGER NOT NULL DEFAULT 0,
  completion_tokens INTEGER NOT NULL DEFAULT 0, n_errors INTEGER NOT NULL DEFAULT 0,
  PRIMARY KEY (key_id, model, hour));
CREATE TABLE IF NOT EXISTS admin_credentials (
  id INTEGER PRIMARY KEY CHECK (id = 1), username TEXT NOT NULL, password_hash TEXT NOT NULL);
CREATE TABLE IF NOT EXISTS webauthn_credentials (
  id INTEGER PRIMARY KEY, cred_json TEXT NOT NULL, created_at INTEGER NOT NULL);
CREATE TABLE IF NOT EXISTS sessions (
  token TEXT PRIMARY KEY, created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
  full INTEGER NOT NULL DEFAULT 1);
CREATE TABLE IF NOT EXISTS settings (key TEXT PRIMARY KEY, value TEXT NOT NULL);
"#;

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;
        Ok(Db(Arc::new(Mutex::new(conn))))
    }

    pub async fn call<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Connection) -> Result<T> + Send + 'static,
    {
        let db = self.0.clone();
        tokio::task::spawn_blocking(move || {
            let mut conn = db.lock().unwrap();
            f(&mut conn)
        })
        .await?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn open_migrates_and_queries() {
        let dir = tempfile::tempdir().unwrap();
        let db = Db::open(&dir.path().join("t.db")).unwrap();
        let n: i64 = db
            .call(|c| Ok(c.query_row("SELECT count(*) FROM api_keys", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(n, 0);
        // migrations are idempotent
        let _db2 = Db::open(&dir.path().join("t.db")).unwrap();
    }
}
