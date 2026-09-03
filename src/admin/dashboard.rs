use super::pages::{backend_up, err500, layout};
use crate::admin::usage;
use crate::db::Db;
use crate::logstore::{get_setting, set_setting};
use crate::proxy::AppState;
use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::header,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use maud::{html, Markup};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct StatsQuery {
    pub range: Option<String>,
    pub key_id: Option<i64>,
    pub model: Option<String>,
}

impl StatsQuery {
    pub fn range(&self) -> usage::Range {
        usage::range(self.range.as_deref())
    }
    pub fn filter(&self) -> usage::Filter {
        usage::Filter {
            key_id: self.key_id,
            model: self.model.clone().filter(|m| !m.is_empty()),
        }
    }
}

/// The series behind a chart. Same endpoint for the dashboard and for one
/// key's page — the filter is in the query string.
pub async fn stats_api(State(st): State<AppState>, Query(q): Query<StatsQuery>) -> Response {
    match usage::series(&st.db, q.range(), &q.filter()).await {
        Ok(s) => axum::Json(s).into_response(),
        Err(e) => err500(e),
    }
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

/// The range selector, rendered as links so it works without JavaScript and
/// so a chosen range is a shareable URL.
pub fn range_picker(base: &str, current: usage::Range) -> Markup {
    html! {
        div class="range-picker" {
            @for r in usage::RANGES {
                @if r.key == current.key {
                    span class="range-option current" { (r.label) }
                } @else {
                    a class="range-option" href=(format!("{base}?range={}", r.key)) { (r.label) }
                }
            }
        }
    }
}

/// The four numbers above the charts.
fn measures(t: &usage::Totals, extra: Option<Markup>) -> Markup {
    html! {
        div class="measures" {
            div class="measure" {
                div class="measure-figure" { (usage::fmt_count(t.requests)) }
                div class="measure-sub" { "requests" }
            }
            div class="measure" {
                div class="measure-figure" { (usage::fmt_count(t.total_tokens())) }
                div class="measure-sub" { "tokens" }
            }
            div class="measure" {
                div class="measure-figure" {
                    (usage::fmt_count(t.prompt_tokens)) " / " (usage::fmt_count(t.completion_tokens))
                }
                div class="measure-sub" { "prompt / completion" }
            }
            div class="measure" {
                div class="measure-figure" { (usage::fmt_count(t.errors)) }
                div class="measure-sub" { "errors" }
            }
            @if let Some(e) = extra { (e) }
        }
    }
}

/// The two charts, wired to `/admin/api/stats` with this page's filter. The
/// URL travels in a data attribute because the CSP forbids inline script.
fn charts(stats_url: &str, bucket_secs: i64) -> Markup {
    let bucket_label = match bucket_secs {
        3600 => "hour",
        86_400 => "day",
        _ => "week",
    };
    html! {
        div class="card" data-stats-url=(stats_url) {
            h2 { "Requests per " (bucket_label) }
            div class="chart-wrap" { div id="chart-req" {} }
            h2 { "Tokens per " (bucket_label) }
            div class="chart-wrap" { div id="chart-tok" {} }
        }
        link rel="stylesheet" href="/admin/assets/uplot.min.css";
        script src="/admin/assets/uplot.iife.min.js" {}
        script src="/admin/assets/dashboard.js" defer {}
    }
}

/// A leaderboard table. `link` turns the name into a link to that row's own
/// statistics page when there is one.
fn breakdown_table(title: &str, rows: &[usage::Row], link: Option<&str>) -> Markup {
    html! {
        div class="card" {
            h2 { (title) }
            table class="grid" {
                thead { tr {
                    th { "Name" } th { "Requests" } th { "Prompt" }
                    th { "Completion" } th { "Total" } th { "Errors" }
                } }
                tbody {
                    @if rows.is_empty() {
                        tr { td colspan="6" class="muted" { "No usage recorded yet." } }
                    }
                    @for r in rows {
                        tr {
                            td {
                                @match (link, r.id) {
                                    (Some(base), Some(id)) => a href=(format!("{base}{id}")) { (r.name) },
                                    _ => span { (r.name) },
                                }
                            }
                            td { (usage::fmt_count(r.requests)) }
                            td { (usage::fmt_count(r.prompt_tokens)) }
                            td { (usage::fmt_count(r.completion_tokens)) }
                            td { (usage::fmt_count(r.total_tokens())) }
                            td { (usage::fmt_count(r.errors)) }
                        }
                    }
                }
            }
        }
    }
}

pub async fn dashboard(State(st): State<AppState>, Query(q): Query<StatsQuery>) -> Response {
    let r = q.range();
    let filter = usage::Filter::default();
    let totals = match usage::totals(&st.db, r, &filter).await {
        Ok(t) => t,
        Err(e) => return err500(e),
    };
    let top_models = usage::breakdown(&st.db, r, &filter, usage::By::Model, 10)
        .await
        .unwrap_or_default();
    let top_keys = usage::breakdown(&st.db, r, &filter, usage::By::Key, 10)
        .await
        .unwrap_or_default();
    let backends = crate::registry::list_backends(&st.db)
        .await
        .unwrap_or_default();
    let mut health = Vec::new();
    for b in &backends {
        health.push(backend_up(&st.http, b).await);
    }
    let health_measure = html! {
        div class="measure" {
            div class="measure-figure" {
                @for (b, up) in backends.iter().zip(&health) {
                    span class=(if *up { "health-dot up" } else { "health-dot down" }) title=(b.name) {}
                }
                @if backends.is_empty() { "—" }
            }
            div class="measure-sub" { "backends" }
        }
    };
    let body = html! {
        (range_picker("/admin", r))
        (measures(&totals, Some(health_measure)))
        (charts(&format!("/admin/api/stats?range={}", r.key), r.bucket_secs))
        div class="pair" {
            (breakdown_table("Top models", &top_models, None))
            (breakdown_table("Top keys", &top_keys, Some("/admin/keys/")))
        }
    };
    Html(layout("Dashboard", "dashboard", body).into_string()).into_response()
}

/// One key's own statistics: the same charts, narrowed to it, plus what it
/// spent per model.
pub async fn key_stats_page(
    State(st): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
    Query(q): Query<StatsQuery>,
) -> Response {
    let key = match crate::admin::pages::key_row(&st.db, id).await {
        Ok(Some(k)) => k,
        Ok(None) => return (axum::http::StatusCode::NOT_FOUND, "no such key").into_response(),
        Err(e) => return err500(e),
    };
    let r = q.range();
    let filter = usage::Filter {
        key_id: Some(id),
        model: None,
    };
    let totals = match usage::totals(&st.db, r, &filter).await {
        Ok(t) => t,
        Err(e) => return err500(e),
    };
    let per_model = usage::breakdown(&st.db, r, &filter, usage::By::Model, 0)
        .await
        .unwrap_or_default();
    let all_time = usage::totals(&st.db, usage::range(Some("all")), &filter)
        .await
        .unwrap_or_default();

    let body = html! {
        div class="row page-head" {
            h2 { (key.name) }
            span class="badge mono" { "igx_" (key.key_prefix) "…" }
            @if key.revoked_at.is_some() { span class="badge badge-danger" { "revoked" } }
            @else { span class="badge badge-success" { "active" } }
            a class="btn btn-sm" href=(format!("/admin/logs?key={id}")) { "Requests" }
            a class="btn btn-sm" href="/admin/keys" { "All keys" }
        }
        p class="hint" {
            "Created " (crate::admin::pages::fmt_ts(key.created_at)) " · "
            @if let Some(e) = key.expires_at { "expires " (crate::admin::pages::fmt_ts(e)) }
            @else { "never expires" }
            " · all time: " (usage::fmt_count(all_time.total_tokens())) " tokens over "
            (usage::fmt_count(all_time.requests)) " requests"
        }
        (range_picker(&format!("/admin/keys/{id}"), r))
        (measures(&totals, None))
        (charts(&format!("/admin/api/stats?range={}&key_id={id}", r.key), r.bucket_secs))
        (breakdown_table("Per model", &per_model, None))
    };
    Html(layout(&format!("Key · {}", key.name), "keys", body).into_string()).into_response()
}

// ---------------------------------------------------------------------------
// Settings
// ---------------------------------------------------------------------------

pub async fn settings_page(State(st): State<AppState>) -> Response {
    let retention_days = get_setting(&st.db, "retention_max_age_secs")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .map(|s| s / 86_400);
    let retention_usage_days = get_setting(&st.db, "retention_usage_max_age_secs")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse::<i64>().ok())
        .map(|s| s / 86_400);
    let retention_gb = get_setting(&st.db, "retention_max_db_bytes")
        .await
        .ok()
        .flatten()
        .and_then(|v| v.parse::<f64>().ok())
        .map(|b| b / 1e9);
    let (rp_id, rp_origin) = (
        get_setting(&st.db, "rp_id")
            .await
            .ok()
            .flatten()
            .unwrap_or_default(),
        get_setting(&st.db, "rp_origin")
            .await
            .ok()
            .flatten()
            .unwrap_or_default(),
    );
    let db_bytes: i64 = st
        .db
        .call(|c| {
            Ok(c.query_row(
                "SELECT page_count * page_size FROM pragma_page_count(), pragma_page_size()",
                [],
                |r| r.get(0),
            )?)
        })
        .await
        .unwrap_or(0);
    let passkeys: Vec<(i64, i64)> = st
        .db
        .call(|c| {
            let mut stmt =
                c.prepare("SELECT id, created_at FROM webauthn_credentials ORDER BY id")?;
            let rows = stmt.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await
        .unwrap_or_default();

    let body = html! {
        div class="card" {
            h2 { "Retention" }
            p class="hint" { "Database size: " (format!("{:.2} GB", db_bytes as f64 / 1e9))
                ". Full request/response payloads are logged for every request; pruning runs hourly." }
            form method="post" action="/admin/settings/retention" {
                label { "Delete logs older than (days, empty = keep forever)" }
                input type="number" name="max_age_days" min="1"
                    value=(retention_days.map(|d| d.to_string()).unwrap_or_default());
                label { "Keep database under (GB, empty = unlimited)" }
                input type="number" step="0.1" name="max_db_gb" min="0.1"
                    value=(retention_gb.map(|g| format!("{g:.1}")).unwrap_or_default());
                label { "Delete usage statistics older than (days, empty = keep forever)" }
                input type="number" name="usage_max_age_days" min="1"
                    value=(retention_usage_days.map(|d| d.to_string()).unwrap_or_default());
                p class="hint" {
                    "Statistics are the hourly rollup behind the dashboard and the key pages — "
                    "a few dozen bytes per key, model and hour. Keeping them long outlives the "
                    "payload retention above and costs almost nothing." }
                button type="submit" class="primary" { "Save retention" }
            }
        }
        div class="card" {
            h2 { "Export logs" }
            form method="get" action="/admin/export" {
                label { "Format" }
                select name="format" {
                    option value="native" { "Native JSONL (lossless)" }
                    option value="otel" { "OpenTelemetry GenAI JSONL (OTLP/JSON)" }
                }
                label { "From (optional)" }
                input type="date" name="from";
                label { "To (optional)" }
                input type="date" name="to";
                button type="submit" class="primary" { "Download" }
            }
        }
        div class="card" {
            h2 { "Passkeys (FIDO2 second factor)" }
            @if passkeys.is_empty() {
                p class="hint" { "No passkeys enrolled — login uses password only. Enroll one to enable 2FA." }
            }
            table class="grid" {
                tbody { @for (id, created) in &passkeys {
                    tr {
                        td { "Passkey #" (id) }
                        td class="muted" { "enrolled " (super::pages::fmt_ts(*created)) }
                        td { form class="inline" method="post" action=(format!("/admin/settings/passkeys/{id}/delete"))
                                 data-confirm="Remove this passkey?" {
                            button class="danger" { "Remove" }
                        } }
                    }
                } }
            }
            // Wired by webauthn.js on load — no inline handler, so the page
            // needs no script-src exception. `data-passkey-error` is where a
            // failure is shown; success reloads the list.
            button class="primary" data-passkey-enroll data-passkey-reload { "Enroll new passkey" }
            p class="err" data-passkey-error {}
            script src="/admin/assets/webauthn.js" defer {}
        }
        div class="card" {
            h2 { "WebAuthn relying party" }
            p class="hint" { "Must match the domain the admin GUI is served on. Changing it invalidates enrolled passkeys." }
            form method="post" action="/admin/settings/rp" {
                label { "RP ID (domain)" }
                input type="text" name="rp_id" value=(rp_id) placeholder="admin.example.com";
                label { "Origin" }
                input type="text" name="rp_origin" value=(rp_origin) placeholder="https://admin.example.com";
                button type="submit" { "Save" }
            }
        }
        div class="card" {
            h2 { "Change admin password" }
            form method="post" action="/admin/settings/password" {
                label { "Current password" }
                input type="password" name="current" required;
                label { "New password (min 12 characters)" }
                input type="password" name="new" required minlength="12";
                button type="submit" class="primary" { "Change password" }
            }
        }
    };
    Html(layout("Settings", "settings", body).into_string()).into_response()
}

#[derive(Deserialize)]
pub struct RetentionForm {
    max_age_days: Option<String>,
    max_db_gb: Option<String>,
    usage_max_age_days: Option<String>,
}

pub async fn save_retention(State(st): State<AppState>, Form(f): Form<RetentionForm>) -> Response {
    let age = f
        .max_age_days
        .as_deref()
        .unwrap_or("")
        .trim()
        .parse::<i64>()
        .ok();
    let usage_age = f
        .usage_max_age_days
        .as_deref()
        .unwrap_or("")
        .trim()
        .parse::<i64>()
        .ok();
    let gb = f
        .max_db_gb
        .as_deref()
        .unwrap_or("")
        .trim()
        .parse::<f64>()
        .ok();
    let res: Result<()> = async {
        match age {
            Some(d) if d > 0 => {
                set_setting(&st.db, "retention_max_age_secs", &(d * 86_400).to_string()).await?
            }
            _ => clear_setting(&st.db, "retention_max_age_secs").await?,
        }
        match usage_age {
            Some(d) if d > 0 => {
                set_setting(
                    &st.db,
                    "retention_usage_max_age_secs",
                    &(d * 86_400).to_string(),
                )
                .await?
            }
            _ => clear_setting(&st.db, "retention_usage_max_age_secs").await?,
        }
        match gb {
            Some(g) if g > 0.0 => {
                set_setting(
                    &st.db,
                    "retention_max_db_bytes",
                    &((g * 1e9) as i64).to_string(),
                )
                .await?
            }
            _ => clear_setting(&st.db, "retention_max_db_bytes").await?,
        }
        Ok(())
    }
    .await;
    match res {
        Ok(()) => Redirect::to("/admin/settings").into_response(),
        Err(e) => err500(e),
    }
}

async fn clear_setting(db: &Db, key: &str) -> Result<()> {
    let key = key.to_string();
    db.call(move |c| {
        c.execute("DELETE FROM settings WHERE key = ?1", [key])?;
        Ok(())
    })
    .await
}

#[derive(Deserialize)]
pub struct RpForm {
    rp_id: String,
    rp_origin: String,
}

pub async fn save_rp(State(st): State<AppState>, Form(f): Form<RpForm>) -> Response {
    let res: Result<()> = async {
        set_setting(&st.db, "rp_id", f.rp_id.trim()).await?;
        set_setting(&st.db, "rp_origin", f.rp_origin.trim()).await?;
        Ok(())
    }
    .await;
    match res {
        Ok(()) => Redirect::to("/admin/settings").into_response(),
        Err(e) => err500(e),
    }
}

#[derive(Deserialize)]
pub struct PasswordForm {
    current: String,
    new: String,
}

pub async fn change_password(State(st): State<AppState>, Form(f): Form<PasswordForm>) -> Response {
    if f.new.len() < 12 {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "password must be at least 12 characters",
        )
            .into_response();
    }
    let stored: Option<String> = st
        .db
        .call(|c| {
            Ok(c.query_row(
                "SELECT password_hash FROM admin_credentials WHERE id = 1",
                [],
                |r| r.get(0),
            )
            .ok())
        })
        .await
        .ok()
        .flatten();
    let Some(hash) = stored else {
        return (axum::http::StatusCode::CONFLICT, "no admin account").into_response();
    };
    if !super::auth::verify_password(&f.current, &hash) {
        return (
            axum::http::StatusCode::FORBIDDEN,
            "current password is wrong",
        )
            .into_response();
    }
    let new_hash = match super::auth::hash_password(&f.new) {
        Ok(h) => h,
        Err(e) => return err500(e),
    };
    let res = st
        .db
        .call(move |c| {
            c.execute(
                "UPDATE admin_credentials SET password_hash = ?1 WHERE id = 1",
                [new_hash],
            )?;
            Ok(())
        })
        .await;
    match res {
        Ok(()) => Redirect::to("/admin/settings").into_response(),
        Err(e) => err500(e),
    }
}

pub async fn delete_passkey(
    State(st): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> Response {
    let res = st
        .db
        .call(move |c| {
            c.execute("DELETE FROM webauthn_credentials WHERE id = ?1", [id])?;
            Ok(())
        })
        .await;
    match res {
        Ok(()) => Redirect::to("/admin/settings").into_response(),
        Err(e) => err500(e),
    }
}

// ---------------------------------------------------------------------------
// Export
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct ExportQuery {
    format: Option<String>,
    from: Option<String>,
    to: Option<String>,
    key: Option<i64>,
    model: Option<String>,
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

pub async fn export(State(st): State<AppState>, Query(q): Query<ExportQuery>) -> Response {
    let filter = crate::export::Filter {
        from: parse_date(&q.from, false),
        to: parse_date(&q.to, true),
        key_id: q.key,
        model: q.model.clone().filter(|m| !m.is_empty()),
    };
    let format = q.format.as_deref().unwrap_or("native");
    let result = match format {
        "otel" => crate::export::otel_jsonl(&st.db, &filter).await,
        _ => crate::export::native_jsonl(&st.db, &filter).await,
    };
    match result {
        Ok(data) => (
            [
                (header::CONTENT_TYPE, "application/x-ndjson".to_string()),
                (
                    header::CONTENT_DISPOSITION,
                    format!("attachment; filename=\"inginx-export-{format}.jsonl\""),
                ),
            ],
            data,
        )
            .into_response(),
        Err(e) => err500(e),
    }
}
