//! Optional bridge to yt-dlp for sites whose videos aren't plain files or HLS.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::Ordering::Relaxed;

use anyhow::{bail, Context, Result};
use serde::Serialize;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio_util::sync::CancellationToken;

use crate::engine::{Cancelled, Progress};

#[derive(Serialize)]
pub struct Quality {
    pub label: String,
    /// yt-dlp `-f` selector.
    pub format: String,
    pub size: Option<u64>,
}

pub async fn available() -> bool {
    Command::new("yt-dlp").arg("--version").stdout(Stdio::null()).stderr(Stdio::null()).status().await.is_ok_and(|s| s.success())
}

/// Returns (title, qualities) for a page URL.
pub async fn formats(url: &str) -> Result<(String, Vec<Quality>)> {
    let out = Command::new("yt-dlp").args(["-J", "--no-playlist", "--no-warnings", "--", url]).output().await?;
    if !out.status.success() {
        bail!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("failed"));
    }
    let info: serde_json::Value = serde_json::from_slice(&out.stdout)?;
    let title = info["title"].as_str().unwrap_or("video").to_string();
    let formats = info["formats"].as_array().cloned().unwrap_or_default();

    let size = |f: &serde_json::Value| f["filesize"].as_u64().or(f["filesize_approx"].as_u64());
    let best_audio = formats
        .iter()
        .filter(|f| f["vcodec"] == "none" && f["acodec"] != "none")
        .filter_map(size)
        .max();

    let mut heights: Vec<(u64, Option<u64>)> = Vec::new();
    for f in &formats {
        let Some(h) = f["height"].as_u64() else { continue };
        if f["vcodec"] == "none" {
            continue;
        }
        // Biggest file at this height approximates what yt-dlp will pick.
        let s = size(f).map(|s| s + if f["acodec"] == "none" { best_audio.unwrap_or(0) } else { 0 });
        match heights.iter_mut().find(|(hh, _)| *hh == h) {
            Some(e) => e.1 = e.1.max(s),
            None => heights.push((h, s)),
        }
    }
    heights.sort_by(|a, b| b.0.cmp(&a.0));

    let mut q: Vec<Quality> = heights
        .into_iter()
        .map(|(h, size)| Quality {
            label: format!("{h}p"),
            format: format!("bv*[height<={h}]+ba/b[height<={h}]"),
            size,
        })
        .collect();
    if best_audio.is_some() {
        q.push(Quality { label: "Audio only".into(), format: "ba[ext=m4a]/ba".into(), size: best_audio });
    }
    if q.is_empty() {
        q.push(Quality { label: "Best".into(), format: "b".into(), size: None });
    }
    Ok((title, q))
}

pub async fn download(
    url: &str,
    format: &str,
    dir: &Path,
    conns: usize,
    rate_limit: u64,
    progress: &Progress,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    tokio::fs::create_dir_all(dir).await?;
    let mut cmd = Command::new("yt-dlp");
    cmd.args(["--no-playlist", "--newline", "--no-warnings", "-f", format])
        .args(["-N", &conns.to_string()])
        .args(["--progress-template", "download:DX %(progress.downloaded_bytes)s %(progress.total_bytes)s %(progress.total_bytes_estimate)s"])
        .args(["--print", "after_move:DXPATH %(filepath)s"])
        .arg("-P")
        .arg(dir)
        .args(["-o", "%(title).150B [%(id)s].%(ext)s"]);
    if rate_limit > 0 {
        cmd.args(["-r", &rate_limit.to_string()]);
    }
    let mut child = cmd.arg("--").arg(url).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true).spawn().context("starting yt-dlp")?;
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut path = None;
    // Separate video+audio downloads restart the counters; keep a running offset.
    let mut base = 0u64;
    let mut last = 0u64;

    let read = async {
        while let Some(line) = lines.next_line().await? {
            if let Some(rest) = line.strip_prefix("DX ") {
                let mut it = rest.split(' ').map(|v| v.parse::<f64>().ok().map(|f| f as u64));
                let done = it.next().flatten().unwrap_or(0);
                let total = it.next().flatten().or(it.next().flatten()).unwrap_or(0);
                if done < last {
                    base += last;
                }
                last = done;
                progress.downloaded.store(base + done, Relaxed);
                progress.total.store(base + total.max(done), Relaxed);
            } else if let Some(p) = line.strip_prefix("DXPATH ") {
                path = Some(PathBuf::from(p));
                *progress.path.lock().unwrap() = path.clone();
            }
        }
        Ok::<_, anyhow::Error>(())
    };
    tokio::select! {
        r = read => r?,
        _ = cancel.cancelled() => {
            child.kill().await.ok();
            return Err(Cancelled.into());
        }
    }
    let out = child.wait_with_output().await?;
    if !out.status.success() {
        bail!("yt-dlp: {}", String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("failed"));
    }
    path.context("yt-dlp finished without a file")
}
