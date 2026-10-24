//! YouTube without yt-dlp: ask YouTube's player API for the stream URLs the way its
//! Android VR app does (plain URLs, no signature to decipher), then list the qualities.
//! HD comes as a video-only and an audio-only file; they are joined after download.

use anyhow::{Context, Result, bail};
use reqwest::Client;
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

const BLOCKED: &str = "YouTube is blocking downloads of this video right now: its stream links stop after the first part";

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

pub async fn probe(client: &Client, id: &str) -> Result<Video> {
    let mut last = None;
    for app in &APPS {
        match from_app(client, app, id).await {
            Ok(v) => return Ok(v),
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap())
}

/// Qualities from one app's answer. YouTube may hand out stream links that stop working
/// after the first part (they need a token only its real apps can make), so the links are
/// tried before they're offered; the HLS stream is the fallback when they don't work.
async fn from_app(client: &Client, app: &App, id: &str) -> Result<Video> {
    let mut resp = player(client, app, id, None).await?;
    // "Sign in to confirm you're not a bot": asking again with the visitor id it returned
    // usually works.
    if resp["playabilityStatus"]["status"] != "OK" {
        if let Some(visitor) = resp["responseContext"]["visitorData"].as_str().map(str::to_string) {
            resp = player(client, app, id, Some(&visitor)).await?;
        }
    }
    let headers = vec![("User-Agent".to_string(), app.agent.to_string())];
    let video = parse(&resp)?;
    if let Some(first) = video.choices.first() {
        let mut urls = vec![first.url.as_str()];
        urls.extend(first.kind["audio"].as_str());
        let mut ok = true;
        for u in urls {
            ok &= reads_to_end(client, app, u).await;
        }
        if ok {
            return Ok(Video { headers, ..video });
        }
    }
    let Some(master) = resp["streamingData"]["hlsManifestUrl"].as_str() else { bail!(BLOCKED) };
    let choices = hls_choices(client, app, master).await?;
    Ok(Video { title: video.title, choices, headers })
}

async fn player(client: &Client, app: &App, id: &str, visitor: Option<&str>) -> Result<Value> {
    let mut ctx = json!({ "clientName": app.name, "clientVersion": app.version, "hl": "en", "gl": "US" });
    if let (Some(c), Value::Object(extra)) = (ctx.as_object_mut(), (app.extra)()) {
        c.extend(extra);
        if let Some(v) = visitor {
            c.insert("visitorData".into(), v.into());
        }
    }
    let body = json!({ "context": { "client": ctx }, "videoId": id, "contentCheckOk": true, "racyCheckOk": true });
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
        let label = f["qualityLabel"].as_str().unwrap_or("video");
        choices.push(Choice { label: format!("{label} · single file"), kind: json!({ "type": "direct" }), url: f["url"].as_str().unwrap().to_string(), size: size(&f) });
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
