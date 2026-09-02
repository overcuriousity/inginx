use crate::db::{now, Db};
use crate::keys::ApiKey;
use crate::logstore::{LogRecord, LogWriter};
use crate::ratelimit::LoginThrottle;
use crate::{keys, metering, registry};
use axum::{
    body::Bytes,
    extract::{DefaultBodyLimit, Extension, Request, State},
    http::{header, HeaderMap, Method, StatusCode, Uri},
    middleware::{from_fn_with_state, Next},
    response::{IntoResponse, Response},
    routing::{any, get},
    Json, Router,
};
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone)]
pub struct AppState {
    pub db: Db,
    pub log: LogWriter,
    pub http: reqwest::Client,
    pub login_throttle: Arc<LoginThrottle>,
    /// Largest request body the proxy will buffer. Carried on the state so the
    /// admin router, which shares the state, can leave its own small default
    /// in place — this ceiling is for the proxy alone.
    pub max_body_bytes: usize,
    /// Whether to believe `X-Real-IP` / `X-Forwarded-For` / `X-Forwarded-Proto`
    /// from a fronting reverse proxy. See `config::Config::trust_proxy_headers`.
    pub trust_proxy_headers: bool,
}

impl AppState {
    /// Builds an `AppState` with the security knobs at their production
    /// defaults, so a test names only the three things it varies. Not behind
    /// `#[cfg(test)]` because the integration tests are a separate crate.
    #[doc(hidden)]
    pub fn for_test(db: Db, log: LogWriter, http: reqwest::Client) -> Self {
        Self {
            db,
            log,
            http,
            login_throttle: Arc::new(LoginThrottle::new(5, std::time::Duration::from_secs(300))),
            max_body_bytes: 32 * 1024 * 1024,
            trust_proxy_headers: false,
        }
    }
}

pub fn router(state: AppState) -> Router {
    let limit = state.max_body_bytes;
    Router::new()
        .route("/v1/models", get(models_handler))
        .route("/v1/{*path}", any(proxy_handler))
        // The order matters. `from_fn` middleware runs before the handler's
        // own extractors, so the key is checked while the request body is
        // still a stream on the wire — an unauthenticated caller is turned
        // away before a single byte of a body is buffered. The body limit is
        // the backstop for everyone who *is* authenticated.
        .layer(from_fn_with_state(state.clone(), require_api_key))
        .layer(DefaultBodyLimit::max(limit))
        .with_state(state)
}

/// Pulls the bearer token, authenticates it, and stashes the resolved key for
/// the handler — all before the body is read. A miss returns the OpenAI-shaped
/// 401 here rather than deep in a handler that would first have paid to buffer
/// the body. See the note in `router` for why this is a layer and not a step
/// inside `proxy_handler`.
async fn require_api_key(State(st): State<AppState>, mut req: Request, next: Next) -> Response {
    let Some(token) = bearer(req.headers()) else {
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "Missing API key",
            "invalid_api_key",
        );
    };
    match keys::authenticate(&st.db, token).await {
        Ok(Some(key)) => {
            req.extensions_mut().insert(key);
            next.run(req).await
        }
        Ok(None) => openai_error(
            StatusCode::UNAUTHORIZED,
            "Invalid API key",
            "invalid_api_key",
        ),
        Err(e) => {
            tracing::error!("auth failure: {e}");
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
                "internal",
            )
        }
    }
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

/// True for a header whose value is a secret we must never write to the log
/// store. `Authorization` was the only name redacted before, on the
/// assumption that it is the only place a credential rides — but a client can
/// authenticate to an upstream with `X-Api-Key`, carry a session in `Cookie`,
/// or invent a header for a token, and every one of those was landing in the
/// database in cleartext and rendering in the log viewer. Match by shape
/// rather than by an exhaustive list: any name that says key, token, secret,
/// auth, cookie, session or password is redacted, which catches the vendor
/// headers we have not thought of as well as the ones we have.
fn is_sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    const NEEDLES: [&str; 8] = [
        "authorization",
        "cookie",
        "key",
        "token",
        "secret",
        "auth",
        "session",
        "password",
    ];
    NEEDLES.iter().any(|needle| n.contains(needle))
}

/// Serializes headers as a JSON object, redacting the value of any header that
/// looks like it carries a credential. See `is_sensitive_header`.
fn headers_json(headers: &HeaderMap) -> String {
    let map: serde_json::Map<String, serde_json::Value> = headers
        .iter()
        .map(|(k, v)| {
            let val = if is_sensitive_header(k.as_str()) {
                "***redacted***".to_string()
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
            // An upstream's `Set-Cookie` is as much a credential as the
            // request's `Authorization`; redact the response side by the same
            // rule.
            let val = if is_sensitive_header(k.as_str()) {
                "***redacted***".to_string()
            } else {
                v.to_str().unwrap_or("<binary>").to_string()
            };
            (k.to_string(), serde_json::Value::String(val))
        })
        .collect();
    serde_json::Value::Object(map).to_string()
}

async fn models_handler(State(st): State<AppState>, Extension(key): Extension<ApiKey>) -> Response {
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
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
                "internal",
            )
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
        let blank = rest
            .find("\r\n\r\n")
            .map(|i| i + 4)
            .or_else(|| rest.find("\n\n").map(|i| i + 2))?;
        let val = rest[blank..].lines().next()?.trim().trim_end_matches('\r');
        return (!val.is_empty()).then(|| val.to_string());
    }
    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    v.get("model")?.as_str().map(String::from)
}

pub async fn proxy_handler(
    State(st): State<AppState>,
    Extension(key): Extension<ApiKey>,
    method: Method,
    uri: Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let endpoint = uri.path().to_string();
    let started = Instant::now();

    // 1. Authenticate — done in `require_api_key` before the body was read;
    //    the resolved key arrives as an extension.

    // 2. Resolve model -> backend
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();
    let model_alias = extract_model(&body, &content_type);
    let (upstream_model, backend, model_alias) = match &model_alias {
        Some(alias) => {
            match registry::resolve(&st.db, alias).await {
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
                        &format!(
                            "The model `{alias}` does not exist or you do not have access to it."
                        ),
                        "model_not_found",
                    )
                }
                Err(e) => {
                    tracing::error!("resolve failed: {e}");
                    return openai_error(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "internal error",
                        "internal",
                    );
                }
            }
        }
        None => match registry::default_backend(&st.db).await {
            Ok(Some(b)) => (None, b, None),
            Ok(None) => {
                return openai_error(
                    StatusCode::BAD_GATEWAY,
                    "No backend configured",
                    "no_backend",
                )
            }
            Err(e) => {
                tracing::error!("default backend lookup failed: {e}");
                return openai_error(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "internal error",
                    "internal",
                );
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
        .unwrap_or_else(|_| {
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
                "internal",
            )
        })
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

/// Streams the upstream SSE response to the client while capturing every byte
/// for logging. When the usage chunk was injected by inginx (the client did
/// not ask for it), that chunk is filtered out of the client stream. Logging
/// happens in the tee's Drop, which fires on natural end AND client
/// disconnect.
fn stream_response(
    st: AppState,
    base: LogBase,
    upstream: reqwest::Response,
    status: reqwest::StatusCode,
    resp_headers: String,
    resp_content_type: String,
    usage_injected: bool,
) -> Response {
    use futures_util::StreamExt;
    let tee = StreamTee {
        inner: upstream.bytes_stream().boxed(),
        st,
        base: Some(base),
        status: status.as_u16(),
        resp_headers,
        filter: usage_injected,
        pending: Vec::new(),
        capture: Vec::new(),
        ttfb_ms: None,
        completed: false,
    };
    Response::builder()
        .status(status.as_u16())
        .header(header::CONTENT_TYPE, resp_content_type)
        .body(axum::body::Body::from_stream(tee))
        .unwrap_or_else(|_| {
            openai_error(
                StatusCode::INTERNAL_SERVER_ERROR,
                "internal error",
                "internal",
            )
        })
}

struct StreamTee {
    inner: futures_util::stream::BoxStream<'static, reqwest::Result<Bytes>>,
    st: AppState,
    base: Option<LogBase>,
    status: u16,
    resp_headers: String,
    filter: bool,
    pending: Vec<u8>,
    capture: Vec<u8>,
    ttfb_ms: Option<u32>,
    completed: bool,
}

/// Finds the end of the first complete SSE event in `buf` (delimiter
/// included), handling both `\n\n` and `\r\n\r\n`.
fn event_boundary(buf: &[u8]) -> Option<usize> {
    let lf = buf.windows(2).position(|w| w == b"\n\n").map(|i| i + 2);
    let crlf = buf.windows(4).position(|w| w == b"\r\n\r\n").map(|i| i + 4);
    match (lf, crlf) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    }
}

/// True for the injected final chunk: carries `usage` with no choices content.
fn is_usage_only_event(event: &[u8]) -> bool {
    let Ok(text) = std::str::from_utf8(event) else {
        return false;
    };
    for line in text.lines() {
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload == "[DONE]" {
            return false;
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(payload) {
            let has_usage = v.get("usage").map(|u| u.is_object()).unwrap_or(false);
            let choices_empty = v
                .get("choices")
                .and_then(|c| c.as_array())
                .map(|a| a.is_empty())
                .unwrap_or(true);
            return has_usage && choices_empty;
        }
    }
    false
}

impl futures_util::Stream for StreamTee {
    type Item = std::io::Result<Bytes>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => return Poll::Pending,
                Poll::Ready(None) => {
                    this.completed = true;
                    if this.pending.is_empty() {
                        this.finish();
                        return Poll::Ready(None);
                    }
                    // flush whatever incomplete tail remains
                    let tail = std::mem::take(&mut this.pending);
                    this.finish();
                    return Poll::Ready(Some(Ok(Bytes::from(tail))));
                }
                Poll::Ready(Some(Err(e))) => {
                    this.completed = true;
                    this.finish_with_error(format!("upstream stream error: {e}"));
                    return Poll::Ready(Some(Err(std::io::Error::other(e))));
                }
                Poll::Ready(Some(Ok(chunk))) => {
                    if this.ttfb_ms.is_none() {
                        if let Some(base) = &this.base {
                            this.ttfb_ms = Some(base.started.elapsed().as_millis() as u32);
                        }
                    }
                    this.capture.extend_from_slice(&chunk);
                    if !this.filter {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    this.pending.extend_from_slice(&chunk);
                    let mut out = Vec::new();
                    while let Some(end) = event_boundary(&this.pending) {
                        let event: Vec<u8> = this.pending.drain(..end).collect();
                        if !is_usage_only_event(&event) {
                            out.extend_from_slice(&event);
                        }
                    }
                    if !out.is_empty() {
                        return Poll::Ready(Some(Ok(Bytes::from(out))));
                    }
                    // nothing to emit yet; poll upstream again
                }
            }
        }
    }
}

impl StreamTee {
    fn build_record(&mut self, error: Option<String>) -> Option<LogRecord> {
        let base = self.base.take()?;
        let (usage, usage_source) = match metering::extract_usage_sse(&self.capture) {
            Some(u) => (u, "upstream"),
            None => (
                metering::Usage {
                    prompt_tokens: metering::estimate_body_tokens(&base.req_body),
                    completion_tokens: metering::estimate_body_tokens(&self.capture),
                },
                "estimated",
            ),
        };
        Some(LogRecord {
            id: ulid::Ulid::new().to_string(),
            ts: now(),
            key_id: base.key_id,
            model: base.model,
            endpoint: base.endpoint,
            status: self.status,
            latency_ms: base.started.elapsed().as_millis() as u32,
            ttfb_ms: self.ttfb_ms,
            usage,
            usage_source,
            error,
            req_headers: base.req_headers,
            resp_headers: std::mem::take(&mut self.resp_headers),
            req_body: base.req_body,
            resp_body: std::mem::take(&mut self.capture),
        })
    }

    fn finish(&mut self) {
        if let Some(rec) = self.build_record(None) {
            self.st.log.send(rec);
        }
    }

    fn finish_with_error(&mut self, error: String) {
        if let Some(rec) = self.build_record(Some(error)) {
            self.st.log.send(rec);
        }
    }
}

impl Drop for StreamTee {
    fn drop(&mut self) {
        if self.base.is_some() {
            let error = (!self.completed).then(|| "client_disconnected".to_string());
            if let Some(rec) = self.build_record(error) {
                self.st.log.send(rec);
            }
        }
    }
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
    fn sensitive_headers_are_redacted() {
        for name in [
            "authorization",
            "Authorization",
            "Cookie",
            "X-Api-Key",
            "x-secret-token",
            "proxy-authorization",
            "x-session-id",
        ] {
            assert!(is_sensitive_header(name), "{name} should be redacted");
        }
        for name in ["content-type", "accept", "user-agent", "x-request-id"] {
            assert!(!is_sensitive_header(name), "{name} should be kept");
        }

        let mut h = HeaderMap::new();
        h.insert(header::AUTHORIZATION, "Bearer igx_secret".parse().unwrap());
        h.insert(header::CONTENT_TYPE, "application/json".parse().unwrap());
        h.insert("x-api-key", "sk-live-123".parse().unwrap());
        let json = headers_json(&h);
        assert!(!json.contains("igx_secret"), "auth value leaked: {json}");
        assert!(!json.contains("sk-live-123"), "api key leaked: {json}");
        assert!(json.contains("***redacted***"));
        assert!(json.contains("application/json"), "safe header dropped");
    }

    #[test]
    fn extract_model_from_multipart() {
        let body =
            b"--X\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\nwhisper-1\r\n--X--\r\n";
        assert_eq!(
            extract_model(body, "multipart/form-data; boundary=X"),
            Some("whisper-1".into())
        );
    }
}
