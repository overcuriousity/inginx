use crate::db::{now, Db};
use crate::logstore::{LogRecord, LogWriter};
use crate::{keys, metering, registry};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, State},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use std::time::Instant;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub log: LogWriter,
    pub http: reqwest::Client,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/v1/models", get(models_handler))
        .route("/v1/{*path}", any(proxy_handler))
        .layer(DefaultBodyLimit::max(512 * 1024 * 1024))
        .with_state(state)
}

pub fn openai_error(status: StatusCode, msg: &str, code: &str) -> Response {
    let err_type = if status.is_client_error() {
        "invalid_request_error"
    } else {
        "api_error"
    };
    (
        status,
        Json(serde_json::json!({
            "error": {"message": msg, "type": err_type, "code": code}
        })),
    )
        .into_response()
}

fn bearer(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::AUTHORIZATION)?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
}

/// Serializes headers as a JSON object, redacting the authorization value.
fn headers_json(headers: &HeaderMap) -> String {
    let map: serde_json::Map<String, serde_json::Value> = headers
        .iter()
        .map(|(k, v)| {
            let val = if k == header::AUTHORIZATION {
                "Bearer igx_***redacted***".to_string()
            } else {
                v.to_str().unwrap_or("<binary>").to_string()
            };
            (k.to_string(), serde_json::Value::String(val))
        })
        .collect();
    serde_json::Value::Object(map).to_string()
}

fn resp_headers_json(headers: &reqwest::header::HeaderMap) -> String {
    let map: serde_json::Map<String, serde_json::Value> = headers
        .iter()
        .map(|(k, v)| {
            (
                k.to_string(),
                serde_json::Value::String(v.to_str().unwrap_or("<binary>").to_string()),
            )
        })
        .collect();
    serde_json::Value::Object(map).to_string()
}

async fn models_handler(State(st): State<AppState>, headers: HeaderMap) -> Response {
    let Some(token) = bearer(&headers) else {
        return openai_error(StatusCode::UNAUTHORIZED, "Missing API key", "invalid_api_key");
    };
    let key = match keys::authenticate(&st.db, token).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            return openai_error(StatusCode::UNAUTHORIZED, "Invalid API key", "invalid_api_key")
        }
        Err(e) => {
            tracing::error!("auth failure: {e}");
            return openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error", "internal");
        }
    };
    match registry::models_for_key(&st.db, &key).await {
        Ok(models) => Json(serde_json::json!({
            "object": "list",
            "data": models.iter().map(|m| serde_json::json!({
                "id": m.alias, "object": "model", "owned_by": "inginx"
            })).collect::<Vec<_>>(),
        }))
        .into_response(),
        Err(e) => {
            tracing::error!("models query failed: {e}");
            openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error", "internal")
        }
    }
}

/// Extracts the `model` value from a JSON body, or from a multipart body by a
/// cheap scan for the `model` field section.
fn extract_model(body: &[u8], content_type: &str) -> Option<String> {
    if content_type.starts_with("multipart/") {
        let text = String::from_utf8_lossy(body);
        let idx = text.find("name=\"model\"")?;
        let rest = &text[idx..];
        let blank = rest.find("\r\n\r\n").map(|i| i + 4).or_else(|| rest.find("\n\n").map(|i| i + 2))?;
        let val = rest[blank..].lines().next()?.trim().trim_end_matches('\r');
        return (!val.is_empty()).then(|| val.to_string());
    }
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")?.as_str().map(String::from)
}

pub async fn proxy_handler(
    State(st): State<AppState>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let endpoint = uri.path().to_string();
    let started = Instant::now();

    // 1. Authenticate
    let Some(token) = bearer(&headers) else {
        return openai_error(StatusCode::UNAUTHORIZED, "Missing API key", "invalid_api_key");
    };
    let key = match keys::authenticate(&st.db, token).await {
        Ok(Some(k)) => k,
        Ok(None) => {
            return openai_error(StatusCode::UNAUTHORIZED, "Invalid API key", "invalid_api_key")
        }
        Err(e) => {
            tracing::error!("auth failure: {e}");
            return openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error", "internal");
        }
    };

    // 2. Resolve model -> backend
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let model_alias = extract_model(&body, &content_type);
    let (upstream_model, backend, model_alias) = match &model_alias {
        Some(alias) => match registry::resolve(&st.db, alias).await {
            Ok(Some((model, backend))) => {
                match keys::key_allows_model(&st.db, &key, model.id).await {
                    Ok(true) => (Some(model.upstream_name), backend, Some(alias.clone())),
                    Ok(false) => {
                        return openai_error(
                            StatusCode::NOT_FOUND,
                            &format!("The model `{alias}` does not exist or you do not have access to it."),
                            "model_not_found",
                        )
                    }
                    Err(e) => {
                        tracing::error!("grant check failed: {e}");
                        return openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error", "internal");
                    }
                }
            }
            Ok(None) => {
                return openai_error(
                    StatusCode::NOT_FOUND,
                    &format!("The model `{alias}` does not exist or you do not have access to it."),
                    "model_not_found",
                )
            }
            Err(e) => {
                tracing::error!("resolve failed: {e}");
                return openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error", "internal");
            }
        },
        None => match registry::default_backend(&st.db).await {
            Ok(Some(b)) => (None, b, None),
            Ok(None) => {
                return openai_error(StatusCode::BAD_GATEWAY, "No backend configured", "no_backend")
            }
            Err(e) => {
                tracing::error!("default backend lookup failed: {e}");
                return openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error", "internal");
            }
        },
    };

    // 3. Prepare and forward
    let (upstream_body, usage_injected) = if content_type.starts_with("application/json") {
        metering::prepare_upstream_body(&body, upstream_model.as_deref())
    } else {
        (body.to_vec(), false)
    };

    // base_url is the OpenAI-compatible base including `/v1`; forward the
    // request path with its `/v1` prefix stripped.
    let path_and_query = uri
        .path_and_query()
        .map(|pq| pq.as_str())
        .unwrap_or(&endpoint);
    let suffix = path_and_query.strip_prefix("/v1").unwrap_or(path_and_query);
    let url = format!("{}{}", backend.base_url, suffix);

    let mut req = st
        .http
        .request(method.clone(), &url)
        .body(upstream_body.clone());
    for h in [header::CONTENT_TYPE, header::ACCEPT] {
        if let Some(v) = headers.get(&h) {
            req = req.header(h, v);
        }
    }
    if let Some(k) = &backend.upstream_api_key {
        req = req.bearer_auth(k);
    }

    let req_headers = headers_json(&headers);
    let log_base = LogBase {
        key_id: Some(key.id),
        model: model_alias,
        endpoint,
        req_headers,
        req_body: body.to_vec(),
        started,
    };

    let upstream_resp = match req.send().await {
        Ok(r) => r,
        Err(e) => {
            let msg = format!("Upstream backend unreachable: {e}");
            let resp = openai_error(StatusCode::BAD_GATEWAY, &msg, "upstream_unreachable");
            log_failure(&st, log_base, 502, msg);
            return resp;
        }
    };

    let status = upstream_resp.status();
    let resp_headers = resp_headers_json(upstream_resp.headers());
    let resp_content_type = upstream_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if resp_content_type.starts_with("text/event-stream") {
        return stream_response(
            st,
            log_base,
            upstream_resp,
            status,
            resp_headers,
            resp_content_type,
            usage_injected,
        );
    }

    // 4. Buffered response
    let ttfb_ms = started.elapsed().as_millis() as u32;
    let resp_body = match upstream_resp.bytes().await {
        Ok(b) => b,
        Err(e) => {
            let msg = format!("Failed reading upstream response: {e}");
            let resp = openai_error(StatusCode::BAD_GATEWAY, &msg, "upstream_unreachable");
            log_failure(&st, log_base, 502, msg);
            return resp;
        }
    };

    let (usage, usage_source) = match metering::extract_usage_json(&resp_body) {
        Some(u) => (u, "upstream"),
        None => (
            metering::Usage {
                prompt_tokens: metering::estimate_body_tokens(&log_base.req_body),
                completion_tokens: 0,
            },
            "estimated",
        ),
    };

    st.log.send(LogRecord {
        id: ulid::Ulid::new().to_string(),
        ts: now(),
        key_id: log_base.key_id,
        model: log_base.model.clone(),
        endpoint: log_base.endpoint.clone(),
        status: status.as_u16(),
        latency_ms: started.elapsed().as_millis() as u32,
        ttfb_ms: Some(ttfb_ms),
        usage,
        usage_source,
        error: (!status.is_success()).then(|| format!("upstream status {status}")),
        req_headers: log_base.req_headers,
        resp_headers,
        req_body: log_base.req_body,
        resp_body: resp_body.to_vec(),
    });

    let mut builder = Response::builder().status(status.as_u16());
    if !resp_content_type.is_empty() {
        builder = builder.header(header::CONTENT_TYPE, &resp_content_type);
    }
    builder
        .body(axum::body::Body::from(resp_body))
        .unwrap_or_else(|_| openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error", "internal"))
}

pub(crate) struct LogBase {
    pub key_id: Option<i64>,
    pub model: Option<String>,
    pub endpoint: String,
    pub req_headers: String,
    pub req_body: Vec<u8>,
    pub started: Instant,
}

fn log_failure(st: &AppState, base: LogBase, status: u16, error: String) {
    st.log.send(LogRecord {
        id: ulid::Ulid::new().to_string(),
        ts: now(),
        key_id: base.key_id,
        model: base.model,
        endpoint: base.endpoint,
        status,
        latency_ms: base.started.elapsed().as_millis() as u32,
        ttfb_ms: None,
        usage: metering::Usage::default(),
        usage_source: "estimated",
        error: Some(error),
        req_headers: base.req_headers,
        resp_headers: "{}".into(),
        req_body: base.req_body,
        resp_body: Vec::new(),
    });
}

/// Task 9 replaces this with a filtering tee; for now: raw passthrough that
/// logs after the stream ends.
fn stream_response(
    st: AppState,
    base: LogBase,
    upstream: reqwest::Response,
    status: reqwest::StatusCode,
    resp_headers: String,
    resp_content_type: String,
    _usage_injected: bool,
) -> Response {
    use futures_util::StreamExt;
    let stream = upstream.bytes_stream().map(|c| c.map_err(std::io::Error::other));
    let _ = (st, base, resp_headers);
    Response::builder()
        .status(status.as_u16())
        .header(header::CONTENT_TYPE, resp_content_type)
        .body(axum::body::Body::from_stream(stream))
        .unwrap_or_else(|_| openai_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error", "internal"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_model_from_json() {
        assert_eq!(
            extract_model(br#"{"model":"m1"}"#, "application/json"),
            Some("m1".into())
        );
        assert_eq!(extract_model(b"junk", "application/json"), None);
    }

    #[test]
    fn extract_model_from_multipart() {
        let body = b"--X\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n--X--\r\n";
        assert_eq!(
            extract_model(body, "multipart/form-data; boundary=X"),
            Some("whisper-1".into())
        );
    }
}
