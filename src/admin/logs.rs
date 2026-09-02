use super::pages::{err500, fmt_ts, layout};
use crate::db::Db;
use crate::proxy::AppState;
use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
};
use maud::{html, Markup};
use serde::Deserialize;

const PAGE_SIZE: i64 = 100;

#[derive(Deserialize, Default, Clone)]
pub struct LogQuery {
    pub key: Option<i64>,
    pub model: Option<String>,
    pub status: Option<String>,
    pub from: Option<String>,
    pub to: Option<String>,
    pub page: Option<i64>,
}

pub struct LogSummary {
    pub id: String,
    pub ts: i64,
    pub key_name: Option<String>,
    pub model: Option<String>,
    pub endpoint: String,
    pub status: u16,
    pub latency_ms: u32,
    pub prompt_tokens: u32,
    pub completion_tokens: u32,
    pub usage_source: String,
    pub error: Option<String>,
}

fn parse_date(s: &Option<String>, end_of_day: bool) -> Option<i64> {
    let s = s.as_deref()?.trim();
    if s.is_empty() {
        return None;
    }
    let date = time::Date::parse(
        s,
        &time::macros::format_description!("[year]-[month]-[day]"),
    )
    .ok()?;
    let t = if end_of_day {
        date.with_hms(23, 59, 59).ok()?
    } else {
        date.midnight()
    };
    Some(t.assume_utc().unix_timestamp())
}

fn status_range(s: &Option<String>) -> Option<(u16, u16)> {
    match s.as_deref() {
        Some("2xx") => Some((200, 299)),
        Some("4xx") => Some((400, 499)),
        Some("5xx") => Some((500, 599)),
        _ => None,
    }
}

async fn fetch_summaries(db: &Db, q: &LogQuery) -> Result<Vec<LogSummary>> {
    let q = q.clone();
    db.call(move |c| {
        let mut sql = String::from(
            "SELECT r.id, r.ts, k.name, r.model, r.endpoint, r.status, r.latency_ms,
                    r.prompt_tokens, r.completion_tokens, r.usage_source, r.error
             FROM requests r LEFT JOIN api_keys k ON k.id = r.key_id WHERE 1=1",
        );
        let mut params: Vec<Box<dyn rusqlite::ToSql>> = Vec::new();
        if let Some(kid) = q.key {
            sql.push_str(" AND r.key_id = ?");
            params.push(Box::new(kid));
        }
        if let Some(m) = q.model.clone().filter(|m| !m.is_empty()) {
            sql.push_str(" AND r.model = ?");
            params.push(Box::new(m));
        }
        if let Some((lo, hi)) = status_range(&q.status) {
            sql.push_str(" AND r.status BETWEEN ? AND ?");
            params.push(Box::new(lo));
            params.push(Box::new(hi));
        }
        if let Some(from) = parse_date(&q.from, false) {
            sql.push_str(" AND r.ts >= ?");
            params.push(Box::new(from));
        }
        if let Some(to) = parse_date(&q.to, true) {
            sql.push_str(" AND r.ts <= ?");
            params.push(Box::new(to));
        }
        sql.push_str(" ORDER BY r.ts DESC LIMIT ? OFFSET ?");
        params.push(Box::new(PAGE_SIZE));
        params.push(Box::new(q.page.unwrap_or(0).max(0) * PAGE_SIZE));
        let mut stmt = c.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params.iter().map(|p| p.as_ref()).collect();
        let rows = stmt.query_map(refs.as_slice(), |r| {
            Ok(LogSummary {
                id: r.get(0)?,
                ts: r.get(1)?,
                key_name: r.get(2)?,
                model: r.get(3)?,
                endpoint: r.get(4)?,
                status: r.get(5)?,
                latency_ms: r.get(6)?,
                prompt_tokens: r.get(7)?,
                completion_tokens: r.get(8)?,
                usage_source: r.get(9)?,
                error: r.get(10)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

fn status_badge(status: u16) -> Markup {
    let class = match status {
        200..=299 => "badge ok",
        400..=499 => "badge warn",
        _ => "badge err",
    };
    html! { span class=(class) { (status) } }
}

fn query_string(q: &LogQuery, page: i64) -> String {
    let mut parts = vec![format!("page={page}")];
    if let Some(k) = q.key {
        parts.push(format!("key={k}"));
    }
    if let Some(m) = &q.model {
        if !m.is_empty() {
            parts.push(format!("model={}", urlencoding::encode(m)));
        }
    }
    for (name, v) in [("status", &q.status), ("from", &q.from), ("to", &q.to)] {
        if let Some(v) = v {
            if !v.is_empty() {
                parts.push(format!("{name}={}", urlencoding::encode(v)));
            }
        }
    }
    parts.join("&")
}

pub async fn logs_page(State(st): State<AppState>, Query(q): Query<LogQuery>) -> Response {
    let rows = match fetch_summaries(&st.db, &q).await {
        Ok(r) => r,
        Err(e) => return err500(e),
    };
    let keys = match super::pages::key_rows(&st.db).await {
        Ok(k) => k,
        Err(e) => return err500(e),
    };
    let models = match crate::registry::list_models(&st.db).await {
        Ok(m) => m,
        Err(e) => return err500(e),
    };
    let page = q.page.unwrap_or(0).max(0);
    let body = html! {
        form class="filters" method="get" action="/admin/logs" {
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
            div {
                label { "Status" }
                select name="status" {
                    @for (v, l) in [("", "any"), ("2xx", "2xx"), ("4xx", "4xx"), ("5xx", "5xx")] {
                        option value=(v) selected[q.status.as_deref() == Some(v)] { (l) }
                    }
                }
            }
            div { label { "From" } input type="date" name="from" value=(q.from.clone().unwrap_or_default()); }
            div { label { "To" } input type="date" name="to" value=(q.to.clone().unwrap_or_default()); }
            button type="submit" { "Filter" }
            a class="btn" href=(format!("/admin/export?format=native&{}", query_string(&q, 0))) { "Export" }
        }
        div class="panel" {
            table {
                thead { tr {
                    th { "Time" } th { "Key" } th { "Model" } th { "Endpoint" }
                    th { "Status" } th { "Latency" } th { "Tokens" } th { "Source" }
                } }
                tbody {
                    @if rows.is_empty() { tr { td colspan="8" class="muted" { "No requests match." } } }
                    @for r in &rows {
                        tr class="clickable" onclick=(format!("location.href='/admin/logs/{}'", r.id)) {
                            td { (fmt_ts(r.ts)) }
                            td { (r.key_name.clone().unwrap_or_else(|| "—".into())) }
                            td { (r.model.clone().unwrap_or_else(|| "—".into())) }
                            td class="mono" { (r.endpoint) }
                            td { (status_badge(r.status)) }
                            td { (r.latency_ms) " ms" }
                            td { (r.prompt_tokens) " / " (r.completion_tokens) }
                            td {
                                @if r.usage_source == "upstream" { span class="badge ok" { "exact" } }
                                @else { span class="badge warn" { "est." } }
                            }
                        }
                    }
                }
            }
        }
        div {
            @if page > 0 {
                a class="btn" href=(format!("/admin/logs?{}", query_string(&q, page - 1))) { "← Newer" }
                " "
            }
            @if rows.len() as i64 == PAGE_SIZE {
                a class="btn" href=(format!("/admin/logs?{}", query_string(&q, page + 1))) { "Older →" }
            }
        }
    };
    Html(layout("Request Logs", "logs", body).into_string()).into_response()
}

/// Renders a payload body for the detail view: pretty JSON, reassembled SSE
/// (with raw toggle), plain text, or a binary size note.
pub fn render_payload(body: &[u8], download_url: &str) -> Markup {
    if body.is_empty() {
        return html! { p class="muted" { "empty" } };
    }
    if let Ok(text) = std::str::from_utf8(body) {
        if text.trim_start().starts_with("data:") {
            // SSE stream: reassemble delta/content text, keep the raw bytes a click away
            let mut assembled = String::new();
            for line in text.lines() {
                let Some(payload) = line.strip_prefix("data:") else {
                    continue;
                };
                let payload = payload.trim();
                if payload == "[DONE]" || payload.is_empty() {
                    continue;
                }
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
                    for choice in v["choices"].as_array().unwrap_or(&vec![]) {
                        for field in [&choice["delta"]["content"], &choice["text"]] {
                            if let Some(s) = field.as_str() {
                                assembled.push_str(s);
                            }
                        }
                    }
                }
            }
            return html! {
                pre class="payload" { (assembled) }
                details {
                    summary class="hint" { "Raw SSE bytes" }
                    pre class="payload" { (text) }
                }
            };
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(text) {
            let pretty = serde_json::to_string_pretty(&v).unwrap_or_else(|_| text.to_string());
            return html! { pre class="payload" { (pretty) } };
        }
        return html! { pre class="payload" { (text) } };
    }
    html! {
        p class="muted" {
            "binary payload, " (body.len()) " bytes — "
            a href=(download_url) { "download" }
        }
    }
}

/// (ts, key_name, model, endpoint, status, latency, ttfb, prompt_tokens,
/// completion_tokens, usage_source, error, req_headers, resp_headers)
type DetailRow = (
    i64,
    Option<String>,
    Option<String>,
    String,
    u16,
    u32,
    Option<u32>,
    u32,
    u32,
    String,
    Option<String>,
    String,
    String,
);

pub async fn log_detail(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    let idq = id.clone();
    let row: Option<DetailRow> =
        match st
            .db
            .call(move |c| {
                let v = c
                .query_row(
                    "SELECT r.ts, k.name, r.model, r.endpoint, r.status, r.latency_ms, r.ttfb_ms,
                            r.prompt_tokens, r.completion_tokens, r.usage_source, r.error,
                            COALESCE(r.req_headers, '{}'), COALESCE(r.resp_headers, '{}')
                     FROM requests r LEFT JOIN api_keys k ON k.id = r.key_id WHERE r.id = ?1",
                    [idq],
                    |r| {
                        Ok((
                            r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?,
                            r.get(6)?, r.get(7)?, r.get(8)?, r.get(9)?, r.get(10)?, r.get(11)?,
                            r.get(12)?,
                        ))
                    },
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })?;
                Ok(v)
            })
            .await
        {
            Ok(v) => v,
            Err(e) => return err500(e),
        };
    let Some((
        ts,
        key_name,
        model,
        endpoint,
        status,
        latency,
        ttfb,
        pt,
        ct,
        src,
        error,
        req_h,
        resp_h,
    )) = row
    else {
        return (StatusCode::NOT_FOUND, "no such request").into_response();
    };
    let bodies = match crate::logstore::get_bodies(&st.db, &id).await {
        Ok(b) => b.unwrap_or_default(),
        Err(e) => return err500(e),
    };
    let pretty_headers = |raw: &str| -> String {
        serde_json::from_str::<serde_json::Value>(raw)
            .and_then(|v| serde_json::to_string_pretty(&v))
            .unwrap_or_else(|_| raw.to_string())
    };
    let body = html! {
        div class="stats-row" {
            div class="stat" { div class="num" { (status_badge(status)) } div class="cap" { "status" } }
            div class="stat" { div class="num" { (latency) " ms" } div class="cap" {
                "latency" @if let Some(t) = ttfb { " (TTFB " (t) " ms)" } } }
            div class="stat" { div class="num" { (pt) " / " (ct) } div class="cap" { "tokens (" (src) ")" } }
            div class="stat" { div class="num" { (model.clone().unwrap_or_else(|| "—".into())) }
                div class="cap" { (key_name.clone().unwrap_or_else(|| "—".into())) " · " (fmt_ts(ts)) } }
        }
        @if let Some(err) = &error {
            div class="panel" { span class="badge err" { "error" } " " (err) }
        }
        div class="panel" {
            h2 style="margin-top:0" { span class="mono" { (endpoint) } }
            h2 { "Request " a class="hint" href=(format!("/admin/logs/{id}/download?part=req")) { "(download)" } }
            (render_payload(&bodies.0, &format!("/admin/logs/{id}/download?part=req")))
            details { summary class="hint" { "Request headers" } pre class="payload" { (pretty_headers(&req_h)) } }
            h2 { "Response " a class="hint" href=(format!("/admin/logs/{id}/download?part=resp")) { "(download)" } }
            (render_payload(&bodies.1, &format!("/admin/logs/{id}/download?part=resp")))
            details { summary class="hint" { "Response headers" } pre class="payload" { (pretty_headers(&resp_h)) } }
        }
        a class="btn" href="/admin/logs" { "← Back to logs" }
    };
    Html(layout("Request Detail", "logs", body).into_string()).into_response()
}

#[derive(Deserialize)]
pub struct DownloadQuery {
    part: String,
}

pub async fn log_download(
    State(st): State<AppState>,
    Path(id): Path<String>,
    Query(q): Query<DownloadQuery>,
) -> Response {
    let bodies = match crate::logstore::get_bodies(&st.db, &id).await {
        Ok(Some(b)) => b,
        Ok(None) => return (StatusCode::NOT_FOUND, "no such request").into_response(),
        Err(e) => return err500(e),
    };
    let (data, name) = match q.part.as_str() {
        "req" => (bodies.0, format!("{id}-request.bin")),
        _ => (bodies.1, format!("{id}-response.bin")),
    };
    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (
                header::CONTENT_DISPOSITION,
                format!("attachment; filename=\"{name}\""),
            ),
        ],
        data,
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_payload_json_pretty() {
        let out = render_payload(br#"{"a":{"b":1}}"#, "/x").into_string();
        assert!(out.contains("&quot;b&quot;: 1"), "{out}");
    }

    #[test]
    fn render_payload_sse_reassembled() {
        let sse = b"data: {\"choices\":[{\"delta\":{\"content\":\"Hel\"}}]}\n\ndata: {\"choices\":[{\"delta\":{\"content\":\"lo\"}}]}\n\ndata: [DONE]\n\n";
        let out = render_payload(sse, "/x").into_string();
        assert!(out.contains("Hello"), "{out}");
        assert!(out.contains("Raw SSE bytes"), "{out}");
    }

    #[test]
    fn render_payload_binary() {
        let out = render_payload(&[0u8, 159, 146, 150], "/dl").into_string();
        assert!(out.contains("binary payload"), "{out}");
        assert!(out.contains("/dl"), "{out}");
    }
}
