pub mod actions;
pub mod auth;
pub mod dashboard;
pub mod logs;
pub mod pages;

use crate::proxy::AppState;
use axum::routing::{get, post};
use axum::Router;

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route(
            "/admin/keys",
            get(pages::keys_page).post(actions::create_key),
        )
        .route("/admin/keys/{id}/revoke", post(actions::revoke_key))
        .route("/admin/keys/{id}/rotate", post(actions::rotate_key))
        .route(
            "/admin/backends",
            get(pages::backends_page).post(actions::add_backend),
        )
        .route("/admin/backends/{id}/import", post(actions::import_backend))
        .route(
            "/admin/backends/{id}/default",
            post(actions::make_default_backend),
        )
        .route("/admin/models/{id}/toggle", post(actions::toggle_model))
        .route("/admin/models/{id}/alias", post(actions::rename_model))
        .route("/admin/logs", get(logs::logs_page))
        .route("/admin/logs/{id}", get(logs::log_detail))
        .route("/admin/logs/{id}/download", get(logs::log_download))
        .route("/admin", get(dashboard::dashboard))
        .route("/admin/api/stats", get(dashboard::stats_api))
        .route("/admin/settings", get(dashboard::settings_page))
        .route("/admin/settings/retention", post(dashboard::save_retention))
        .route("/admin/settings/password", post(dashboard::change_password))
        .route("/admin/settings/rp", post(dashboard::save_rp))
        .route(
            "/admin/settings/passkeys/{id}/delete",
            post(dashboard::delete_passkey),
        )
        .route("/admin/export", get(dashboard::export))
        .with_state(state.clone());
    Router::new()
        .route("/admin/assets/{file}", get(pages::assets))
        .with_state(state.clone())
        .merge(auth::router(state.clone()))
        .merge(auth::protect(state, protected))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::testdb;
    use crate::logstore::spawn_writer;
    use axum::http::{header, StatusCode};
    use tower::ServiceExt;

    async fn logged_in() -> (AppState, Router, String) {
        let db = testdb().await;
        let st = AppState {
            log: spawn_writer(db.clone()),
            db,
            http: reqwest::Client::new(),
        };
        let app = router(st.clone());
        let token = auth::ensure_setup_token(&st.db).await.unwrap().unwrap();
        let setup = axum::http::Request::builder()
            .method("POST")
            .uri("/admin/setup")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(format!(
                "t={token}&username=admin&password=longenoughpw1"
            )))
            .unwrap();
        app.clone().oneshot(setup).await.unwrap();
        let login = axum::http::Request::builder()
            .method("POST")
            .uri("/admin/login")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(
                "username=admin&password=longenoughpw1".to_string(),
            ))
            .unwrap();
        let r = app.clone().oneshot(login).await.unwrap();
        let cookie = r
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_string();
        (st, app, cookie)
    }

    pub(crate) async fn get_page(app: &Router, path: &str, cookie: &str) -> (StatusCode, String) {
        let req = axum::http::Request::builder()
            .method("GET")
            .uri(path)
            .header("cookie", cookie)
            .body(axum::body::Body::empty())
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let status = r.status();
        let body = axum::body::to_bytes(r.into_body(), 1 << 22).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    pub(crate) async fn post_form(
        app: &Router,
        path: &str,
        form: &str,
        cookie: &str,
    ) -> (StatusCode, String) {
        let req = axum::http::Request::builder()
            .method("POST")
            .uri(path)
            .header("cookie", cookie)
            .header("content-type", "application/x-www-form-urlencoded")
            .body(axum::body::Body::from(form.to_string()))
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        let status = r.status();
        let body = axum::body::to_bytes(r.into_body(), 1 << 22).await.unwrap();
        (status, String::from_utf8_lossy(&body).into_owned())
    }

    #[tokio::test]
    async fn keys_page_and_lifecycle() {
        let (st, app, cookie) = logged_in().await;

        let (status, body) = get_page(&app, "/admin/keys", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("Create key"), "{body}");

        let (status, body) =
            post_form(&app, "/admin/keys", "name=test&all_models=on", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("igx_"), "plaintext key shown once: {body}");

        let id: i64 = st
            .db
            .call(|c| Ok(c.query_row("SELECT id FROM api_keys", [], |r| r.get(0))?))
            .await
            .unwrap();
        let (status, body) =
            post_form(&app, &format!("/admin/keys/{id}/revoke"), "", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("revoked"), "{body}");
        let revoked: Option<i64> = st
            .db
            .call(move |c| {
                Ok(
                    c.query_row("SELECT revoked_at FROM api_keys WHERE id = ?1", [id], |r| {
                        r.get(0)
                    })?,
                )
            })
            .await
            .unwrap();
        assert!(revoked.is_some());
    }

    #[tokio::test]
    async fn backends_page_and_import() {
        let (st, app, cookie) = logged_in().await;

        // mock upstream for import
        let mock = axum::Router::new().route(
            "/models",
            axum::routing::get(|| async {
                axum::Json(serde_json::json!({"data":[{"id":"qwen3-8b"},{"id":"bge-m3"}]}))
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move { axum::serve(listener, mock).await.unwrap() });

        let (status, _) = post_form(
            &app,
            "/admin/backends",
            &format!("name=gpu1&base_url=http://{addr}&is_default=on"),
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let backends = crate::registry::list_backends(&st.db).await.unwrap();
        assert_eq!(backends.len(), 1);

        let (status, body) = post_form(
            &app,
            &format!("/admin/backends/{}/import", backends[0].id),
            "",
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(
            body.contains("qwen3-8b") && body.contains("bge-m3"),
            "{body}"
        );

        let models = crate::registry::list_models(&st.db).await.unwrap();
        assert_eq!(models.len(), 2);
        let m = &models[0];
        let (status, _) =
            post_form(&app, &format!("/admin/models/{}/toggle", m.id), "", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        let models = crate::registry::list_models(&st.db).await.unwrap();
        assert!(!models.iter().find(|x| x.id == m.id).unwrap().enabled);

        let (status, _) = post_form(
            &app,
            &format!("/admin/models/{}/alias", m.id),
            "alias=renamed",
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let models = crate::registry::list_models(&st.db).await.unwrap();
        assert!(models.iter().any(|x| x.alias == "renamed"));
    }

    #[tokio::test]
    async fn logs_page_filter_and_detail() {
        let (st, app, cookie) = logged_in().await;
        // seed two rows directly
        for (model, status) in [("m1", 200u16), ("m2", 500u16)] {
            let rec = crate::logstore::LogRecord {
                id: ulid::Ulid::new().to_string(),
                ts: crate::db::now(),
                key_id: None,
                model: Some(model.into()),
                endpoint: "/v1/chat/completions".into(),
                status,
                latency_ms: 5,
                ttfb_ms: None,
                usage: crate::metering::Usage {
                    prompt_tokens: 1,
                    completion_tokens: 2,
                },
                usage_source: "upstream",
                error: None,
                req_headers: "{}".into(),
                resp_headers: "{}".into(),
                req_body: br#"{"messages":[{"content":"seekable-needle"}]}"#.to_vec(),
                resp_body: br#"{"choices":[]}"#.to_vec(),
            };
            st.log.send(rec);
        }
        for _ in 0..100 {
            let n: i64 = st
                .db
                .call(|c| Ok(c.query_row("SELECT count(*) FROM requests", [], |r| r.get(0))?))
                .await
                .unwrap();
            if n == 2 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        let (status, body) = get_page(&app, "/admin/logs", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("m1") && body.contains("m2"), "{body}");

        let (_, body) = get_page(&app, "/admin/logs?status=5xx", &cookie).await;
        assert!(body.contains("m2") && !body.contains(">m1<"), "{body}");

        let id: String = st
            .db
            .call(|c| Ok(c.query_row("SELECT id FROM requests LIMIT 1", [], |r| r.get(0))?))
            .await
            .unwrap();
        let (status, body) = get_page(&app, &format!("/admin/logs/{id}"), &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("seekable-needle"), "{body}");
    }

    #[tokio::test]
    async fn stats_retention_and_export() {
        let (st, app, cookie) = logged_in().await;
        // seed a usage_hourly row and a request row
        let hour = crate::db::now() - crate::db::now() % 3600;
        st.db
            .call(move |c| {
                c.execute(
                    "INSERT INTO usage_hourly (key_id, model, hour, n_requests, prompt_tokens, completion_tokens, n_errors)
                     VALUES (-1, 'm', ?1, 4, 10, 20, 1)",
                    [hour],
                )?;
                Ok(())
            })
            .await
            .unwrap();
        st.log.send(crate::logstore::LogRecord {
            id: ulid::Ulid::new().to_string(),
            ts: crate::db::now(),
            key_id: None,
            model: Some("m".into()),
            endpoint: "/v1/chat/completions".into(),
            status: 200,
            latency_ms: 5,
            ttfb_ms: None,
            usage: crate::metering::Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
            },
            usage_source: "upstream",
            error: None,
            req_headers: "{}".into(),
            resp_headers: "{}".into(),
            req_body: b"{}".to_vec(),
            resp_body: b"{}".to_vec(),
        });
        for _ in 0..100 {
            let n: i64 = st
                .db
                .call(|c| Ok(c.query_row("SELECT count(*) FROM requests", [], |r| r.get(0))?))
                .await
                .unwrap();
            if n == 1 {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // stats endpoint: aligned arrays containing the seeded totals
        let (status, body) = get_page(&app, "/admin/api/stats?days=1", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        let v: serde_json::Value = serde_json::from_str(&body).unwrap();
        let hours = v["hours"].as_array().unwrap();
        let requests = v["requests"].as_array().unwrap();
        assert_eq!(hours.len(), requests.len());
        assert!(requests.iter().any(|r| r.as_i64().unwrap() >= 4));

        // retention persists to settings
        let (status, _) = post_form(
            &app,
            "/admin/settings/retention",
            "max_age_days=30&max_db_gb=2.0",
            &cookie,
        )
        .await;
        assert_eq!(status, StatusCode::SEE_OTHER);
        assert_eq!(
            crate::logstore::get_setting(&st.db, "retention_max_age_secs")
                .await
                .unwrap()
                .unwrap(),
            (30i64 * 86_400).to_string()
        );
        assert_eq!(
            crate::logstore::get_setting(&st.db, "retention_max_db_bytes")
                .await
                .unwrap()
                .unwrap(),
            "2000000000"
        );

        // export returns NDJSON attachment with one line
        let req = axum::http::Request::builder()
            .uri("/admin/export?format=native")
            .header("cookie", &cookie)
            .body(axum::body::Body::empty())
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert!(r
            .headers()
            .get(header::CONTENT_DISPOSITION)
            .unwrap()
            .to_str()
            .unwrap()
            .contains("attachment"));
        let body = axum::body::to_bytes(r.into_body(), 1 << 22).await.unwrap();
        let text = String::from_utf8_lossy(&body);
        assert_eq!(text.trim().lines().count(), 1, "{text}");

        // dashboard renders
        let (status, body) = get_page(&app, "/admin", &cookie).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body.contains("requests (24h)"), "{body}");
    }

    #[tokio::test]
    async fn assets_served() {
        let (_st, app, _cookie) = logged_in().await;
        let req = axum::http::Request::builder()
            .uri("/admin/assets/app.css")
            .body(axum::body::Body::empty())
            .unwrap();
        let r = app.clone().oneshot(req).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);
        assert_eq!(r.headers().get(header::CONTENT_TYPE).unwrap(), "text/css");
    }
}
