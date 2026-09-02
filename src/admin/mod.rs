pub mod actions;
pub mod auth;
pub mod pages;

use crate::proxy::AppState;
use axum::routing::{get, post};
use axum::Router;

pub fn router(state: AppState) -> Router {
    let protected = Router::new()
        .route("/admin/keys", get(pages::keys_page).post(actions::create_key))
        .route("/admin/keys/{id}/revoke", post(actions::revoke_key))
        .route("/admin/keys/{id}/rotate", post(actions::rotate_key))
        .route(
            "/admin/backends",
            get(pages::backends_page).post(actions::add_backend),
        )
        .route("/admin/backends/{id}/import", post(actions::import_backend))
        .route("/admin/backends/{id}/default", post(actions::make_default_backend))
        .route("/admin/models/{id}/toggle", post(actions::toggle_model))
        .route("/admin/models/{id}/alias", post(actions::rename_model))
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

        let (status, body) = post_form(&app, "/admin/keys", "name=test&all_models=on", &cookie).await;
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
                Ok(c.query_row("SELECT revoked_at FROM api_keys WHERE id = ?1", [id], |r| {
                    r.get(0)
                })?)
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
        assert!(body.contains("qwen3-8b") && body.contains("bge-m3"), "{body}");

        let models = crate::registry::list_models(&st.db).await.unwrap();
        assert_eq!(models.len(), 2);
        let m = &models[0];
        let (status, _) = post_form(&app, &format!("/admin/models/{}/toggle", m.id), "", &cookie).await;
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
