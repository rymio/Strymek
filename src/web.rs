//! HTTPS server: login, dashboard, JSON API, per-stream pages and WebSockets.

use crate::auth::{self, Auth, LoginCheck, COOKIE};
use crate::desktop;
use crate::stream::launch::Family;
use crate::stream::{ClientMsg, Cmd, Manager, Out, Stream};
use axum::body::Body;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{ConnectInfo, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::{Form, Json, Router};
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

pub struct AppState {
    pub mgr: Arc<Manager>,
    pub auth: Auth,
    pub password_hash: String,
    pub ca_pem: Option<String>,
}

type St = Arc<AppState>;

// ---------- embedded web client ----------

const LOGIN_HTML: &str = include_str!("../assets/login.html");
const DASHBOARD_HTML: &str = include_str!("../assets/dashboard.html");
const STREAM_HTML: &str = include_str!("../assets/stream.html");
const LAUNCH_HTML: &str = include_str!("../assets/launch.html");
const GONE_HTML: &str = include_str!("../assets/gone.html");

fn asset(name: &str) -> Option<(&'static str, &'static [u8])> {
    Some(match name {
        "app.css" => ("text/css; charset=utf-8", include_bytes!("../assets/app.css")),
        "dashboard.js" => ("text/javascript; charset=utf-8", include_bytes!("../assets/dashboard.js")),
        "stream.js" => ("text/javascript; charset=utf-8", include_bytes!("../assets/stream.js")),
        "launch.js" => ("text/javascript; charset=utf-8", include_bytes!("../assets/launch.js")),
        "icon.svg" => ("image/svg+xml", include_bytes!("../assets/icon.svg")),
        "app-default.svg" => ("image/svg+xml", include_bytes!("../assets/app-default.svg")),
        _ => return None,
    })
}

fn esc(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&#39;")
}

// ---------- auth helpers ----------

fn session_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get_all(header::COOKIE)
        .iter()
        .filter_map(|v| v.to_str().ok())
        .find_map(auth::token_from_cookie_header)
}

fn authed(st: &AppState, headers: &HeaderMap) -> Option<String> {
    let t = session_token(headers)?;
    st.auth.validate(&t).then_some(t)
}

/// Same-origin check for state-changing requests and WebSocket upgrades.
fn same_origin(headers: &HeaderMap) -> bool {
    let host = headers.get(header::HOST).and_then(|v| v.to_str().ok());
    let origin = headers.get(header::ORIGIN).and_then(|v| v.to_str().ok());
    match (host, origin) {
        (Some(h), Some(o)) => o == format!("https://{h}"),
        _ => false,
    }
}

fn api_guard(st: &AppState, headers: &HeaderMap) -> Result<String, Response> {
    let Some(t) = authed(st, headers) else {
        return Err((StatusCode::UNAUTHORIZED, "login required").into_response());
    };
    Ok(t)
}

fn write_guard(st: &AppState, headers: &HeaderMap) -> Result<String, Response> {
    let t = api_guard(st, headers)?;
    let csrf = headers.get("x-strymek").and_then(|v| v.to_str().ok()) == Some("1");
    if !csrf || !same_origin(headers) {
        return Err((StatusCode::FORBIDDEN, "cross-site request refused").into_response());
    }
    Ok(t)
}

fn login_redirect(next: &str) -> Response {
    Redirect::to(&format!("/login?next={}", urlencode(next))).into_response()
}

fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'/' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn safe_next(n: Option<&str>) -> String {
    match n {
        Some(n) if n.starts_with('/') && !n.starts_with("//") && !n.contains('\\') => n.to_string(),
        _ => "/".into(),
    }
}

// ---------- security headers ----------

async fn security_headers(mut req: Request, next: Next) -> Response {
    // HTTP/2 carries the host in :authority, not a Host header; normalise it so
    // the Origin checks below work for both protocol versions.
    if !req.headers().contains_key(header::HOST) {
        if let Some(auth) = req.uri().authority().map(|a| a.as_str().to_string()) {
            if let Ok(v) = HeaderValue::from_str(&auth) {
                req.headers_mut().insert(header::HOST, v);
            }
        }
    }
    let host = req
        .headers()
        .get(header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '-' | '[' | ']'))
        .collect::<String>();
    let is_asset = req.uri().path().starts_with("/assets/") || req.uri().path().starts_with("/api/icon/");
    let mut res = next.run(req).await;
    let h = res.headers_mut();
    let csp = format!(
        "default-src 'self'; script-src 'self'; style-src 'self'; img-src 'self' data: blob:; \
         connect-src 'self' wss://{host}; manifest-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'"
    );
    if let Ok(v) = HeaderValue::from_str(&csp) {
        h.insert(header::CONTENT_SECURITY_POLICY, v);
    }
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    h.insert(header::X_FRAME_OPTIONS, HeaderValue::from_static("DENY"));
    h.insert(header::REFERRER_POLICY, HeaderValue::from_static("same-origin"));
    h.insert(header::STRICT_TRANSPORT_SECURITY, HeaderValue::from_static("max-age=31536000"));
    if !h.contains_key(header::CACHE_CONTROL) {
        h.insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static(if is_asset { "no-cache" } else { "no-store" }),
        );
    }
    res
}

// ---------- pages ----------

#[derive(Deserialize)]
struct LoginQuery {
    next: Option<String>,
    e: Option<String>,
}

async fn login_page(Query(q): Query<LoginQuery>) -> Html<String> {
    let msg = match q.e.as_deref() {
        Some("bad") => "Wrong password.",
        Some("locked") => "Too many attempts. Try again in a few minutes.",
        _ => "",
    };
    Html(
        LOGIN_HTML
            .replace("{{NEXT}}", &esc(&safe_next(q.next.as_deref())))
            .replace("{{ERROR}}", msg),
    )
}

#[derive(Deserialize)]
struct LoginForm {
    password: String,
    next: Option<String>,
}

async fn login_post(
    State(st): State<St>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Form(f): Form<LoginForm>,
) -> Response {
    let next = safe_next(f.next.as_deref());
    if headers.get(header::ORIGIN).is_some() && !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-site login refused").into_response();
    }
    let ip = addr.ip();
    if let LoginCheck::LockedOut = st.auth.check_lockout(ip) {
        return Redirect::to(&format!("/login?e=locked&next={}", urlencode(&next))).into_response();
    }
    let hash = st.password_hash.clone();
    let ok = tokio::task::spawn_blocking(move || auth::verify_password(&hash, &f.password))
        .await
        .unwrap_or(false);
    if !ok {
        st.auth.record_failure(ip);
        tracing::warn!(%ip, "failed login");
        tokio::time::sleep(Duration::from_millis(500)).await;
        return Redirect::to(&format!("/login?e=bad&next={}", urlencode(&next))).into_response();
    }
    let token = st.auth.create_session(ip);
    tracing::info!(%ip, "login");
    let max_age = st.mgr.cfg.session_idle_hours.max(1) * 3600;
    let cookie = format!("{COOKIE}={token}; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age={max_age}");
    let mut res = Redirect::to(&next).into_response();
    res.headers_mut().insert(header::SET_COOKIE, HeaderValue::from_str(&cookie).unwrap());
    res
}

async fn logout(State(st): State<St>, headers: HeaderMap) -> Response {
    if !same_origin(&headers) {
        return (StatusCode::FORBIDDEN, "cross-site request refused").into_response();
    }
    if let Some(t) = session_token(&headers) {
        st.auth.destroy(&t);
    }
    let mut res = Redirect::to("/login").into_response();
    res.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!("{COOKIE}=; Path=/; Secure; HttpOnly; SameSite=Strict; Max-Age=0")).unwrap(),
    );
    res
}

async fn dashboard(State(st): State<St>, headers: HeaderMap) -> Response {
    if authed(&st, &headers).is_none() {
        return login_redirect("/");
    }
    Html(DASHBOARD_HTML.to_string()).into_response()
}

async fn static_asset(Path(file): Path<String>) -> Response {
    match asset(&file) {
        Some((ct, body)) => ([(header::CONTENT_TYPE, ct)], body).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn ca_pem(State(st): State<St>) -> Response {
    match &st.ca_pem {
        Some(p) => (
            [
                (header::CONTENT_TYPE, "application/x-x509-ca-cert"),
                (header::CONTENT_DISPOSITION, "attachment; filename=\"strymek-ca.pem\""),
            ],
            p.clone(),
        )
            .into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn stream_page(State(st): State<St>, Path(slug): Path<String>, headers: HeaderMap) -> Response {
    if authed(&st, &headers).is_none() {
        return login_redirect(&format!("/{slug}"));
    }
    match st.mgr.get(&slug) {
        Some(s) => Html(
            STREAM_HTML
                .replace("{{SLUG}}", &esc(&s.slug))
                .replace("{{NAME}}", &esc(&s.app.name))
                .replace("{{APPID}}", &esc(&s.app.id)),
        )
        .into_response(),
        None => {
            // Offer to start the same kind of app again if the slug looks like one of ours.
            let base: String = slug.trim_end_matches(|c: char| c.is_ascii_digit()).to_string();
            let app = st.mgr.apps().into_iter().find(|a| a.short == base);
            let (name, id) = app.map(|a| (a.name, a.id)).unwrap_or_default();
            (
                StatusCode::NOT_FOUND,
                Html(GONE_HTML.replace("{{SLUG}}", &esc(&slug)).replace("{{NAME}}", &esc(&name)).replace("{{APPID}}", &esc(&id))),
            )
                .into_response()
        }
    }
}

async fn app_page(State(st): State<St>, Path(id): Path<String>, headers: HeaderMap) -> Response {
    if authed(&st, &headers).is_none() {
        return login_redirect(&format!("/app/{id}"));
    }
    match st.mgr.app(&id) {
        Some(a) => Html(LAUNCH_HTML.replace("{{NAME}}", &esc(&a.name)).replace("{{APPID}}", &esc(&a.id))).into_response(),
        None => (StatusCode::NOT_FOUND, "unknown app").into_response(),
    }
}

async fn manifest(State(st): State<St>, Path(slug): Path<String>) -> Response {
    let Some(s) = st.mgr.get(&slug) else { return StatusCode::NOT_FOUND.into_response() };
    let icon_type = s
        .app
        .icon_path
        .as_ref()
        .and_then(|p| p.extension())
        .map(|e| if e == "svg" { "image/svg+xml" } else { "image/png" })
        .unwrap_or("image/svg+xml");
    let m = serde_json::json!({
        "name": s.app.name,
        "short_name": s.app.name,
        "id": format!("/app/{}", s.app.id),
        "start_url": format!("/app/{}", s.app.id),
        "scope": "/",
        "display": "standalone",
        "background_color": "#15171a",
        "theme_color": "#15171a",
        "icons": [{"src": format!("/api/icon/{}", s.app.id), "sizes": "any", "type": icon_type}],
    });
    ([(header::CONTENT_TYPE, "application/manifest+json")], m.to_string()).into_response()
}

// ---------- API ----------

async fn api_apps(State(st): State<St>, headers: HeaderMap) -> Response {
    if let Err(r) = api_guard(&st, &headers) {
        return r;
    }
    let mgr = st.mgr.clone();
    let apps = tokio::task::spawn_blocking(move || mgr.apps()).await.unwrap_or_default();
    Json(apps).into_response()
}

async fn api_icon(State(st): State<St>, Path(id): Path<String>) -> Response {
    let app = st.mgr.app(&id);
    let path = app.and_then(|a| a.icon_path);
    if let Some(p) = path {
        if let Ok(bytes) = tokio::fs::read(&p).await {
            let ct = if p.extension().map(|e| e == "svg").unwrap_or(false) { "image/svg+xml" } else { "image/png" };
            return ([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "max-age=86400")], bytes).into_response();
        }
    }
    let (ct, body) = asset("app-default.svg").unwrap();
    ([(header::CONTENT_TYPE, ct), (header::CACHE_CONTROL, "max-age=86400")], body).into_response()
}

async fn api_streams(State(st): State<St>, headers: HeaderMap) -> Response {
    if let Err(r) = api_guard(&st, &headers) {
        return r;
    }
    Json(st.mgr.list()).into_response()
}

#[derive(Deserialize)]
struct LaunchReq {
    app: String,
    w: i32,
    h: i32,
    scale: Option<u32>,
    /// Reuse a running stream of this app instead of starting a new one.
    reuse: Option<bool>,
}

async fn api_launch(State(st): State<St>, headers: HeaderMap, Json(req): Json<LaunchReq>) -> Response {
    if let Err(r) = write_guard(&st, &headers) {
        return r;
    }
    if req.reuse.unwrap_or(false) {
        if let Some(s) = st.mgr.list().into_iter().find(|s| s.app_id == req.app) {
            return Json(serde_json::json!({"slug": s.slug, "url": format!("/{}", s.slug), "reused": true})).into_response();
        }
    }
    let mgr = st.mgr.clone();
    let res = tokio::task::spawn_blocking(move || mgr.launch(&req.app, req.w, req.h, req.scale.unwrap_or(1))).await;
    match res {
        Ok(Ok(slug)) => Json(serde_json::json!({"slug": slug, "url": format!("/{slug}")})).into_response(),
        Ok(Err(e)) => {
            tracing::warn!("launch failed: {e:#}");
            (StatusCode::INTERNAL_SERVER_ERROR, Json(serde_json::json!({"error": format!("{e:#}")}))).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn api_stop(State(st): State<St>, Path(slug): Path<String>, headers: HeaderMap) -> Response {
    if let Err(r) = write_guard(&st, &headers) {
        return r;
    }
    let mgr = st.mgr.clone();
    let ok = tokio::task::spawn_blocking(move || mgr.stop(&slug)).await.unwrap_or(false);
    if ok { StatusCode::NO_CONTENT.into_response() } else { StatusCode::NOT_FOUND.into_response() }
}

async fn api_thumb(State(st): State<St>, Path(slug): Path<String>, headers: HeaderMap) -> Response {
    if let Err(r) = api_guard(&st, &headers) {
        return r;
    }
    let Some(s) = st.mgr.get(&slug) else { return StatusCode::NOT_FOUND.into_response() };
    match tokio::task::spawn_blocking(move || s.thumbnail()).await {
        Ok(Some(bytes)) => ([(header::CONTENT_TYPE, "image/webp")], bytes).into_response(),
        _ => StatusCode::SERVICE_UNAVAILABLE.into_response(),
    }
}

// ---------- your real desktop ----------

async fn api_desktop(State(st): State<St>, headers: HeaderMap) -> Response {
    if let Err(r) = api_guard(&st, &headers) {
        return r;
    }
    let mgr = st.mgr.clone();
    let body = tokio::task::spawn_blocking(move || {
        let sess = desktop::session();
        let moved: Vec<(Family, String)> = mgr
            .list()
            .into_iter()
            .filter(|s| s.real_profile)
            .filter_map(|s| mgr.get(&s.slug).map(|x| (crate::stream::launch::app_family(&x.app), s.slug)))
            .collect();
        let apps: Vec<serde_json::Value> = desktop::running_apps()
            .into_iter()
            .map(|a| {
                let app = mgr.app_for_family(a.family);
                serde_json::json!({
                    "family": a.family,
                    "label": a.label,
                    "processes": a.pids.len(),
                    "app_id": app.as_ref().map(|x| x.id.clone()),
                    "name": app.as_ref().map(|x| x.name.clone()).unwrap_or_else(|| a.label.to_string()),
                })
            })
            .collect();
        let live = sess.live_windows_supported();
        let (windows, windows_error) = if live {
            match desktop::list_windows(&sess) {
                Ok(ws) => (
                    ws.into_iter()
                        .map(|w| {
                            let app_id = mgr.app_for_window(w.family, &w.class).map(|a| a.id);
                            serde_json::json!({"window": w, "app_id": app_id})
                        })
                        .collect::<Vec<_>>(),
                    None,
                ),
                Err(e) => (vec![], Some(format!("{e:#}"))),
            }
        } else {
            (vec![], None)
        };
        serde_json::json!({
            "session": sess,
            "live": live,
            "apps": apps,
            "moved": moved.into_iter().map(|(f, s)| serde_json::json!({"family": f, "slug": s})).collect::<Vec<_>>(),
            "windows": windows,
            "windows_error": windows_error,
        })
    })
    .await
    .unwrap_or_default();
    Json(body).into_response()
}

#[derive(Deserialize)]
struct MoveReq {
    family: Family,
    w: i32,
    h: i32,
    scale: Option<u32>,
}

async fn api_desktop_move(State(st): State<St>, headers: HeaderMap, Json(req): Json<MoveReq>) -> Response {
    if let Err(r) = write_guard(&st, &headers) {
        return r;
    }
    let mgr = st.mgr.clone();
    let res = tokio::task::spawn_blocking(move || mgr.move_here(req.family, req.w, req.h, req.scale.unwrap_or(1))).await;
    slug_response(res)
}

#[derive(Deserialize)]
struct WindowReq {
    window: u32,
}

async fn api_desktop_stream(State(st): State<St>, headers: HeaderMap, Json(req): Json<WindowReq>) -> Response {
    if let Err(r) = write_guard(&st, &headers) {
        return r;
    }
    let mgr = st.mgr.clone();
    let res = tokio::task::spawn_blocking(move || mgr.stream_desktop_window(req.window)).await;
    slug_response(res)
}

fn slug_response(res: Result<anyhow::Result<String>, tokio::task::JoinError>) -> Response {
    match res {
        Ok(Ok(slug)) => Json(serde_json::json!({"slug": slug, "url": format!("/{slug}")})).into_response(),
        Ok(Err(e)) => {
            tracing::warn!("desktop action failed: {e:#}");
            (StatusCode::CONFLICT, Json(serde_json::json!({"error": format!("{e:#}")}))).into_response()
        }
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn api_desktop_lock(State(st): State<St>, Path(action): Path<String>, headers: HeaderMap) -> Response {
    if let Err(r) = write_guard(&st, &headers) {
        return r;
    }
    let mgr = st.mgr.clone();
    let res = tokio::task::spawn_blocking(move || {
        let sess = desktop::session();
        match action.as_str() {
            "unlock" => desktop::unlock(&sess).map(|_| mgr.note_unlocked_by_us()),
            "lock" => desktop::lock(&sess),
            _ => Err(anyhow::anyhow!("unknown action")),
        }
    })
    .await;
    match res {
        Ok(Ok(())) => StatusCode::NO_CONTENT.into_response(),
        Ok(Err(e)) => (StatusCode::CONFLICT, Json(serde_json::json!({"error": format!("{e:#}")}))).into_response(),
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn api_desktop_thumb(State(st): State<St>, Path(win): Path<u32>, headers: HeaderMap) -> Response {
    if let Err(r) = api_guard(&st, &headers) {
        return r;
    }
    let res = tokio::task::spawn_blocking(move || desktop::window_thumbnail(&desktop::session(), win)).await;
    match res {
        Ok(Ok(bytes)) => ([(header::CONTENT_TYPE, "image/webp")], bytes).into_response(),
        _ => StatusCode::NOT_FOUND.into_response(),
    }
}

// ---------- WebSocket ----------

async fn ws_handler(
    State(st): State<St>,
    Path(slug): Path<String>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    ws: WebSocketUpgrade,
) -> Response {
    let Some(token) = authed(&st, &headers) else {
        return (StatusCode::UNAUTHORIZED, "login required").into_response();
    };
    if !same_origin(&headers) {
        tracing::warn!(ip = %addr.ip(), "WebSocket with foreign Origin refused");
        return (StatusCode::FORBIDDEN, "cross-site WebSocket refused").into_response();
    }
    let Some(stream) = st.mgr.get(&slug) else { return StatusCode::NOT_FOUND.into_response() };
    tracing::info!(ip = %addr.ip(), %slug, "attach");
    ws.max_message_size(8 * 1024 * 1024)
        .on_upgrade(move |socket| run_socket(st, socket, stream, token))
}

async fn run_socket(st: St, socket: WebSocket, stream: Arc<Stream>, token: String) {
    let id = st.mgr.client_id();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<Out>();
    stream.send(Cmd::Attach { id, tx });
    let (mut sink, mut source) = socket.split();

    let writer = tokio::spawn(async move {
        while let Some(out) = rx.recv().await {
            let r = match out {
                Out::Text(t) => sink.send(Message::Text(t.into())).await,
                Out::Bin(b) => sink.send(Message::Binary(b.into())).await,
                Out::Close => {
                    let _ = sink.send(Message::Close(None)).await;
                    break;
                }
            };
            if r.is_err() {
                break;
            }
        }
    });

    let mut keepalive = tokio::time::interval(Duration::from_secs(60));
    loop {
        tokio::select! {
            msg = source.next() => {
                let Some(Ok(msg)) = msg else { break };
                match msg {
                    Message::Text(t) => {
                        match serde_json::from_str::<ClientMsg>(t.as_str()) {
                            Ok(m) => stream.send(Cmd::Client { id, msg: m }),
                            Err(e) => tracing::debug!("bad client message: {e}"),
                        }
                    }
                    Message::Close(_) => break,
                    _ => {}
                }
            }
            _ = keepalive.tick() => {
                // Streaming counts as activity; a revoked or expired session ends the stream.
                if !st.auth.validate(&token) {
                    break;
                }
            }
        }
        if writer.is_finished() {
            break;
        }
    }
    stream.send(Cmd::Detach { id });
    writer.abort();
}

pub fn router(st: St) -> Router {
    Router::new()
        .route("/", get(dashboard))
        .route("/login", get(login_page).post(login_post))
        .route("/logout", post(logout))
        .route("/ca.pem", get(ca_pem))
        .route("/assets/{file}", get(static_asset))
        .route("/app/{id}", get(app_page))
        .route("/api/apps", get(api_apps))
        .route("/api/icon/{id}", get(api_icon))
        .route("/api/streams", get(api_streams).post(api_launch))
        .route("/api/streams/{slug}", delete(api_stop))
        .route("/api/streams/{slug}/thumb", get(api_thumb))
        .route("/api/desktop", get(api_desktop))
        .route("/api/desktop/move", post(api_desktop_move))
        .route("/api/desktop/stream", post(api_desktop_stream))
        .route("/api/desktop/thumb/{win}", get(api_desktop_thumb))
        .route("/api/desktop/{action}", post(api_desktop_lock))
        .route("/{slug}", get(stream_page))
        .route("/{slug}/manifest.webmanifest", get(manifest))
        .route("/{slug}/ws", get(ws_handler))
        .fallback(|| async { (StatusCode::NOT_FOUND, Body::from("not found")) })
        .layer(middleware::from_fn(security_headers))
        .with_state(st)
}
