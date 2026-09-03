use crate::db::{now, Db};
use crate::keys::ApiKey;
use crate::logstore::{LogRecord, LogWriter};
use crate::ratelimit::{client_ip, LoginThrottle, PeerAddr};
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
use std::future::Future;
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
    /// Ceiling on the wait for upstream response headers. `None` disables it.
    /// Without one, a backend that accepts the connection and then goes quiet
    /// holds the request open forever and — because the log record is written
    /// only once the response is in hand — leaves nothing in the log.
    pub upstream_timeout: Option<std::time::Duration>,
    /// Ceiling on the gap between two bytes of a streaming response.
    pub upstream_stream_idle: Option<std::time::Duration>,
    /// Retry once when a non-streaming request comes back 502/503. A model
    /// swapper answers that way while it is starting a model.
    pub upstream_retry_on_5xx: bool,
    /// Brute-force wall for `/v1` API-key authentication, keyed on the client
    /// address exactly as the admin login's is.
    pub api_throttle: Arc<LoginThrottle>,
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
            upstream_timeout: Some(std::time::Duration::from_secs(300)),
            upstream_stream_idle: Some(std::time::Duration::from_secs(120)),
            upstream_retry_on_5xx: true,
            api_throttle: Arc::new(LoginThrottle::new(20, std::time::Duration::from_secs(300))),
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
    let ip = client_ip(
        req.headers(),
        PeerAddr(
            req.extensions()
                .get::<axum::extract::ConnectInfo<std::net::SocketAddr>>()
                .map(|ci| ci.0),
        ),
        st.trust_proxy_headers,
    );
    // A locked-out address is turned away before the hash comparison and
    // before anything is written to the log, so a guesser can neither spend
    // our CPU nor flood the request log by hammering the endpoint.
    if let Some(left) = st.api_throttle.check(ip) {
        return openai_error(
            StatusCode::TOO_MANY_REQUESTS,
            &format!(
                "Too many failed authentication attempts. Try again in {}s.",
                left.as_secs().max(1)
            ),
            "rate_limit_exceeded",
        );
    }
    let endpoint = req.uri().path().to_string();
    let req_headers = headers_json(req.headers());
    let Some(token) = bearer(req.headers()) else {
        st.api_throttle.on_failure(ip);
        log_auth_failure(&st, endpoint, req_headers, "missing API key");
        return openai_error(
            StatusCode::UNAUTHORIZED,
            "Missing API key",
            "invalid_api_key",
        );
    };
    match keys::authenticate(&st.db, token).await {
        Ok(Some(key)) => {
            st.api_throttle.on_success(ip);
            req.extensions_mut().insert(key);
            next.run(req).await
        }
        Ok(None) => {
            st.api_throttle.on_failure(ip);
            log_auth_failure(&st, endpoint, req_headers, "invalid API key");
            openai_error(
                StatusCode::UNAUTHORIZED,
                "Invalid API key",
                "invalid_api_key",
            )
        }
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

/// Writes the log row for a rejected request. Rejections used to return from
/// the middleware without touching the log at all, which made credential
/// stuffing against `/v1` invisible in the GUI — the one thing an operator
/// most wants to see. The body is deliberately absent: the layer runs before
/// it is read, so an unauthenticated caller still cannot make us buffer one.
fn log_auth_failure(st: &AppState, endpoint: String, req_headers: String, error: &str) {
    st.log.send(LogRecord {
        id: ulid::Ulid::new().to_string(),
        ts: now(),
        key_id: None,
        model: None,
        endpoint,
        status: 401,
        latency_ms: 0,
        ttfb_ms: None,
        usage: metering::Usage::default(),
        usage_source: "none",
        error: Some(error.to_string()),
        req_headers,
        resp_headers: "{}".into(),
        req_body: Vec::new(),
        resp_body: Vec::new(),
    });
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
            // `created` is required by stricter clients (some LangChain and
            // LibreChat builds refuse a model entry without it). Rows written
            // before the column existed carry 0, which is a valid timestamp.
            "data": models.iter().map(|m| serde_json::json!({
                "id": m.alias, "object": "model", "owned_by": "inginx",
                "created": m.created_at
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

/// Headers that belong to this connection rather than to the request, plus the
/// two we always set ourselves. `Authorization` is dropped because the client's
/// inginx key means nothing upstream — the backend's own key, if any, is added
/// after this loop — and `Accept-Encoding` because a compressed upstream body
/// would defeat both the usage extraction and the log store.
fn is_hop_by_hop(name: &str) -> bool {
    const DENY: [&str; 13] = [
        "authorization",
        "accept-encoding",
        "connection",
        "content-length",
        "cookie",
        "host",
        "keep-alive",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
    ];
    let n = name.to_ascii_lowercase();
    DENY.contains(&n.as_str())
}

/// Rewrites the `voice` field of an audio request through a model's map.
/// `map` is a JSON object of `client name -> backend name`; anything else
/// (a malformed map, a body that is not an object, a voice with no entry) is
/// left alone, so a bad map degrades to today's behaviour rather than breaking
/// every request.
pub(crate) fn apply_voice_map(body: &[u8], map: &str) -> Option<Vec<u8>> {
    let table = serde_json::from_str::<serde_json::Value>(map).ok()?;
    let table = table.as_object()?;
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    let current = v.get("voice")?.as_str()?.to_string();
    let mapped = table.get(&current)?.as_str()?;
    if mapped == current {
        return None;
    }
    v.as_object_mut()?.insert(
        "voice".into(),
        serde_json::Value::String(mapped.to_string()),
    );
    serde_json::to_vec(&v).ok()
}

/// Parses the admin form's `alloy=af_alloy, nova=af_nova` into the JSON object
/// stored on the model. An empty input clears the map.
pub fn parse_voice_map(input: &str) -> Result<Option<String>, String> {
    let mut map = serde_json::Map::new();
    for pair in input.split([',', '\n']) {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let (from, to) = pair
            .split_once('=')
            .ok_or_else(|| format!("expected `from=to`, got `{pair}`"))?;
        let (from, to) = (from.trim(), to.trim());
        if from.is_empty() || to.is_empty() {
            return Err(format!("expected `from=to`, got `{pair}`"));
        }
        map.insert(from.to_string(), serde_json::Value::String(to.to_string()));
    }
    if map.is_empty() {
        return Ok(None);
    }
    Ok(Some(serde_json::Value::Object(map).to_string()))
}

/// Renders a stored voice map back into the form's `from=to, from=to` shape.
pub fn format_voice_map(map: Option<&str>) -> String {
    let Some(raw) = map else {
        return String::new();
    };
    let Ok(serde_json::Value::Object(obj)) = serde_json::from_str::<serde_json::Value>(raw) else {
        return raw.to_string();
    };
    obj.iter()
        .filter_map(|(k, v)| v.as_str().map(|s| format!("{k}={s}")))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Why a request never produced a response. The two cases want different
/// status codes: a backend we could not reach is a 502, a backend that took
/// the request and went quiet is a 504 — and telling them apart in the log is
/// the difference between "the GPU box is down" and "the model would not load".
pub(crate) enum UpstreamError {
    Timeout(u64),
    Transport(String),
}

impl UpstreamError {
    fn status(&self) -> StatusCode {
        match self {
            UpstreamError::Timeout(_) => StatusCode::GATEWAY_TIMEOUT,
            UpstreamError::Transport(_) => StatusCode::BAD_GATEWAY,
        }
    }
    fn code(&self) -> &'static str {
        match self {
            UpstreamError::Timeout(_) => "upstream_timeout",
            UpstreamError::Transport(_) => "upstream_unreachable",
        }
    }
    fn message(&self) -> String {
        match self {
            UpstreamError::Timeout(s) => {
                format!("Upstream backend did not respond within {s}s")
            }
            UpstreamError::Transport(e) => format!("Upstream backend unreachable: {e}"),
        }
    }
}

/// Sends one attempt, bounded by `timeout` if there is one. The timeout covers
/// the wait for response *headers*; the body has its own.
async fn send_upstream(
    req: reqwest::RequestBuilder,
    timeout: Option<std::time::Duration>,
) -> Result<reqwest::Response, UpstreamError> {
    match timeout {
        Some(d) => tokio::time::timeout(d, req.send())
            .await
            .map_err(|_| UpstreamError::Timeout(d.as_secs()))?
            .map_err(|e| UpstreamError::Transport(e.to_string())),
        None => req
            .send()
            .await
            .map_err(|e| UpstreamError::Transport(e.to_string())),
    }
}

/// True when a request body is text we can meaningfully count tokens in. A
/// multipart upload (audio, images) is not, and neither is anything that is
/// not valid UTF-8 — see the `usage_source = "none"` path in `proxy_handler`.
fn is_countable_body(content_type: &str) -> bool {
    let ct = content_type.to_ascii_lowercase();
    !ct.starts_with("multipart/")
        && !ct.starts_with("audio/")
        && !ct.starts_with("image/")
        && !ct.starts_with("video/")
        && !ct.starts_with("application/octet-stream")
}

/// True when the body already carries an OpenAI-shaped `error` object, in
/// which case it is passed through untouched.
fn looks_like_openai_error(body: &[u8]) -> bool {
    serde_json::from_slice::<serde_json::Value>(body)
        .ok()
        .and_then(|v| v.get("error").cloned())
        .map(|e| e.is_object() || e.is_string())
        .unwrap_or(false)
}

/// Wraps a non-OpenAI upstream failure body in the shape every client parses,
/// keeping the upstream's own words as the message. Long bodies (an HTML error
/// page, say) are truncated — the full text is in the request log.
fn normalized_upstream_error(status: reqwest::StatusCode, body: &[u8]) -> Response {
    const MAX: usize = 2048;
    let text = String::from_utf8_lossy(body);
    let text = text.trim();
    let mut msg = if text.is_empty() {
        format!("Upstream backend returned {status}")
    } else if text.len() > MAX {
        format!("{}…", &text[..text.floor_char_boundary(MAX)])
    } else {
        text.to_string()
    };
    if msg.len() > MAX + 8 {
        msg.truncate(MAX);
    }
    let status = StatusCode::from_u16(status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    openai_error(status, &msg, "upstream_error")
}

/// Replaces the value of one multipart form field, returning the new body, or
/// `None` when the field is absent or the body does not parse — in which case
/// the caller forwards what the client sent.
///
/// Byte-level on purpose: the body holds an audio file, so it is not UTF-8,
/// and a part carries no length prefix, which is what makes an in-place
/// substitution of a different length safe.
pub(crate) fn replace_multipart_field(body: &[u8], field: &str, value: &str) -> Option<Vec<u8>> {
    let needle = format!("name=\"{field}\"");
    let at = find_bytes(body, needle.as_bytes())?;
    // The part's headers end at the first blank line after the disposition.
    let rest = &body[at..];
    let (offset, gap) = match (find_bytes(rest, b"\r\n\r\n"), find_bytes(rest, b"\n\n")) {
        (Some(crlf), Some(lf)) if crlf <= lf => (crlf, 4),
        (_, Some(lf)) => (lf, 2),
        (Some(crlf), None) => (crlf, 4),
        (None, None) => return None,
    };
    let start = at + offset + gap;
    // The value runs to the delimiter that opens the next part.
    let end = start
        + find_bytes(&body[start..], b"\r\n--").or_else(|| find_bytes(&body[start..], b"\n--"))?;
    let mut out = Vec::with_capacity(body.len() + value.len());
    out.extend_from_slice(&body[..start]);
    out.extend_from_slice(value.as_bytes());
    out.extend_from_slice(&body[end..]);
    Some(out)
}

fn find_bytes(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// Maps the backend's model name in a response back to the alias the client
/// asked for. Returns `None` when there is nothing to change.
///
/// inginx rewrites the model on the way in; without this the way out still
/// named the backend — a chat completion answered `"model":
/// "unsloth/Qwen3.5-9B-GGUF:q5_k_m"` to a client that asked for
/// `gpgpu/qwen3-5`. That leaks the operator's file layout and quantization
/// choices to every caller, and clients that check the response model against
/// the request see a mismatch. Only a value equal to the upstream name is
/// replaced, so an unrelated `model` field is left alone.
pub(crate) fn rewrite_model_json(body: &[u8], upstream: &str, alias: &str) -> Option<Vec<u8>> {
    if upstream == alias {
        return None;
    }
    let mut v: serde_json::Value = serde_json::from_slice(body).ok()?;
    if !replace_model_value(&mut v, upstream, alias) {
        return None;
    }
    serde_json::to_vec(&v).ok()
}

/// Replaces `model` at the top level and one level into a `data` array (where
/// embeddings and image responses put theirs). Reports whether anything
/// changed.
fn replace_model_value(v: &mut serde_json::Value, upstream: &str, alias: &str) -> bool {
    let mut changed = false;
    let mut set = |obj: &mut serde_json::Value| {
        if obj.get("model").and_then(|m| m.as_str()) == Some(upstream) {
            obj["model"] = serde_json::Value::String(alias.to_string());
            changed = true;
        }
    };
    set(v);
    if let Some(items) = v.get_mut("data").and_then(|d| d.as_array_mut()) {
        for item in items {
            if item.get("model").and_then(|m| m.as_str()) == Some(upstream) {
                item["model"] = serde_json::Value::String(alias.to_string());
                changed = true;
            }
        }
    }
    changed
}

/// The same substitution inside one SSE event. The event's framing — the
/// `data:` prefix, any other fields, the blank-line terminator — is preserved
/// byte for byte; only the JSON payload of a `data:` line is re-serialized,
/// and only when it actually names the upstream model.
pub(crate) fn rewrite_model_sse(event: &[u8], upstream: &str, alias: &str) -> Option<Vec<u8>> {
    if upstream == alias || find_bytes(event, upstream.as_bytes()).is_none() {
        return None;
    }
    let text = std::str::from_utf8(event).ok()?;
    let mut out = String::with_capacity(text.len());
    let mut changed = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_end_matches(['\r', '\n']);
        let terminator = &line[trimmed.len()..];
        match trimmed.strip_prefix("data:") {
            Some(payload) if payload.trim() != "[DONE]" => {
                let mut renamed = None;
                if let Ok(mut v) = serde_json::from_str::<serde_json::Value>(payload.trim()) {
                    if replace_model_value(&mut v, upstream, alias) {
                        renamed = Some(v.to_string());
                    }
                }
                match renamed {
                    Some(json) => {
                        changed = true;
                        out.push_str("data: ");
                        out.push_str(&json);
                        out.push_str(terminator);
                    }
                    None => out.push_str(line),
                }
            }
            _ => out.push_str(line),
        }
    }
    changed.then(|| out.into_bytes())
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
    let (upstream_model, voice_map, backend, model_alias) = match &model_alias {
        Some(alias) => {
            match registry::resolve(&st.db, alias).await {
                Ok(Some((model, backend))) => {
                    match keys::key_allows_model(&st.db, &key, model.id).await {
                    Ok(true) => (
                        Some(model.upstream_name),
                        model.voice_map,
                        backend,
                        Some(alias.clone()),
                    ),
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
            Ok(Some(b)) => (None, None, b, None),
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
    let (mut upstream_body, usage_injected) = if content_type.starts_with("application/json") {
        metering::prepare_upstream_body(&body, upstream_model.as_deref())
    } else if content_type.starts_with("multipart/") {
        // A transcription upload carries its model in a form field, not in
        // JSON. The alias was resolved from it but never rewritten, so the
        // backend was asked for inginx's public name — llama-swap answered
        // "could not find suitable handler for gpgpu/whisper".
        let rewritten = upstream_model
            .as_deref()
            .and_then(|up| replace_multipart_field(&body, "model", up));
        (rewritten.unwrap_or_else(|| body.to_vec()), false)
    } else {
        (body.to_vec(), false)
    };
    // Voice names are backend vocabulary, not protocol: Kokoro answers to
    // `af_alloy`, Piper to a language code, while every stock OpenAI client
    // asks for `alloy` and gets a 400. The per-model map is the translation.
    if let Some(map) = voice_map.as_deref() {
        if content_type.starts_with("application/json") {
            if let Some(rewritten) = apply_voice_map(&upstream_body, map) {
                upstream_body = rewritten;
            }
        }
    }

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
    // Forward the client's headers except the ones that belong to this hop or
    // that we set ourselves. Only `Content-Type` and `Accept` used to survive,
    // which silently dropped `User-Agent`, `OpenAI-Beta`, `X-Title` and the
    // rest — headers some backends route or meter on.
    for (name, value) in headers.iter() {
        if is_hop_by_hop(name.as_str()) {
            continue;
        }
        req = req.header(name, value);
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

    // Keep a second copy of the request for the swap-in-progress retry below.
    // `try_clone` returns None for a streaming body; ours is always in memory.
    let retry = st.upstream_retry_on_5xx.then(|| req.try_clone()).flatten();

    let mut upstream_resp = match send_upstream(req, st.upstream_timeout).await {
        Ok(r) => r,
        Err(e) => {
            let resp = openai_error(e.status(), &e.message(), e.code());
            let status = e.status().as_u16();
            log_failure(&st, log_base, status, e.message());
            return resp;
        }
    };

    // A model swapper answers 502/503 while it brings a model up. The first
    // request paid for the swap; the second usually lands on a running model.
    // Only safe here because nothing has reached the client yet.
    if let Some(retry_req) = retry {
        if matches!(upstream_resp.status().as_u16(), 502 | 503) {
            tracing::info!(
                status = upstream_resp.status().as_u16(),
                endpoint = %log_base.endpoint,
                "upstream refused, retrying once"
            );
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            if let Ok(second) = send_upstream(retry_req, st.upstream_timeout).await {
                upstream_resp = second;
            }
        }
    }

    let status = upstream_resp.status();
    let resp_headers = resp_headers_json(upstream_resp.headers());
    let resp_content_type = upstream_resp
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .to_string();

    if resp_content_type.starts_with("text/event-stream") {
        // Same mapping as the buffered path, applied per SSE event.
        let rename = match (&upstream_model, &log_base.model) {
            (Some(up), Some(alias)) if up != alias => Some((up.clone(), alias.clone())),
            _ => None,
        };
        return stream_response(
            st,
            log_base,
            upstream_resp,
            StreamCtx {
                status,
                resp_headers,
                resp_content_type,
                usage_injected,
                rename,
            },
        );
    }

    // 4. Buffered response
    let ttfb_ms = started.elapsed().as_millis() as u32;
    let read_body = async {
        match st.upstream_timeout {
            Some(d) => tokio::time::timeout(d, upstream_resp.bytes())
                .await
                .map_err(|_| UpstreamError::Timeout(d.as_secs()))?
                .map_err(|e| UpstreamError::Transport(e.to_string())),
            None => upstream_resp
                .bytes()
                .await
                .map_err(|e| UpstreamError::Transport(e.to_string())),
        }
    };
    let resp_body = match read_body.await {
        Ok(b) => b,
        Err(e) => {
            let resp = openai_error(e.status(), &e.message(), e.code());
            let code = e.status().as_u16();
            log_failure(&st, log_base, code, e.message());
            return resp;
        }
    };

    let (usage, usage_source) = match metering::extract_usage_json(&resp_body) {
        Some(u) => (u, "upstream"),
        // Estimating from the request body only means anything when that body
        // is text. A transcription upload is a blob of audio; running it
        // through the estimator booked a 50 MB file as twelve million prompt
        // tokens. Nothing is a better number than a made-up one.
        None if is_countable_body(&content_type) => (
            metering::Usage {
                prompt_tokens: metering::estimate_body_tokens(&log_base.req_body),
                completion_tokens: 0,
            },
            "estimated",
        ),
        None => (metering::Usage::default(), "none"),
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

    // An upstream failure is passed through with its status, but clients parse
    // `{"error": {...}}` and nothing else: llama-swap's plain-text 502 reached
    // callers as an unhandled exception rather than a message. Reshape it,
    // keeping the original text as the message — and note that the log above
    // already holds the untouched body.
    let client_body = if status.is_success() || looks_like_openai_error(&resp_body) {
        // The log above holds the upstream's own words; the client gets the
        // name it asked for.
        match (&upstream_model, &log_base.model) {
            (Some(up), Some(alias)) if resp_content_type.starts_with("application/json") => {
                rewrite_model_json(&resp_body, up, alias)
                    .map(Bytes::from)
                    .unwrap_or(resp_body)
            }
            _ => resp_body,
        }
    } else {
        return normalized_upstream_error(status, &resp_body);
    };

    let mut builder = Response::builder().status(status.as_u16());
    if !resp_content_type.is_empty() {
        builder = builder.header(header::CONTENT_TYPE, &resp_content_type);
    }
    builder
        .body(axum::body::Body::from(client_body))
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
/// Everything the streaming path needs to know about the response it is
/// about to tee, gathered so the function keeps one argument per idea.
pub(crate) struct StreamCtx {
    pub status: reqwest::StatusCode,
    pub resp_headers: String,
    pub resp_content_type: String,
    /// The client did not ask for a usage chunk, so the one inginx requested
    /// is filtered back out.
    pub usage_injected: bool,
    /// `(upstream name, alias)` when they differ.
    pub rename: Option<(String, String)>,
}

fn stream_response(
    st: AppState,
    base: LogBase,
    upstream: reqwest::Response,
    ctx: StreamCtx,
) -> Response {
    let StreamCtx {
        status,
        resp_headers,
        resp_content_type,
        usage_injected,
        rename,
    } = ctx;
    use futures_util::StreamExt;
    let idle = st.upstream_stream_idle;
    let tee = StreamTee {
        inner: upstream.bytes_stream().boxed(),
        idle,
        idle_timer: idle.map(|d| Box::pin(tokio::time::sleep(d))),
        st,
        base: Some(base),
        status: status.as_u16(),
        resp_headers,
        filter: usage_injected,
        rename,
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
    /// How long the stream may go silent before it is abandoned. A stalled
    /// upstream otherwise holds the connection — and the log record, which is
    /// written when the stream ends — open indefinitely.
    idle: Option<std::time::Duration>,
    idle_timer: Option<std::pin::Pin<Box<tokio::time::Sleep>>>,
    st: AppState,
    base: Option<LogBase>,
    status: u16,
    resp_headers: String,
    filter: bool,
    /// `(upstream name, alias)` when the two differ: every `data:` payload
    /// naming the backend's model is rewritten to the alias on its way out.
    rename: Option<(String, String)>,
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
    // `poll` on the idle timer needs `Future` in scope.

    type Item = std::io::Result<Bytes>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        use std::task::Poll;
        let this = self.get_mut();
        loop {
            match this.inner.as_mut().poll_next(cx) {
                Poll::Pending => {
                    let (Some(timer), Some(idle)) = (this.idle_timer.as_mut(), this.idle) else {
                        return Poll::Pending;
                    };
                    match timer.as_mut().poll(cx) {
                        Poll::Pending => return Poll::Pending,
                        Poll::Ready(()) => {
                            this.completed = true;
                            let msg = format!("upstream stream stalled for {}s", idle.as_secs());
                            this.finish_with_error(msg.clone());
                            return Poll::Ready(Some(Err(std::io::Error::other(msg))));
                        }
                    }
                }
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
                    // A byte arrived: the stream is alive, so the clock restarts.
                    if let (Some(timer), Some(idle)) = (this.idle_timer.as_mut(), this.idle) {
                        timer.as_mut().reset(tokio::time::Instant::now() + idle);
                    }
                    if this.ttfb_ms.is_none() {
                        if let Some(base) = &this.base {
                            this.ttfb_ms = Some(base.started.elapsed().as_millis() as u32);
                        }
                    }
                    // `capture` is the upstream's own bytes, before any
                    // filtering or renaming: the log stays the record of what
                    // the backend actually said.
                    this.capture.extend_from_slice(&chunk);
                    // A chunk needs assembling into events only when
                    // something downstream inspects them.
                    if !this.filter && this.rename.is_none() {
                        return Poll::Ready(Some(Ok(chunk)));
                    }
                    this.pending.extend_from_slice(&chunk);
                    let mut out = Vec::new();
                    while let Some(end) = event_boundary(&this.pending) {
                        let event: Vec<u8> = this.pending.drain(..end).collect();
                        if this.filter && is_usage_only_event(&event) {
                            continue;
                        }
                        match &this.rename {
                            Some((up, alias)) => match rewrite_model_sse(&event, up, alias) {
                                Some(renamed) => out.extend_from_slice(&renamed),
                                None => out.extend_from_slice(&event),
                            },
                            None => out.extend_from_slice(&event),
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
    fn voice_map_rewrites_known_voice_only() {
        let map = r#"{"alloy":"af_alloy","nova":"af_nova"}"#;
        let out = apply_voice_map(br#"{"model":"tts-1","voice":"alloy"}"#, map).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["voice"], "af_alloy");
        assert_eq!(v["model"], "tts-1");
        // A voice with no entry, a body that is not an object, and a broken
        // map all leave the request exactly as the client sent it.
        assert!(apply_voice_map(br#"{"voice":"af_heart"}"#, map).is_none());
        assert!(apply_voice_map(br#"{"input":"hi"}"#, map).is_none());
        assert!(apply_voice_map(br#"{"voice":"alloy"}"#, "not json").is_none());
    }

    #[test]
    fn voice_map_form_round_trip() {
        let stored = parse_voice_map(" alloy=af_alloy , nova = af_nova ")
            .unwrap()
            .unwrap();
        let v: serde_json::Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(v["alloy"], "af_alloy");
        assert_eq!(v["nova"], "af_nova");
        let rendered = format_voice_map(Some(&stored));
        assert!(rendered.contains("alloy=af_alloy"), "{rendered}");
        assert!(parse_voice_map("   ").unwrap().is_none());
        assert!(parse_voice_map("alloy").is_err());
        assert!(parse_voice_map("alloy=").is_err());
    }

    #[test]
    fn hop_by_hop_headers_are_not_forwarded() {
        for name in [
            "authorization",
            "Accept-Encoding",
            "connection",
            "content-length",
            "host",
            "cookie",
        ] {
            assert!(is_hop_by_hop(name), "{name} should be dropped");
        }
        for name in [
            "content-type",
            "accept",
            "user-agent",
            "openai-beta",
            "x-title",
        ] {
            assert!(!is_hop_by_hop(name), "{name} should be forwarded");
        }
    }

    #[test]
    fn binary_uploads_are_not_token_counted() {
        assert!(!is_countable_body("multipart/form-data; boundary=x"));
        assert!(!is_countable_body("audio/mpeg"));
        assert!(is_countable_body("application/json"));
        assert!(is_countable_body(""));
    }

    #[test]
    fn upstream_error_bodies_are_reshaped() {
        assert!(looks_like_openai_error(
            br#"{"error":{"message":"nope","type":"invalid_request_error"}}"#
        ));
        assert!(!looks_like_openai_error(
            b"unable to start process: process was already starting"
        ));
        assert!(!looks_like_openai_error(b"{\"detail\":\"nope\"}"));
    }

    #[tokio::test]
    async fn plain_text_upstream_failure_reaches_the_client_as_json() {
        let resp = normalized_upstream_error(
            reqwest::StatusCode::BAD_GATEWAY,
            b"unable to start process: process was already starting but wound up in state stopped",
        );
        assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
        let body = axum::body::to_bytes(resp.into_body(), 64 * 1024)
            .await
            .unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(v["error"]["code"], "upstream_error");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("wound up in state stopped"));
    }

    #[test]
    fn response_model_is_mapped_back_to_the_alias() {
        let body = br#"{"model":"unsloth/Qwen3.5-9B-GGUF:q5_k_m","choices":[]}"#;
        let out =
            rewrite_model_json(body, "unsloth/Qwen3.5-9B-GGUF:q5_k_m", "gpgpu/qwen3-5").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["model"], "gpgpu/qwen3-5");
        // An unrelated value is left alone, and so is a body that never
        // mentions the backend's name.
        assert!(rewrite_model_json(br#"{"model":"something-else"}"#, "up", "alias").is_none());
        assert!(rewrite_model_json(b"not json", "up", "alias").is_none());
        assert!(rewrite_model_json(br#"{"model":"same"}"#, "same", "same").is_none());
    }

    #[test]
    fn embeddings_data_entries_are_mapped_too() {
        let body = br#"{"object":"list","data":[{"model":"bge-up","embedding":[]}]}"#;
        let out = rewrite_model_json(body, "bge-up", "gpgpu/bge").unwrap();
        let v: serde_json::Value = serde_json::from_slice(&out).unwrap();
        assert_eq!(v["data"][0]["model"], "gpgpu/bge");
    }

    #[test]
    fn sse_events_are_renamed_without_losing_framing() {
        let event =
            b"data: {\"model\":\"up-name\",\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n";
        let out = rewrite_model_sse(event, "up-name", "alias").unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.starts_with("data: "), "{text}");
        assert!(text.ends_with("\n\n"), "{text:?}");
        assert!(text.contains("\"model\":\"alias\""), "{text}");
        assert!(
            text.contains("\"content\":\"hi\""),
            "content must survive: {text}"
        );
        // The terminator event and anything not naming the model are
        // untouched, so the cheap path stays cheap.
        assert!(rewrite_model_sse(b"data: [DONE]\n\n", "up-name", "alias").is_none());
        assert!(rewrite_model_sse(b"data: {\"choices\":[]}\n\n", "up-name", "alias").is_none());
    }

    #[test]
    fn multipart_model_field_is_rewritten() {
        // The exact shape a transcription upload has: the model field, then a
        // binary file part.
        let b = b"--BOUND\r\nContent-Disposition: form-data; name=\"model\"\r\n\r\ngpgpu/whisper\r\n--BOUND\r\nContent-Disposition: form-data; name=\"file\"; filename=\"a.wav\"\r\nContent-Type: audio/wav\r\n\r\nRIFF\x00\xff\r\n--BOUND--\r\n";
        let out = replace_multipart_field(b, "model", "whisper").unwrap();
        let text = String::from_utf8_lossy(&out);
        assert!(text.contains("\r\n\r\nwhisper\r\n--BOUND"), "{text}");
        assert!(!text.contains("gpgpu/whisper"));
        // The binary part survives byte for byte.
        assert!(out.windows(6).any(|w| w == b"RIFF\x00\xff"));
        // And the rewritten body still parses as the model we just set.
        assert_eq!(
            extract_model(&out, "multipart/form-data; boundary=BOUND"),
            Some("whisper".into())
        );
    }

    #[test]
    fn multipart_without_the_field_is_left_alone() {
        let b = b"--B\r\nContent-Disposition: form-data; name=\"file\"\r\n\r\ndata\r\n--B--\r\n";
        assert!(replace_multipart_field(b, "model", "x").is_none());
        assert!(replace_multipart_field(b"not multipart at all", "model", "x").is_none());
    }

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
