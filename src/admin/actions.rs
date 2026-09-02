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

async fn render_area(st: &AppState, new_key: Option<&str>) -> Response {
    match key_rows(&st.db).await {
        Ok(rows) => Html(keys_area(&rows, new_key).into_string()).into_response(),
        Err(e) => err500(e),
    }
}
