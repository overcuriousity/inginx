use super::pages::{backend_up, err500, layout};
use crate::db::{now, Db};
use crate::logstore::{get_setting, set_setting};
use crate::proxy::AppState;
use anyhow::Result;
use axum::{
    extract::{Query, State},
    http::header,
    response::{Html, IntoResponse, Redirect, Response},
    Form,
};
use maud::{html, PreEscaped};
use serde::Deserialize;

// ---------------------------------------------------------------------------
// Stats
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
pub struct StatsQuery {
    days: Option<i64>,
}

#[derive(serde::Serialize)]
pub struct Stats {
    hours: Vec<i64>,
    requests: Vec<i64>,
    prompt_tokens: Vec<i64>,
    completion_tokens: Vec<i64>,
    errors: Vec<i64>,
}

async fn stats(db: &Db, days: i64) -> Result<Stats> {
    let since = now() - days * 86_400;
    let since_hour = since - since % 3600;
    let rows: Vec<(i64, i64, i64, i64, i64)> = db
        .call(move |c| {
            let mut stmt = c.prepare(
                "SELECT hour, SUM(n_requests), SUM(prompt_tokens), SUM(completion_tokens), SUM(n_errors)
                 FROM usage_hourly WHERE hour >= ?1 GROUP BY hour ORDER BY hour",
            )?;
            let rows = stmt.query_map([since_hour], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await?;
    let mut s = Stats {
        hours: Vec::new(),
        requests: Vec::new(),
        prompt_tokens: Vec::new(),
        completion_tokens: Vec::new(),
        errors: Vec::new(),
    };
    let map: std::collections::HashMap<i64, (i64, i64, i64, i64)> = rows
        .into_iter()
        .map(|(h, r, p, ct, e)| (h, (r, p, ct, e)))
        .collect();
    let mut h = since_hour;
    let end = now();
    while h <= end {
        let (r, p, ct, e) = map.get(&h).copied().unwrap_or_default();
        s.hours.push(h);
        s.requests.push(r);
        s.prompt_tokens.push(p);
        s.completion_tokens.push(ct);
        s.errors.push(e);
        h += 3600;
    }
    Ok(s)
}

pub async fn stats_api(State(st): State<AppState>, Query(q): Query<StatsQuery>) -> Response {
    match stats(&st.db, q.days.unwrap_or(7).clamp(1, 90)).await {
        Ok(s) => axum::Json(s).into_response(),
        Err(e) => err500(e),
    }
}

// ---------------------------------------------------------------------------
// Dashboard
// ---------------------------------------------------------------------------

async fn top_rows(db: &Db, group: &'static str, days: i64) -> Result<Vec<(String, i64, i64)>> {
    let since = now() - days * 86_400;
    db.call(move |c| {
        let sql = if group == "model" {
            "SELECT COALESCE(NULLIF(model, ''), '(none)'), SUM(n_requests), SUM(prompt_tokens + completion_tokens)
             FROM usage_hourly WHERE hour >= ?1 GROUP BY model ORDER BY 2 DESC LIMIT 10"
        } else {
            "SELECT COALESCE(k.name, '(no key)'), SUM(u.n_requests), SUM(u.prompt_tokens + u.completion_tokens)
             FROM usage_hourly u LEFT JOIN api_keys k ON k.id = u.key_id
             WHERE u.hour >= ?1 GROUP BY u.key_id ORDER BY 2 DESC LIMIT 10"
        };
        let mut stmt = c.prepare(sql)?;
        let rows = stmt.query_map([since], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

pub async fn dashboard(State(st): State<AppState>) -> Response {
    let day = match stats(&st.db, 1).await {
        Ok(s) => s,
        Err(e) => return err500(e),
    };
    let (req_24, tok_24, err_24) = (
        day.requests.iter().sum::<i64>(),
        day.prompt_tokens.iter().sum::<i64>() + day.completion_tokens.iter().sum::<i64>(),
        day.errors.iter().sum::<i64>(),
    );
    let top_models = top_rows(&st.db, "model", 7).await.unwrap_or_default();
    let top_keys = top_rows(&st.db, "key", 7).await.unwrap_or_default();
    let backends = crate::registry::list_backends(&st.db)
        .await
        .unwrap_or_default();
    let mut health = Vec::new();
    for b in &backends {
        health.push(backend_up(&st.http, b).await);
    }
    let body = html! {
        div class="stats-row" {
            div class="stat" { div class="num" { (req_24) } div class="cap" { "requests (24h)" } }
            div class="stat" { div class="num" { (tok_24) } div class="cap" { "tokens (24h)" } }
            div class="stat" { div class="num" { (err_24) } div class="cap" { "errors (24h)" } }
            div class="stat" {
                div class="num" {
                    @for (b, up) in backends.iter().zip(&health) {
                        span class=(if *up { "health-dot up" } else { "health-dot down" }) title=(b.name) {}
                    }
                    @if backends.is_empty() { "—" }
                }
                div class="cap" { "backends" }
            }
        }
        div class="panel" {
            h2 style="margin-top:0" { "Requests per hour (7d)" }
            div class="chart-wrap" { div id="chart-req" {} }
            h2 { "Tokens per hour (7d)" }
            div class="chart-wrap" { div id="chart-tok" {} }
        }
        div style="display:flex; gap:1.2rem; flex-wrap:wrap" {
            div class="panel" style="flex:1; min-width:300px" {
                h2 style="margin-top:0" { "Top models (7d)" }
                table {
                    thead { tr { th { "Model" } th { "Requests" } th { "Tokens" } } }
                    tbody { @for (name, reqs, toks) in &top_models {
                        tr { td { (name) } td { (reqs) } td { (toks) } }
                    } }
                }
            }
            div class="panel" style="flex:1; min-width:300px" {
                h2 style="margin-top:0" { "Top keys (7d)" }
                table {
                    thead { tr { th { "Key" } th { "Requests" } th { "Tokens" } } }
                    tbody { @for (name, reqs, toks) in &top_keys {
                        tr { td { (name) } td { (reqs) } td { (toks) } }
                    } }
                }
            }
        }
        link rel="stylesheet" href="/admin/assets/uplot.min.css";
        script src="/admin/assets/uplot.iife.min.js" {}
        script { (PreEscaped(DASHBOARD_JS)) }
    };
    Html(layout("Dashboard", "dashboard", body).into_string()).into_response()
}

const DASHBOARD_JS: &str = r#"
(async function () {
  const s = await (await fetch('/admin/api/stats?days=7')).json();
  const width = Math.min(document.querySelector('main').clientWidth - 80, 1080);
  const axisStyle = { stroke: '#8b949e', grid: { stroke: '#21262d' }, ticks: { stroke: '#30363d' } };
  function mk(el, title, series, data) {
    new uPlot({
      width, height: 200, title,
      cursor: { drag: { setScale: false } },
      series: [{}, ...series],
      axes: [axisStyle, axisStyle],
    }, [s.hours, ...data], document.getElementById(el));
  }
  mk('chart-req', '', [
    { label: 'requests', stroke: '#2f81f7', width: 2 },
    { label: 'errors', stroke: '#f85149', width: 1 },
  ], [s.requests, s.errors]);
  mk('chart-tok', '', [
    { label: 'prompt', stroke: '#3fb950', width: 2 },
    { label: 'completion', stroke: '#d29922', width: 2 },
  ], [s.prompt_tokens, s.completion_tokens]);
})();
"#;

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
        div class="panel" {
            h2 style="margin-top:0" { "Retention" }
            p class="hint" { "Database size: " (format!("{:.2} GB", db_bytes as f64 / 1e9))
                ". Full request/response payloads are logged for every request; pruning runs hourly." }
            form method="post" action="/admin/settings/retention" {
                label { "Delete logs older than (days, empty = keep forever)" }
                input type="number" name="max_age_days" min="1"
                    value=(retention_days.map(|d| d.to_string()).unwrap_or_default());
                label { "Keep database under (GB, empty = unlimited)" }
                input type="number" step="0.1" name="max_db_gb" min="0.1"
                    value=(retention_gb.map(|g| format!("{g:.1}")).unwrap_or_default());
                button type="submit" class="primary" style="margin-top:1rem" { "Save retention" }
            }
        }
        div class="panel" {
            h2 style="margin-top:0" { "Export logs" }
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
                button type="submit" class="primary" style="margin-top:1rem" { "Download" }
            }
        }
        div class="panel" {
            h2 style="margin-top:0" { "Passkeys (FIDO2 second factor)" }
            @if passkeys.is_empty() {
                p class="hint" { "No passkeys enrolled — login uses password only. Enroll one to enable 2FA." }
            }
            table {
                tbody { @for (id, created) in &passkeys {
                    tr {
                        td { "Passkey #" (id) }
                        td class="muted" { "enrolled " (super::pages::fmt_ts(*created)) }
                        td { form class="inline" method="post" action=(format!("/admin/settings/passkeys/{id}/delete")) {
                            button class="danger" onclick="return confirm('Remove this passkey?')" { "Remove" }
                        } }
                    }
                } }
            }
            button id="enroll" class="primary" style="margin-top:.8rem" { "Enroll new passkey" }
            p class="err" id="enroll-err" {}
            script src="/admin/assets/webauthn.js" {}
            script { (PreEscaped(r#"
                document.getElementById('enroll').addEventListener('click', async () => {
                    try { await window.inginxRegisterPasskey(); location.reload(); }
                    catch (e) { document.getElementById('enroll-err').textContent = e.message; }
                });
            "#)) }
        }
        div class="panel" {
            h2 style="margin-top:0" { "WebAuthn relying party" }
            p class="hint" { "Must match the domain the admin GUI is served on. Changing it invalidates enrolled passkeys." }
            form method="post" action="/admin/settings/rp" {
                label { "RP ID (domain)" }
                input type="text" name="rp_id" value=(rp_id) placeholder="admin.example.com";
                label { "Origin" }
                input type="text" name="rp_origin" value=(rp_origin) placeholder="https://admin.example.com";
                button type="submit" style="margin-top:1rem" { "Save" }
            }
        }
        div class="panel" {
            h2 style="margin-top:0" { "Change admin password" }
            form method="post" action="/admin/settings/password" {
                label { "Current password" }
                input type="password" name="current" required;
                label { "New password (min 12 characters)" }
                input type="password" name="new" required minlength="12";
                button type="submit" class="primary" style="margin-top:1rem" { "Change password" }
            }
        }
    };
    Html(layout("Settings", "settings", body).into_string()).into_response()
}

#[derive(Deserialize)]
pub struct RetentionForm {
    max_age_days: Option<String>,
    max_db_gb: Option<String>,
}

pub async fn save_retention(State(st): State<AppState>, Form(f): Form<RetentionForm>) -> Response {
    let age = f
        .max_age_days
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
