use crate::db::Db;
use crate::proxy::AppState;
use anyhow::Result;
use axum::{
    extract::{Path, State},
    http::{header, StatusCode},
    response::{Html, IntoResponse, Response},
};
use maud::{html, Markup, DOCTYPE};

pub fn layout(title: &str, active: &str, body: Markup) -> Markup {
    let nav = [
        ("/admin", "Dashboard", "dashboard"),
        ("/admin/keys", "API Keys", "keys"),
        ("/admin/backends", "Backends", "backends"),
        ("/admin/logs", "Logs", "logs"),
        ("/admin/settings", "Settings", "settings"),
    ];
    html! {
        (DOCTYPE)
        html {
            head {
                meta charset="utf-8";
                meta name="viewport" content="width=device-width, initial-scale=1";
                title { (title) " · inginx" }
                link rel="stylesheet" href="/admin/assets/app.css";
                script src="/admin/assets/htmx.min.js" {}
            }
            body {
                nav class="sidebar" {
                    div class="brand" { "in" span { "ginx" } }
                    @for (href, label, key) in nav {
                        a href=(href) class=(if key == active { "active" } else { "" }) { (label) }
                    }
                    div class="spacer" {}
                    form method="post" action="/admin/logout" {
                        button type="submit" { "Sign out" }
                    }
                }
                main {
                    h1 { (title) }
                    (body)
                }
            }
        }
    }
}

pub async fn assets(Path(file): Path<String>) -> Response {
    let (bytes, ctype): (&'static [u8], &str) = match file.as_str() {
        "app.css" => (include_bytes!("../../assets/app.css"), "text/css"),
        "htmx.min.js" => (
            include_bytes!("../../assets/htmx.min.js"),
            "text/javascript",
        ),
        "webauthn.js" => (
            include_bytes!("../../assets/webauthn.js"),
            "text/javascript",
        ),
        "uplot.iife.min.js" => (
            include_bytes!("../../assets/uplot.iife.min.js"),
            "text/javascript",
        ),
        "uplot.min.css" => (include_bytes!("../../assets/uplot.min.css"), "text/css"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    (
        [
            (header::CONTENT_TYPE, ctype.to_string()),
            (header::CACHE_CONTROL, "max-age=86400".to_string()),
        ],
        bytes,
    )
        .into_response()
}

pub struct KeyRow {
    pub id: i64,
    pub name: String,
    pub key_prefix: String,
    pub all_models: bool,
    pub created_at: i64,
    pub revoked_at: Option<i64>,
    pub expires_at: Option<i64>,
    pub n_requests: i64,
    pub n_models: i64,
}

pub async fn key_rows(db: &Db) -> Result<Vec<KeyRow>> {
    db.call(|c| {
        let mut stmt = c.prepare(
            "SELECT k.id, k.name, k.key_prefix, k.all_models, k.created_at, k.revoked_at, k.expires_at,
                    COALESCE((SELECT SUM(n_requests) FROM usage_hourly u WHERE u.key_id = k.id), 0),
                    (SELECT COUNT(*) FROM key_models km WHERE km.key_id = k.id)
             FROM api_keys k ORDER BY k.created_at DESC",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(KeyRow {
                id: r.get(0)?,
                name: r.get(1)?,
                key_prefix: r.get(2)?,
                all_models: r.get::<_, i64>(3)? != 0,
                created_at: r.get(4)?,
                revoked_at: r.get(5)?,
                expires_at: r.get(6)?,
                n_requests: r.get(7)?,
                n_models: r.get(8)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
    })
    .await
}

pub fn fmt_ts(ts: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(ts)
        .ok()
        .and_then(|t| {
            t.format(&time::macros::format_description!(
                "[year]-[month]-[day] [hour]:[minute]"
            ))
            .ok()
        })
        .unwrap_or_else(|| ts.to_string())
}

/// The swap target for all key actions: an optional one-time plaintext box
/// plus the keys table.
pub fn keys_area(rows: &[KeyRow], new_key: Option<&str>) -> Markup {
    html! {
        div id="keys-area" {
            @if let Some(plain) = new_key {
                div class="panel" {
                    strong { "Key created — copy it now, it will not be shown again:" }
                    div class="keybox" { (plain) }
                }
            }
            div class="panel" {
                table {
                    thead { tr {
                        th { "Name" } th { "Key" } th { "Models" } th { "Created" }
                        th { "Expires" } th { "Requests" } th { "Status" } th {}
                    } }
                    tbody {
                        @if rows.is_empty() {
                            tr { td colspan="8" class="muted" { "No API keys yet." } }
                        }
                        @for k in rows {
                            tr {
                                td { (k.name) }
                                td class="mono" { "igx_" (k.key_prefix) "…" }
                                td { @if k.all_models { "all" } @else { (k.n_models) } }
                                td { (fmt_ts(k.created_at)) }
                                td { @if let Some(e) = k.expires_at { (fmt_ts(e)) } @else { "—" } }
                                td { (k.n_requests) }
                                td {
                                    @if k.revoked_at.is_some() { span class="badge err" { "revoked" } }
                                    @else { span class="badge ok" { "active" } }
                                }
                                td {
                                    @if k.revoked_at.is_none() {
                                        button class="danger"
                                            hx-post=(format!("/admin/keys/{}/revoke", k.id))
                                            hx-target="#keys-area" hx-swap="outerHTML"
                                            hx-confirm=(format!("Revoke key '{}'?", k.name)) { "Revoke" }
                                        " "
                                        button
                                            hx-post=(format!("/admin/keys/{}/rotate", k.id))
                                            hx-target="#keys-area" hx-swap="outerHTML"
                                            hx-confirm=(format!("Rotate key '{}'? The old key stops working immediately.", k.name)) { "Rotate" }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}

pub async fn keys_page(State(st): State<AppState>) -> Response {
    let rows = match key_rows(&st.db).await {
        Ok(r) => r,
        Err(e) => return err500(e),
    };
    let models = match crate::registry::list_models(&st.db).await {
        Ok(m) => m,
        Err(e) => return err500(e),
    };
    let body = html! {
        div class="panel" {
            h2 style="margin-top:0" { "Create key" }
            form hx-post="/admin/keys" hx-target="#keys-area" hx-swap="outerHTML" {
                label { "Name" }
                input type="text" name="name" required placeholder="my-app";
                label { input type="checkbox" name="all_models" checked; " All models" }
                label { "Or select specific models" }
                select name="model_ids" multiple size="5" {
                    @for m in &models { option value=(m.id) { (m.alias) } }
                }
                label { "Expires after (days, empty = never)" }
                input type="number" name="expires_days" min="1" placeholder="never";
                button type="submit" class="primary" style="margin-top:1rem" { "Create key" }
            }
        }
        (keys_area(&rows, None))
    };
    Html(layout("API Keys", "keys", body).into_string()).into_response()
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

/// Pings `{base_url}/models` with a short timeout; used for the health dot.
pub async fn backend_up(http: &reqwest::Client, b: &crate::registry::Backend) -> bool {
    let mut req = http
        .get(format!("{}/models", b.base_url))
        .timeout(std::time::Duration::from_millis(1500));
    if let Some(k) = &b.upstream_api_key {
        req = req.bearer_auth(k);
    }
    matches!(req.send().await, Ok(r) if r.status().is_success())
}

pub async fn backends_area(st: &AppState, notice: Option<String>) -> anyhow::Result<Markup> {
    let backends = crate::registry::list_backends(&st.db).await?;
    let models = crate::registry::list_models(&st.db).await?;
    let mut health = Vec::new();
    for b in &backends {
        health.push(backend_up(&st.http, b).await);
    }
    Ok(html! {
        div id="backends-area" {
            @if let Some(n) = notice {
                div class="panel" { (n) }
            }
            @for (b, up) in backends.iter().zip(&health) {
                div class="panel" {
                    h2 style="margin-top:0" {
                        span class=(if *up { "health-dot up" } else { "health-dot down" }) {}
                        (b.name) " "
                        span class="muted mono" style="font-weight:400;font-size:.8rem" { (b.base_url) }
                        @if b.is_default { " " span class="badge muted" { "default" } }
                        @if !b.enabled { " " span class="badge err" { "disabled" } }
                    }
                    div style="margin-bottom:.6rem" {
                        button hx-post=(format!("/admin/backends/{}/import", b.id))
                            hx-target="#backends-area" hx-swap="outerHTML" { "Import from /models" }
                        " "
                        @if !b.is_default {
                            button hx-post=(format!("/admin/backends/{}/default", b.id))
                                hx-target="#backends-area" hx-swap="outerHTML" { "Make default" }
                        }
                    }
                    table {
                        thead { tr { th { "Alias (public)" } th { "Upstream name" } th { "Status" } th {} } }
                        tbody {
                            @for m in models.iter().filter(|m| m.backend_id == b.id) {
                                tr {
                                    td {
                                        form class="inline" hx-post=(format!("/admin/models/{}/alias", m.id))
                                            hx-target="#backends-area" hx-swap="outerHTML" {
                                            input type="text" name="alias" value=(m.alias)
                                                style="max-width:220px" title="Edit and press Enter to rename";
                                        }
                                    }
                                    td class="mono" { (m.upstream_name) }
                                    td {
                                        @if m.enabled { span class="badge ok" { "enabled" } }
                                        @else { span class="badge muted" { "disabled" } }
                                    }
                                    td {
                                        button hx-post=(format!("/admin/models/{}/toggle", m.id))
                                            hx-target="#backends-area" hx-swap="outerHTML" {
                                            @if m.enabled { "Disable" } @else { "Enable" }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    })
}

pub async fn backends_page(State(st): State<AppState>) -> Response {
    let area = match backends_area(&st, None).await {
        Ok(a) => a,
        Err(e) => return err500(e),
    };
    let body = html! {
        div class="panel" {
            h2 style="margin-top:0" { "Add backend" }
            form hx-post="/admin/backends" hx-target="#backends-area" hx-swap="outerHTML" {
                label { "Name" }
                input type="text" name="name" required placeholder="gpu1";
                label { "Base URL (OpenAI-compatible, including /v1)" }
                input type="text" name="base_url" required placeholder="http://10.0.0.5:8080/v1";
                label { "Upstream API key (optional)" }
                input type="password" name="upstream_api_key";
                label { input type="checkbox" name="is_default"; " Default backend (for requests without a known model)" }
                button type="submit" class="primary" style="margin-top:1rem" { "Add backend" }
            }
        }
        (area)
    };
    Html(layout("Backends", "backends", body).into_string()).into_response()
}

pub fn err500(e: anyhow::Error) -> Response {
    tracing::error!("admin page error: {e}");
    (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
}
