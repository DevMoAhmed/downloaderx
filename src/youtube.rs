//! YouTube without yt-dlp: ask YouTube's player API for the stream URLs the way its apps
//! do, then list the qualities. HD comes as a video-only and an audio-only file; they are
//! joined after download.
//!
//! The iOS and Android VR apps get plain URLs. When YouTube blocks those, the Android and
//! TV apps are used instead: their URLs carry challenges (`n`, and sometimes a signature)
//! that only YouTube's player code can answer. The browser extension runs that code in a
//! sandbox (yt-dlp's EJS solver) and sends the answers back; see [`Solver`].

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use reqwest::Client;
use serde::Deserialize;
use serde_json::{Value, json};

/// The app whose player API answers are used. Its stream URLs expect its user agent.
struct App {
    name: &'static str,
    id: &'static str,
    version: &'static str,
    agent: &'static str,
    extra: fn() -> Value,
}

const APPS: [App; 2] = [
    App {
        name: "IOS",
        id: "5",
        version: "20.10.4",
        agent: "com.google.ios.youtube/20.10.4 (iPhone16,2; U; CPU iOS 18_3_2 like Mac OS X;)",
        extra: || json!({ "deviceMake": "Apple", "deviceModel": "iPhone16,2", "osName": "iPhone", "osVersion": "18.3.2.22D82" }),
    },
    App {
        name: "ANDROID_VR",
        id: "28",
        version: "1.65.10",
        agent: "com.google.android.apps.youtube.vr.oculus/1.65.10 (Linux; U; Android 12L; eureka-user Build/SQ3A.220605.009.A1) gzip",
        extra: || json!({ "deviceMake": "Oculus", "deviceModel": "Quest 3", "androidSdkVersion": 32, "osName": "Android", "osVersion": "12L" }),
    },
];

/// Apps whose stream URLs need the player challenges solved (by the extension).
const SOLVED_APPS: [App; 2] = [
    App {
        name: "ANDROID",
        id: "3",
        version: "20.10.38",
        agent: "com.google.android.youtube/20.10.38 (Linux; U; Android 11) gzip",
        extra: || json!({ "androidSdkVersion": 30, "osName": "Android", "osVersion": "11" }),
    },
    App {
        name: "TVHTML5_SIMPLY",
        id: "75",
        version: "1.0",
        agent: "Mozilla/5.0 (ChromiumStylePlatform) Cobalt/Version",
        extra: || json!({}),
    },
];

const BLOCKED: &str = "YouTube is blocking downloads of this video right now: its stream links stop after the first part";
const USE_EXTENSION: &str = "YouTube is blocking the simple way for this video: download it from the YouTube page in Chrome or Edge with the DownloaderX button";

/// The extension's side of the challenge exchange, sent with every YouTube probe it makes:
/// empty at first, then `sts` (from the current player code), then the answers.
#[derive(Deserialize, Default, Debug)]
pub struct Solver {
    /// signatureTimestamp of YouTube's current player code.
    pub sts: Option<u64>,
    /// A YouTube visitor id and a BotGuard session token (PO token) bound to it, minted by
    /// the extension. The TV app's links play in full with them when YouTube cuts others off.
    pub visitor: Option<String>,
    pub pot: Option<String>,
    /// Which answered challenge set this is.
    pub token: Option<String>,
    #[serde(default)]
    pub n: HashMap<String, String>,
    #[serde(default)]
    pub sig: HashMap<String, String>,
}

pub enum Outcome {
    Ready(Video),
    /// Send the player code's signatureTimestamp.
    NeedPlayer,
    /// Solve these and send the answers with `token`.
    Challenges { token: String, n: Vec<String>, sig: Vec<String> },
}

/// Player answers (and the session token to add to their links) waiting for their
/// challenges to be solved, by token.
type Pending = (Instant, Vec<(usize, Value)>, Option<String>);
static PENDING: LazyLock<Mutex<HashMap<String, Pending>>> = LazyLock::new(Default::default);

/// The video id of a YouTube watch, short, embed or youtu.be URL.
pub fn video_id(url: &str) -> Option<String> {
    let u = url::Url::parse(url).ok()?;
    let host = u.host_str()?.trim_start_matches("www.").trim_start_matches("m.").trim_start_matches("music.");
    let id = match host {
        "youtu.be" => u.path_segments()?.next()?.to_string(),
        "youtube.com" | "youtube-nocookie.com" => {
            let mut seg = u.path_segments()?;
            match seg.next()? {
                "watch" => u.query_pairs().find(|(k, _)| k == "v")?.1.into_owned(),
                "shorts" | "embed" | "live" | "v" => seg.next()?.to_string(),
                _ => return None,
            }
        }
        _ => return None,
    };
    (id.len() == 11 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')).then_some(id)
}

pub struct Choice {
    pub label: String,
    /// "direct" or "merge".
    pub kind: Value,
    pub url: String,
    pub size: Option<u64>,
}

pub struct Video {
    pub title: String,
    pub choices: Vec<Choice>,
    /// Headers the stream URLs need.
    pub headers: Vec<(String, String)>,
}

/// Qualities for video `id`. `solver` is present when the caller (the extension) can solve
/// player challenges.
pub async fn probe(client: &Client, id: &str, solver: Option<&Solver>) -> Result<Outcome> {
    if let Some(s) = solver {
        if let Some(token) = &s.token {
            return answered(client, token, s).await.map(Outcome::Ready);
        }
        if let Some(sts) = s.sts {
            return challenges(client, id, sts, s).await;
        }
    }
    let mut last = None;
    // DOWNLOADERX_YT_SOLVER_ONLY skips the plain-URL apps (to test the challenge path).
    if std::env::var_os("DOWNLOADERX_YT_SOLVER_ONLY").is_none() {
        for app in &APPS {
            match from_app(client, app, id).await {
                Ok(v) => return Ok(Outcome::Ready(v)),
                Err(e) => last = Some(e),
            }
        }
    }
    match (solver, last) {
        (Some(_), _) => Ok(Outcome::NeedPlayer),
        (None, Some(e)) if e.to_string() == BLOCKED => bail!(USE_EXTENSION),
        (None, Some(e)) => Err(e),
        (None, None) => bail!(USE_EXTENSION),
    }
}

/// Ask the challenge apps, keep their answers, and return the challenges to solve.
async fn challenges(client: &Client, id: &str, sts: u64, s: &Solver) -> Result<Outcome> {
    let mut answers = Vec::new();
    let mut last = None;
    for (i, app) in SOLVED_APPS.iter().enumerate() {
        // The token is bound to the visitor id, so the player is asked as that visitor.
        let resp = match &s.visitor {
            Some(v) => player(client, app, id, Some(v), Some(sts)).await,
            None => ask(client, app, id, Some(sts)).await,
        };
        match resp {
            Ok(resp) if resp["playabilityStatus"]["status"] == "OK" => answers.push((i, resp)),
            Ok(resp) => last = Some(parse(&resp).err().map(|e| e.to_string()).unwrap_or_default()),
            Err(e) => last = Some(e.to_string()),
        }
    }
    if answers.is_empty() {
        bail!("{}", last.unwrap_or(BLOCKED.into()));
    }
    let (mut n, mut sig) = (Vec::new(), Vec::new());
    for (_, resp) in &answers {
        for f in formats(resp) {
            let (url, s, _) = stream_url(f);
            if let Some(v) = url.as_deref().and_then(|u| query(u, "n")) {
                n.push(v);
            }
            sig.extend(s);
        }
    }
    n.sort();
    n.dedup();
    sig.sort();
    sig.dedup();
    let token = format!("{id}-{}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    let mut pending = PENDING.lock().unwrap();
    pending.retain(|_, (at, _, _)| at.elapsed() < Duration::from_secs(600));
    pending.insert(token.clone(), (Instant::now(), answers, s.pot.clone()));
    Ok(Outcome::Challenges { token, n, sig })
}

/// Apply the solved challenges to the kept answers and offer the first app whose links work.
async fn answered(client: &Client, token: &str, s: &Solver) -> Result<Video> {
    let (_, answers, pot) = PENDING.lock().unwrap().remove(token).context("the YouTube answer expired, try again")?;
    for (i, mut resp) in answers {
        let app = &SOLVED_APPS[i];
        for key in ["formats", "adaptiveFormats"] {
            let Some(list) = resp["streamingData"][key].as_array_mut() else { continue };
            for f in list {
                let (url, sig, sp) = stream_url(f);
                let Some(url) = url else { continue };
                let Ok(mut u) = url::Url::parse(&url) else { continue };
                let pairs: Vec<(String, String)> = u
                    .query_pairs()
                    .map(|(k, v)| {
                        let v = if k == "n" { s.n.get(v.as_ref()).cloned().unwrap_or_else(|| v.into_owned()) } else { v.into_owned() };
                        (k.into_owned(), v)
                    })
                    .collect();
                u.query_pairs_mut().clear().extend_pairs(pairs);
                if let Some(solved) = sig.and_then(|x| s.sig.get(&x)) {
                    u.query_pairs_mut().append_pair(&sp, solved);
                }
                f["url"] = Value::String(u.to_string());
            }
        }
        let Ok(video) = parse(&resp) else { continue };
        let headers = vec![("User-Agent".to_string(), app.agent.to_string())];
        if links_work(client, app, &video).await {
            return Ok(Video { headers, ..video });
        }
        // Links YouTube cuts off after the first part play in full with the session token.
        if let Some(pot) = &pot {
            let video = with_pot(video, pot);
            if links_work(client, app, &video).await {
                return Ok(Video { headers, ..video });
            }
        }
    }
    bail!(BLOCKED)
}

/// Add the session token to every stream link of `video`.
fn with_pot(mut video: Video, pot: &str) -> Video {
    let add = |u: &str| -> String {
        let mut url = match url::Url::parse(u) {
            Ok(url) => url,
            Err(_) => return u.to_string(),
        };
        url.query_pairs_mut().append_pair("pot", pot);
        url.to_string()
    };
    for c in &mut video.choices {
        c.url = add(&c.url);
        if let Some(a) = c.kind["audio"].as_str().map(add) {
            c.kind["audio"] = Value::String(a);
        }
    }
    video
}

fn formats(resp: &Value) -> impl Iterator<Item = &Value> {
    ["formats", "adaptiveFormats"].into_iter().flat_map(|k| resp["streamingData"][k].as_array().into_iter().flatten())
}

/// A format's URL, plus its scrambled signature and the parameter the answer goes in.
fn stream_url(f: &Value) -> (Option<String>, Option<String>, String) {
    if let Some(u) = f["url"].as_str() {
        return (Some(u.to_string()), None, String::new());
    }
    let Some(c) = f["signatureCipher"].as_str() else { return (None, None, String::new()) };
    let q: HashMap<String, String> = url::form_urlencoded::parse(c.as_bytes()).into_owned().collect();
    (q.get("url").cloned(), q.get("s").cloned(), q.get("sp").cloned().unwrap_or("signature".into()))
}

fn query(url: &str, key: &str) -> Option<String> {
    url::Url::parse(url).ok()?.query_pairs().find(|(k, _)| k == key).map(|(_, v)| v.into_owned())
}

/// Whether the top choice's video and audio links read to the end.
async fn links_work(client: &Client, app: &App, video: &Video) -> bool {
    let Some(first) = video.choices.first() else { return false };
    let mut urls = vec![first.url.as_str()];
    urls.extend(first.kind["audio"].as_str());
    for u in urls {
        if !reads_to_end(client, app, u).await {
            return false;
        }
    }
    true
}

/// Qualities from one app's answer. YouTube may hand out stream links that stop working
/// after the first part (they need a token only its real apps can make), so the links are
/// tried before they're offered; the HLS stream is the fallback when they don't work.
async fn from_app(client: &Client, app: &App, id: &str) -> Result<Video> {
    let resp = ask(client, app, id, None).await?;
    let headers = vec![("User-Agent".to_string(), app.agent.to_string())];
    let video = parse(&resp)?;
    if links_work(client, app, &video).await {
        return Ok(Video { headers, ..video });
    }
    let Some(master) = resp["streamingData"]["hlsManifestUrl"].as_str() else { bail!(BLOCKED) };
    let choices = hls_choices(client, app, master).await?;
    Ok(Video { title: video.title, choices, headers })
}

/// The player answer, asked again with the visitor id it returned when YouTube says
/// "Sign in to confirm you're not a bot" (that usually works).
async fn ask(client: &Client, app: &App, id: &str, sts: Option<u64>) -> Result<Value> {
    let resp = player(client, app, id, None, sts).await?;
    if resp["playabilityStatus"]["status"] != "OK" {
        if let Some(visitor) = resp["responseContext"]["visitorData"].as_str().map(str::to_string) {
            return player(client, app, id, Some(&visitor), sts).await;
        }
    }
    Ok(resp)
}

async fn player(client: &Client, app: &App, id: &str, visitor: Option<&str>, sts: Option<u64>) -> Result<Value> {
    let mut ctx = json!({ "clientName": app.name, "clientVersion": app.version, "hl": "en", "gl": "US" });
    if let (Some(c), Value::Object(extra)) = (ctx.as_object_mut(), (app.extra)()) {
        c.extend(extra);
        if let Some(v) = visitor {
            c.insert("visitorData".into(), v.into());
        }
    }
    let mut body = json!({ "context": { "client": ctx }, "videoId": id, "contentCheckOk": true, "racyCheckOk": true });
    if let Some(sts) = sts {
        body["playbackContext"] = json!({ "contentPlaybackContext": { "html5Preference": "HTML5_PREF_WANTS", "signatureTimestamp": sts } });
    }
    let mut req = client
        .post("https://www.youtube.com/youtubei/v1/player?prettyPrint=false")
        .header("User-Agent", app.agent)
        .header("X-YouTube-Client-Name", app.id)
        .header("X-YouTube-Client-Version", app.version)
        .header("Origin", "https://www.youtube.com")
        .header("Content-Type", "application/json");
    if let Some(v) = visitor {
        req = req.header("X-Goog-Visitor-Id", v);
    }
    let bytes = req.body(body.to_string()).send().await?.error_for_status()?.bytes().await?;
    Ok(serde_json::from_slice(&bytes)?)
}

/// Whether a stream link still answers at its last byte (blocked links only serve the start).
async fn reads_to_end(client: &Client, app: &App, url: &str) -> bool {
    let len = url::Url::parse(url).ok().and_then(|u| u.query_pairs().find(|(k, _)| k == "clen").and_then(|(_, v)| v.parse::<u64>().ok()));
    let last = len.map_or(1_000_000, |l| l.saturating_sub(1));
    let r = client.get(url).header("User-Agent", app.agent).header("Range", format!("bytes={last}-{last}")).send().await;
    r.is_ok_and(|r| r.status() == reqwest::StatusCode::PARTIAL_CONTENT || r.status().is_success())
}

/// Qualities from YouTube's HLS stream: one per height, H.264 first, with its audio.
async fn hls_choices(client: &Client, app: &App, master: &str) -> Result<Vec<Choice>> {
    let resp = client.get(master).header("User-Agent", app.agent).send().await?.error_for_status()?;
    let base = resp.url().clone();
    let text = resp.text().await?;
    let mut variants = crate::hls::parse_master(&text, &base);
    variants.sort_by_key(|v| std::cmp::Reverse((v.height.unwrap_or(0), v.codecs.contains("avc1"), v.bandwidth)));
    let mut seen = Vec::new();
    let mut out = Vec::new();
    for v in variants {
        let h = v.height.unwrap_or(0);
        if seen.contains(&h) {
            continue;
        }
        seen.push(h);
        let codec = if v.codecs.contains("avc1") { "" } else if v.codecs.contains("vp09") { " · VP9" } else if v.codecs.contains("av01") { " · AV1" } else { "" };
        out.push(Choice { label: format!("{h}p{codec}"), kind: json!({ "type": "hls", "audio": v.audio }), url: v.url, size: None });
    }
    if out.is_empty() {
        bail!(BLOCKED);
    }
    Ok(out)
}

/// Turn a player response into download choices: each height as .mp4 (H.264 preferred,
/// then AV1; VP9 needs WebM and is skipped), the small files that already have sound,
/// and the audio alone.
pub fn parse(resp: &Value) -> Result<Video> {
    let status = resp["playabilityStatus"]["status"].as_str().unwrap_or("");
    if status != "OK" {
        let why = resp["playabilityStatus"]["reason"].as_str().unwrap_or(status);
        bail!("YouTube: {}", if why.is_empty() { "video unavailable" } else { why });
    }
    let title = resp["videoDetails"]["title"].as_str().unwrap_or("video").to_string();
    let data = &resp["streamingData"];
    let list = |k: &str| data[k].as_array().cloned().unwrap_or_default();
    let with_url = |f: &Value| f["url"].as_str().is_some();
    let size = |f: &Value| f["contentLength"].as_str().and_then(|s| s.parse::<u64>().ok());
    let mime = |f: &Value| f["mimeType"].as_str().unwrap_or("").to_string();

    let adaptive: Vec<Value> = list("adaptiveFormats").into_iter().filter(with_url).collect();
    let audio = adaptive
        .iter()
        .filter(|f| mime(f).starts_with("audio/mp4"))
        .max_by_key(|f| f["bitrate"].as_u64().unwrap_or(0))
        .cloned();

    let mut choices = Vec::new();
    if let Some(a) = &audio {
        let mut videos: Vec<&Value> = adaptive.iter().filter(|f| mime(f).starts_with("video/mp4")).collect();
        // Best first: height, then H.264 over AV1, then frame rate and bitrate.
        videos.sort_by_key(|f| {
            let h = f["height"].as_u64().unwrap_or(0);
            let avc = mime(f).contains("avc1");
            std::cmp::Reverse((h, avc, f["fps"].as_u64().unwrap_or(0), f["bitrate"].as_u64().unwrap_or(0)))
        });
        let mut heights = Vec::new();
        for v in videos {
            let h = v["height"].as_u64().unwrap_or(0);
            if heights.contains(&h) {
                continue;
            }
            heights.push(h);
            let label = v["qualityLabel"].as_str().map(str::to_string).unwrap_or(format!("{h}p"));
            let codec = if mime(v).contains("avc1") { "" } else { " · AV1" };
            choices.push(Choice {
                label: format!("{label}{codec}"),
                kind: json!({ "type": "merge", "audio": a["url"] }),
                url: v["url"].as_str().unwrap().to_string(),
                size: size(v).zip(size(a)).map(|(x, y)| x + y),
            });
        }
    }
    for f in list("formats").into_iter().filter(with_url) {
        let label = format!("{} · single file", f["qualityLabel"].as_str().unwrap_or("video"));
        if !choices.iter().any(|c| c.label == label) {
            choices.push(Choice { label, kind: json!({ "type": "direct" }), url: f["url"].as_str().unwrap().to_string(), size: size(&f) });
        }
    }
    if let Some(a) = &audio {
        let kbps = a["bitrate"].as_u64().unwrap_or(0) / 1000;
        choices.push(Choice { label: format!("Audio only · {kbps} kbps"), kind: json!({ "type": "direct" }), url: a["url"].as_str().unwrap().to_string(), size: size(a) });
    }
    if choices.is_empty() {
        bail!("YouTube didn't give downloadable streams for this video");
    }
    Ok(Video { title, choices, headers: vec![] })
}

/// File name for a choice: the video title plus quality.
pub fn name(title: &str, c: &Choice) -> Option<String> {
    let q = c.label.split(" · ").next().unwrap_or("");
    let ext = if c.label.starts_with("Audio only") { "m4a" } else { "mp4" };
    crate::engine::sanitize(&format!("{title} ({q}).{ext}")).context("bad name").ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_from_urls() {
        for u in ["https://www.youtube.com/watch?v=jNQXAC9IVRw&t=1", "https://youtu.be/jNQXAC9IVRw", "https://m.youtube.com/shorts/jNQXAC9IVRw", "https://www.youtube-nocookie.com/embed/jNQXAC9IVRw"] {
            assert_eq!(video_id(u).as_deref(), Some("jNQXAC9IVRw"), "{u}");
        }
        assert_eq!(video_id("https://www.youtube.com/feed/trending"), None);
        assert_eq!(video_id("https://example.com/watch?v=jNQXAC9IVRw"), None);
    }

    #[test]
    fn choices_from_player_response() {
        let r = json!({
            "playabilityStatus": { "status": "OK" },
            "videoDetails": { "title": "Me at the zoo" },
            "streamingData": {
                "formats": [{ "itag": 18, "url": "https://g/18", "mimeType": "video/mp4; codecs=\"avc1.42001E, mp4a.40.2\"", "qualityLabel": "240p", "contentLength": "500" }],
                "adaptiveFormats": [
                    { "itag": 160, "url": "https://g/160", "mimeType": "video/mp4; codecs=\"avc1.4d400c\"", "height": 144, "qualityLabel": "144p", "bitrate": 100, "contentLength": "100" },
                    { "itag": 394, "url": "https://g/394", "mimeType": "video/mp4; codecs=\"av01.0.00M.08\"", "height": 144, "qualityLabel": "144p", "bitrate": 90 },
                    { "itag": 133, "url": "https://g/133", "mimeType": "video/mp4; codecs=\"avc1.4d4015\"", "height": 240, "qualityLabel": "240p", "bitrate": 200, "contentLength": "200" },
                    { "itag": 278, "url": "https://g/278", "mimeType": "video/webm; codecs=\"vp9\"", "height": 480, "bitrate": 300 },
                    { "itag": 139, "url": "https://g/139", "mimeType": "audio/mp4; codecs=\"mp4a.40.5\"", "bitrate": 48000, "contentLength": "30" },
                    { "itag": 140, "url": "https://g/140", "mimeType": "audio/mp4; codecs=\"mp4a.40.2\"", "bitrate": 128000, "contentLength": "50" },
                    { "itag": 251, "mimeType": "audio/webm", "signatureCipher": "s=x", "bitrate": 160000 }
                ]
            }
        });
        let v = parse(&r).unwrap();
        let got: Vec<(String, String)> = v.choices.iter().map(|c| (c.label.clone(), c.url.clone())).collect();
        assert_eq!(got, [
            ("240p".to_string(), "https://g/133".to_string()),
            ("144p".into(), "https://g/160".into()),
            ("240p · single file".into(), "https://g/18".into()),
            ("Audio only · 128 kbps".into(), "https://g/140".into()),
        ]);
        assert_eq!(v.choices[0].kind["audio"], "https://g/140");
        assert_eq!(v.choices[0].size, Some(250));
        assert_eq!(name(&v.title, &v.choices[0]).as_deref(), Some("Me at the zoo (240p).mp4"));
    }

    #[test]
    fn unplayable_reason() {
        let r = json!({ "playabilityStatus": { "status": "LOGIN_REQUIRED", "reason": "Sign in to confirm your age" } });
        assert_eq!(parse(&r).err().unwrap().to_string(), "YouTube: Sign in to confirm your age");
    }
}
