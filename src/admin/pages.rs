use crate::db::Db;
use crate::proxy::AppState;
use anyhow::Result;
use axum::{
    extract::{Path, Request, State},
    http::{header, HeaderValue, StatusCode},
    middleware::Next,
    response::{Html, IntoResponse, Response},
};
use maud::{html, Markup, DOCTYPE};

/// One Content-Security-Policy for the whole admin. It is strict because it
/// can be: every script and style is a served, same-origin file, so nothing
/// here needs `'unsafe-inline'` or a nonce. `script-src 'self'` is the load-
/// bearing line — an injected `<script>` or `onclick` simply does not run —
/// and it is only honest because the pages carry no inline script (the theme
/// boot, the charts and the passkey buttons were all moved out to files for
/// exactly this). `frame-ancestors 'none'` is the clickjacking defense;
/// `object-src`/`base-uri 'none'` close two old injection side doors;
/// `form-action 'self'` keeps a form from being retargeted off-origin.
const CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self'; \
img-src 'self'; font-src 'self'; connect-src 'self'; object-src 'none'; \
base-uri 'none'; frame-ancestors 'none'; form-action 'self'";

/// Response-header hardening for every admin response. These are cheap, they
/// never depend on the request, and forgetting one on a single route is
/// exactly how these gaps happen — so they go on as a layer over the whole
/// admin router rather than per handler.
pub async fn security_headers(req: Request, next: Next) -> Response {
    let mut resp = next.run(req).await;
    let h = resp.headers_mut();
    let set = |h: &mut axum::http::HeaderMap, k: header::HeaderName, v: &'static str| {
        h.insert(k, HeaderValue::from_static(v));
    };
    set(h, header::CONTENT_SECURITY_POLICY, CSP);
    set(h, header::X_FRAME_OPTIONS, "DENY");
    set(h, header::X_CONTENT_TYPE_OPTIONS, "nosniff");
    set(h, header::REFERRER_POLICY, "no-referrer");
    h.insert(
        header::HeaderName::from_static("cross-origin-opener-policy"),
        HeaderValue::from_static("same-origin"),
    );
    // Only pages get `no-store`; the static assets set their own long
    // `max-age` and must keep it, so an existing Cache-Control is left alone.
    if !h.contains_key(header::CACHE_CONTROL) {
        set(h, header::CACHE_CONTROL, "no-store");
    }
    resp
}

/// The inginx mark, inlined rather than `<img src="/admin/assets/logo.svg">`
/// so it inherits the page's colour: an `<img>` is its own document and sees
/// neither `currentColor` nor the theme override. Same geometry as
/// assets/logo.svg — edit both. Decorative; the link around it carries the
/// label.
pub fn mark() -> Markup {
    html! {
        svg class="mark" viewBox="0 0 64 64" fill="none" aria-hidden="true" focusable="false" {
            g stroke="currentColor" stroke-width="2" stroke-linecap="round" {
                path d="M12 16 L32 32 M12 32 L32 32 M12 48 L32 32 M32 32 L52 18 M32 32 L52 46" {}
            }
            g fill="currentColor" opacity="0.35" {
                circle cx="12" cy="16" r="2.5" {}
                circle cx="12" cy="32" r="2.5" {}
                circle cx="12" cy="48" r="2.5" {}
            }
            g fill="currentColor" {
                circle cx="52" cy="18" r="3.5" {}
                circle cx="52" cy="46" r="3.5" {}
                circle cx="32" cy="32" r="6" {}
            }
        }
    }
}

/// The `<head>` every page shares: the pre-paint theme boot, the stylesheet,
/// the fonts' colours for an installed window, and nothing a page has to
/// remember.
pub fn head(title: &str) -> Markup {
    html! {
        head {
            meta charset="utf-8";
            meta name="viewport" content="width=device-width, initial-scale=1";
            // Blocking and same-origin, ahead of the stylesheet, so a stored
            // theme is applied before the first paint. A served file rather
            // than an inline script, so the Content-Security-Policy can forbid
            // inline script entirely — see `security_headers`.
            script src="/admin/assets/boot.js" {}
            title { (title) " · inginx" }
            link rel="icon" href="/admin/assets/logo.svg" type="image/svg+xml";
            meta name="theme-color" content="#f8f6f1" media="(prefers-color-scheme: light)";
            meta name="theme-color" content="#0e1015" media="(prefers-color-scheme: dark)";
            link rel="stylesheet" href="/admin/assets/app.css";
            script src="/admin/assets/app.js" defer {}
        }
    }
}

pub fn layout(title: &str, active: &str, body: Markup) -> Markup {
    let nav = [
        ("/admin", "Dashboard", "dashboard"),
        ("/admin/keys", "API Keys", "keys"),
        ("/admin/backends", "Backends", "backends"),
        ("/admin/logs", "Logs", "logs"),
        ("/admin/settings", "Settings", "settings"),
    ];
    // Prose or a grid: Settings is forms and sentences and takes a reading
    // measure; everything else is a table and is as wide as its columns need.
    // Decided here from the page's own key rather than passed in by every
    // caller, because a page that knows its name already knows which it is.
    let regions = if active == "settings" {
        "regions regions-focus"
    } else {
        "regions regions-table"
    };
    html! {
        (DOCTYPE)
        html lang="en" {
            (head(title))
            body {
                // Chrome, not content. Outside `.shell` so the rule under the
                // row spans the window rather than stopping at the measure the
                // page below happens to use.
                header class="topbar" {
                    nav class="top" {
                        a href="/admin" class="brand" aria-label="inginx" {
                            (mark())
                            span { "inginx" }
                        }
                        @for (href, label, key) in nav {
                            a href=(href) aria-current=[(key == active).then_some("page")] { (label) }
                        }
                        span class="spacer" {}
                        button class="ghost btn-sm" type="button" data-theme-toggle
                            aria-label="Switch between dark and light" {
                            span data-theme-label { "Theme" }
                        }
                        form method="post" action="/admin/logout" class="inline" {
                            button class="ghost btn-sm" type="submit" { "Sign out" }
                        }
                    }
                }
                div class="shell" {
                    div class=(regions) {
                        div { h1 { (title) } }
                        (body)
                    }
                }
                script src="/admin/assets/htmx.min.js" {}
            }
        }
    }
}

pub async fn assets(Path(file): Path<String>) -> Response {
    let (bytes, ctype): (&'static [u8], &str) = match file.as_str() {
        "app.css" => (include_bytes!("../../assets/app.css"), "text/css"),
        "app.js" => (include_bytes!("../../assets/app.js"), "text/javascript"),
        "boot.js" => (include_bytes!("../../assets/boot.js"), "text/javascript"),
        "dashboard.js" => (
            include_bytes!("../../assets/dashboard.js"),
            "text/javascript",
        ),
        "logo.svg" => (include_bytes!("../../assets/logo.svg"), "image/svg+xml"),
        "inter-400.woff2" => (
            include_bytes!("../../assets/fonts/inter-400.woff2"),
            "font/woff2",
        ),
        "inter-500.woff2" => (
            include_bytes!("../../assets/fonts/inter-500.woff2"),
            "font/woff2",
        ),
        "inter-600.woff2" => (
            include_bytes!("../../assets/fonts/inter-600.woff2"),
            "font/woff2",
        ),
        "jetbrains-mono-400.woff2" => (
            include_bytes!("../../assets/fonts/jetbrains-mono-400.woff2"),
            "font/woff2",
        ),
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
    /// All-time tokens, filled from the hourly rollup by `key_rows`.
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
}

impl KeyRow {
    pub fn total_tokens(&self) -> i64 {
        self.prompt_tokens + self.completion_tokens
    }
}

const KEY_COLS: &str = "k.id, k.name, k.key_prefix, k.all_models, k.created_at, k.revoked_at,
     k.expires_at, (SELECT COUNT(*) FROM key_models km WHERE km.key_id = k.id)";

fn key_row_from(r: &rusqlite::Row) -> rusqlite::Result<KeyRow> {
    Ok(KeyRow {
        id: r.get(0)?,
        name: r.get(1)?,
        key_prefix: r.get(2)?,
        all_models: r.get::<_, i64>(3)? != 0,
        created_at: r.get(4)?,
        revoked_at: r.get(5)?,
        expires_at: r.get(6)?,
        n_models: r.get(7)?,
        // Filled from the rollup afterwards; one query for every key rather
        // than a correlated subquery per column per row.
        n_requests: 0,
        prompt_tokens: 0,
        completion_tokens: 0,
    })
}

pub async fn key_rows(db: &Db) -> Result<Vec<KeyRow>> {
    let mut rows: Vec<KeyRow> = db
        .call(|c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {KEY_COLS} FROM api_keys k ORDER BY k.created_at DESC"
            ))?;
            let rows = stmt.query_map([], key_row_from)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await?;
    let usage = super::usage::totals_by_key(db).await?;
    for row in &mut rows {
        if let Some(t) = usage.get(&row.id) {
            row.n_requests = t.requests;
            row.prompt_tokens = t.prompt_tokens;
            row.completion_tokens = t.completion_tokens;
        }
    }
    Ok(rows)
}

/// One key by id, for its statistics page.
pub async fn key_row(db: &Db, id: i64) -> Result<Option<KeyRow>> {
    let row: Option<KeyRow> = db
        .call(move |c| {
            let mut stmt = c.prepare(&format!(
                "SELECT {KEY_COLS} FROM api_keys k WHERE k.id = ?1"
            ))?;
            Ok(stmt
                .query_row([id], key_row_from)
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })?)
        })
        .await?;
    let Some(mut row) = row else {
        return Ok(None);
    };
    if let Some(t) = super::usage::totals_by_key(db).await?.get(&row.id) {
        row.n_requests = t.requests;
        row.prompt_tokens = t.prompt_tokens;
        row.completion_tokens = t.completion_tokens;
    }
    Ok(Some(row))
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
                div class="card" {
                    strong { "Key created — copy it now, it will not be shown again:" }
                    div class="keybox" { (plain) }
                }
            }
            div class="card" {
                table class="grid" {
                    thead { tr {
                        th { "Name" } th { "Key" } th { "Models" } th { "Created" }
                        th { "Expires" } th { "Requests" }
                        th { "Prompt" } th { "Completion" } th { "Tokens" }
                        th { "Status" } th {}
                    } }
                    tbody {
                        @if rows.is_empty() {
                            tr { td colspan="11" class="muted" { "No API keys yet." } }
                        }
                        @for k in rows {
                            // The whole row opens that key's statistics; the
                            // delegated handler in app.js leaves clicks on the
                            // buttons alone.
                            tr class="clickable" data-href=(format!("/admin/keys/{}", k.id)) {
                                td { (k.name) }
                                td class="mono" { (crate::keys::display_prefix(&k.key_prefix)) }
                                td { @if k.all_models { "all" } @else { (k.n_models) } }
                                td { (fmt_ts(k.created_at)) }
                                td { @if let Some(e) = k.expires_at { (fmt_ts(e)) } @else { "—" } }
                                td { (super::usage::fmt_count(k.n_requests)) }
                                td { (super::usage::fmt_count(k.prompt_tokens)) }
                                td { (super::usage::fmt_count(k.completion_tokens)) }
                                td { strong { (super::usage::fmt_count(k.total_tokens())) } }
                                td {
                                    @if k.revoked_at.is_some() { span class="badge badge-danger" { "revoked" } }
                                    @else { span class="badge badge-success" { "active" } }
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

/// The checkbox list of models shared by the create form and a key's model
/// access card.
///
/// A native `<select multiple>` asked the operator to ctrl-click inside a
/// five-row window; with a couple of dozen models that is a guessing game.
/// Checkboxes say what is selected without being clicked, and the filter box
/// makes a long list usable. While "All models" is checked the list is inert —
/// that is a CSS `:has()` rule rather than a JS toggle, so the form still
/// behaves correctly if app.js never loads.
///
/// `filter_id` keeps the two instances' label/input pairs distinct, since both
/// can be on the page at once.
fn model_picker(models: &[crate::registry::Model], selected: &[i64], filter_id: &str) -> Markup {
    let n = selected.len();
    html! {
        div class="model-picker" data-model-picker {
            label for=(filter_id) { "Or select specific models" }
            input type="search" id=(filter_id) placeholder="Filter models…"
                autocomplete="off" data-model-filter;
            div class="model-picker__list" {
                @for m in models {
                    label class="model-option" data-model-name=(m.alias) {
                        input type="checkbox" name="model_ids" value=(m.id)
                            checked[selected.contains(&m.id)];
                        span { (m.alias) }
                    }
                }
                @if models.is_empty() {
                    p class="hint" { "No models yet — add a backend and import its models first." }
                }
            }
            div class="model-picker__foot" {
                span class="muted" data-model-count { (n) " selected" }
                button type="button" class="ghost btn-sm" data-model-clear { "Clear" }
            }
        }
    }
}

/// The swap target for editing one key's model scope. A key's grants outlive
/// the moment it was issued — the models an app needs change — so the same
/// picker used at creation is offered here, without rotating the key.
pub fn key_models_card(
    key: &KeyRow,
    models: &[crate::registry::Model],
    granted: &[i64],
    notice: Option<&str>,
) -> Markup {
    html! {
        div id="key-models" class="card" {
            h2 { "Model access" }
            @if let Some(n) = notice {
                p class="hint" { (n) }
            }
            @if key.revoked_at.is_some() {
                // Nothing this key is allowed to reach matters any more; an
                // editable form would only invite pointless saves.
                p class="hint" { "This key is revoked." }
                p {
                    @if key.all_models { "Had access to all models." }
                    @else if granted.is_empty() { "Had access to no models." }
                    @else {
                        "Had access to: "
                        (models.iter().filter(|m| granted.contains(&m.id))
                            .map(|m| m.alias.as_str()).collect::<Vec<_>>().join(", "))
                    }
                }
            } @else {
                form hx-post=(format!("/admin/keys/{}/models", key.id))
                    hx-target="#key-models" hx-swap="outerHTML" {
                    label {
                        input type="checkbox" name="all_models" checked[key.all_models] data-all-models;
                        " All models"
                    }
                    (model_picker(models, granted, "key-model-filter"))
                    button type="submit" class="primary" { "Save access" }
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
        div class="card" {
            h2 { "Create key" }
            form hx-post="/admin/keys" hx-target="#keys-area" hx-swap="outerHTML" {
                label { "Name" }
                input type="text" name="name" required placeholder="my-app";
                label { input type="checkbox" name="all_models" checked data-all-models; " All models" }
                (model_picker(&models, &[], "model-filter"))
                label { "Expires after (days, empty = never)" }
                input type="number" name="expires_days" min="1" placeholder="never";
                button type="submit" class="primary" { "Create key" }
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
                div class="card" { (n) }
            }
            @for (b, up) in backends.iter().zip(&health) {
                div class="card" {
                    h2 {
                        span class=(if *up { "health-dot up" } else { "health-dot down" }) {}
                        (b.name) " "
                        span class="muted mono backend-url" { (b.base_url) }
                        @if b.is_default { " " span class="badge badge-muted" { "default" } }
                        @if !b.enabled { " " span class="badge badge-danger" { "disabled" } }
                    }
                    div class="row backend-actions" {
                        button hx-post=(format!("/admin/backends/{}/import", b.id))
                            hx-target="#backends-area" hx-swap="outerHTML" { "Import from /models" }
                        " "
                        @if !b.is_default {
                            button hx-post=(format!("/admin/backends/{}/default", b.id))
                                hx-target="#backends-area" hx-swap="outerHTML" { "Make default" }
                        }
                    }
                    table class="grid" {
                        thead { tr {
                            th { "Alias (public)" } th { "Upstream name" }
                            th title="Rewrites the `voice` field of /v1/audio/speech before forwarding" { "Voice map" }
                            th title="Send this model's requests to a path of your choosing instead of the client's" { "Upstream path" }
                            th { "Status" } th {}
                        } }
                        tbody {
                            @for m in models.iter().filter(|m| m.backend_id == b.id) {
                                tr {
                                    td {
                                        // The form submitted on Enter and had
                                        // no other affordance, so an edited
                                        // field looked saved when it was not.
                                        // The button says what to press.
                                        form class="cell-form" hx-post=(format!("/admin/models/{}/alias", m.id))
                                            hx-target="#backends-area" hx-swap="outerHTML" {
                                            input type="text" name="alias" value=(m.alias)
                                                class="alias-input" title="The public model name";
                                            button type="submit" class="ghost btn-sm" { "Save" }
                                        }
                                    }
                                    td class="mono" { (m.upstream_name) }
                                    // Backends name their voices their own way
                                    // (Kokoro `af_alloy`, Piper by language),
                                    // while OpenAI clients send `alloy`. One
                                    // `from=to` list per model bridges that.
                                    td {
                                        form class="cell-form" hx-post=(format!("/admin/models/{}/voices", m.id))
                                            hx-target="#backends-area" hx-swap="outerHTML" {
                                            input type="text" name="voice_map"
                                                value=(crate::proxy::format_voice_map(m.voice_map.as_deref()))
                                                class="alias-input voice-input" placeholder="alloy=af_alloy, nova=af_nova"
                                                title="Comma-separated from=to pairs";
                                            button type="submit" class="ghost btn-sm" { "Save" }
                                        }
                                    }
                                    // For a backend that speaks the OpenAI
                                    // body at a path of its own — whisper.cpp
                                    // on /inference, a service behind
                                    // /api/v2/chat.
                                    td {
                                        form class="cell-form" hx-post=(format!("/admin/models/{}/path", m.id))
                                            hx-target="#backends-area" hx-swap="outerHTML" {
                                            input type="text" name="upstream_path"
                                                value=(m.upstream_path.clone().unwrap_or_default())
                                                class="alias-input voice-input" placeholder="(default) /chat/completions"
                                                title="Empty = the client's path without /v1. {model} and {path} are substituted.";
                                            button type="submit" class="ghost btn-sm" { "Save" }
                                        }
                                    }
                                    td {
                                        @if m.enabled { span class="badge badge-success" { "enabled" } }
                                        @else { span class="badge badge-muted" { "disabled" } }
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
        div class="card" {
            h2 { "Add backend" }
            form hx-post="/admin/backends" hx-target="#backends-area" hx-swap="outerHTML" {
                label { "Name" }
                input type="text" name="name" required placeholder="gpu1";
                label { "Base URL (OpenAI-compatible, including /v1)" }
                input type="text" name="base_url" required placeholder="http://10.0.0.5:8080/v1";
                label { "Upstream API key (optional)" }
                input type="password" name="upstream_api_key";
                label { input type="checkbox" name="is_default"; " Default backend (for requests without a known model)" }
                button type="submit" class="primary" { "Add backend" }
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
