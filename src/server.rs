//! Local HTTP API on 127.0.0.1 for the browser extension and the web dashboard.

use std::sync::Arc;

use axum::extract::{Path, Request, State};
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
use crate::{engine, hls, ytdlp};

type AppState = Arc<Manager>;

/// Every API call must carry this header. Plain web pages can't add custom headers to
/// cross-origin requests without a CORS preflight, and CORS only allows extension origins.
const API_HEADER: &str = "x-downloaderx";

pub async fn serve(mgr: Arc<Manager>, port: u16) -> anyhow::Result<()> {
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
        .route("/probe", post(probe));

    let app = Router::new()
        .route("/", get(ui))
        .nest("/api", api)
        .layer(middleware::from_fn(require_header))
        .layer(cors)
        .with_state(mgr);
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    println!("DownloaderX running at http://127.0.0.1:{port}");
    axum::serve(listener, app).await?;
    Ok(())
}

async fn require_header(req: Request, next: Next) -> Response {
    // Reject other Host names so a website can't reach us through DNS rebinding.
    let host = req.headers().get(header::HOST).and_then(|h| h.to_str().ok()).unwrap_or("");
    let host = host.rsplit_once(':').map_or(host, |(h, _)| h);
    if !matches!(host, "127.0.0.1" | "localhost") {
        return (StatusCode::FORBIDDEN, "bad host").into_response();
    }
    if req.method() == Method::OPTIONS || req.uri().path() == "/" || req.headers().contains_key(API_HEADER) {
        next.run(req).await
    } else {
        (StatusCode::FORBIDDEN, "missing x-downloaderx header").into_response()
    }
}

async fn ui() -> Html<&'static str> {
    Html(include_str!("ui.html"))
}

async fn status(State(m): State<AppState>) -> Json<serde_json::Value> {
    Json(json!({ "ok": true, "version": env!("CARGO_PKG_VERSION"), "ffmpeg": m.ffmpeg, "ytdlp": m.ytdlp }))
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
}

/// Work out what a URL is and which qualities can be downloaded from it.
async fn probe(State(m): State<AppState>, Json(r): Json<ProbeReq>) -> Response {
    match probe_inner(&m, &r).await {
        Ok(v) => Json(v).into_response(),
        Err(e) => (StatusCode::BAD_GATEWAY, Json(json!({ "error": format!("{e:#}") }))).into_response(),
    }
}

async fn probe_inner(m: &Manager, r: &ProbeReq) -> anyhow::Result<serde_json::Value> {
    let client = reqwest::Client::new();
    let mut headers = HeaderMap::new();
    for (k, v) in &r.headers {
        if let (Ok(k), Ok(v)) = (HeaderName::try_from(k.as_str()), v.parse()) {
            headers.insert(k, v);
        }
    }
    let lower = r.url.to_ascii_lowercase();
    let looks_hls = lower.split('?').next().unwrap_or("").ends_with(".m3u8");

    if !looks_hls {
        let p = engine::probe(&client, &r.url, &headers).await?;
        let ct = p.content_type.clone().unwrap_or_default();
        if ct.contains("text/html") {
            if !m.ytdlp {
                anyhow::bail!("this is a web page; install yt-dlp to download videos from it");
            }
            let (title, qs) = ytdlp::formats(&r.url).await?;
            let choices: Vec<Choice> = qs
                .into_iter()
                .map(|q| Choice { label: q.label, kind: json!({ "type": "ytdlp", "format": q.format }), url: r.url.clone(), size: q.size })
                .collect();
            return Ok(json!({ "title": title, "choices": choices }));
        }
        if !ct.contains("mpegurl") {
            let label = p.filename.clone().unwrap_or("file".into());
            return Ok(json!({
                "title": p.filename,
                "choices": [Choice { label, kind: json!({ "type": "direct" }), url: r.url.clone(), size: p.size }],
            }));
        }
    }

    let (text, base) = hls::fetch_text(&client, &r.url, &headers).await?;
    let choices: Vec<Choice> = if hls::is_master(&text) {
        hls::parse_master(&text, &base)
            .into_iter()
            .map(|v| Choice { label: v.label, kind: json!({ "type": "hls", "audio": v.audio }), url: v.url, size: None })
            .collect()
    } else {
        vec![Choice { label: "Stream".into(), kind: json!({ "type": "hls", "audio": null }), url: r.url.clone(), size: None }]
    };
    Ok(json!({ "title": null, "choices": choices }))
}
