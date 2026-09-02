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

async fn spawn_mock_upstream() -> SocketAddr {
    use axum::{routing::get, routing::post, Json, Router};
    let app = Router::new()
        .route("/chat/completions", post(mock_chat))
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
    let dir = tempfile::tempdir().unwrap();
    let db = Db::open(&dir.path().join("t.db")).unwrap();
    std::mem::forget(dir);
    let upstream = spawn_mock_upstream().await;
    let b = registry::add_backend(&db, "mock", &format!("http://{upstream}"), None, true)
        .await
        .unwrap();
    registry::add_model(&db, b, "m-upstream", "m").await.unwrap();
    let key = keys::create(&db, "test", true, &[], None).await.unwrap();
    let state = proxy::AppState {
        db: db.clone(),
        log: logstore::spawn_writer(db.clone()),
        http: reqwest::Client::new(),
    };
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
                Ok(c.query_row("SELECT error FROM requests LIMIT 1", [], |r| r.get(0))
                    .ok())
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
    assert_eq!(
        resp.headers().get("content-type").unwrap(),
        "audio/wav"
    );
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
    registry::add_model(&db, b, "dead-m", "dead-m").await.unwrap();
    let key = keys::create(&db, "k", true, &[], None).await.unwrap();
    let state = proxy::AppState {
        db: db.clone(),
        log: logstore::spawn_writer(db.clone()),
        http: reqwest::Client::new(),
    };
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
    let state = proxy::AppState {
        db: db.clone(),
        log: logstore::spawn_writer(db.clone()),
        http: reqwest::Client::new(),
    };
    let app = proxy::router(state.clone()).merge(inginx::admin::router(state));
    let addr = spawn_app(app).await;
    let client = reqwest::Client::new();
    // admin login page renders
    let r = client.get(format!("http://{addr}/admin/login")).send().await.unwrap();
    assert_eq!(r.status(), 200);
    assert!(r.text().await.unwrap().contains("Sign in"));
    // API answers on the same port (401 without key, OpenAI shape)
    let r = client.get(format!("http://{addr}/v1/models")).send().await.unwrap();
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
