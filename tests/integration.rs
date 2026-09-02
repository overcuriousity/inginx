use inginx::{db::Db, keys, logstore, proxy, registry};
use std::net::SocketAddr;

async fn spawn_app(app: axum::Router) -> SocketAddr {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    addr
}

async fn spawn_mock_upstream() -> SocketAddr {
    use axum::{routing::get, routing::post, Json, Router};
    let app = Router::new()
        .route(
            "/chat/completions",
            post(|| async {
                Json(serde_json::json!({
                    "choices":[{"message":{"role":"assistant","content":"hi"}}],
                    "usage":{"prompt_tokens":3,"completion_tokens":2}
                }))
            }),
        )
        .route(
            "/models",
            get(|| async { Json(serde_json::json!({"data":[{"id":"m-upstream"}]})) }),
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
