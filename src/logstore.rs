use crate::db::{now, Db};
use crate::metering::Usage;
use crate::toolcalls::{self, Extracted};
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

pub(crate) fn compress(data: &[u8]) -> Vec<u8> {
    zstd::encode_all(data, 3).unwrap_or_else(|_| data.to_vec())
}

pub(crate) fn insert_record(
    c: &mut rusqlite::Connection,
    rec: &LogRecord,
    tools: &Extracted,
) -> Result<()> {
    let mut tx = c.transaction()?;
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
    // Under a savepoint so a tool-call failure rolls back only itself: the
    // request row and its usage are the record, tool calls are a reading of it.
    {
        let mut sp = tx.savepoint()?;
        match toolcalls::store(&sp, rec, tools) {
            Ok(()) => sp.commit()?,
            Err(e) => {
                tracing::warn!("tool-call extraction for {} skipped: {e}", rec.id);
                sp.rollback()?;
            }
        }
    }
    tx.commit()?;
    Ok(())
}

pub fn spawn_writer(db: Db) -> LogWriter {
    let (tx, mut rx) = mpsc::channel::<LogRecord>(1024);
    tokio::spawn(async move {
        while let Some(rec) = rx.recv().await {
            // Parsed before taking the connection: the writer shares it with key
            // auth on the hot path, and parsing a large body is the slow part.
            let tools = toolcalls::extract(&rec);
            let res = db.call(move |c| insert_record(c, &rec, &tools)).await;
            if let Err(e) = res {
                tracing::error!("failed to write log record: {e}");
            }
        }
    });
    LogWriter(tx)
}

/// Deletes request rows older than `max_age_secs`, then oldest-first until the
/// DB fits `max_db_bytes`. Returns rows deleted.
///
/// The hourly usage rollup has its own, separate age limit. It used to share
/// the request cutoff, which meant a week of body retention also threw away
/// every statistic older than a week — and the rollup is a few dozen bytes an
/// hour, so there is no reason for it to go when the payloads do.
/// `usage_max_age_secs` of `None` keeps it forever.
pub async fn prune(
    db: &Db,
    max_age_secs: Option<i64>,
    max_db_bytes: Option<i64>,
    usage_max_age_secs: Option<i64>,
) -> Result<u64> {
    db.call(move |c| {
        let mut deleted = 0u64;
        if let Some(age) = max_age_secs {
            let cutoff = now() - age;
            deleted += c.execute("DELETE FROM requests WHERE ts < ?1", [cutoff])? as u64;
            // Tool calls carry payload (arguments, results) and go with it.
            c.execute("DELETE FROM tool_calls WHERE ts < ?1", [cutoff])?;
        }
        if let Some(age) = usage_max_age_secs {
            c.execute("DELETE FROM usage_hourly WHERE hour < ?1", [now() - age])?;
            c.execute(
                "DELETE FROM tool_usage_hourly WHERE hour < ?1",
                [now() - age],
            )?;
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
                c.execute(
                    "DELETE FROM tool_calls WHERE request_id NOT IN (SELECT id FROM requests)",
                    [],
                )?;
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
            let usage_max_age = get_setting(&db, "retention_usage_max_age_secs")
                .await
                .ok()
                .flatten()
                .and_then(|v| v.parse::<i64>().ok());
            if max_age.is_some() || max_bytes.is_some() || usage_max_age.is_some() {
                match prune(&db, max_age, max_bytes, usage_max_age).await {
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
    use serde_json::json;

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

    /// A chat-completions record whose bodies are the given JSON.
    pub(crate) fn chat_rec(
        id: &str,
        ts: i64,
        key: i64,
        status: u16,
        req: serde_json::Value,
        resp: serde_json::Value,
    ) -> LogRecord {
        LogRecord {
            id: id.into(),
            key_id: Some(key),
            endpoint: "/v1/chat/completions".into(),
            status,
            req_body: req.to_string().into_bytes(),
            resp_body: resp.to_string().into_bytes(),
            ..rec(ts)
        }
    }

    pub(crate) async fn log(db: &Db, r: LogRecord) {
        let x = toolcalls::extract(&r);
        db.call(move |c| insert_record(c, &r, &x)).await.unwrap();
    }

    fn emits(calls: &[(&str, &str)]) -> serde_json::Value {
        let tcs: Vec<_> = calls
            .iter()
            .map(|(id, t)| json!({"id": id, "type": "function", "function": {"name": t, "arguments": "{}"}}))
            .collect();
        json!({"choices": [{"message": {"tool_calls": tcs}}]})
    }

    /// A request carrying the assistant turn that issued `calls` and one tool
    /// message per `(id, content)`.
    fn answers(calls: &[(&str, &str)], results: &[(&str, &str)]) -> serde_json::Value {
        let mut messages = vec![json!({"role": "user", "content": "go"})];
        messages.push(emits(calls)["choices"][0]["message"].clone());
        messages[1]["role"] = json!("assistant");
        for (id, content) in results {
            messages.push(json!({"role": "tool", "tool_call_id": id, "content": content}));
        }
        json!({"messages": messages})
    }

    /// (request_id, result, result_request_id, result_status) per stored call.
    async fn stored(db: &Db) -> Vec<(String, Option<String>, Option<String>, Option<u16>)> {
        db.call(|c| {
            let mut st = c.prepare(
                "SELECT request_id, result, result_request_id, result_status
                 FROM tool_calls ORDER BY ts, request_id",
            )?;
            let rows = st
                .query_map([], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, Option<Vec<u8>>>(1)?,
                        r.get(2)?,
                        r.get(3)?,
                    ))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .map(|(id, res, rid, rs)| {
                    let res =
                        res.map(|b| String::from_utf8(zstd::decode_all(&b[..]).unwrap()).unwrap());
                    (id, res, rid, rs)
                })
                .collect())
        })
        .await
        .unwrap()
    }

    async fn scalar(db: &Db, sql: &'static str) -> i64 {
        db.call(move |c| Ok(c.query_row(sql, [], |r| r.get(0))?))
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn call_then_result_is_matched() {
        let db = testdb().await;
        let t = now();
        log(
            &db,
            chat_rec(
                "A",
                t,
                1,
                200,
                json!({"messages": []}),
                emits(&[("c1", "t")]),
            ),
        )
        .await;
        log(
            &db,
            chat_rec(
                "B",
                t + 1,
                1,
                200,
                answers(&[("c1", "t")], &[("c1", "out")]),
                json!({}),
            ),
        )
        .await;
        assert_eq!(
            stored(&db).await,
            vec![("A".into(), Some("out".into()), Some("B".into()), Some(200))]
        );
        assert_eq!(
            scalar(&db, "SELECT SUM(n_calls) FROM tool_usage_hourly").await,
            1
        );
    }

    #[tokio::test]
    async fn resent_history_does_not_overwrite() {
        let db = testdb().await;
        let t = now();
        log(
            &db,
            chat_rec(
                "A",
                t,
                1,
                200,
                json!({"messages": []}),
                emits(&[("c1", "t")]),
            ),
        )
        .await;
        log(
            &db,
            chat_rec(
                "B",
                t + 1,
                1,
                200,
                answers(&[("c1", "t")], &[("c1", "out")]),
                json!({}),
            ),
        )
        .await;
        log(
            &db,
            chat_rec(
                "C",
                t + 2,
                1,
                200,
                answers(&[("c1", "t")], &[("c1", "changed")]),
                json!({}),
            ),
        )
        .await;
        let s = stored(&db).await;
        assert_eq!(s[0].1.as_deref(), Some("out"));
        assert_eq!(s[0].2.as_deref(), Some("B"));
    }

    #[tokio::test]
    async fn reused_ids_pick_newest_open_call() {
        let db = testdb().await;
        let t = now();
        log(
            &db,
            chat_rec(
                "A",
                t,
                1,
                200,
                json!({"messages": []}),
                emits(&[("call_0", "t")]),
            ),
        )
        .await;
        log(
            &db,
            chat_rec(
                "B",
                t + 1,
                1,
                200,
                answers(&[("call_0", "t")], &[("call_0", "first")]),
                emits(&[("call_0", "t")]),
            ),
        )
        .await;
        // C resends the old turn and answers the new call, both under `call_0`.
        let mut c_req = answers(&[("call_0", "t")], &[("call_0", "first")]);
        let msgs = c_req["messages"].as_array_mut().unwrap();
        msgs.push(json!({"role": "assistant", "tool_calls": [{"id": "call_0", "function": {"name": "t", "arguments": "{}"}}]}));
        msgs.push(json!({"role": "tool", "tool_call_id": "call_0", "content": "second"}));
        log(&db, chat_rec("C", t + 2, 1, 200, c_req, json!({}))).await;
        let s = stored(&db).await;
        assert_eq!((s[0].0.as_str(), s[0].1.as_deref()), ("A", Some("first")));
        assert_eq!((s[1].0.as_str(), s[1].1.as_deref()), ("B", Some("second")));
    }

    #[tokio::test]
    async fn other_key_cannot_match() {
        let db = testdb().await;
        let t = now();
        log(
            &db,
            chat_rec(
                "A",
                t,
                1,
                200,
                json!({"messages": []}),
                emits(&[("c1", "t")]),
            ),
        )
        .await;
        log(
            &db,
            chat_rec(
                "B",
                t + 1,
                2,
                200,
                answers(&[("c1", "t")], &[("c1", "out")]),
                json!({}),
            ),
        )
        .await;
        assert_eq!(stored(&db).await[0].1, None);
    }

    #[tokio::test]
    async fn failed_request_result_recorded() {
        let db = testdb().await;
        let t = now();
        log(
            &db,
            chat_rec(
                "A",
                t,
                1,
                200,
                json!({"messages": []}),
                emits(&[("c1", "t")]),
            ),
        )
        .await;
        log(
            &db,
            chat_rec(
                "B",
                t + 1,
                1,
                400,
                answers(&[("c1", "t")], &[("c1", "huge")]),
                json!({"error": {}}),
            ),
        )
        .await;
        let s = stored(&db).await;
        assert_eq!((s[0].1.as_deref(), s[0].3), (Some("huge"), Some(400)));
    }

    #[tokio::test]
    async fn garbage_bodies_still_log() {
        let db = testdb().await;
        let mut r = chat_rec("A", now(), 1, 200, json!({}), json!({}));
        r.req_body = b"not json".to_vec();
        log(&db, r).await;
        log(
            &db,
            chat_rec(
                "B",
                now(),
                1,
                200,
                json!({"messages": 5}),
                json!({"choices": 5}),
            ),
        )
        .await;
        assert_eq!(count(&db).await, 2);
        assert_eq!(
            scalar(&db, "SELECT SUM(n_requests) FROM usage_hourly").await,
            2
        );
        assert_eq!(scalar(&db, "SELECT count(*) FROM tool_calls").await, 0);
    }

    #[tokio::test]
    async fn store_failure_keeps_log_row() {
        let db = testdb().await;
        db.call(|c| Ok(c.execute_batch("DROP TABLE tool_calls")?))
            .await
            .unwrap();
        log(
            &db,
            chat_rec(
                "A",
                now(),
                1,
                200,
                json!({"messages": []}),
                emits(&[("c1", "t")]),
            ),
        )
        .await;
        assert_eq!(count(&db).await, 1);
        assert_eq!(
            scalar(&db, "SELECT SUM(n_requests) FROM usage_hourly").await,
            1
        );
    }

    #[tokio::test]
    async fn age_prune_covers_tool_calls() {
        let db = testdb().await;
        let old = now() - 30 * 86_400;
        log(
            &db,
            chat_rec(
                "A",
                old,
                1,
                200,
                json!({"messages": []}),
                emits(&[("c1", "t")]),
            ),
        )
        .await;
        log(
            &db,
            chat_rec(
                "B",
                now(),
                1,
                200,
                json!({"messages": []}),
                emits(&[("c2", "t")]),
            ),
        )
        .await;
        prune(&db, Some(7 * 86_400), None, None).await.unwrap();
        assert_eq!(scalar(&db, "SELECT count(*) FROM tool_calls").await, 1);
        assert_eq!(
            scalar(&db, "SELECT count(*) FROM tool_usage_hourly").await,
            2
        );
        prune(&db, None, None, Some(7 * 86_400)).await.unwrap();
        assert_eq!(
            scalar(&db, "SELECT count(*) FROM tool_usage_hourly").await,
            1
        );
    }

    #[tokio::test]
    async fn size_prune_drops_orphans() {
        let db = testdb().await;
        let t = now();
        log(
            &db,
            chat_rec(
                "A",
                t,
                1,
                200,
                json!({"messages": []}),
                emits(&[("c1", "t")]),
            ),
        )
        .await;
        prune(&db, None, Some(1), None).await.unwrap();
        assert_eq!(count(&db).await, 0);
        assert_eq!(scalar(&db, "SELECT count(*) FROM tool_calls").await, 0);
        // A late answer to the pruned call finds nothing and breaks nothing.
        log(
            &db,
            chat_rec(
                "B",
                t + 1,
                1,
                200,
                answers(&[("c1", "t")], &[("c1", "out")]),
                json!({}),
            ),
        )
        .await;
        assert_eq!(count(&db).await, 1);
        assert_eq!(scalar(&db, "SELECT count(*) FROM tool_calls").await, 0);
    }

    #[tokio::test]
    async fn statistics_outlive_pruned_payloads() {
        let db = testdb().await;
        let old = now() - 30 * 86_400;
        db.call(move |c| insert_record(c, &rec(old), &Extracted::default()))
            .await
            .unwrap();
        db.call(|c| insert_record(c, &rec(now()), &Extracted::default()))
            .await
            .unwrap();

        // A week of payload retention, no limit on the rollup.
        prune(&db, Some(7 * 86_400), None, None).await.unwrap();
        assert_eq!(count(&db).await, 1, "the old payload is gone");
        let hours: i64 = db
            .call(|c| Ok(c.query_row("SELECT count(*) FROM usage_hourly", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(hours, 2, "both hours of statistics survive");

        // Given its own limit, the rollup is pruned too.
        prune(&db, None, None, Some(7 * 86_400)).await.unwrap();
        let hours: i64 = db
            .call(|c| Ok(c.query_row("SELECT count(*) FROM usage_hourly", [], |r| r.get(0))?))
            .await
            .unwrap();
        assert_eq!(hours, 1);
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
            db.call(move |c| insert_record(c, &r, &Extracted::default()))
                .await
                .unwrap();
        }
        let fresh = rec(now());
        db.call(move |c| insert_record(c, &fresh, &Extracted::default()))
            .await
            .unwrap();
        let n = prune(&db, Some(86_400), None, None).await.unwrap();
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
