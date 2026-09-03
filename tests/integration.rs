use inginx::{db::Db, keys, logstore, proxy, registry};
use std::net::SocketAddr;

async fn spawn_app(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

/// Mimics llama.cpp: streams SSE when `stream:true`, and emits the final
/// usage chunk only when `stream_options.include_usage` was requested.
async fn mock_chat(axum::Json(body): axum::Json<serde_json::Value>) -> axum::response::Response {
    use axum::response::IntoResponse;
    if body["stream"].as_bool() != Some(true) {
        return axum::Json(serde_json::json!({
            "choices":[{"message":{"role":"assistant","content":"hi"}}],
            "usage":{"prompt_tokens":3,"completion_tokens":2}
        }))
        .into_response();
    }
    let include_usage = body["stream_options"]["include_usage"].as_bool() == Some(true);
    let mut events: Vec<String> = vec![
        "data: {\"choices\":[{\"delta\":{\"content\":\"He\"}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"llo\"}}]}\n\n".into(),
        "data: {\"choices\":[{\"delta\":{\"content\":\"!\"}}]}\n\n".into(),
    ];
    if include_usage {
        events.push(
            "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":11,\"completion_tokens\":3}}\n\n"
                .into(),
        );
    }
    events.push("data: [DONE]\n\n".into());
    use futures_util::StreamExt;
    let stream = futures_util::stream::iter(events).then(|e| async move {
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        Ok::<_, std::convert::Infallible>(e)
    });
    axum::response::Response::builder()
        .header("content-type", "text/event-stream")
        .body(axum::body::Body::from_stream(stream))
        .unwrap()
}

/// Never answers. Stands in for a backend that accepted the request and then
/// went quiet — llama-swap with a model that failed to start.
async fn mock_hang() -> axum::response::Response {
    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
    axum::response::Response::new(axum::body::Body::empty())
}

/// 502 with a plain-text body the first time, 200 after that: a model swapper
/// reporting a swap in progress and then succeeding.
async fn mock_flaky(
    axum::extract::State(hits): axum::extract::State<
        std::sync::Arc<std::sync::atomic::AtomicUsize>,
    >,
) -> axum::response::Response {
    use axum::response::IntoResponse;
    let n = hits.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    if n == 0 {
        return (
            axum::http::StatusCode::BAD_GATEWAY,
            "unable to start process: process was already starting but wound up in state stopped",
        )
            .into_response();
    }
    axum::Json(serde_json::json!({
        "choices":[{"message":{"content":"second try"}}],
        "usage":{"prompt_tokens":1,"completion_tokens":1}
    }))
    .into_response()
}

async fn spawn_mock_upstream() -> SocketAddr {
    use axum::{routing::get, routing::post, Json, Router};
    let flaky_hits = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let app = Router::new()
        .route("/chat/completions", post(mock_chat))
        .route("/hang", post(mock_hang))
        // One chunk, then silence: a stream that stalls mid-flight.
        .route(
            "/stall",
            post(|| async {
                use futures_util::StreamExt;
                let stream = futures_util::stream::iter(vec![
                    "data: {\"choices\":[{\"delta\":{\"content\":\"hi\"}}]}\n\n".to_string(),
                ])
                .chain(futures_util::stream::once(async {
                    tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                    String::new()
                }))
                .map(Ok::<_, std::convert::Infallible>);
                axum::response::Response::builder()
                    .header("content-type", "text/event-stream")
                    .body(axum::body::Body::from_stream(stream))
                    .unwrap()
            }),
        )
        .route(
            "/flaky",
            post(mock_flaky).with_state(flaky_hits),
        )
        .route(
            "/broken",
            post(|| async {
                (
                    axum::http::StatusCode::BAD_GATEWAY,
                    "unable to start process: process was already starting but wound up in state stopped",
                )
            }),
        )
        // Echoes the body back so a test can see what inginx forwarded.
        .route(
            "/audio/speech-echo",
            post(|axum::Json(b): axum::Json<serde_json::Value>| async move { Json(b) }),
        )
        // Reports the model it was asked for, the way llama-swap would fail
        // on one it does not know.
        .route(
            "/audio/transcriptions",
            post(|body: axum::body::Bytes| async move {
                let text = String::from_utf8_lossy(&body);
                let model = text
                    .split_once("name=\"model\"")
                    .and_then(|(_, rest)| rest.split_once("\r\n\r\n"))
                    .and_then(|(_, v)| v.split("\r\n").next())
                    .unwrap_or("")
                    .to_string();
                Json(serde_json::json!({"text": "hello", "model_seen": model}))
            }),
        )
        // Reports the headers it received, so a test can assert what survived
        // the hop.
        .route(
            "/echo-headers",
            post(|headers: axum::http::HeaderMap| async move {
                let m: serde_json::Map<String, serde_json::Value> = headers
                    .iter()
                    .map(|(k, v)| {
                        (
                            k.to_string(),
                            serde_json::Value::String(v.to_str().unwrap_or("").to_string()),
                        )
                    })
                    .collect();
                Json(serde_json::Value::Object(m))
            }),
        )
        .route(
            "/models",
            get(|| async { Json(serde_json::json!({"data":[{"id":"m-upstream"}]})) }),
        )
        .route(
            "/audio/speech",
            post(|| async {
                axum::response::Response::builder()
                    .header("content-type", "audio/wav")
                    .body(axum::body::Body::from(&b"RIFFfakewavdata"[..]))
                    .unwrap()
            }),
        )
        .route(
            "/echo",
            post(|body: String| async move { format!("echoed:{body}") }),
        );
    spawn_app(app).await
}

struct Ctx {
    db: Db,
    inginx: SocketAddr,
    key: String,
}

/// Backend pointed at the mock upstream, model alias "m" -> "m-upstream",
/// one all-models API key.
async fn setup() -> Ctx {
    setup_with(|_| {}).await
}

/// `setup`, with a chance to change the proxy's knobs before it is served.
async fn setup_with(tweak: impl FnOnce(&mut proxy::AppState)) -> Ctx {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    std::mem::forget(dir);
    let upstream = spawn_mock_upstream().await;
    let b = registry::add_backend(&db, "mock", &format!("http://{upstream}"), None, true)
        .await
        .unwrap();
    registry::add_model(&db, b, "m-upstream", "m")
        .await
        .unwrap();
    let key = keys::create(&db, "test", true, &[], None).await.unwrap();
    let mut state = proxy::AppState::for_test(
        db.clone(),
        logstore::spawn_writer(db.clone()),
        reqwest::Client::new(),
    );
    tweak(&mut state);
    let inginx = spawn_app(proxy::router(state)).await;
    Ctx { db, inginx, key }
}

async fn wait_for_rows(db: &Db, n: i64) {
    for _ in 0..200 {
        let count: i64 = db
            .call(|c| Ok(c.query_row("SELECT count(*) FROM requests", [], |r| r.get(0))?))
            .await
            .unwrap();
        if count >= n {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("timed out waiting for {n} log rows");
}

#[tokio::test]
async fn chat_roundtrip_metered_and_logged() {
    let ctx = setup().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({"model":"m","messages":[{"role":"user","content":"hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "hi");
    wait_for_rows(&ctx.db, 1).await;
    let (pt, ct, src, model): (i64, i64, String, String) = ctx
        .db
        .call(|c| {
            Ok(c.query_row(
                "SELECT prompt_tokens, completion_tokens, usage_source, model FROM requests",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!((pt, ct), (3, 2));
    assert_eq!(src, "upstream");
    assert_eq!(model, "m");
}

#[tokio::test]
async fn stream_passthrough_suppresses_injected_usage() {
    let ctx = setup().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hello"}]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.text().await.unwrap();
    assert!(body.contains("data: [DONE]"), "body: {body}");
    assert!(
        body.contains("\"He\"") && body.contains("\"llo\""),
        "chunks must be forwarded: {body}"
    );
    assert!(
        !body.contains("\"usage\""),
        "injected usage chunk must be suppressed: {body}"
    );
    wait_for_rows(&ctx.db, 1).await;
    let (pt, ct, src): (i64, i64, String) = ctx
        .db
        .call(|c| {
            Ok(c.query_row(
                "SELECT prompt_tokens, completion_tokens, usage_source FROM requests",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!((pt, ct), (11, 3));
    assert_eq!(src, "upstream");
}

#[tokio::test]
async fn stream_forwards_usage_when_client_asked() {
    let ctx = setup().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({
            "model":"m","stream":true,
            "stream_options":{"include_usage":true},
            "messages":[{"role":"user","content":"hello"}]
        }))
        .send()
        .await
        .unwrap();
    let body = resp.text().await.unwrap();
    assert!(body.contains("\"usage\""), "client asked for usage: {body}");
    assert!(body.contains("data: [DONE]"));
}

#[tokio::test]
async fn client_disconnect_still_logs() {
    use futures_util::StreamExt;
    let ctx = setup().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({"model":"m","stream":true,"messages":[{"role":"user","content":"hello"}]}))
        .send()
        .await
        .unwrap();
    let mut stream = resp.bytes_stream();
    let _first = stream.next().await.unwrap().unwrap();
    drop(stream); // client walks away mid-stream

    for _ in 0..300 {
        let err: Option<String> = ctx
            .db
            .call(|c| {
                Ok(
                    c.query_row("SELECT error FROM requests LIMIT 1", [], |r| r.get(0))
                        .ok(),
                )
            })
            .await
            .unwrap();
        if err.as_deref() == Some("client_disconnected") {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    panic!("no client_disconnected log row appeared");
}

#[tokio::test]
async fn bad_key_401_openai_shape() {
    let ctx = setup().await;
    for auth in [None, Some("igx_definitelywrong")] {
        let mut req = reqwest::Client::new()
            .post(format!("http://{}/v1/chat/completions", ctx.inginx))
            .json(&serde_json::json!({"model":"m","messages":[]}));
        if let Some(a) = auth {
            req = req.bearer_auth(a);
        }
        let resp = req.send().await.unwrap();
        assert_eq!(resp.status(), 401);
        let v: serde_json::Value = resp.json().await.unwrap();
        assert_eq!(v["error"]["code"], "invalid_api_key");
    }
}

#[tokio::test]
async fn unknown_model_404() {
    let ctx = setup().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/chat/completions", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({"model":"ghost","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 404);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "model_not_found");
}

#[tokio::test]
async fn tts_binary_response_estimated_usage() {
    let ctx = setup().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/audio/speech", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({"model":"m","input":"Hello world, please speak this.","voice":"alloy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.headers().get("content-type").unwrap(), "audio/wav");
    let body = resp.bytes().await.unwrap();
    assert_eq!(&body[..], b"RIFFfakewavdata");
    wait_for_rows(&ctx.db, 1).await;
    let (pt, ct, src): (i64, i64, String) = ctx
        .db
        .call(|c| {
            Ok(c.query_row(
                "SELECT prompt_tokens, completion_tokens, usage_source FROM requests",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )?)
        })
        .await
        .unwrap();
    assert!(pt > 0, "prompt tokens estimated from input text");
    assert_eq!(ct, 0);
    assert_eq!(src, "estimated");
}

#[tokio::test]
async fn unknown_path_passthrough_to_default_backend() {
    let ctx = setup().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/echo", ctx.inginx))
        .bearer_auth(&ctx.key)
        .body("ping")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    assert_eq!(resp.text().await.unwrap(), "echoed:ping");
    wait_for_rows(&ctx.db, 1).await;
    let endpoint: String = ctx
        .db
        .call(|c| Ok(c.query_row("SELECT endpoint FROM requests", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert_eq!(endpoint, "/v1/echo");
}

#[tokio::test]
async fn unreachable_backend_502_openai_shape() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    std::mem::forget(dir);
    // port from a listener we immediately drop -> nothing listens there
    let dead_addr = {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap()
    };
    let b = registry::add_backend(&db, "dead", &format!("http://{dead_addr}"), None, true)
        .await
        .unwrap();
    registry::add_model(&db, b, "dead-m", "dead-m")
        .await
        .unwrap();
    let key = keys::create(&db, "k", true, &[], None).await.unwrap();
    let state = proxy::AppState::for_test(
        db.clone(),
        logstore::spawn_writer(db.clone()),
        reqwest::Client::new(),
    );
    let inginx = spawn_app(proxy::router(state)).await;

    let resp = reqwest::Client::new()
        .post(format!("http://{inginx}/v1/chat/completions"))
        .bearer_auth(&key)
        .json(&serde_json::json!({"model":"dead-m","messages":[]}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "upstream_unreachable");
    wait_for_rows(&db, 1).await;
    let err: Option<String> = db
        .call(|c| Ok(c.query_row("SELECT error FROM requests", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert!(err.unwrap().contains("unreachable"));
}

#[tokio::test]
async fn merged_listener_serves_api_and_admin() {
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    std::mem::forget(dir);
    let state = proxy::AppState::for_test(
        db.clone(),
        logstore::spawn_writer(db.clone()),
        reqwest::Client::new(),
    );
    let app = proxy::router(state.clone()).merge(inginx::admin::router(state));
    let addr = spawn_app(app).await;
    let client = reqwest::Client::new();
    // admin login page renders
    let r = client
        .get(format!("http://{addr}/admin/login"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("Sign in"));
    // API answers on the same port (401 without key, OpenAI shape)
    let r = client
        .get(format!("http://{addr}/v1/models"))
        .send()
        .await
        .unwrap();
    assert_eq!(r.status(), 401);
    let v: serde_json::Value = r.json().await.unwrap();
    assert_eq!(v["error"]["code"], "invalid_api_key");
}

#[tokio::test]
async fn models_filtered_per_key() {
    let ctx = setup().await;
    // second model the limited key cannot use
    let backends = registry::list_backends(&ctx.db).await.unwrap();
    registry::add_model(&ctx.db, backends[0].id, "other-upstream", "other")
        .await
        .unwrap();
    let models = registry::list_models(&ctx.db).await.unwrap();
    let m_id = models.iter().find(|m| m.alias == "m").unwrap().id;
    let limited = keys::create(&ctx.db, "limited", false, &[m_id], None)
        .await
        .unwrap();

    let v: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{}/v1/models", ctx.inginx))
        .bearer_auth(&limited)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let ids: Vec<&str> = v["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["m"]);

    let v: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{}/v1/models", ctx.inginx))
        .bearer_auth(&ctx.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["data"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn hung_backend_times_out_and_is_logged() {
    // The failure this was written for: the backend takes the request and
    // never answers. Before the timeout the request hung until the client
    // gave up and — because the log row is written after the response — left
    // no trace at all.
    let ctx = setup_with(|st| {
        st.upstream_timeout = Some(std::time::Duration::from_millis(300));
        st.upstream_retry_on_5xx = false;
    })
    .await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/hang", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({"model":"m"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 504);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "upstream_timeout");

    wait_for_rows(&ctx.db, 1).await;
    let (status, error): (i64, String) = ctx
        .db
        .call(|c| {
            Ok(c.query_row("SELECT status, error FROM requests", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })?)
        })
        .await
        .unwrap();
    assert_eq!(status, 504);
    assert!(error.contains("did not respond"), "{error}");
}

#[tokio::test]
async fn swap_in_progress_502_is_retried_once() {
    let ctx = setup().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/flaky", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({"model":"m"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["choices"][0]["message"]["content"], "second try");
}

#[tokio::test]
async fn plain_text_upstream_error_is_reshaped_for_the_client() {
    let ctx = setup_with(|st| st.upstream_retry_on_5xx = false).await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/broken", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({"model":"m"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 502);
    let v: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(v["error"]["code"], "upstream_error");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("wound up in state stopped"));

    // The log keeps the upstream's own body, not the reshaped one.
    wait_for_rows(&ctx.db, 1).await;
    let raw: Vec<u8> = ctx
        .db
        .call(|c| Ok(c.query_row("SELECT resp_body FROM requests", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert!(!raw.is_empty());
}

#[tokio::test]
async fn voice_map_rewrites_the_forwarded_request() {
    let ctx = setup().await;
    registry::set_model_voice_map(&ctx.db, 1, Some(r#"{"alloy":"af_alloy"}"#.to_string()))
        .await
        .unwrap();
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/audio/speech-echo", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({"model":"m","input":"hi","voice":"alloy"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let echoed: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(echoed["voice"], "af_alloy");
    // The model name is still rewritten to the upstream one.
    assert_eq!(echoed["model"], "m-upstream");
}

#[tokio::test]
async fn audio_upload_is_not_token_counted() {
    let ctx = setup().await;
    let form = reqwest::multipart::Form::new().text("model", "m").part(
        "file",
        reqwest::multipart::Part::bytes(vec![0u8, 159, 146, 150, 7, 7, 7])
            .file_name("a.wav")
            .mime_str("audio/wav")
            .unwrap(),
    );
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/audio/transcriptions", ctx.inginx))
        .bearer_auth(&ctx.key)
        .multipart(form)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    wait_for_rows(&ctx.db, 1).await;
    let (pt, src): (i64, String) = ctx
        .db
        .call(|c| {
            Ok(c.query_row(
                "SELECT prompt_tokens, usage_source FROM requests",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(pt, 0, "a blob of audio has no prompt tokens to estimate");
    assert_eq!(src, "none");
}

#[tokio::test]
async fn rejected_requests_are_logged_and_then_throttled() {
    let ctx = setup_with(|st| {
        st.api_throttle = std::sync::Arc::new(inginx::ratelimit::LoginThrottle::new(
            2,
            std::time::Duration::from_secs(60),
        ));
    })
    .await;
    let client = reqwest::Client::new();
    for _ in 0..2 {
        let resp = client
            .post(format!("http://{}/v1/chat/completions", ctx.inginx))
            .bearer_auth("igx_wrong")
            .json(&serde_json::json!({"model":"m"}))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), 401);
    }
    // The wall goes up on the third try.
    let resp = client
        .post(format!("http://{}/v1/chat/completions", ctx.inginx))
        .bearer_auth("igx_wrong")
        .json(&serde_json::json!({"model":"m"}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 429);

    wait_for_rows(&ctx.db, 2).await;
    let (id, status, key_id): (String, i64, Option<i64>) = ctx
        .db
        .call(|c| {
            Ok(
                c.query_row("SELECT id, status, key_id FROM requests LIMIT 1", [], |r| {
                    Ok((r.get(0)?, r.get(1)?, r.get(2)?))
                })?,
            )
        })
        .await
        .unwrap();
    assert_eq!(status, 401);
    assert!(key_id.is_none());
    // The layer runs before the body is read, and that stays true: an
    // unauthenticated caller still cannot make us buffer a body.
    let (req_body, _) = logstore::get_bodies(&ctx.db, &id).await.unwrap().unwrap();
    assert!(req_body.is_empty());
}

#[tokio::test]
async fn models_list_carries_created() {
    let ctx = setup().await;
    let v: serde_json::Value = reqwest::Client::new()
        .get(format!("http://{}/v1/models", ctx.inginx))
        .bearer_auth(&ctx.key)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["data"][0]["id"], "m");
    assert!(
        v["data"][0]["created"].as_i64().unwrap() > 1_600_000_000,
        "created must be a real timestamp: {}",
        v["data"][0]
    );
}

#[tokio::test]
async fn client_headers_reach_the_backend() {
    let ctx = setup().await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/echo-headers", ctx.inginx))
        .bearer_auth(&ctx.key)
        .header("x-title", "my-app")
        .header("user-agent", "probe/1.0")
        .json(&serde_json::json!({"model":"m"}))
        .send()
        .await
        .unwrap();
    let seen: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(seen["x-title"], "my-app");
    assert_eq!(seen["user-agent"], "probe/1.0");
    // The client's inginx key is never handed to the backend.
    assert!(seen.get("authorization").is_none(), "{seen}");
}

#[tokio::test]
async fn stalled_stream_is_abandoned_and_logged() {
    let ctx = setup_with(|st| {
        st.upstream_stream_idle = Some(std::time::Duration::from_millis(300));
    })
    .await;
    let resp = reqwest::Client::new()
        .post(format!("http://{}/v1/stall", ctx.inginx))
        .bearer_auth(&ctx.key)
        .json(&serde_json::json!({"model":"m","stream":true}))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), 200); // headers already went out
                                    // The body ends early rather than hanging for the upstream's 60s.
    let body = tokio::time::timeout(std::time::Duration::from_secs(5), resp.text()).await;
    assert!(body.is_ok(), "stalled stream must not hang the client");

    wait_for_rows(&ctx.db, 1).await;
    let error: Option<String> = ctx
        .db
        .call(|c| Ok(c.query_row("SELECT error FROM requests", [], |r| r.get(0))?))
        .await
        .unwrap();
    assert!(
        error.unwrap_or_default().contains("stalled"),
        "the abandoned stream must say why"
    );
}

#[tokio::test]
async fn usage_is_attributed_to_the_key_and_model() {
    let ctx = setup().await;
    for _ in 0..2 {
        reqwest::Client::new()
            .post(format!("http://{}/v1/chat/completions", ctx.inginx))
            .bearer_auth(&ctx.key)
            .json(&serde_json::json!({"model":"m","messages":[{"role":"user","content":"hi"}]}))
            .send()
            .await
            .unwrap();
    }
    wait_for_rows(&ctx.db, 2).await;
    let (key_id, model, reqs, prompt, completion): (i64, String, i64, i64, i64) = ctx
        .db
        .call(|c| {
            Ok(c.query_row(
                "SELECT key_id, model, SUM(n_requests), SUM(prompt_tokens),
                        SUM(completion_tokens)
                 FROM usage_hourly GROUP BY key_id, model",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )?)
        })
        .await
        .unwrap();
    assert!(key_id > 0);
    assert_eq!(model, "m");
    assert_eq!((reqs, prompt, completion), (2, 6, 4));
}

#[tokio::test]
async fn multipart_uploads_reach_the_backend_under_its_own_model_name() {
    // The alias is rewritten for JSON bodies; a transcription carries its
    // model in a form field, and that used to go upstream untouched — the
    // backend was asked for inginx's public alias and refused it.
    let ctx = setup().await;
    let form = reqwest::multipart::Form::new().text("model", "m").part(
        "file",
        reqwest::multipart::Part::bytes(vec![0u8, 159, 146, 150])
            .file_name("a.wav")
            .mime_str("audio/wav")
            .unwrap(),
    );
    let v: serde_json::Value = reqwest::Client::new()
        .post(format!("http://{}/v1/audio/transcriptions", ctx.inginx))
        .bearer_auth(&ctx.key)
        .multipart(form)
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(v["model_seen"], "m-upstream");
}
