//! HLS (.m3u8) support: list qualities, download segments in parallel, decrypt AES-128, join.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use aes::cipher::{block_padding::Pkcs7, BlockModeDecrypt, KeyIvInit};
use anyhow::{bail, Context, Result};
use futures_util::{stream, StreamExt, TryStreamExt};
use reqwest::header::{HeaderMap, RANGE};
use reqwest::Client;
use serde::Serialize;
use tokio::fs;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{with_suffix, Cancelled, Progress};
use crate::limiter::Limiter;

#[derive(Serialize, Clone, Debug)]
pub struct Variant {
    pub label: String,
    pub url: String,
    pub bandwidth: u64,
    pub height: Option<u32>,
    /// Separate audio rendition, if the variant has one.
    pub audio: Option<String>,
}

/// One piece of a stream to fetch (HLS and DASH).
pub struct Seg {
    pub url: Url,
    pub range: Option<(u64, u64)>, // offset, len
    key: Option<Key>,
}

impl std::fmt::Debug for Seg {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        write!(f, "{}", self.url)
    }
}

impl Seg {
    pub fn new(url: Url, range: Option<(u64, u64)>) -> Self {
        Self { url, range, key: None }
    }
}

#[derive(Clone)]
struct Key {
    uri: Url,
    iv: [u8; 16],
}

/// Parse `KEY=value,KEY="quoted, value"` attribute lists.
fn attrs(s: &str) -> HashMap<String, String> {
    let mut out = HashMap::new();
    let mut rest = s;
    while !rest.is_empty() {
        let Some((k, v)) = rest.split_once('=') else { break };
        let (val, next) = if let Some(q) = v.strip_prefix('"') {
            let end = q.find('"').unwrap_or(q.len());
            (&q[..end], q.get(end + 1..).unwrap_or("").trim_start_matches(','))
        } else {
            let end = v.find(',').unwrap_or(v.len());
            (&v[..end], v.get(end + 1..).unwrap_or(""))
        };
        out.insert(k.trim().to_string(), val.to_string());
        rest = next;
    }
    out
}

pub async fn fetch_text(client: &Client, url: &str, headers: &HeaderMap) -> Result<(String, Url)> {
    let resp = client.get(url).headers(headers.clone()).send().await?.error_for_status()?;
    let final_url = resp.url().clone();
    Ok((resp.text().await?, final_url))
}

pub fn is_master(text: &str) -> bool {
    text.contains("#EXT-X-STREAM-INF")
}

pub fn parse_master(text: &str, base: &Url) -> Vec<Variant> {
    let mut audio_groups: HashMap<String, String> = HashMap::new();
    for line in text.lines() {
        if let Some(a) = line.strip_prefix("#EXT-X-MEDIA:") {
            let a = attrs(a);
            if a.get("TYPE").map(String::as_str) == Some("AUDIO") {
                if let (Some(g), Some(u)) = (a.get("GROUP-ID"), a.get("URI")) {
                    // Prefer the DEFAULT rendition of each group.
                    if !audio_groups.contains_key(g) || a.get("DEFAULT").map(String::as_str) == Some("YES") {
                        if let Ok(u) = base.join(u) {
                            audio_groups.insert(g.clone(), u.to_string());
                        }
                    }
                }
            }
        }
    }
    let mut out = Vec::new();
    let mut lines = text.lines().map(str::trim);
    while let Some(line) = lines.next() {
        let Some(a) = line.strip_prefix("#EXT-X-STREAM-INF:") else { continue };
        let a = attrs(a);
        let Some(uri) = lines.by_ref().find(|l| !l.is_empty() && !l.starts_with('#')) else { break };
        let Ok(url) = base.join(uri) else { continue };
        let bandwidth = a.get("AVERAGE-BANDWIDTH").or(a.get("BANDWIDTH")).and_then(|b| b.parse().ok()).unwrap_or(0);
        let height = a.get("RESOLUTION").and_then(|r| r.split('x').nth(1)?.parse().ok());
        // Low-bitrate (often audio-only) variants would all show as "0.0 Mbps".
        let rate = if bandwidth >= 1_000_000 {
            format!("{:.1} Mbps", bandwidth as f64 / 1e6)
        } else {
            format!("{} kbps", bandwidth / 1000)
        };
        let label = match height {
            Some(h) => format!("{h}p · {rate}"),
            None => rate,
        };
        let audio = a.get("AUDIO").and_then(|g| audio_groups.get(g)).cloned();
        out.push(Variant { label, url: url.to_string(), bandwidth, height, audio });
    }
    out.sort_by(|a, b| b.bandwidth.cmp(&a.bandwidth));
    out.dedup_by(|a, b| a.url == b.url);
    out
}

fn parse_media(text: &str, base: &Url) -> Result<(Option<Seg>, Vec<Seg>)> {
    if text.contains("METHOD=SAMPLE-AES") || text.contains("KEYFORMAT=\"com.") {
        bail!("this stream is DRM protected and can't be downloaded");
    }
    if !text.contains("#EXT-X-ENDLIST") {
        bail!("live streams are not supported yet");
    }
    let mut seq: u64 = 0;
    let mut key: Option<(Url, Option<[u8; 16]>)> = None;
    let mut init = None;
    let mut segs = Vec::new();
    let mut next_range: Option<(u64, u64)> = None;
    let mut last_end = 0u64;
    let parse_range = |v: &str, last_end: u64| -> Option<(u64, u64)> {
        let (len, off) = v.split_once('@').map(|(l, o)| (l, o.parse().ok())).unwrap_or((v, None));
        let len: u64 = len.trim().parse().ok()?;
        Some((off.unwrap_or(last_end), len))
    };
    for line in text.lines().map(str::trim) {
        if let Some(v) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            seq = v.parse().unwrap_or(0);
        } else if let Some(v) = line.strip_prefix("#EXT-X-KEY:") {
            let a = attrs(v);
            key = match a.get("METHOD").map(String::as_str) {
                Some("NONE") | None => None,
                Some("AES-128") => {
                    let uri = base.join(a.get("URI").context("key without URI")?)?;
                    let iv = a.get("IV").map(|iv| parse_iv(iv)).transpose()?;
                    Some((uri, iv))
                }
                Some(m) => bail!("unsupported encryption {m}"),
            };
        } else if let Some(v) = line.strip_prefix("#EXT-X-MAP:") {
            let a = attrs(v);
            let url = base.join(a.get("URI").context("map without URI")?)?;
            let range = a.get("BYTERANGE").and_then(|r| parse_range(r, 0));
            init = Some(Seg { url, range, key: None });
        } else if let Some(v) = line.strip_prefix("#EXT-X-BYTERANGE:") {
            next_range = parse_range(v, last_end);
        } else if !line.is_empty() && !line.starts_with('#') {
            let range = next_range.take();
            if let Some((off, len)) = range {
                last_end = off + len;
            }
            let key = key.as_ref().map(|(uri, iv)| Key {
                uri: uri.clone(),
                iv: iv.unwrap_or_else(|| (seq as u128).to_be_bytes()),
            });
            segs.push(Seg { url: base.join(line)?, range, key });
            seq += 1;
        }
    }
    if segs.is_empty() {
        bail!("playlist has no segments");
    }
    Ok((init, segs))
}

fn parse_iv(s: &str) -> Result<[u8; 16]> {
    let h = s.trim_start_matches("0x").trim_start_matches("0X");
    let v = u128::from_str_radix(h, 16).context("bad IV")?;
    Ok(v.to_be_bytes())
}

async fn fetch_bytes(client: &Client, seg: &Seg, headers: &HeaderMap, retries: u32) -> Result<Vec<u8>> {
    let mut attempt = 0;
    loop {
        let mut req = client.get(seg.url.clone()).headers(headers.clone());
        if let Some((off, len)) = seg.range {
            req = req.header(RANGE, format!("bytes={off}-{}", off + len - 1));
        }
        let r = async {
            let resp = req.send().await?.error_for_status()?;
            let whole = resp.status() != reqwest::StatusCode::PARTIAL_CONTENT;
            let body = resp.bytes().await?;
            // A server that ignores Range sends the whole file: cut out the part we asked for.
            match seg.range {
                Some((off, len)) if whole => {
                    let end = (off + len).min(body.len() as u64) as usize;
                    Ok::<_, anyhow::Error>(body.get(off as usize..end).context("byte range past the end of the file")?.to_vec())
                }
                _ => Ok(body.to_vec()),
            }
        }
        .await;
        match r {
            Ok(b) => return Ok(b),
            Err(_) if attempt < retries => {
                attempt += 1;
                tokio::time::sleep(Duration::from_millis(500 * 2u64.pow(attempt))).await;
            }
            Err(e) => return Err(e.context(format!("segment {}", seg.url))),
        }
    }
}

/// Download one media playlist into `out`, returning whether it was fMP4.
async fn download_track(
    client: &Client,
    url: &str,
    out: &Path,
    headers: &HeaderMap,
    conns: usize,
    progress: &Progress,
    limiter: &Limiter,
) -> Result<bool> {
    let (text, base) = fetch_text(client, url, headers).await?;
    let (text, base) = if is_master(&text) {
        // Given a master playlist directly: take the best quality.
        let v = parse_master(&text, &base).into_iter().next().context("empty master playlist")?;
        fetch_text(client, &v.url, headers).await?
    } else {
        (text, base)
    };
    let (init, segs) = parse_media(&text, &base)?;
    download_segments(client, init.as_ref(), &segs, out, headers, conns, progress, limiter).await?;
    Ok(init.is_some())
}

/// Fetch `segs` (decrypting AES-128 ones) in parallel and write them in order to `out`,
/// after the `init` segment if there is one.
#[allow(clippy::too_many_arguments)]
pub async fn download_segments(
    client: &Client,
    init: Option<&Seg>,
    segs: &[Seg],
    out: &Path,
    headers: &HeaderMap,
    conns: usize,
    progress: &Progress,
    limiter: &Limiter,
) -> Result<()> {
    let mut keys: HashMap<Url, [u8; 16]> = HashMap::new();
    for k in segs.iter().filter_map(|s| s.key.as_ref()) {
        if !keys.contains_key(&k.uri) {
            let b = client.get(k.uri.clone()).headers(headers.clone()).send().await?.error_for_status()?.bytes().await?;
            let key: [u8; 16] = b.as_ref().try_into().context("AES key must be 16 bytes")?;
            keys.insert(k.uri.clone(), key);
        }
    }

    let mut file = fs::File::create(out).await?;
    if let Some(init) = init {
        file.write_all(&fetch_bytes(client, init, headers, 5).await?).await?;
    }
    let count = segs.len() as u64;
    let start_bytes = progress.downloaded.load(Relaxed);
    let mut done = 0u64;
    let keys = &keys;
    // Futures are built in a plain loop: a closure here trips the "Send is not general enough" bug.
    let mut futs = Vec::with_capacity(segs.len());
    for seg in segs {
        futs.push(fetch_segment(client, seg, headers, keys, limiter));
    }
    let mut chunks = stream::iter(futs).buffered(conns.max(1));
    while let Some(data) = chunks.try_next().await? {
        file.write_all(&data).await?;
        progress.add(data.len() as u64);
        done += 1;
        // Total is unknown up front; estimate it from the average segment size so far.
        let got = progress.downloaded.load(Relaxed) - start_bytes;
        progress.total.store(start_bytes + got * count / done, Relaxed);
    }
    file.flush().await?;
    Ok(())
}

async fn fetch_segment(client: &Client, seg: &Seg, headers: &HeaderMap, keys: &HashMap<Url, [u8; 16]>, limiter: &Limiter) -> Result<Vec<u8>> {
    let data = fetch_bytes(client, seg, headers, 5).await?;
    limiter.take(data.len() as u64).await;
    match &seg.key {
        Some(k) => decrypt(&keys[&k.uri], &k.iv, data),
        None => Ok(data),
    }
}

fn decrypt(key: &[u8; 16], iv: &[u8; 16], mut data: Vec<u8>) -> Result<Vec<u8>> {
    let dec = cbc::Decryptor::<aes::Aes128>::new(key.into(), iv.into());
    let n = dec.decrypt_padded::<Pkcs7>(&mut data).map_err(|_| anyhow::anyhow!("AES decrypt failed"))?.len();
    data.truncate(n);
    Ok(data)
}

/// Download `url` (media or master playlist) plus optional separate audio into `out`.
/// The built-in remuxer joins them into an .mp4; if it can't, the raw stream is kept.
#[allow(clippy::too_many_arguments)]
pub async fn download(
    client: &Client,
    url: &str,
    audio: Option<&str>,
    out: PathBuf,
    headers: &HeaderMap,
    conns: usize,
    progress: &Progress,
    limiter: &Limiter,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let work = async {
        if let Some(dir) = out.parent() {
            fs::create_dir_all(dir).await?;
        }
        *progress.path.lock().unwrap() = Some(out.clone());
        progress.downloaded.store(0, Relaxed);
        let video_tmp = with_suffix(&out, ".video.part");
        let fmp4 = download_track(client, url, &video_tmp, headers, conns, progress, limiter).await?;
        let audio_tmp = with_suffix(&out, ".audio.part");
        if let Some(a) = audio {
            download_track(client, a, &audio_tmp, headers, conns, progress, limiter).await?;
        }

        let mp4 = out.with_extension("mp4");
        let mut inputs = vec![video_tmp.clone()];
        if audio.is_some() {
            inputs.push(audio_tmp.clone());
        }
        match crate::mux::remux_async(inputs, mp4.clone()).await {
            Ok(()) => {
                fs::remove_file(&video_tmp).await.ok();
                fs::remove_file(&audio_tmp).await.ok();
                return Ok(mp4);
            }
            Err(e) => eprintln!("remux failed ({e:#}), keeping the raw stream"),
        }
        let video = out.with_extension(if fmp4 { "mp4" } else { "ts" });
        fs::rename(&video_tmp, &video).await?;
        if audio.is_some() {
            fs::rename(&audio_tmp, video.with_extension(if fmp4 { "audio.mp4" } else { "audio.ts" })).await?;
        }
        Ok(video)
    };
    tokio::select! {
        r = work => r,
        _ = cancel.cancelled() => Err(Cancelled.into()),
    }
}
