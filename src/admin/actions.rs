use super::pages::{err500, key_rows, keys_area};
use crate::db::now;
use crate::keys;
use crate::proxy::AppState;
use axum::{
    extract::{Path, RawForm, State},
    response::{Html, IntoResponse, Response},
};

/// Form parser that tolerates repeated fields (`model_ids` multi-select).
fn form_fields(body: &[u8]) -> Vec<(String, String)> {
    body.split(|&b| b == b'&')
        .filter_map(|pair| {
            let s = std::str::from_utf8(pair).ok()?;
            let (k, v) = s.split_once('=')?;
            Some((
                urlencoding::decode(k).ok()?.into_owned(),
                urlencoding::decode(&v.replace('+', " ")).ok()?.into_owned(),
            ))
        })
        .collect()
}

pub async fn create_key(State(st): State<AppState>, RawForm(body): RawForm) -> Response {
    let fields = form_fields(&body);
    let name = fields
        .iter()
        .find(|(k, _)| k == "name")
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    if name.is_empty() {
        return (axum::http::StatusCode::BAD_REQUEST, "name is required").into_response();
    }
    let model_ids: Vec<i64> = fields
        .iter()
        .filter(|(k, _)| k == "model_ids")
        .filter_map(|(_, v)| v.parse().ok())
        .collect();
    let all_models = fields.iter().any(|(k, _)| k == "all_models") || model_ids.is_empty();
    let expires_at = fields
        .iter()
        .find(|(k, _)| k == "expires_days")
        .and_then(|(_, v)| v.parse::<i64>().ok())
        .map(|days| now() + days * 86_400);

    let plain = match keys::create(&st.db, &name, all_models, &model_ids, expires_at).await {
        Ok(p) => p,
        Err(e) => return err500(e),
    };
    render_area(&st, Some(&plain)).await
}

pub async fn revoke_key(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    if let Err(e) = keys::revoke(&st.db, id).await {
        return err500(e);
    }
    render_area(&st, None).await
}

pub async fn rotate_key(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    match keys::rotate(&st.db, id).await {
        Ok(plain) => render_area(&st, Some(&plain)).await,
        Err(e) => err500(e),
    }
}

// ---------------------------------------------------------------------------
// Backends
// ---------------------------------------------------------------------------

use super::pages::backends_area;
use crate::registry;

async fn render_backends(st: &AppState, notice: Option<String>) -> Response {
    match backends_area(st, notice).await {
        Ok(a) => Html(a.into_string()).into_response(),
        Err(e) => err500(e),
    }
}

pub async fn add_backend(State(st): State<AppState>, RawForm(body): RawForm) -> Response {
    let fields = form_fields(&body);
    let get = |name: &str| {
        fields
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.trim().to_string())
            .unwrap_or_default()
    };
    let (name, base_url, api_key) = (get("name"), get("base_url"), get("upstream_api_key"));
    if name.is_empty() || base_url.is_empty() {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "name and base_url are required",
        )
            .into_response();
    }
    // A bad URL is the operator mistyping, not the server failing — answer it
    // as the client error it is. `registry::add_backend` re-checks the same
    // rules as a backstop for any other caller.
    let lower = base_url.to_ascii_lowercase();
    if !(lower.starts_with("http://") || lower.starts_with("https://")) {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "base_url must be an http(s) URL",
        )
            .into_response();
    }
    let has_userinfo = base_url
        .split_once("//")
        .and_then(|(_, rest)| rest.split('/').next())
        .map(|authority| authority.contains('@'))
        .unwrap_or(false);
    if has_userinfo {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "base_url must not embed credentials",
        )
            .into_response();
    }
    let is_default = fields.iter().any(|(k, _)| k == "is_default");
    let key = (!api_key.is_empty()).then_some(api_key);
    match registry::add_backend(&st.db, &name, &base_url, key, is_default).await {
        Ok(_) => render_backends(&st, None).await,
        Err(e) => err500(e),
    }
}

pub async fn import_backend(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    match registry::import_models(&st.db, id).await {
        Ok(added) if added.is_empty() => {
            render_backends(&st, Some("No new models found.".into())).await
        }
        Ok(added) => {
            render_backends(
                &st,
                Some(format!(
                    "Imported {} models: {}",
                    added.len(),
                    added.join(", ")
                )),
            )
            .await
        }
        Err(e) => render_backends(&st, Some(format!("Import failed: {e}"))).await,
    }
}

pub async fn make_default_backend(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    match registry::make_default(&st.db, id).await {
        Ok(()) => render_backends(&st, None).await,
        Err(e) => err500(e),
    }
}

pub async fn toggle_model(State(st): State<AppState>, Path(id): Path<i64>) -> Response {
    let enabled: Option<bool> = st
        .db
        .call(move |c| {
            Ok(
                c.query_row("SELECT enabled FROM models WHERE id = ?1", [id], |r| {
                    Ok(r.get::<_, i64>(0)? != 0)
                })
                .ok(),
            )
        })
        .await
        .ok()
        .flatten();
    let Some(enabled) = enabled else {
        return (axum::http::StatusCode::NOT_FOUND, "no such model").into_response();
    };
    match registry::set_model_enabled(&st.db, id, !enabled).await {
        Ok(()) => render_backends(&st, None).await,
        Err(e) => err500(e),
    }
}

pub async fn rename_model(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    RawForm(body): RawForm,
) -> Response {
    let fields = form_fields(&body);
    let alias = fields
        .iter()
        .find(|(k, _)| k == "alias")
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    if alias.is_empty() {
        return (axum::http::StatusCode::BAD_REQUEST, "alias is required").into_response();
    }
    match registry::set_model_alias(&st.db, id, &alias).await {
        Ok(()) => render_backends(&st, None).await,
        Err(e) => render_backends(&st, Some(format!("Rename failed: {e}"))).await,
    }
}

/// Sets a model's voice map from the `from=to, from=to` form field. An empty
/// field clears it.
pub async fn set_model_voices(
    State(st): State<AppState>,
    Path(id): Path<i64>,
    RawForm(body): RawForm,
) -> Response {
    let fields = form_fields(&body);
    let raw = fields
        .iter()
        .find(|(k, _)| k == "voice_map")
        .map(|(_, v)| v.trim().to_string())
        .unwrap_or_default();
    let map = match crate::proxy::parse_voice_map(&raw) {
        Ok(m) => m,
        Err(e) => return render_backends(&st, Some(format!("Voice map rejected: {e}"))).await,
    };
    match registry::set_model_voice_map(&st.db, id, map).await {
        Ok(()) => render_backends(&st, None).await,
        Err(e) => render_backends(&st, Some(format!("Voice map update failed: {e}"))).await,
    }
}

async fn render_area(st: &AppState, new_key: Option<&str>) -> Response {
    match key_rows(&st.db).await {
        Ok(rows) => Html(keys_area(&rows, new_key).into_string()).into_response(),
        Err(e) => err500(e),
    }
}
