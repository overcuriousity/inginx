use crate::db::{now, Db};
use crate::logstore::{get_setting, set_setting};
use crate::proxy::AppState;
use crate::ratelimit::{client_ip, PeerAddr};
use anyhow::Result;
use argon2::password_hash::{
    rand_core::OsRng, PasswordHash, PasswordHasher, PasswordVerifier, SaltString,
};
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
    rand::rng().fill_bytes(&mut b);
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
        .map(|h| {
            Argon2::default()
                .verify_password(password.as_bytes(), &h)
                .is_ok()
        })
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

/// `Secure` is added only when the request arrived over TLS. inginx terminates
/// plain HTTP and expects TLS at a reverse proxy, so the signal is
/// `X-Forwarded-Proto`; adding `Secure` unconditionally would silently break
/// the cookie on a plain-HTTP localhost run, where the browser drops Secure
/// cookies and the admin could never stay logged in. `SameSite=Lax` is the
/// CSRF defense — the cookie is simply not sent on a cross-site POST, which is
/// why there are no CSRF tokens anywhere in this codebase and there never need
/// to be.
fn set_session_cookie(token: &str, secure: bool) -> String {
    let sec = if secure { "; Secure" } else { "" };
    format!(
        "{SESSION_COOKIE}={token}; HttpOnly; SameSite=Lax; Path=/admin; Max-Age={SESSION_TTL}{sec}"
    )
}

/// A cleared cookie carrying the same attributes, so the browser matches and
/// drops it. `Secure` has to agree with how it was set or some browsers ignore
/// the deletion.
fn clear_session_cookie(secure: bool) -> String {
    let sec = if secure { "; Secure" } else { "" };
    format!("{SESSION_COOKIE}=; HttpOnly; SameSite=Lax; Path=/admin; Max-Age=0{sec}")
}

/// Whether this request reached us over TLS. inginx serves plain HTTP, so the
/// only signal is a fronting proxy's `X-Forwarded-Proto: https` — and that is
/// believed only when `trust_proxy_headers` says a proxy is actually there. A
/// client reaching inginx directly cannot forge itself a `Secure` cookie it
/// would then never send back.
fn is_https(headers: &HeaderMap, trust_proxy: bool) -> bool {
    trust_proxy
        && headers
            .get("x-forwarded-proto")
            .and_then(|v| v.to_str().ok())
            .map(|v| v.eq_ignore_ascii_case("https"))
            .unwrap_or(false)
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

/// The pages you reach without a session: signing in, the second factor, and
/// the one-time account setup. They carry the same identity as the admin
/// itself — same stylesheet, same mark, same theme — rather than the private
/// copy of a dark palette they used to inline. One card, centred, and no top
/// row: there is nowhere to navigate to yet.
fn page(title: &str, body: Markup) -> Html<String> {
    Html(
        html! {
            (DOCTYPE)
            html lang="en" {
                (crate::admin::pages::head(title))
                body {
                    div class="shell" {
                        div class="regions regions-center" {
                            div {
                                div class="brand-lockup" {
                                    (crate::admin::pages::mark())
                                    span class="brand-name" { "inginx" }
                                }
                                div class="card" {
                                    h2 { (title) }
                                    (body)
                                }
                            }
                        }
                    }
                }
            }
        }
        .into_string(),
    )
}

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
                    button class="primary" { "Create account" }
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
        return (
            StatusCode::BAD_REQUEST,
            "password must be at least 12 characters",
        )
            .into_response();
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

async fn login_form() -> Response {
    login_page(None)
}

#[derive(Deserialize)]
struct LoginForm {
    username: String,
    password: String,
}

/// The sign-in form, optionally carrying an error line. One place, so the
/// three ways in — first visit, wrong password, locked out — cannot drift
/// apart.
fn login_page(err: Option<&str>) -> Response {
    page(
        "Sign in",
        html! {
            form method="post" action="/admin/login" {
                label { "Username" }
                input type="text" name="username" required autofocus;
                label { "Password" }
                input type="password" name="password" required;
                button class="primary" { "Sign in" }
                @if let Some(e) = err { p class="err" { (e) } }
            }
        },
    )
    .into_response()
}

/// A valid Argon2 hash of a fixed string, computed once. When the username is
/// wrong we verify the password against this so the request costs the same as
/// a right-username-wrong-password one. Without it, "no such user" returned in
/// microseconds while a real miss spent Argon2's tens of milliseconds — a
/// timing oracle that confirmed the account name for free.
fn dummy_hash() -> &'static str {
    static H: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    H.get_or_init(|| hash_password("inginx-timing-equalizer").expect("hash dummy"))
}

async fn login_submit(
    State(st): State<AppState>,
    peer: PeerAddr,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let ip = client_ip(&headers, peer, st.trust_proxy_headers);
    let secure = is_https(&headers, st.trust_proxy_headers);

    // The wall goes up before the password is even looked at: a locked
    // address is turned away without a hash computed or a row read.
    if let Some(left) = st.login_throttle.check(ip) {
        let secs = left.as_secs().max(1);
        tracing::warn!(%ip, "login attempt from locked-out address");
        return (
            StatusCode::TOO_MANY_REQUESTS,
            [(header::RETRY_AFTER, secs.to_string())],
            Html(
                page(
                    "Sign in",
                    html! {
                        p class="err" {
                            "Too many failed attempts. Try again in "
                            (secs) " seconds."
                        }
                    },
                )
                .0,
            ),
        )
            .into_response();
    }

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
    // Always run a verification, against the real hash or the dummy, so the
    // answer takes the same time whether or not the username matched.
    let ok = match &creds {
        Some((user, hash)) => {
            let pw_ok = verify_password(&f.password, hash);
            *user == f.username && pw_ok
        }
        None => {
            verify_password(&f.password, dummy_hash());
            false
        }
    };
    if !ok {
        st.login_throttle.on_failure(ip);
        return login_page(Some("Invalid username or password."));
    }
    st.login_throttle.on_success(ip);

    let has_passkeys: bool = st
        .db
        .call(|c| {
            let n: i64 = c.query_row("SELECT count(*) FROM webauthn_credentials", [], |r| {
                r.get(0)
            })?;
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
            [(header::SET_COOKIE, set_session_cookie(&token, secure))],
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
            clear_session_cookie(is_https(&headers, st.trust_proxy_headers)),
        )],
        Redirect::to("/admin/login"),
    )
        .into_response()
}

// ---------------------------------------------------------------------------
// WebAuthn (FIDO2) second factor
// ---------------------------------------------------------------------------

use webauthn_rs::prelude::*;

/// Stable user handle for the single admin account.
fn admin_uuid() -> Uuid {
    use sha2::{Digest, Sha256};
    let d = Sha256::digest(b"inginx-admin");
    Uuid::from_slice(&d[..16]).expect("16 bytes")
}

/// Builds a Webauthn instance from stored rp settings, deriving and
/// persisting them from the request Host on first use.
async fn webauthn_for(db: &Db, headers: &HeaderMap) -> Result<Webauthn> {
    let rp_id = match get_setting(db, "rp_id").await? {
        Some(v) => v,
        None => {
            let host = headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("localhost");
            let rp = host.split(':').next().unwrap_or("localhost").to_string();
            set_setting(db, "rp_id", &rp).await?;
            rp
        }
    };
    let rp_origin = match get_setting(db, "rp_origin").await? {
        Some(v) => v,
        None => {
            let proto = headers
                .get("x-forwarded-proto")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("http");
            let host = headers
                .get(header::HOST)
                .and_then(|v| v.to_str().ok())
                .unwrap_or("localhost");
            let origin = format!("{proto}://{host}");
            set_setting(db, "rp_origin", &origin).await?;
            origin
        }
    };
    let url = Url::parse(&rp_origin)?;
    Ok(WebauthnBuilder::new(&rp_id, &url)?
        .rp_name("inginx")
        .build()?)
}

async fn stored_passkeys(db: &Db) -> Result<Vec<Passkey>> {
    let rows: Vec<String> = db
        .call(|c| {
            let mut stmt = c.prepare("SELECT cred_json FROM webauthn_credentials")?;
            let rows = stmt.query_map([], |r| r.get(0))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
        .await?;
    Ok(rows
        .iter()
        .filter_map(|j| serde_json::from_str(j).ok())
        .collect())
}

async fn register_start(State(st): State<AppState>, headers: HeaderMap) -> Response {
    // full session enforced by middleware wrapper in router()
    let result: Result<_> = async {
        let webauthn = webauthn_for(&st.db, &headers).await?;
        let exclude: Vec<CredentialID> = stored_passkeys(&st.db)
            .await?
            .iter()
            .map(|p| p.cred_id().clone())
            .collect();
        let (ccr, reg_state) = webauthn.start_passkey_registration(
            admin_uuid(),
            "admin",
            "inginx admin",
            (!exclude.is_empty()).then_some(exclude),
        )?;
        set_setting(
            &st.db,
            "webauthn_reg_state",
            &serde_json::to_string(&reg_state)?,
        )
        .await?;
        Ok(axum::Json(ccr))
    }
    .await;
    match result {
        Ok(json) => json.into_response(),
        Err(e) => {
            tracing::error!("webauthn register start failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "webauthn error").into_response()
        }
    }
}

async fn register_finish(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let result: Result<()> = async {
        let webauthn = webauthn_for(&st.db, &headers).await?;
        let reg: RegisterPublicKeyCredential = serde_json::from_slice(&body)?;
        let state_json = get_setting(&st.db, "webauthn_reg_state")
            .await?
            .ok_or_else(|| anyhow::anyhow!("no registration in progress"))?;
        let reg_state: PasskeyRegistration = serde_json::from_str(&state_json)?;
        let passkey = webauthn.finish_passkey_registration(&reg, &reg_state)?;
        let json = serde_json::to_string(&passkey)?;
        st.db
            .call(move |c| {
                c.execute(
                    "INSERT INTO webauthn_credentials (cred_json, created_at) VALUES (?1, ?2)",
                    rusqlite::params![json, now()],
                )?;
                c.execute("DELETE FROM settings WHERE key = 'webauthn_reg_state'", [])?;
                Ok(())
            })
            .await?;
        Ok(())
    }
    .await;
    match result {
        Ok(()) => axum::Json(serde_json::json!({"ok": true})).into_response(),
        Err(e) => {
            tracing::warn!("webauthn register finish rejected: {e}");
            (StatusCode::BAD_REQUEST, "registration rejected").into_response()
        }
    }
}

async fn auth_start(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if session_state(&st.db, &headers).await.is_none() {
        return (StatusCode::UNAUTHORIZED, "no session").into_response();
    }
    let result: Result<_> = async {
        let webauthn = webauthn_for(&st.db, &headers).await?;
        let passkeys = stored_passkeys(&st.db).await?;
        if passkeys.is_empty() {
            anyhow::bail!("no passkeys enrolled");
        }
        let (rcr, auth_state) = webauthn.start_passkey_authentication(&passkeys)?;
        set_setting(
            &st.db,
            "webauthn_auth_state",
            &serde_json::to_string(&auth_state)?,
        )
        .await?;
        Ok(axum::Json(rcr))
    }
    .await;
    match result {
        Ok(json) => json.into_response(),
        Err(e) => {
            tracing::error!("webauthn auth start failed: {e}");
            (StatusCode::INTERNAL_SERVER_ERROR, "webauthn error").into_response()
        }
    }
}

async fn auth_finish(
    State(st): State<AppState>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let Some((token, _)) = session_state(&st.db, &headers).await else {
        return (StatusCode::UNAUTHORIZED, "no session").into_response();
    };
    let result: Result<String> = async {
        let webauthn = webauthn_for(&st.db, &headers).await?;
        let cred: PublicKeyCredential = serde_json::from_slice(&body)?;
        let state_json = get_setting(&st.db, "webauthn_auth_state")
            .await?
            .ok_or_else(|| anyhow::anyhow!("no authentication in progress"))?;
        let auth_state: PasskeyAuthentication = serde_json::from_str(&state_json)?;
        webauthn.finish_passkey_authentication(&cred, &auth_state)?;
        // Rotate rather than elevate in place. The half-session token existed
        // before the second factor was proven; minting a fresh full-session
        // token and dropping the old one means a token that ever leaked as a
        // half-session cannot be the one that ends up fully privileged.
        let new_token = create_session(&st.db, true).await?;
        st.db
            .call(move |c| {
                c.execute("DELETE FROM sessions WHERE token = ?1", [token])?;
                c.execute("DELETE FROM settings WHERE key = 'webauthn_auth_state'", [])?;
                Ok(())
            })
            .await?;
        Ok(new_token)
    }
    .await;
    match result {
        Ok(new_token) => (
            [(
                header::SET_COOKIE,
                set_session_cookie(&new_token, is_https(&headers, st.trust_proxy_headers)),
            )],
            axum::Json(serde_json::json!({"ok": true})),
        )
            .into_response(),
        Err(e) => {
            tracing::warn!("webauthn auth finish rejected: {e}");
            (StatusCode::UNAUTHORIZED, "authentication rejected").into_response()
        }
    }
}

async fn twofa_page(State(st): State<AppState>, headers: HeaderMap) -> Response {
    if session_state(&st.db, &headers).await.is_none() {
        return Redirect::to("/admin/login").into_response();
    }
    // No inline script: the button carries data attributes and webauthn.js
    // (served, same-origin) wires them on load, so the page satisfies a
    // `script-src 'self'` CSP with no nonce. `data-passkey-redirect` is where
    // it goes on success.
    page(
        "Two-factor authentication",
        html! {
            p { "Confirm your identity with your passkey." }
            button class="primary" data-passkey-auth data-passkey-redirect="/admin" { "Use passkey" }
            p class="err" data-passkey-error {}
            script src="/admin/assets/webauthn.js" defer {}
        },
    )
    .into_response()
}

pub fn router(state: AppState) -> Router {
    let register = Router::new()
        .route("/admin/webauthn/register/start", post(register_start))
        .route("/admin/webauthn/register/finish", post(register_finish))
        .with_state(state.clone());
    Router::new()
        .route("/admin/setup", get(setup_form).post(setup_submit))
        .route("/admin/login", get(login_form).post(login_submit))
        .route("/admin/login/2fa", get(twofa_page))
        .route("/admin/logout", post(logout))
        .route("/admin/webauthn/auth/start", post(auth_start))
        .route("/admin/webauthn/auth/finish", post(auth_finish))
        .with_state(state.clone())
        .merge(protect(state, register))
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
    use std::net::IpAddr;
    use tower::ServiceExt;

    async fn state() -> AppState {
        let db = testdb().await;
        AppState::for_test(db.clone(), spawn_writer(db.clone()), reqwest::Client::new())
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
        let r = req(
            &app,
            "POST",
            "/admin/login",
            Some("username=admin&password=nope"),
            None,
        )
        .await;
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

    async fn setup_admin(st: &AppState, app: &Router) -> String {
        let token = ensure_setup_token(&st.db).await.unwrap().unwrap();
        req(
            app,
            "POST",
            "/admin/setup",
            Some(&format!("t={token}&username=admin&password=longenoughpw1")),
            None,
        )
        .await;
        let r = req(
            app,
            "POST",
            "/admin/login",
            Some("username=admin&password=longenoughpw1"),
            None,
        )
        .await;
        r.headers()
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
            .to_string()
    }

    #[tokio::test]
    async fn webauthn_register_start_returns_challenge() {
        let st = state().await;
        let app = full_router(st.clone());
        let cookie = setup_admin(&st, &app).await;
        let r = req(
            &app,
            "POST",
            "/admin/webauthn/register/start",
            None,
            Some(&cookie),
        )
        .await;
        assert_eq!(r.status(), StatusCode::OK);
        let body = axum::body::to_bytes(r.into_body(), 1 << 20).await.unwrap();
        let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
        assert!(v["publicKey"]["challenge"].is_string());
        assert_eq!(v["publicKey"]["rp"]["name"], "inginx");
    }

    #[tokio::test]
    async fn webauthn_auth_finish_garbage_rejected() {
        let st = state().await;
        let app = full_router(st.clone());
        let cookie = setup_admin(&st, &app).await;
        let mut b = axum::http::Request::builder()
            .method("POST")
            .uri("/admin/webauthn/auth/finish")
            .header("content-type", "application/json");
        b = b.header("cookie", format!("{SESSION_COOKIE}={cookie}"));
        let r = app
            .clone()
            .oneshot(
                b.body(axum::body::Body::from("{\"garbage\":true}"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
        // and with no session at all
        let r = req(&app, "POST", "/admin/webauthn/auth/finish", None, None).await;
        assert_eq!(r.status(), StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn half_session_cannot_pass_require_session() {
        let st = state().await;
        let app = full_router(st.clone());
        let half = create_session(&st.db, false).await.unwrap();
        let r = req(&app, "GET", "/admin/keys", None, Some(&half)).await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        let full = create_session(&st.db, true).await.unwrap();
        let r = req(&app, "GET", "/admin/keys", None, Some(&full)).await;
        assert_eq!(r.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn login_locks_out_after_repeated_failures() {
        let st = state().await;
        let app = full_router(st.clone());
        setup_admin(&st, &app).await;

        // The tests carry no connect-info, so every attempt keys on the same
        // unknown address — one bucket, which is what we want to watch fill.
        // `for_test` sets the threshold to 5.
        for _ in 0..5 {
            let r = req(
                &app,
                "POST",
                "/admin/login",
                Some("username=admin&password=wrong"),
                None,
            )
            .await;
            // Each failure re-renders the form; none is a lockout yet.
            assert_eq!(r.status(), StatusCode::OK);
        }

        // The next attempt is refused before the password is even checked —
        // and the *correct* password does not get through while locked.
        let r = req(
            &app,
            "POST",
            "/admin/login",
            Some("username=admin&password=longenoughpw1"),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::TOO_MANY_REQUESTS);
        assert!(r.headers().get(header::RETRY_AFTER).is_some());
        assert!(r.headers().get(header::SET_COOKIE).is_none());
    }

    #[test]
    fn client_ip_trusts_proxy_headers_only_when_enabled() {
        let peer = || PeerAddr(Some("203.0.113.9:5000".parse().unwrap()));
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-for", "198.51.100.7".parse().unwrap());
        h.insert("x-real-ip", "198.51.100.7".parse().unwrap());

        // Off: the forwarded headers are ignored, we key on the real peer.
        assert_eq!(
            client_ip(&h, peer(), false),
            "203.0.113.9".parse::<IpAddr>().unwrap()
        );
        // On: the proxy's client address wins.
        assert_eq!(
            client_ip(&h, peer(), true),
            "198.51.100.7".parse::<IpAddr>().unwrap()
        );

        // A client-supplied prefix cannot displace the address the proxy
        // appended: with only XFF present, the rightmost entry is used.
        let mut xff = HeaderMap::new();
        xff.insert("x-forwarded-for", "1.1.1.1, 198.51.100.7".parse().unwrap());
        assert_eq!(
            client_ip(&xff, peer(), true),
            "198.51.100.7".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn https_only_believed_behind_trusted_proxy() {
        let mut h = HeaderMap::new();
        h.insert("x-forwarded-proto", "https".parse().unwrap());
        assert!(!is_https(&h, false), "not trusted: proto header ignored");
        assert!(is_https(&h, true), "trusted: proto header honored");
    }

    #[tokio::test]
    async fn login_success_before_threshold_is_not_blocked() {
        let st = state().await;
        let app = full_router(st.clone());
        setup_admin(&st, &app).await;

        // A few misses, then the right password — under the threshold, the
        // good login still goes through and clears the counter.
        for _ in 0..3 {
            req(
                &app,
                "POST",
                "/admin/login",
                Some("username=admin&password=wrong"),
                None,
            )
            .await;
        }
        let r = req(
            &app,
            "POST",
            "/admin/login",
            Some("username=admin&password=longenoughpw1"),
            None,
        )
        .await;
        assert_eq!(r.status(), StatusCode::SEE_OTHER);
        assert!(r.headers().get(header::SET_COOKIE).is_some());
    }
}
