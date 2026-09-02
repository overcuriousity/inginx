use crate::db::{now, Db};
use crate::metering::Usage;
use anyhow::Result;
use tokio::sync::mpsc;

#[derive(Debug)]
pub struct LogRecord {
    pub id: String,
    pub ts: i64,
    pub key_id: Option<i64>,
    pub model: Option<String>,
    pub endpoint: String,
    pub status: u16,
    pub latency_ms: u32,
    pub ttfb_ms: Option<u32>,
    pub usage: Usage,
    pub usage_source: &'static str,
    pub error: Option<String>,
    pub req_headers: String,
    pub resp_headers: String,
    pub req_body: Vec<u8>,
    pub resp_body: Vec<u8>,
}

#[derive(Clone)]
pub struct LogWriter(mpsc::Sender<LogRecord>);

impl LogWriter {
    pub fn send(&self, rec: LogRecord) {
        if let Err(e) = self.0.try_send(rec) {
            tracing::warn!("log channel full or closed, dropping record: {e}");
        }
    }
}

fn compress(data: &[u8]) -> Vec<u8> {
    zstd::encode_all(data, 3).unwrap_or_else(|_| data.to_vec())
}

fn insert_record(c: &mut rusqlite::Connection, rec: &LogRecord) -> Result<()> {
    let tx = c.transaction()?;
    tx.execute(
        "INSERT INTO requests (id, ts, key_id, model, endpoint, status, latency_ms, ttfb_ms,
            prompt_tokens, completion_tokens, usage_source, error, req_headers, resp_headers,
            req_body, resp_body)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)",
        rusqlite::params![
            rec.id,
            rec.ts,
            rec.key_id,
            rec.model,
            rec.endpoint,
            rec.status,
            rec.latency_ms,
            rec.ttfb_ms,
            rec.usage.prompt_tokens,
            rec.usage.completion_tokens,
            rec.usage_source,
            rec.error,
            rec.req_headers,
            rec.resp_headers,
            compress(&rec.req_body),
            compress(&rec.resp_body),
        ],
    )?;
    let hour = rec.ts - rec.ts % 3600;
    tx.execute(
        "INSERT INTO usage_hourly (key_id, model, hour, n_requests, prompt_tokens, completion_tokens, n_errors)
         VALUES (?1, ?2, ?3, 1, ?4, ?5, ?6)
         ON CONFLICT(key_id, model, hour) DO UPDATE SET
            n_requests = n_requests + 1,
            prompt_tokens = prompt_tokens + excluded.prompt_tokens,
            completion_tokens = completion_tokens + excluded.completion_tokens,
            n_errors = n_errors + excluded.n_errors",
        rusqlite::params![
            rec.key_id.unwrap_or(-1),
            rec.model.as_deref().unwrap_or(""),
            hour,
            rec.usage.prompt_tokens,
            rec.usage.completion_tokens,
            (rec.status >= 400) as i64,
        ],
    )?;
    tx.commit()?;
    Ok(())
}

pub fn spawn_writer(db: Db) -> LogWriter {
    let (tx, mut rx) = mpsc::channel::<LogRecord>(1024);
    tokio::spawn(async move {
        while let Some(rec) = rx.recv().await {
            let res = db.call(move |c| insert_record(c, &rec)).await;
            if let Err(e) = res {
                tracing::error!("failed to write log record: {e}");
            }
        }
    });
    LogWriter(tx)
}

/// Deletes rows older than `max_age_secs`, then oldest-first until the DB fits
/// `max_db_bytes`. Returns rows deleted.
pub async fn prune(db: &Db, max_age_secs: Option<i64>, max_db_bytes: Option<i64>) -> Result<u64> {
    db.call(move |c| {
        let mut deleted = 0u64;
        if let Some(age) = max_age_secs {
            let cutoff = now() - age;
            deleted += c.execute("DELETE FROM requests WHERE ts < ?1", [cutoff])? as u64;
            c.execute("DELETE FROM usage_hourly WHERE hour < ?1", [cutoff])?;
        }
        if let Some(max_bytes) = max_db_bytes {
            loop {
                let size: i64 = c.query_row(
                    "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                    [],
                    |r| r.get(0),
                )?;
                if size <= max_bytes {
                    break;
                }
                let n = c.execute(
                    "DELETE FROM requests WHERE id IN
                       (SELECT id FROM requests ORDER BY ts ASC LIMIT 200)",
                    [],
                )?;
                if n == 0 {
                    break;
                }
                deleted += n as u64;
                // reclaim pages so the size check sees progress
                c.execute_batch("PRAGMA incremental_vacuum;")?;
            }
        }
        Ok(deleted)
    })
    .await
}

pub async fn get_setting(db: &Db, key: &str) -> Result<Option<String>> {
    let key = key.to_string();
    db.call(move |c| {
        let v = c
            .query_row("SELECT value FROM settings WHERE key = ?1", [key], |r| {
                r.get(0)
            })
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(v)
    })
    .await
}

pub async fn set_setting(db: &Db, key: &str, value: &str) -> Result<()> {
    let key = key.to_string();
    let value = value.to_string();
    db.call(move |c| {
        c.execute(
            "INSERT INTO settings (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            rusqlite::params![key, value],
        )?;
        Ok(())
    })
    .await
}

pub fn spawn_pruner(db: Db) {
    tokio::spawn(async move {
        loop {
            // Expired sessions are already refused at lookup, but a dead row
            // is still a row: sweep them so the table tracks live sessions
            // rather than every login since the database was made.
            let _ = db
                .call(|c| {
                    c.execute("DELETE FROM sessions WHERE expires_at < ?1", [now()])?;
                    Ok(())
                })
                .await;
            let max_age = get_setting(&db, "retention_max_age_secs")
                .await
                .ok()
                .flatten()
                .and_then(|v| v.parse::<i64>().ok());
            let max_bytes = get_setting(&db, "retention_max_db_bytes")
                .await
                .ok()
                .flatten()
                .and_then(|v| v.parse::<i64>().ok());
            if max_age.is_some() || max_bytes.is_some() {
                match prune(&db, max_age, max_bytes).await {
                    Ok(n) if n > 0 => tracing::info!("retention pruned {n} request records"),
                    Ok(_) => {}
                    Err(e) => tracing::error!("retention pruning failed: {e}"),
                }
            }
            tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
        }
    });
}

/// Returns the decompressed (request, response) bodies for a log row.
pub async fn get_bodies(db: &Db, id: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
    let id = id.to_string();
    db.call(move |c| {
        let row = c
            .query_row(
                "SELECT req_body, resp_body FROM requests WHERE id = ?1",
                [id],
                |r| Ok((r.get::<_, Vec<u8>>(0)?, r.get::<_, Vec<u8>>(1)?)),
            )
            .map(Some)
            .or_else(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => Ok(None),
                e => Err(e),
            })?;
        Ok(row.map(|(rq, rs)| {
            (
                zstd::decode_all(&rq[..]).unwrap_or(rq),
                zstd::decode_all(&rs[..]).unwrap_or(rs),
            )
        }))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::testdb;

    fn rec(ts: i64) -> LogRecord {
        LogRecord {
            id: ulid::Ulid::new().to_string(),
            ts,
            key_id: Some(1),
            model: Some("m".into()),
            endpoint: "/v1/chat/completions".into(),
            status: 200,
            latency_ms: 50,
            ttfb_ms: Some(10),
            usage: Usage {
                prompt_tokens: 5,
                completion_tokens: 7,
            },
            usage_source: "upstream",
            error: None,
            req_headers: "{}".into(),
            resp_headers: "{}".into(),
            req_body: b"request-payload".to_vec(),
            resp_body: b"response-payload".to_vec(),
        }
    }

    async fn count(db: &Db) -> i64 {
        db.call(|c| Ok(c.query_row("SELECT count(*) FROM requests", [], |r| r.get(0))?))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn write_rollup_and_roundtrip() {
        let db = testdb().await;
        let w = spawn_writer(db.clone());
        w.send(rec(now()));
        for _ in 0..100 {
            if count(&db).await == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(count(&db).await, 1);
        let (pt, n): (i64, i64) = db
            .call(|c| {
                Ok(c.query_row(
                    "SELECT prompt_tokens, n_requests FROM usage_hourly",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )?)
            })
            .await
            .unwrap();
        assert_eq!((pt, n), (5, 1));
        let id: String = db
            .call(|c| Ok(c.query_row("SELECT id FROM requests", [], |r| r.get(0))?))
            .await
            .unwrap();
        let (rq, rs) = get_bodies(&db, &id).await.unwrap().unwrap();
        assert_eq!(rq, b"request-payload");
        assert_eq!(rs, b"response-payload");
    }

    #[tokio::test]
    async fn prune_by_age() {
        let db = testdb().await;
        for _ in 0..3 {
            let r = rec(now() - 100_000);
            db.call(move |c| insert_record(c, &r)).await.unwrap();
        }
        let fresh = rec(now());
        db.call(move |c| insert_record(c, &fresh)).await.unwrap();
        let n = prune(&db, Some(86_400), None).await.unwrap();
        assert_eq!(n, 3);
        assert_eq!(count(&db).await, 1);
    }

    #[tokio::test]
    async fn settings_roundtrip() {
        let db = testdb().await;
        assert!(get_setting(&db, "x").await.unwrap().is_none());
        set_setting(&db, "x", "42").await.unwrap();
        assert_eq!(get_setting(&db, "x").await.unwrap().unwrap(), "42");
    }
}
