//! Segmented, resumable HTTP download of a single file.

use std::io::SeekFrom;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::Mutex;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use futures_util::StreamExt;
use reqwest::header::{HeaderMap, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, ETAG, IF_RANGE, LAST_MODIFIED, RANGE};
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use tokio::fs::{self, OpenOptions};
use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::limiter::Limiter;

/// Don't split a segment if either half would be smaller than this.
const MIN_SPLIT: u64 = 1024 * 1024;

/// Returned (inside anyhow) when a download stops because it was paused or removed.
#[derive(Debug)]
pub struct Cancelled;
impl std::fmt::Display for Cancelled {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("cancelled")
    }
}
impl std::error::Error for Cancelled {}

/// Live progress, read by the queue manager and CLI.
#[derive(Default)]
pub struct Progress {
    pub downloaded: AtomicU64,
    pub total: AtomicU64,
    pub path: Mutex<Option<PathBuf>>,
    /// [start, end, pos] per segment, for the UI.
    pub segments: Mutex<Vec<[u64; 3]>>,
}

impl Progress {
    pub fn add(&self, n: u64) {
        self.downloaded.fetch_add(n, Relaxed);
    }
}

pub struct Options {
    pub connections: usize,
    pub retries: u32,
    pub headers: HeaderMap,
}

pub enum Target {
    /// Exact output path (used when resuming).
    File(PathBuf),
    /// Pick a free name inside this folder.
    Dir { dir: PathBuf, name: Option<String> },
}

#[derive(Serialize, Deserialize, Clone)]
struct Segment {
    start: u64,
    end: u64, // exclusive
    pos: u64,
    #[serde(skip)]
    active: bool,
}

impl Segment {
    fn remaining(&self) -> u64 {
        self.end.saturating_sub(self.pos)
    }
}

#[derive(Serialize, Deserialize)]
struct State {
    url: String,
    size: u64,
    validator: Option<String>,
    segments: Vec<Segment>,
}

impl State {
    fn done(&self) -> u64 {
        self.segments.iter().map(|s| s.pos - s.start).sum()
    }

    /// Give an idle worker a segment: an unclaimed one, or half of the largest active one.
    fn claim(&mut self) -> Option<usize> {
        if let Some(i) = self.segments.iter().position(|s| !s.active && s.remaining() > 0) {
            self.segments[i].active = true;
            return Some(i);
        }
        let (i, big) = self
            .segments
            .iter()
            .enumerate()
            .filter(|(_, s)| s.active)
            .max_by_key(|(_, s)| s.remaining())?;
        if big.remaining() < 2 * MIN_SPLIT {
            return None;
        }
        let mid = big.pos + big.remaining() / 2;
        let end = big.end;
        self.segments[i].end = mid;
        self.segments.push(Segment { start: mid, end, pos: mid, active: true });
        Some(self.segments.len() - 1)
    }
}

pub struct Probe {
    pub size: Option<u64>,
    pub ranges: bool,
    pub validator: Option<String>,
    pub filename: Option<String>,
    pub content_type: Option<String>,
}

struct Ctx<'a> {
    client: &'a Client,
    url: &'a str,
    headers: &'a HeaderMap,
    part: PathBuf,
    validator: Option<String>,
    state: Mutex<State>,
    progress: &'a Progress,
    limiter: &'a Limiter,
}

pub async fn download(
    client: &Client,
    url: &str,
    target: Target,
    opts: &Options,
    progress: &Progress,
    limiter: &Limiter,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let probe = probe(client, url, &opts.headers).await?;
    let out = match target {
        Target::File(p) => p,
        Target::Dir { dir, name } => {
            let name = name.or(probe.filename.clone()).or_else(|| name_from_url(url)).unwrap_or("download.bin".into());
            fs::create_dir_all(&dir).await?;
            unique_path(&dir.join(name))
        }
    };
    *progress.path.lock().unwrap() = Some(out.clone());
    let part = with_suffix(&out, ".part");
    let state_path = with_suffix(&out, ".part.state");
    progress.total.store(probe.size.unwrap_or(0), Relaxed);

    match (probe.size, probe.ranges) {
        (Some(size), true) if size > 0 => {
            let state = load_or_new_state(&state_path, url, size, &probe.validator, opts.connections).await;
            progress.downloaded.store(state.done(), Relaxed);
            let file = OpenOptions::new().create(true).write(true).truncate(false).open(&part).await?;
            file.set_len(size).await?;
            drop(file);
            let ctx = Ctx {
                client,
                url,
                headers: &opts.headers,
                part: part.clone(),
                validator: probe.validator.clone(),
                state: Mutex::new(state),
                progress,
                limiter,
            };
            let result = tokio::select! {
                r = run_parallel(&ctx, &state_path, opts.connections, opts.retries) => r,
                _ = cancel.cancelled() => Err(Cancelled.into()),
            };
            if result.is_err() {
                // Keep the latest progress so a resume continues from here.
                save_state(&ctx, &state_path)?;
            }
            result?;
            fs::remove_file(&state_path).await.ok();
        }
        _ => {
            progress.downloaded.store(0, Relaxed);
            tokio::select! {
                r = single(client, url, &opts.headers, &part, progress, limiter) => r?,
                _ = cancel.cancelled() => return Err(Cancelled.into()),
            }
        }
    }

    fs::rename(&part, &out).await?;
    Ok(out)
}

/// GET bytes=0-0 tells us size, range support and validator in one request.
pub async fn probe(client: &Client, url: &str, headers: &HeaderMap) -> Result<Probe> {
    let resp = client.get(url).headers(headers.clone()).header(RANGE, "bytes=0-0").send().await?.error_for_status()?;
    let h = resp.headers();
    let get = |k| h.get(k).and_then(|v: &reqwest::header::HeaderValue| v.to_str().ok()).map(str::to_owned);
    let ranges = resp.status() == StatusCode::PARTIAL_CONTENT;
    let size = if ranges {
        get(CONTENT_RANGE).and_then(|v| v.rsplit('/').next()?.parse().ok())
    } else {
        get(CONTENT_LENGTH).and_then(|v| v.parse().ok())
    };
    // Weak ETags are not allowed in If-Range, so fall back to Last-Modified.
    let validator = get(ETAG).filter(|e| !e.starts_with("W/")).or_else(|| get(LAST_MODIFIED));
    let filename = get(CONTENT_DISPOSITION).and_then(|v| {
        let f = v.split("filename=").nth(1)?.split(';').next()?.trim().trim_matches('"');
        sanitize(f)
    });
    let content_type = get(reqwest::header::CONTENT_TYPE);
    // Name from the final URL after redirects is better than the original one.
    let filename = filename.or_else(|| name_from_url(resp.url().as_str()));
    Ok(Probe { size, ranges, validator, filename, content_type })
}

async fn load_or_new_state(path: &Path, url: &str, size: u64, validator: &Option<String>, n: usize) -> State {
    if let Ok(data) = fs::read(path).await {
        if let Ok(s) = serde_json::from_slice::<State>(&data) {
            if s.url == url && s.size == size && &s.validator == validator {
                return s;
            }
        }
    }
    let n = n.max(1) as u64;
    let chunk = size.div_ceil(n).max(MIN_SPLIT);
    let segments = (0..size)
        .step_by(chunk as usize)
        .map(|start| Segment { start, end: (start + chunk).min(size), pos: start, active: false })
        .collect();
    State { url: url.into(), size, validator: validator.clone(), segments }
}

/// Blocking on purpose: the file is tiny, and with no await points a `select!` can't drop
/// this halfway. tokio::fs work keeps running on a blocking thread after its future is
/// dropped, so an abandoned save used to race the next one on the same `.tmp` file.
fn save_state(ctx: &Ctx<'_>, path: &Path) -> Result<()> {
    let json = {
        let st = ctx.state.lock().unwrap();
        *ctx.progress.segments.lock().unwrap() = st.segments.iter().map(|s| [s.start, s.end, s.pos]).collect();
        serde_json::to_vec(&*st)?
    };
    let tmp = with_suffix(path, ".tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

async fn run_parallel(ctx: &Ctx<'_>, state_path: &Path, n: usize, retries: u32) -> Result<()> {
    let saver = async {
        loop {
            tokio::time::sleep(Duration::from_secs(1)).await;
            save_state(ctx, state_path).ok();
        }
    };
    let mut workers = Vec::new();
    for _ in 0..n.max(1) {
        workers.push(worker(ctx, retries));
    }
    let workers = futures_util::future::try_join_all(workers);
    tokio::select! {
        r = workers => { r?; }
        _ = saver => unreachable!(),
    }
    if ctx.state.lock().unwrap().segments.iter().any(|s| s.remaining() > 0) {
        bail!("download incomplete");
    }
    Ok(())
}

async fn worker(ctx: &Ctx<'_>, retries: u32) -> Result<()> {
    loop {
        let Some(idx) = ctx.state.lock().unwrap().claim() else { return Ok(()) };
        let mut attempt = 0;
        let r = loop {
            match fetch_segment(ctx, idx).await {
                Ok(()) => break Ok(()),
                Err(e) if attempt < retries => {
                    attempt += 1;
                    let wait = Duration::from_millis(500 * 2u64.pow(attempt));
                    eprintln!("segment {idx}: {e:#}, retry {attempt}/{retries} in {wait:?}");
                    tokio::time::sleep(wait).await;
                }
                Err(e) => break Err(e),
            }
        };
        ctx.state.lock().unwrap().segments[idx].active = false;
        r?;
    }
}

async fn fetch_segment(ctx: &Ctx<'_>, idx: usize) -> Result<()> {
    let (mut pos, end) = {
        let s = &ctx.state.lock().unwrap().segments[idx];
        (s.pos, s.end)
    };
    if pos >= end {
        return Ok(());
    }

    let mut req = ctx.client.get(ctx.url).headers(ctx.headers.clone()).header(RANGE, format!("bytes={pos}-{}", end - 1));
    if let Some(v) = &ctx.validator {
        req = req.header(IF_RANGE, v);
    }
    let resp = req.send().await?.error_for_status()?;
    if resp.status() != StatusCode::PARTIAL_CONTENT {
        bail!("server sent {} instead of 206 (file changed?)", resp.status());
    }

    let mut file = OpenOptions::new().write(true).open(&ctx.part).await?;
    file.seek(SeekFrom::Start(pos)).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("read failed")?;
        let end = ctx.state.lock().unwrap().segments[idx].end;
        let take = (chunk.len() as u64).min(end.saturating_sub(pos));
        ctx.limiter.take(take).await;
        file.write_all(&chunk[..take as usize]).await?;
        file.flush().await?;
        // Only count bytes once they are on disk, so a saved state never points past real data.
        // If a split shrank `end` meanwhile, the extra bytes are identical to what the new
        // segment will write, so the overlap is harmless.
        let counted = {
            let mut st = ctx.state.lock().unwrap();
            let seg = &mut st.segments[idx];
            let new_pos = (pos + take).min(seg.end);
            let counted = new_pos.saturating_sub(seg.pos);
            seg.pos = seg.pos.max(new_pos);
            pos = seg.pos;
            if pos >= seg.end {
                drop(st);
                ctx.progress.add(counted);
                return Ok(());
            }
            counted
        };
        ctx.progress.add(counted);
    }
    bail!("connection closed early")
}

async fn single(client: &Client, url: &str, headers: &HeaderMap, part: &Path, progress: &Progress, limiter: &Limiter) -> Result<()> {
    let resp = client.get(url).headers(headers.clone()).send().await?.error_for_status()?;
    if let Some(len) = resp.content_length() {
        progress.total.store(len, Relaxed);
    }
    let mut file = fs::File::create(part).await?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk?;
        limiter.take(chunk.len() as u64).await;
        file.write_all(&chunk).await?;
        progress.add(chunk.len() as u64);
    }
    file.flush().await?;
    Ok(())
}

pub fn name_from_url(url: &str) -> Option<String> {
    let path = url.split(['?', '#']).next()?;
    let last = path.rsplit('/').next()?;
    sanitize(&percent_decode(last))
}

fn percent_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let Some(v) = std::str::from_utf8(&b[i + 1..i + 3]).ok().and_then(|h| u8::from_str_radix(h, 16).ok()) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

pub fn sanitize(name: &str) -> Option<String> {
    let n: String = name.chars().filter(|c| !"/\\:*?\"<>|".contains(*c) && !c.is_control()).collect();
    let n = n.trim().trim_end_matches('.').to_string();
    (!n.is_empty() && n != "..").then_some(n)
}

pub fn with_suffix(p: &Path, suffix: &str) -> PathBuf {
    let mut s = p.as_os_str().to_owned();
    s.push(suffix);
    s.into()
}

/// `file.zip` → `file (1).zip` until neither the file nor its `.part` exists.
pub fn unique_path(p: &Path) -> PathBuf {
    let taken = |p: &Path| p.exists() || with_suffix(p, ".part").exists();
    if !taken(p) {
        return p.to_path_buf();
    }
    let stem = p.file_stem().unwrap_or_default().to_string_lossy();
    let ext = p.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default();
    (1..)
        .map(|i| p.with_file_name(format!("{stem} ({i}){ext}")))
        .find(|c| !taken(c))
        .unwrap()
}
