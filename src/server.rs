//! Local HTTP API on 127.0.0.1 for the browser extension and the web dashboard.

use std::sync::Arc;

use axum::extract::{Path, Query, Request, State};
use axum::http::{header, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use reqwest::header::{HeaderMap, HeaderName};
use serde::{Deserialize, Serialize};
use serde_json::json;
use tower_http::cors::{AllowOrigin, CorsLayer};

use crate::queue::{AddRequest, Manager, Settings};
use crate::{dash, engine, hls, youtube};

type AppState = Arc<Manager>;

/// Every API call must carry this header. Plain web pages can't add custom headers to
/// cross-origin requests without a CORS preflight, and CORS only allows extension origins.
const API_HEADER: &str = "x-downloaderx";

/// `listener` is bound by the caller, so a second app instance can notice the first one.
pub async fn serve(mgr: Arc<Manager>, listener: std::net::TcpListener) -> anyhow::Result<()> {
    let cors = CorsLayer::new()
        .allow_origin(AllowOrigin::predicate(|o: &HeaderValue, _| {
            let o = o.as_bytes();
            o.starts_with(b"chrome-extension://") || o.starts_with(b"moz-extension://")
        }))
        .allow_methods([Method::GET, Method::POST])
        .allow_headers([header::CONTENT_TYPE, HeaderName::from_static(API_HEADER)]);

    let api = Router::new()
        .route("/status", get(status))
        .route("/items", get(items))
        .route("/add", post(add))
        .route("/items/{id}/{action}", post(item_action))
        .route("/all/{action}", post(all_action))
        .route("/clear", post(clear))
        .route("/settings", get(get_settings).post(set_settings))
        .route("/schedule", post(schedule))
        .route("/show", post(show))
        .route("/icon", get(icon))
        .route("/probe", post(probe));

    let app = Router::new()
        .route("/", get(ui))
        .route("/favicon.ico", get(favicon))
        .route("/icon.png", get(icon_png))
        .route("/fonts/{name}", get(font))
        .nest("/api", api)
        .layer(middleware::from_fn(require_header))
        .layer(cors)
        .with_state(mgr);
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    println!("DownloaderX running at http://{}", listener.local_addr()?);
    axum::serve(listener, app).await?;
    Ok(())
}

/// When the browser extension last called the API, and from which browser, so the app can
/// show whether it is set up.
static EXTENSION_SEEN: std::sync::Mutex<Option<(u64, &'static str)>> = std::sync::Mutex::new(None);

async fn require_header(req: Request, next: Next) -> Response {
    // Reject other Host names so a website can't reach us through DNS rebinding.
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    if !matches!(host, "127.0.0.1" | "localhost") {
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }
    // Extensions with host permissions often send no Origin on GET, so it also says so in the header.
    let from_extension = req.headers().get(API_HEADER).is_some_and(|v| v == "ext")
        || req.headers().get(header::ORIGIN).is_some_and(|o| o.as_bytes().starts_with(b"chrome-extension://"));
    if from_extension && req.headers().contains_key(API_HEADER) {
        let ua = req.headers().get(header::USER_AGENT).and_then(|v| v.to_str().ok()).unwrap_or("");
        *EXTENSION_SEEN.lock().unwrap() = Some((crate::queue::now(), if ua.contains("Edg/") { "Edge" } else { "Chrome" }));
    }
    let public = matches!(req.uri().path(), "/" | "/favicon.ico" | "/icon.png") || req.uri().path().starts_with("/fonts/");
    if req.method() == Method::OPTIONS || public || req.headers().contains_key(API_HEADER) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "missing x-downloaderx header").into_response()
    }
}

async fn ui() -> Html<&'static str> {
    Html(include_str!("ui.html"))
}

/// The UI's two typefaces, bundled so the window looks right offline.
async fn font(Path(name): Path<String>) -> Response {
    let data: &[u8] = match name.as_str() {
        "titan-one.woff2" => include_bytes!("../assets/fonts/titan-one.woff2"),
        "nunito.woff2" => include_bytes!("../assets/fonts/nunito.woff2"),
        _ => return StatusCode::NOT_FOUND.into_response(),
    };
    ([(header::CONTENT_TYPE, "font/woff2"), (header::CACHE_CONTROL, "max-age=31536000")], data).into_response()
}

async fn icon_png() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/png")], &include_bytes!("../assets/icon-256.png")[..])
}

async fn favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/x-icon")], &include_bytes!("../assets/downloaderx.ico")[..])
}

async fn status() -> Json<serde_json::Value> {
    let ext = EXTENSION_SEEN.lock().unwrap().map(|(at, browser)| json!({ "browser": browser, "ago": crate::queue::now().saturating_sub(at) }));
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION"), "extension": ext }))
}

async fn items(State(m): State<AppState>) -> impl IntoResponse {
    Json(m.items())
}

async fn add(State(m): State<AppState>, Json(r): Json<AddRequest>) -> Response {
    if !(r.url.starts_with("http://") || r.url.starts_with("https://")) {
        return (StatusCode::BAD_REQUEST, "only http(s) URLs").into_response();
    }
    Json(json!({ "id": m.add(r) })).into_response()
}

async fn item_action(State(m): State<AppState>, Path((id, action)): Path<(u64, String)>) -> StatusCode {
    if m.action(id, &action) { StatusCode::OK } else { StatusCode::NOT_FOUND }
}

async fn all_action(State(m): State<AppState>, Path(action): Path<String>) -> StatusCode {
    match action.as_str() {
        "pause" | "resume" => {
            m.all(&action);
            StatusCode::OK
        }
        "stop_queue" => {
            m.stop_queue();
            StatusCode::OK
        }
        _ => StatusCode::BAD_REQUEST,
    }
}

async fn clear(State(m): State<AppState>) -> StatusCode {
    m.clear_finished();
    StatusCode::OK
}

async fn get_settings(State(m): State<AppState>) -> Json<Settings> {
    Json(m.settings())
}

async fn set_settings(State(m): State<AppState>, Json(s): Json<Settings>) -> StatusCode {
    m.set_settings(s);
    StatusCode::OK
}

#[derive(Deserialize)]
struct ScheduleReq {
    ids: Vec<u64>,
    at: Option<u64>,
}

async fn schedule(State(m): State<AppState>, Json(r): Json<ScheduleReq>) -> StatusCode {
    m.schedule(&r.ids, r.at);
    StatusCode::OK
}

/// Bring the desktop window to the front (used by a second launch and the extension).
/// An optional `{"open": "extension"}` body also opens that panel.
async fn show(State(m): State<AppState>, body: axum::body::Bytes) -> StatusCode {
    let panel = serde_json::from_slice::<serde_json::Value>(&body).ok().and_then(|v| v["open"].as_str().map(String::from));
    match m.show_window.get() {
        Some(f) => {
            f(panel);
            StatusCode::OK
        }
        // The window is still starting (two launches at once): it opens the panel when ready.
        None => {
            *m.pending_panel.lock().unwrap() = panel;
            StatusCode::ACCEPTED
        }
    }
}

#[derive(Deserialize)]
struct IconReq {
    /// A finished item: icon of the real file (e.g. an installer's own icon).
    id: Option<u64>,
    /// Otherwise the generic icon for this extension.
    ext: Option<String>,
}

/// File-type icon from the Windows shell, as PNG.
async fn icon(State(m): State<AppState>, Query(q): Query<IconReq>) -> Response {
    let file = q.id.and_then(|id| m.items().into_iter().find(|i| i.id == id)).and_then(|i| i.path.filter(|p| p.exists()));
    let ext = q.ext.unwrap_or_default();
    let ext: String = ext.trim_start_matches('.').chars().filter(char::is_ascii_alphanumeric).take(12).collect();
    let png = tokio::task::spawn_blocking(move || crate::shell_icon(file.as_deref(), &ext)).await.ok().flatten();
    match png {
        Some(png) => ([(header::CONTENT_TYPE, "image/png"), (header::CACHE_CONTROL, "max-age=3600")], png).into_response(),
        None => StatusCode::NOT_FOUND.into_response(),
    }
}

#[derive(Deserialize)]
struct ProbeReq {
    url: String,
    #[serde(default)]
    headers: Vec<(String, String)>,
}

#[derive(Serialize)]
struct Choice {
    label: String,
    kind: serde_json::Value,
    url: String,
    size: Option<u64>,
    /// Suggested file name.
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    /// Extra request headers the download needs (sent back with /add).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    headers: Vec<(String, String)>,
}

/// Work out what a URL is and which qualities can be downloaded from it.
async fn probe(Json(r): Json<ProbeReq>) -> Response {
    match probe_inner(&r).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

async fn probe_inner(r: &ProbeReq) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let mut headers = HeaderMap::new();
    for (k, v) in &r.headers {
        if let (Ok(k), Ok(v)) = (HeaderName::try_from(k.as_str()), v.parse()) {
            headers.insert(k, v);
        }
    }
    if let Some(id) = youtube::video_id(&r.url) {
        let v = youtube::probe(&client, &id).await?;
        let choices: Vec<Choice> = v
            .choices
            .iter()
            .map(|c| Choice { label: c.label.clone(), kind: c.kind.clone(), url: c.url.clone(), size: c.size, name: youtube::name(&v.title, c), headers: v.headers.clone() })
            .collect();
        return Ok(json!({ "title": v.title, "choices": choices }));
    }
    let lower = r.url.to_ascii_lowercase();
    let path = lower.split(['?', '#']).next().unwrap_or("");
    let looks_hls = path.ends_with(".m3u8");

    if !looks_hls {
        let p = if path.ends_with(".mpd") { None } else { Some(engine::probe(&client, &r.url, &headers).await?) };
        let ct = match &p {
            Some(p) => p.content_type.clone().unwrap_or_default(),
            None => "application/dash+xml".into(),
        };
        if ct.contains("dash+xml") {
            let (text, base) = hls::fetch_text(&client, &r.url, &headers).await?;
            let reps = dash::parse(&text, &base)?;
            let audio = dash::best_audio(&reps).map(|a| a.id.clone());
            let mut choices: Vec<Choice> = reps
                .iter()
                .filter(|v| v.kind == dash::Kind::Video)
                .map(|v| Choice { label: v.label.clone(), kind: json!({ "type": "dash", "video": v.id, "audio": audio }), url: r.url.clone(), size: None, name: None, headers: vec![] })
                .collect();
            if let Some(a) = audio.as_ref().and_then(|id| reps.iter().find(|r| &r.id == id)) {
                let label = format!("Audio only · {}", a.label.trim_start_matches("Audio · "));
                choices.push(Choice { label, kind: json!({ "type": "dash", "video": null, "audio": a.id }), url: r.url.clone(), size: None, name: None, headers: vec![] });
            }
            return Ok(json!({ "title": null, "choices": choices }));
        }
        if ct.contains("text/html") {
            anyhow::bail!("this is a web page: open it in Chrome or Edge and use the DownloaderX button on the video");
        }
        if let Some(p) = p.filter(|_| !ct.contains("mpegurl")) {
            let label = p.filename.clone().unwrap_or("file".into());
            return Ok(json!({
                "title": p.filename,
                "choices": [Choice { label, kind: json!({ "type": "direct" }), url: r.url.clone(), size: p.size, name: None, headers: vec![] }],
            }));
        }
    }

    let (text, base) = hls::fetch_text(&client, &r.url, &headers).await?;
    let choices: Vec<Choice> = if hls::is_master(&text) {
        hls::parse_master(&text, &base)
            .into_iter()
            .map(|v| Choice { label: v.label, kind: json!({ "type": "hls", "audio": v.audio }), url: v.url, size: None, name: None, headers: vec![] })
            .collect()
    } else {
        vec![Choice { label: "Stream".into(), kind: json!({ "type": "hls", "audio": null }), url: r.url.clone(), size: None, name: None, headers: vec![] }]
    };
    Ok(json!({ "title": null, "choices": choices }))
}
