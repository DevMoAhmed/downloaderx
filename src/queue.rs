//! Download queue: concurrency cap, priorities, scheduling, categories, persistence.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering::Relaxed;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::sync::Notify;
use tokio_util::sync::CancellationToken;

use crate::engine::{self, with_suffix, Cancelled, Progress, Target};
use crate::limiter::Limiter;
use crate::{dash, hls, merge};

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Kind {
    Direct,
    Hls { audio: Option<String> },
    /// DASH manifest; the representation ids to download.
    Dash { video: Option<String>, audio: Option<String> },
    /// Video file at `url` plus a separate audio file, joined after download (YouTube).
    Merge { audio: String },
    /// Saved by versions that used yt-dlp; can't be downloaded any more.
    Ytdlp { format: String },
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Debug)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Paused,
    Done,
    Failed,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Item {
    pub id: u64,
    pub url: String,
    pub kind: Kind,
    pub name: Option<String>,
    pub path: Option<PathBuf>,
    pub category: String,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    pub status: Status,
    pub error: Option<String>,
    pub priority: i32,
    pub added: u64,
    pub start_after: Option<u64>,
    pub downloaded: u64,
    pub total: u64,
    #[serde(default)]
    pub speed: u64,
    pub sha256: Option<String>,
    pub verified: Option<bool>,
    #[serde(default)]
    pub segments: Vec<[u64; 3]>,
    /// When the download was last started.
    #[serde(default)]
    pub last_try: Option<u64>,
    /// Free text shown in the list, e.g. the chosen quality.
    #[serde(default)]
    pub description: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
#[serde(default)]
pub struct Settings {
    pub max_active: usize,
    pub connections: usize,
    /// KiB/s, 0 = unlimited.
    pub speed_limit: u64,
    pub download_dir: PathBuf,
    pub categorize: bool,
    /// Bring the window up with the progress window when a download comes from the browser.
    pub show_progress: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            max_active: 3,
            connections: 8,
            speed_limit: 0,
            download_dir: dirs::download_dir().unwrap_or_else(|| ".".into()).join("DownloaderX"),
            categorize: true,
            show_progress: true,
        }
    }
}

#[derive(Deserialize)]
pub struct AddRequest {
    pub url: String,
    #[serde(default = "direct")]
    pub kind: Kind,
    pub name: Option<String>,
    #[serde(default)]
    pub headers: Vec<(String, String)>,
    #[serde(default)]
    pub priority: i32,
    pub start_after: Option<u64>,
    pub sha256: Option<String>,
    /// Add paused instead of queued.
    #[serde(default)]
    pub paused: bool,
    pub description: Option<String>,
}

fn direct() -> Kind {
    Kind::Direct
}

#[derive(Serialize, Deserialize, Default)]
struct Saved {
    next_id: u64,
    items: Vec<Item>,
    settings: Settings,
}

struct Running {
    progress: Arc<Progress>,
    cancel: CancellationToken,
    last: u64,
}

pub struct Manager {
    data: Mutex<Saved>,
    running: Mutex<HashMap<u64, Running>>,
    client: Client,
    pub limiter: Limiter,
    wake: Notify,
    file: PathBuf,
    /// Brings the desktop window to the front (set by the GUI), optionally opening a
    /// panel such as "extension".
    pub show_window: OnceLock<Box<dyn Fn(Option<String>) + Send + Sync>>,
    /// A panel asked for before the window existed; the window picks it up when it loads.
    pub pending_panel: Mutex<Option<String>>,
}

pub fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_secs()
}

pub fn category(name: &str) -> &'static str {
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    match ext.as_str() {
        "mp4" | "mkv" | "webm" | "avi" | "mov" | "m4v" | "ts" | "flv" | "wmv" | "3gp" => "Video",
        "mp3" | "m4a" | "aac" | "flac" | "wav" | "ogg" | "opus" | "wma" => "Music",
        "zip" | "rar" | "7z" | "tar" | "gz" | "xz" | "bz2" | "zst" | "tgz" => "Compressed",
        "exe" | "msi" | "dmg" | "pkg" | "deb" | "rpm" | "appimage" | "apk" => "Programs",
        "pdf" | "doc" | "docx" | "xls" | "xlsx" | "ppt" | "pptx" | "txt" | "epub" | "csv" | "odt" => "Documents",
        "jpg" | "jpeg" | "png" | "gif" | "webp" | "svg" | "bmp" => "Images",
        _ => "General",
    }
}

impl Manager {
    pub async fn new(client: Client, data_dir: &Path) -> Arc<Self> {
        let file = data_dir.join("queue.json");
        let mut saved: Saved = std::fs::read(&file).ok().and_then(|d| serde_json::from_slice(&d).ok()).unwrap_or_default();
        // Anything that was running when we exited resumes as queued.
        for it in &mut saved.items {
            if it.status == Status::Running {
                it.status = Status::Queued;
            }
        }
        let limiter = Limiter::new(saved.settings.speed_limit * 1024);
        Arc::new(Self {
            data: Mutex::new(saved),
            running: Mutex::default(),
            client,
            limiter,
            wake: Notify::new(),
            file,
            show_window: OnceLock::new(),
            pending_panel: Mutex::new(None),
        })
    }

    pub fn items(&self) -> Vec<Item> {
        self.data.lock().unwrap().items.clone()
    }

    pub fn settings(&self) -> Settings {
        self.data.lock().unwrap().settings.clone()
    }

    pub fn set_settings(&self, s: Settings) {
        self.limiter.set_rate(s.speed_limit * 1024);
        self.data.lock().unwrap().settings = s;
        self.wake.notify_one();
    }

    pub fn add(&self, r: AddRequest) -> u64 {
        let mut d = self.data.lock().unwrap();
        // Same URL already waiting or downloading: don't add twice.
        if let Some(it) = d.items.iter().find(|i| i.url == r.url && i.kind == r.kind && matches!(i.status, Status::Queued | Status::Running)) {
            return it.id;
        }
        d.next_id += 1;
        let id = d.next_id;
        let name = r.name.as_deref().and_then(engine::sanitize).or_else(|| match r.kind {
            Kind::Direct => engine::name_from_url(&r.url),
            _ => None,
        });
        let cat = match (&r.kind, &name) {
            (Kind::Hls { .. } | Kind::Merge { .. }, _) => "Video",
            (Kind::Dash { video: None, .. }, _) => "Music",
            (Kind::Dash { .. }, _) => "Video",
            (Kind::Ytdlp { format }, _) if format.starts_with("ba") => "Music",
            (Kind::Ytdlp { .. }, _) => "Video",
            (_, Some(n)) => category(n),
            _ => "General",
        };
        d.items.push(Item {
            id,
            url: r.url,
            kind: r.kind,
            name,
            path: None,
            category: cat.into(),
            headers: r.headers,
            status: if r.paused { Status::Paused } else { Status::Queued },
            error: None,
            priority: r.priority,
            added: now(),
            start_after: r.start_after,
            downloaded: 0,
            total: 0,
            speed: 0,
            sha256: r.sha256.map(|s| s.trim().to_ascii_lowercase()).filter(|s| !s.is_empty()),
            verified: None,
            segments: vec![],
            last_try: None,
            description: r.description.filter(|d| !d.trim().is_empty()),
        });
        drop(d);
        self.wake.notify_one();
        id
    }

    /// pause | resume | remove | delete (remove + files) | retry | up | down
    pub fn action(&self, id: u64, action: &str) -> bool {
        let mut d = self.data.lock().unwrap();
        let Some(idx) = d.items.iter().position(|i| i.id == id) else { return false };
        let it = &mut d.items[idx];
        match action {
            "pause" => {
                if matches!(it.status, Status::Queued | Status::Running) {
                    it.status = Status::Paused;
                    self.stop(id);
                }
            }
            "resume" | "retry" => {
                if matches!(it.status, Status::Paused | Status::Failed) {
                    it.status = Status::Queued;
                    it.error = None;
                    it.start_after = None;
                }
            }
            "up" => it.priority += 1,
            "down" => it.priority -= 1,
            "remove" | "delete" => {
                self.stop(id);
                let it = d.items.remove(idx);
                if let Some(p) = &it.path {
                    // Partial files are useless without the queue entry.
                    for s in [".part", ".part.state", ".video.part", ".audio.part", ".video", ".audio", ".video.part.state", ".audio.part.state"] {
                        std::fs::remove_file(with_suffix(p, s)).ok();
                    }
                    if action == "delete" && it.status == Status::Done {
                        std::fs::remove_file(p).ok();
                    }
                }
            }
            _ => return false,
        }
        drop(d);
        self.wake.notify_one();
        true
    }

    pub fn all(&self, action: &str) {
        let ids: Vec<u64> = self.items().iter().map(|i| i.id).collect();
        for id in ids {
            self.action(id, action);
        }
    }

    /// Hold the queue: waiting items are paused, running ones keep going.
    pub fn stop_queue(&self) {
        for it in self.data.lock().unwrap().items.iter_mut() {
            if it.status == Status::Queued {
                it.status = Status::Paused;
            }
        }
    }

    /// Queue unfinished items to start at `at` (unix seconds), or now when `None`.
    pub fn schedule(&self, ids: &[u64], at: Option<u64>) {
        let mut d = self.data.lock().unwrap();
        for it in d.items.iter_mut().filter(|i| ids.contains(&i.id)) {
            if matches!(it.status, Status::Queued | Status::Paused | Status::Failed) {
                it.status = Status::Queued;
                it.error = None;
                it.start_after = at;
            }
        }
        drop(d);
        self.wake.notify_one();
    }

    pub fn clear_finished(&self) {
        self.data.lock().unwrap().items.retain(|i| i.status != Status::Done);
    }

    fn stop(&self, id: u64) {
        if let Some(r) = self.running.lock().unwrap().get(&id) {
            r.cancel.cancel();
        }
    }

    /// Scheduler loop: refresh progress, start eligible items, save the queue.
    pub async fn run(self: Arc<Self>) {
        loop {
            tokio::select! {
                _ = self.wake.notified() => {}
                _ = tokio::time::sleep(Duration::from_secs(1)) => {}
            }
            self.tick();
            self.save();
        }
    }

    fn tick(self: &Arc<Self>) {
        let mut d = self.data.lock().unwrap();
        let mut running = self.running.lock().unwrap();
        for it in d.items.iter_mut() {
            if let Some(r) = running.get_mut(&it.id) {
                let now = r.progress.downloaded.load(Relaxed);
                // Smooth the per-second delta so the speed and ETA don't jump around.
                let inst = now.saturating_sub(r.last);
                it.speed = if it.speed == 0 { inst } else { (it.speed * 7 + inst * 3) / 10 };
                r.last = now;
                it.downloaded = now;
                it.total = r.progress.total.load(Relaxed);
                it.segments = r.progress.segments.lock().unwrap().clone();
                if let Some(p) = r.progress.path.lock().unwrap().clone() {
                    it.name = p.file_name().map(|n| n.to_string_lossy().into_owned());
                    it.path = Some(p);
                }
            }
        }

        let max = d.settings.max_active.max(1);
        let t = now();
        let mut free = max.saturating_sub(running.len());
        while free > 0 {
            let next = d
                .items
                .iter()
                .enumerate()
                .filter(|(_, i)| i.status == Status::Queued && i.start_after.is_none_or(|s| s <= t))
                .max_by(|(_, a), (_, b)| a.priority.cmp(&b.priority).then(b.added.cmp(&a.added)).then(b.id.cmp(&a.id)))
                .map(|(i, _)| i);
            let Some(idx) = next else { break };
            let it = &mut d.items[idx];
            it.status = Status::Running;
            it.speed = 0;
            it.last_try = Some(t);
            let progress = Arc::new(Progress::default());
            let cancel = CancellationToken::new();
            running.insert(it.id, Running { progress: progress.clone(), cancel: cancel.clone(), last: it.downloaded });
            let job = it.clone();
            let settings = d.settings.clone();
            let me = self.clone();
            tokio::spawn(async move { me.execute(job, settings, progress, cancel).await });
            free -= 1;
        }
    }

    async fn execute(self: Arc<Self>, job: Item, s: Settings, progress: Arc<Progress>, cancel: CancellationToken) {
        let mut headers = HeaderMap::new();
        for (k, v) in &job.headers {
            if let (Ok(k), Ok(v)) = (HeaderName::try_from(k.as_str()), HeaderValue::try_from(v.as_str())) {
                headers.insert(k, v);
            }
        }
        let dir = if s.categorize { s.download_dir.join(&job.category) } else { s.download_dir.clone() };
        let result: Result<PathBuf> = match &job.kind {
            Kind::Direct => {
                let target = match &job.path {
                    Some(p) => Target::File(p.clone()),
                    None => Target::Dir { dir, name: job.name.clone() },
                };
                let opts = engine::Options { connections: s.connections, retries: 5, headers };
                engine::download(&self.client, &job.url, target, &opts, &progress, &self.limiter, &cancel).await
            }
            Kind::Hls { audio } => {
                let out = job.path.clone().unwrap_or_else(|| {
                    let name = job.name.clone().unwrap_or("video".into());
                    let name = if name.ends_with(".mp4") { name } else { format!("{name}.mp4") };
                    engine::unique_path(&dir.join(name))
                });
                hls::download(&self.client, &job.url, audio.as_deref(), out, &headers, s.connections, &progress, &self.limiter, &cancel).await
            }
            Kind::Dash { video, audio } => {
                let out = job.path.clone().unwrap_or_else(|| {
                    let name = job.name.clone().unwrap_or("video".into());
                    let ext = if video.is_some() { "mp4" } else { "m4a" };
                    let name = if name.ends_with(&format!(".{ext}")) { name } else { format!("{name}.{ext}") };
                    engine::unique_path(&dir.join(name))
                });
                dash::download(&self.client, &job.url, video.as_deref(), audio.as_deref(), out, &headers, s.connections, &progress, &self.limiter, &cancel).await
            }
            Kind::Merge { audio } => {
                let out = job.path.clone().unwrap_or_else(|| {
                    let name = job.name.clone().unwrap_or("video".into());
                    let name = if name.ends_with(".mp4") { name } else { format!("{name}.mp4") };
                    engine::unique_path(&dir.join(name))
                });
                let opts = engine::Options { connections: s.connections, retries: 5, headers };
                merge::download(&self.client, &job.url, audio, out, &opts, &progress, &self.limiter, &cancel).await
            }
            Kind::Ytdlp { .. } => Err(anyhow::anyhow!("added by an older version that used yt-dlp: open the page in your browser and download it again with the extension")),
        };

        let verified = match (&result, &job.sha256) {
            (Ok(p), Some(want)) => {
                let p = p.clone();
                let got = tokio::task::spawn_blocking(move || sha256_file(&p)).await.ok().and_then(Result::ok);
                Some(got.as_deref() == Some(want.as_str()))
            }
            _ => None,
        };

        self.running.lock().unwrap().remove(&job.id);
        let mut d = self.data.lock().unwrap();
        if let Some(it) = d.items.iter_mut().find(|i| i.id == job.id) {
            it.speed = 0;
            it.downloaded = progress.downloaded.load(Relaxed);
            it.total = progress.total.load(Relaxed).max(it.downloaded);
            if let Some(p) = progress.path.lock().unwrap().clone() {
                it.path = Some(p);
            }
            match result {
                Ok(p) => {
                    // Nothing was counted when every part was already on disk.
                    if it.downloaded == 0 {
                        if let Ok(m) = std::fs::metadata(&p) {
                            it.downloaded = m.len();
                        }
                    }
                    it.name = p.file_name().map(|n| n.to_string_lossy().into_owned());
                    it.path = Some(p);
                    it.status = Status::Done;
                    it.verified = verified;
                    it.total = it.downloaded.max(it.total);
                    it.segments.clear();
                }
                Err(e) if e.is::<Cancelled>() => {
                    if it.status == Status::Running {
                        it.status = Status::Paused;
                    }
                }
                Err(e) => {
                    it.status = Status::Failed;
                    it.error = Some(format!("{e:#}"));
                }
            }
        }
        drop(d);
        self.wake.notify_one();
    }

    pub fn save(&self) {
        let json = serde_json::to_vec_pretty(&*self.data.lock().unwrap());
        if let Ok(json) = json {
            if let Some(dir) = self.file.parent() {
                std::fs::create_dir_all(dir).ok();
            }
            let tmp = with_suffix(&self.file, ".tmp");
            if std::fs::write(&tmp, json).is_ok() {
                std::fs::rename(&tmp, &self.file).ok();
            }
        }
    }
}

fn sha256_file(p: &Path) -> Result<String> {
    let mut f = std::fs::File::open(p)?;
    let mut h = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = std::io::Read::read(&mut f, &mut buf)?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex::encode(h.finalize()))
}
