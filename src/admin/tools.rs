//! The Tools pages: which tools were called, by whom, on which model, and
//! what went in and came out of each call.
//!
//! Counts come from `tool_usage_hourly`, which outlives payload retention the
//! way `usage_hourly` does; the raw calls come from `tool_calls`, which goes
//! with the request bodies.

use super::logs::render_payload;
use super::pages::{err500, fmt_ts, key_rows, layout};
use super::usage::{self, Filter, Range};
use crate::db::Db;
use crate::proxy::AppState;
use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    response::{Html, IntoResponse, Response},
};
use maud::{html, Markup};
use serde::Deserialize;
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

    // tool -> (total, per key, per model)
    type Split = HashMap<String, i64>;
    let mut tools: HashMap<String, (i64, Split, Split)> = HashMap::new();
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

#[derive(Deserialize, Default, Clone)]
pub struct ToolsQuery {
    pub range: Option<String>,
    pub key: Option<i64>,
    pub model: Option<String>,
    pub page: Option<i64>,
}

impl ToolsQuery {
    fn filter(&self) -> Filter {
        Filter {
            key_id: self.key,
            model: self.model.clone().filter(|m| !m.is_empty()),
        }
    }

    /// The query string for the same filters at another page.
    fn query_string(&self, page: i64) -> String {
        let mut parts = vec![format!("range={}", usage::range(self.range.as_deref()).key)];
        if let Some(k) = self.key {
            parts.push(format!("key={k}"));
        }
        if let Some(m) = self.model.as_deref().filter(|m| !m.is_empty()) {
            parts.push(format!("model={}", urlencoding::encode(m)));
        }
        if page > 0 {
            parts.push(format!("page={page}"));
        }
        parts.join("&")
    }
}

fn tool_href(tool: &str) -> String {
    format!("/admin/tools/{}", urlencoding::encode(tool))
}

/// The filter bar both pages share. `action` is where it submits to, so the
/// detail page keeps its tool.
async fn filters(st: &AppState, q: &ToolsQuery, action: &str) -> Result<Markup> {
    let keys = key_rows(&st.db).await?;
    let models = crate::registry::list_models(&st.db).await?;
    let current = usage::range(q.range.as_deref());
    Ok(html! {
        form class="filters" method="get" action=(action) {
            div {
                label { "Range" }
                select name="range" {
                    @for r in usage::RANGES {
                        option value=(r.key) selected[r.key == current.key] { (r.label) }
                    }
                }
            }
            div {
                label { "Key" }
                select name="key" {
                    option value="" { "any" }
                    @for k in &keys {
                        option value=(k.id) selected[q.key == Some(k.id)] { (k.name) }
                    }
                }
            }
            div {
                label { "Model" }
                select name="model" {
                    option value="" { "any" }
                    @for m in &models {
                        option value=(m.alias) selected[q.model.as_deref() == Some(m.alias.as_str())] { (m.alias) }
                    }
                }
            }
            button type="submit" { "Filter" }
        }
    })
}

/// `name count` pairs, busiest first, as one compact line.
fn split(pairs: &[(String, i64)]) -> Markup {
    html! {
        @for (i, (name, n)) in pairs.iter().enumerate() {
            @if i > 0 { ", " }
            span { (name) " " span class="muted" { (usage::fmt_count(*n)) } }
        }
    }
}

pub async fn tools_page(State(st): State<AppState>, Query(q): Query<ToolsQuery>) -> Response {
    let r = usage::range(q.range.as_deref());
    let rows = match overview(&st.db, r, &q.filter()).await {
        Ok(rows) => rows,
        Err(e) => return err500(e),
    };
    let filter_bar = match filters(&st, &q, "/admin/tools").await {
        Ok(m) => m,
        Err(e) => return err500(e),
    };
    // Carry the filters into the detail links so a click narrows, not resets.
    let carry = q.query_string(0);
    let body = html! {
        (filter_bar)
        div class="card" {
            table class="grid" {
                thead { tr { th { "Tool" } th { "Calls" } th { "By key" } th { "By model" } } }
                tbody {
                    @if rows.is_empty() {
                        tr { td colspan="4" class="muted" {
                            "No tool calls in this range. Tool calls are recorded from chat completions whose backend returns structured "
                            code { "tool_calls" } "."
                        } }
                    }
                    @for row in &rows {
                        tr class="clickable" data-href=(format!("{}?{carry}", tool_href(&row.tool))) {
                            td class="mono" { a href=(format!("{}?{carry}", tool_href(&row.tool))) { (row.tool) } }
                            td { (usage::fmt_count(row.calls)) }
                            td { (split(&row.by_key)) }
                            td { (split(&row.by_model)) }
                        }
                    }
                }
            }
        }
    };
    Html(layout("Tools", "tools", body).into_string()).into_response()
}

const PREVIEW_CHARS: usize = 120;

fn preview(s: &str) -> String {
    let flat: String = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > PREVIEW_CHARS {
        let cut: String = flat.chars().take(PREVIEW_CHARS).collect();
        format!("{cut}…")
    } else {
        flat
    }
}

/// A link to a log entry, or the bare id once retention has removed it.
fn log_link(id: &str, logged: bool) -> Markup {
    html! {
        @if logged {
            a class="mono" href=(format!("/admin/logs/{id}")) { (id) }
        } @else {
            span class="mono muted" title="pruned by retention" { (id) }
        }
    }
}

pub async fn tool_detail(
    State(st): State<AppState>,
    Path(tool): Path<String>,
    Query(q): Query<ToolsQuery>,
) -> Response {
    let r = usage::range(q.range.as_deref());
    let page = q.page.unwrap_or(0).max(0);
    let rows = match calls(&st.db, &tool, r, &q.filter(), page).await {
        Ok(rows) => rows,
        Err(e) => return err500(e),
    };
    let here = tool_href(&tool);
    let filter_bar = match filters(&st, &q, &here).await {
        Ok(m) => m,
        Err(e) => return err500(e),
    };
    let body = html! {
        p { a href=(format!("/admin/tools?{}", q.query_string(0))) { "← All tools" } }
        (filter_bar)
        div class="card" {
            table class="grid" {
                thead { tr { th { "Time" } th { "Key" } th { "Model" } th { "Call" } } }
                tbody {
                    @if rows.is_empty() { tr { td colspan="4" class="muted" { "No calls of this tool in this range." } } }
                    @for c in &rows {
                        tr {
                            td { (fmt_ts(c.ts)) }
                            td { (c.key_name.clone().unwrap_or_else(|| "—".into())) }
                            td { (c.model.clone().unwrap_or_else(|| "—".into())) }
                            td {
                                details {
                                    summary {
                                        span class="mono" { (preview(&c.arguments)) }
                                        " → "
                                        @match &c.result {
                                            Some(res) => {
                                                @if let Some(s) = c.result_status.filter(|s| *s >= 400) {
                                                    span class="badge badge-warning" { (s) } " "
                                                }
                                                span class="mono" { (preview(res)) }
                                            }
                                            None => span class="muted" { "no result" },
                                        }
                                    }
                                    h3 { "Arguments" }
                                    (render_payload(c.arguments.as_bytes(), &format!("/admin/logs/{}/download", c.request_id)))
                                    p class="hint" { "Called in " (log_link(&c.request_id, c.request_logged)) " · id " span class="mono" { (c.call_id) } }
                                    h3 { "Result" }
                                    @match (&c.result, &c.result_request_id) {
                                        (Some(res), Some(rid)) => {
                                            (render_payload(res.as_bytes(), &format!("/admin/logs/{rid}/download")))
                                            p class="hint" {
                                                "Sent back in " (log_link(rid, c.result_logged))
                                                @if let Some(s) = c.result_status.filter(|s| *s >= 400) {
                                                    ", which failed with status " (s)
                                                }
                                            }
                                        }
                                        _ => p class="muted" { "no result — the client never sent one back" },
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
        div class="row" {
            @if page > 0 {
                a class="btn" href=(format!("{here}?{}", q.query_string(page - 1))) { "← Newer" }
            }
            @if rows.len() as i64 == CALLS_PAGE {
                a class="btn" href=(format!("{here}?{}", q.query_string(page + 1))) { "Older →" }
            }
        }
    };
    Html(layout(&format!("Tool: {tool}"), "tools", body).into_string()).into_response()
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
