use crate::db::{now, Db};
use crate::logstore::{get_setting, set_setting};
use crate::proxy::AppState;
use anyhow::Result;
use argon2::password_hash::{rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString};
use argon2::Argon2;
use axum::{
    extract::{Query, State},
    http::{header, HeaderMap, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Router,
};
use maud::{html, Markup, DOCTYPE};
use rand::RngCore;
use serde::Deserialize;

const SESSION_COOKIE: &str = "inginx_session";
const SESSION_TTL: i64 = 7 * 24 * 3600;

fn random_token() -> String {
    let mut b = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut b);
    hex::encode(b)
}

/// On startup: if no admin account exists, ensure a one-time setup token and
/// return it so main can print the setup URL.
pub async fn ensure_setup_token(db: &Db) -> Result<Option<String>> {
    let has_admin: bool = db
        .call(|c| {
            let n: i64 = c.query_row("SELECT count(*) FROM admin_credentials", [], |r| r.get(0))?;
            Ok(n > 0)
        })
        .await?;
    if has_admin {
        return Ok(None);
    }
    if let Some(t) = get_setting(db, "setup_token").await? {
        return Ok(Some(t));
    }
    let t = random_token();
    set_setting(db, "setup_token", &t).await?;
    Ok(Some(t))
}

pub fn hash_password(password: &str) -> Result<String> {
    let salt = SaltString::generate(&mut OsRng);
    Ok(Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map_err(|e| anyhow::anyhow!("{e}"))?
        .to_string())
}

pub fn verify_password(password: &str, hash: &str) -> bool {
    PasswordHash::new(hash)
        .map(|h| Argon2::default().verify_password(password.as_bytes(), &h).is_ok())
        .unwrap_or(false)
}

async fn create_session(db: &Db, full: bool) -> Result<String> {
    let token = random_token();
    let t = token.clone();
    db.call(move |c| {
        c.execute(
            "INSERT INTO sessions (token, created_at, expires_at, full) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![t, now(), now() + SESSION_TTL, full as i64],
        )?;
        Ok(())
    })
    .await?;
    Ok(token)
}

fn cookie_value(headers: &HeaderMap) -> Option<String> {
    let cookies = headers.get(header::COOKIE)?.to_str().ok()?;
    cookies.split(';').find_map(|c| {
        let (k, v) = c.trim().split_once('=')?;
        (k == SESSION_COOKIE).then(|| v.to_string())
    })
}

fn set_session_cookie(token: &str) -> String {
    format!("{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/admin; Max-Age={SESSION_TTL}")
}

/// Looks up the session for this request. Returns (token, full).
pub async fn session_state(db: &Db, headers: &HeaderMap) -> Option<(String, bool)> {
    let token = cookie_value(headers)?;
    let t = token.clone();
    let full: Option<bool> = db
        .call(move |c| {
            let v = c
                .query_row(
                    "SELECT full FROM sessions WHERE token = ?1 AND expires_at > ?2",
                    rusqlite::params![t, now()],
                    |r| Ok(r.get::<_, i64>(0)? != 0),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })?;
            Ok(v)
        })
        .await
        .ok()
        .flatten();
    full.map(|f| (token, f))
}

/// Middleware: requires a full session; redirects to the login page otherwise.
pub async fn require_session(
    State(st): State<AppState>,
    headers: HeaderMap,
    req: axum::extract::Request,
    next: Next,
) -> Response {
    match session_state(&st.db, &headers).await {
        Some((_, true)) => next.run(req).await,
        _ => Redirect::to("/admin/login").into_response(),
    }
}

fn page(title: &str, body: Markup) -> Html<String> {
    Html(
        html! {
            (DOCTYPE)
            html {
                head {
                    meta charset="utf-8";
                    meta name="viewport" content="width=device-width, initial-scale=1";
                    title { (title) " · inginx" }
                    style { (maud::PreEscaped(AUTH_CSS)) }
                }
                body { div class="card" { h1 { (title) } (body) } }
            }
        }
        .into_string(),
    )
}

const AUTH_CSS: &str = r#"
:root { color-scheme: dark; }
body { margin:0; min-height:100vh; display:flex; align-items:center; justify-content:center;
  background:#0d1117; color:#e6edf3; font:15px/1.5 system-ui, sans-serif; }
.card { background:#161b22; border:1px solid #30363d; border-radius:12px; padding:2rem 2.5rem; min-width:320px; }
h1 { font-size:1.2rem; margin:0 0 1rem; }
label { display:block; margin:.8rem 0 .25rem; color:#8b949e; font-size:.85rem; }
input { width:100%; box-sizing:border-box; padding:.5rem .7rem; border-radius:8px;
  border:1px solid #30363d; background:#0d1117; color:#e6edf3; font-size:1rem; }
button { margin-top:1.2rem; width:100%; padding:.6rem; border-radius:8px; border:0;
  background:#2f81f7; color:#fff; font-size:1rem; cursor:pointer; }
button:hover { background:#1f6feb; }
.err { color:#f85149; margin-top:.8rem; font-size:.9rem; }
"#;

#[derive(Deserialize)]
struct SetupQuery {
    t: Option<String>,
}

async fn setup_form(State(st): State<AppState>, Query(q): Query<SetupQuery>) -> Response {
    match valid_setup_token(&st.db, q.t.as_deref()).await {
        Ok(true) => page(
            "Create admin account",
            html! {
                form method="post" action="/admin/setup" {
                    input type="hidden" name="t" value=(q.t.unwrap_or_default());
                    label { "Username" }
                    input type="text" name="username" required autofocus;
                    label { "Password" }
                    input type="password" name="password" required minlength="12";
                    button { "Create account" }
                }
            },
        )
        .into_response(),
        _ => (StatusCode::FORBIDDEN, "invalid or used setup token").into_response(),
    }
}

async fn valid_setup_token(db: &Db, t: Option<&str>) -> Result<bool> {
    let Some(t) = t else { return Ok(false) };
    let stored = get_setting(db, "setup_token").await?;
    Ok(stored.as_deref() == Some(t) && !t.is_empty())
}

#[derive(Deserialize)]
struct SetupForm {
    t: String,
    username: String,
    password: String,
}

async fn setup_submit(State(st): State<AppState>, Form(f): Form<SetupForm>) -> Response {
    match valid_setup_token(&st.db, Some(&f.t)).await {
        Ok(true) => {}
        _ => return (StatusCode::FORBIDDEN, "invalid or used setup token").into_response(),
    }
    if f.password.len() < 12 {
        return (StatusCode::BAD_REQUEST, "password must be at least 12 characters").into_response();
    }
    let hash = match hash_password(&f.password) {
        Ok(h) => h,
        Err(e) => {
            tracing::error!("hashing failed: {e}");
            return (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response();
        }
    };
    let username = f.username.clone();
    let res = st
        .db
        .call(move |c| {
            let tx = c.transaction()?;
            tx.execute(
                "INSERT INTO admin_credentials (id, username, password_hash) VALUES (1, ?1, ?2)",
                rusqlite::params![username, hash],
            )?;
            tx.execute("DELETE FROM settings WHERE key = 'setup_token'", [])?;
            tx.commit()?;
            Ok(())
        })
        .await;
    match res {
        Ok(()) => Redirect::to("/admin/login").into_response(),
        Err(e) => {
            tracing::error!("setup failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn login_form() -> Html<String> {
    page(
        "Sign in",
        html! {
            form method="post" action="/admin/login" {
                label { "Username" }
                input type="text" name="username" required autofocus;
                label { "Password" }
                input type="password" name="password" required;
                button { "Sign in" }
            }
        },
    )
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

async fn login_submit(State(st): State<AppState>, Form(f): Form<LoginForm>) -> Response {
    let creds: Option<(String, String)> = st
        .db
        .call(|c| {
            let v = c
                .query_row(
                    "SELECT username, password_hash FROM admin_credentials WHERE id = 1",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map(Some)
                .or_else(|e| match e {
                    rusqlite::Error::QueryReturnedNoRows => Ok(None),
                    e => Err(e),
                })?;
            Ok(v)
        })
        .await
        .ok()
        .flatten();
    let ok = creds
        .map(|(user, hash)| user == f.username && verify_password(&f.password, &hash))
        .unwrap_or(false);
    if !ok {
        return page(
            "Sign in",
            html! {
                form method="post" action="/admin/login" {
                    label { "Username" }
                    input type="text" name="username" required autofocus;
                    label { "Password" }
                    input type="password" name="password" required;
                    button { "Sign in" }
                    p class="err" { "Invalid username or password." }
                }
            },
        )
        .into_response();
    }
    let has_passkeys: bool = st
        .db
        .call(|c| {
            let n: i64 = c.query_row("SELECT count(*) FROM webauthn_credentials", [], |r| r.get(0))?;
            Ok(n > 0)
        })
        .await
        .unwrap_or(false);
    let (full, target) = if has_passkeys {
        (false, "/admin/login/2fa")
    } else {
        (true, "/admin")
    };
    match create_session(&st.db, full).await {
        Ok(token) => (
            [(header::SET_COOKIE, set_session_cookie(&token))],
            Redirect::to(target),
        )
            .into_response(),
        Err(e) => {
            tracing::error!("session creation failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
        }
    }
}

async fn logout(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if let Some(token) = cookie_value(&headers) {
        let _ = st
            .db
            .call(move |c| {
                c.execute("DELETE FROM sessions WHERE token = ?1", [token])?;
                Ok(())
            })
            .await;
    }
    (
        [(
            header::SET_COOKIE,
            format!("{SESSION_COOKIE}=; Path=/admin; Max-Age=0"),
        )],
        Redirect::to("/admin/login"),
    )
        .into_response()
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/admin/setup", get(setup_form).post(setup_submit))
        .route("/admin/login", get(login_form).post(login_submit))
        .route("/admin/logout", post(logout))
        .with_state(state)
}

/// Wraps a router of protected admin pages with the session middleware.
pub fn protect(state: AppState, inner: Router) -> Router {
    inner.layer(middleware::from_fn_with_state(state, require_session))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keys::testdb;
    use crate::logstore::spawn_writer;
    use tower::ServiceExt;

    async fn state() -> AppState {
        let db = testdb().await;
        AppState {
            log: spawn_writer(db.clone()),
            db,
            http: reqwest::Client::new(),
        }
    }

    fn full_router(st: AppState) -> Router {
        let protected = protect(
            st.clone(),
            Router::new()
                .route("/admin/keys", get(|| async { "keys page" }))
                .with_state(st.clone()),
        );
        router(st).merge(protected)
    }

    async fn req(
        app: &Router,
        method: &str,
        path: &str,
        body: Option<&str>,
        cookie: Option<&str>,
    ) -> axum::response::Response {
        let mut b = axum::http::Request::builder().method(method).uri(path);
        if body.is_some() {
            b = b.header("content-type", "application/x-www-form-urlencoded");
        }
        if let Some(c) = cookie {
            b = b.header("cookie", format!("{SESSION_COOKIE}={c}"));
        }
        let request = b
            .body(axum::body::Body::from(body.unwrap_or("").to_string()))
            .unwrap();
        app.clone().oneshot(request).await.unwrap()
    }

    #[tokio::test]
    async fn setup_flow_and_login() {
        let st = state().await;
        let app = full_router(st.clone());
        let token = ensure_setup_token(&st.db).await.unwrap().unwrap();

        // wrong token -> 403
        let r = req(&app, "GET", "/admin/setup?t=wrong", None, None).await;
        assert_eq!(r.status(), StatusCode::FORBIDDEN);

        // correct token -> form, then create account
        let r = req(&app, "GET", &format!("/admin/setup?t={token}"), None, None).await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = req(
            &app,
            "POST",
            "/admin/setup",
            Some(&format!("t={token}&username=admin&password=longenoughpw1")),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        // token consumed
        assert!(ensure_setup_token(&st.db).await.unwrap().is_none());

        // bad password -> 200 with error page, no cookie
        let r = req(&app, "POST", "/admin/login", Some("username=admin&password=nope"), None).await;
        assert!(r.headers().get(header::SET_COOKIE).is_none());

        // good login -> cookie + redirect to /admin (no passkeys yet)
        let r = req(
            &app,
            "POST",
            "/admin/login",
            Some("username=admin&password=longenoughpw1"),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers().get(header::LOCATION).unwrap(), "/admin");
        let cookie = r
            .headers()
            .get(header::SET_COOKIE)
            .unwrap()
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .split_once('=')
            .unwrap()
            .1
            .to_string();

        // protected page works with cookie, redirects without
        let r = req(&app, "GET", "/admin/keys", None, Some(&cookie)).await;
        assert_eq!(r.status(), StatusCode::OK);
        let r = req(&app, "GET", "/admin/keys", None, None).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert_eq!(r.headers().get(header::LOCATION).unwrap(), "/admin/login");
    }
}
