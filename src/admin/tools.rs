//! The Tools pages: which tools were called, by whom, on which model, and
//! what went in and came out of each call.
//!
//! Counts come from `tool_usage_hourly`, which outlives payload retention the
//! way `usage_hourly` does; the raw calls come from `tool_calls`, which goes
//! with the request bodies.

use super::usage::{Filter, Range};
use crate::db::Db;
use anyhow::Result;
use std::collections::HashMap;

pub struct ToolRow {
    pub tool: String,
    pub calls: i64,
    pub by_key: Vec<(String, i64)>,
    pub by_model: Vec<(String, i64)>,
}

pub struct CallRow {
    pub request_id: String,
    pub call_id: String,
    pub ts: i64,
    pub key_name: Option<String>,
    pub model: Option<String>,
    pub arguments: String,
    pub result: Option<String>,
    pub result_request_id: Option<String>,
    pub result_status: Option<u16>,
    pub request_logged: bool,
    pub result_logged: bool,
}

pub const CALLS_PAGE: i64 = 50;

/// Calls per tool over the window, busiest first, each with its split by key
/// and by model. One `GROUP BY` over the rollup, folded here.
pub async fn overview(db: &Db, r: Range, f: &Filter) -> Result<Vec<ToolRow>> {
    let since = r.since(db).await?;
    let binds = f.bind(since);
    // Same placeholders as the usage breakdown: `key_id = -1` is traffic
    // without a key, a missing join is a key deleted since.
    let sql = format!(
        "SELECT u.tool,
                COALESCE(k.name, CASE WHEN u.key_id < 0 THEN '(no key)' ELSE '(deleted key)' END),
                COALESCE(NULLIF(u.model, ''), '(none)'),
                SUM(u.n_calls)
         FROM tool_usage_hourly u LEFT JOIN api_keys k ON k.id = u.key_id
         WHERE u.hour >= :since{}
         GROUP BY u.tool, u.key_id, u.model",
        f.sql("u.")
    );
    let cells: Vec<(String, String, String, i64)> = db
        .call(move |c| {
            let mut stmt = c.prepare(&sql)?;
            let params: Vec<(&str, &dyn rusqlite::ToSql)> = binds
                .iter()
                .map(|(n, v)| (*n, v.as_ref() as &dyn rusqlite::ToSql))
                .collect();
            let rows = stmt.query_map(params.as_slice(), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await?;

    let mut tools: HashMap<String, (i64, HashMap<String, i64>, HashMap<String, i64>)> =
        HashMap::new();
    for (tool, key, model, n) in cells {
        let e = tools.entry(tool).or_default();
        e.0 += n;
        *e.1.entry(key).or_default() += n;
        *e.2.entry(model).or_default() += n;
    }
    let busiest_first = |m: HashMap<String, i64>| {
        let mut v: Vec<(String, i64)> = m.into_iter().collect();
        v.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        v
    };
    let mut rows: Vec<ToolRow> = tools
        .into_iter()
        .map(|(tool, (calls, by_key, by_model))| ToolRow {
            tool,
            calls,
            by_key: busiest_first(by_key),
            by_model: busiest_first(by_model),
        })
        .collect();
    rows.sort_by(|a, b| b.calls.cmp(&a.calls).then_with(|| a.tool.cmp(&b.tool)));
    Ok(rows)
}

fn unpack(blob: Option<Vec<u8>>) -> Option<String> {
    blob.map(|b| {
        let raw = zstd::decode_all(&b[..]).unwrap_or(b);
        String::from_utf8_lossy(&raw).into_owned()
    })
}

/// One tool's calls in the window, newest first, a page at a time.
pub async fn calls(db: &Db, tool: &str, r: Range, f: &Filter, page: i64) -> Result<Vec<CallRow>> {
    let since = r.since(db).await?;
    let mut binds = f.bind(since);
    binds.push((":tool", Box::new(tool.to_string())));
    let sql = format!(
        "SELECT t.request_id, t.call_id, t.ts, k.name, t.model, t.arguments, t.result,
                t.result_request_id, t.result_status,
                EXISTS (SELECT 1 FROM requests WHERE id = t.request_id),
                EXISTS (SELECT 1 FROM requests WHERE id = t.result_request_id)
         FROM tool_calls t LEFT JOIN api_keys k ON k.id = t.key_id
         WHERE t.tool = :tool AND t.ts >= :since{}
         ORDER BY t.ts DESC, t.rowid DESC
         LIMIT {CALLS_PAGE} OFFSET {}",
        f.sql("t."),
        page.max(0) * CALLS_PAGE
    );
    db.call(move |c| {
        let mut stmt = c.prepare(&sql)?;
        let params: Vec<(&str, &dyn rusqlite::ToSql)> = binds
            .iter()
            .map(|(n, v)| (*n, v.as_ref() as &dyn rusqlite::ToSql))
            .collect();
        let rows = stmt.query_map(params.as_slice(), |r| {
            Ok(CallRow {
                request_id: r.get(0)?,
                call_id: r.get(1)?,
                ts: r.get(2)?,
                key_name: r.get(3)?,
                model: r.get(4)?,
                arguments: unpack(r.get(5)?).unwrap_or_default(),
                result: unpack(r.get(6)?),
                result_request_id: r.get(7)?,
                result_status: r.get(8)?,
                request_logged: r.get(9)?,
                result_logged: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::admin::usage::range;
    use crate::db::now;
    use crate::keys::testdb;
    use crate::logstore::tests::{answers, chat_rec, emits, log};
    use serde_json::json;

    async fn seed_call(db: &Db, id: &str, ts: i64, key: i64, model: &str, tool: &str) {
        let mut r = chat_rec(
            id,
            ts,
            key,
            200,
            json!({"messages": []}),
            emits(&[(id, tool)]),
        );
        r.model = Some(model.into());
        log(db, r).await;
    }

    #[tokio::test]
    async fn overview_counts_by_tool_key_model() {
        let db = testdb().await;
        let t = now();
        seed_call(&db, "1", t, 1, "m1", "a").await;
        seed_call(&db, "2", t, 1, "m1", "a").await;
        seed_call(&db, "3", t, 2, "m2", "a").await;
        seed_call(&db, "4", t, 1, "m1", "b").await;
        let rows = overview(&db, range(Some("24h")), &Filter::default())
            .await
            .unwrap();
        let summary: Vec<(&str, i64)> = rows.iter().map(|r| (r.tool.as_str(), r.calls)).collect();
        assert_eq!(summary, vec![("a", 3), ("b", 1)]);
        assert_eq!(rows[0].by_key.iter().map(|(_, n)| n).sum::<i64>(), 3);
        assert_eq!(
            rows[0].by_model,
            vec![("m1".to_string(), 2), ("m2".to_string(), 1)]
        );
    }

    #[tokio::test]
    async fn overview_respects_filter() {
        let db = testdb().await;
        let t = now();
        seed_call(&db, "1", t, 1, "m1", "a").await;
        seed_call(&db, "2", t, 1, "m1", "b").await;
        seed_call(&db, "3", t, 2, "m2", "a").await;
        let f = Filter {
            key_id: Some(2),
            model: None,
        };
        let rows = overview(&db, range(Some("24h")), &f).await.unwrap();
        let summary: Vec<(&str, i64)> = rows.iter().map(|r| (r.tool.as_str(), r.calls)).collect();
        assert_eq!(summary, vec![("a", 1)]);
    }

    #[tokio::test]
    async fn calls_newest_first_with_result() {
        let db = testdb().await;
        let t = now();
        seed_call(&db, "old", t - 10, 1, "m1", "a").await;
        seed_call(&db, "new", t - 5, 1, "m1", "a").await;
        log(
            &db,
            chat_rec(
                "ans",
                t,
                1,
                200,
                answers(&[("old", "a")], &[("old", "fine")]),
                json!({}),
            ),
        )
        .await;
        let rows = calls(&db, "a", range(Some("24h")), &Filter::default(), 0)
            .await
            .unwrap();
        let ids: Vec<&str> = rows.iter().map(|r| r.request_id.as_str()).collect();
        assert_eq!(ids, vec!["new", "old"]);
        assert_eq!(rows[0].result, None);
        assert_eq!(rows[1].result.as_deref(), Some("fine"));
        assert_eq!(rows[1].result_request_id.as_deref(), Some("ans"));
        assert_eq!(rows[1].arguments, "{}");
        assert!(rows[1].request_logged && rows[1].result_logged);
    }

    #[tokio::test]
    async fn calls_pagination() {
        let db = testdb().await;
        let t = now();
        for i in 0..51 {
            seed_call(&db, &format!("r{i}"), t - i, 1, "m1", "a").await;
        }
        let r = range(Some("24h"));
        assert_eq!(
            calls(&db, "a", r, &Filter::default(), 0)
                .await
                .unwrap()
                .len(),
            50
        );
        assert_eq!(
            calls(&db, "a", r, &Filter::default(), 1)
                .await
                .unwrap()
                .len(),
            1
        );
    }
}
