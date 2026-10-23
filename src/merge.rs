//! A video file and a separate audio file (how YouTube serves HD), downloaded with the
//! normal multi-connection engine and joined into one .mp4 by the built-in remuxer.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering::Relaxed;
use std::time::Duration;

use anyhow::Result;
use reqwest::Client;
use tokio_util::sync::CancellationToken;

use crate::engine::{self, Options, Progress, Target, with_suffix};
use crate::limiter::Limiter;

#[allow(clippy::too_many_arguments)]
pub async fn download(
    client: &Client,
    video: &str,
    audio: &str,
    out: PathBuf,
    opts: &Options,
    progress: &Arc<Progress>,
    limiter: &Limiter,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    if let Some(dir) = out.parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    *progress.path.lock().unwrap() = Some(out.clone());
    let parts = [with_suffix(&out, ".video"), with_suffix(&out, ".audio")];
    let mut base = 0u64;
    for (url, part) in [video, audio].into_iter().zip(&parts) {
        if done(part) {
            base += std::fs::metadata(part).map_or(0, |m| m.len());
            continue;
        }
        // The engine reports one file; show both as one download.
        let sub = Arc::new(Progress::default());
        let mirror = {
            let (sub, p) = (sub.clone(), progress.clone());
            tokio::spawn(async move {
                loop {
                    p.downloaded.store(base + sub.downloaded.load(Relaxed), Relaxed);
                    p.total.store(base + sub.total.load(Relaxed), Relaxed);
                    tokio::time::sleep(Duration::from_millis(250)).await;
                }
            })
        };
        let r = engine::download(client, url, Target::File(part.clone()), opts, &sub, limiter, cancel).await;
        mirror.abort();
        r?;
        base += sub.total.load(Relaxed).max(sub.downloaded.load(Relaxed));
        progress.downloaded.store(base, Relaxed);
    }
    *progress.path.lock().unwrap() = Some(out.clone());
    crate::mux::remux_async(parts.to_vec(), out.clone()).await?;
    for p in &parts {
        tokio::fs::remove_file(p).await.ok();
    }
    Ok(out)
}

/// A part that finished downloading earlier (the engine renames .part away at the end).
fn done(part: &Path) -> bool {
    part.exists() && !with_suffix(part, ".part").exists()
}
