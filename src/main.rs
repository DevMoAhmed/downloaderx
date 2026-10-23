// No console window when started from the Start menu; `get` attaches to the caller's console.
#![cfg_attr(windows, windows_subsystem = "windows")]

mod dash;
mod engine;
#[cfg(windows)]
mod gui;
mod hls;
mod limiter;
mod merge;
mod mux;
mod queue;
mod server;
#[cfg(windows)]
mod win;
mod youtube;

use std::path::PathBuf;
use std::sync::atomic::Ordering::Relaxed;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use clap::{Parser, Subcommand};
use indicatif::{ProgressBar, ProgressStyle};
use reqwest::Client;
use tokio_util::sync::CancellationToken;

pub const DEFAULT_PORT: u16 = 6801;

#[derive(Parser)]
#[command(about = "Open-source accelerated download manager", version)]
struct Args {
    #[command(subcommand)]
    cmd: Option<Cmd>,
    /// Start in the tray without showing the window (used for autostart)
    #[arg(long)]
    hidden: bool,
    /// Open the window on the browser-extension setup guide (used by the installer)
    #[arg(long)]
    setup_extension: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Run without a window: dashboard in the browser + extension API
    Serve {
        #[arg(short, long, default_value_t = DEFAULT_PORT)]
        port: u16,
        /// Don't open the dashboard in the browser
        #[arg(long)]
        no_open: bool,
    },
    /// Download one file from the command line
    Get {
        url: String,
        /// Output file (default: name from server or URL)
        #[arg(short, long)]
        output: Option<PathBuf>,
        /// Parallel connections
        #[arg(short, long, default_value_t = 8)]
        connections: usize,
        /// Retries per segment
        #[arg(short, long, default_value_t = 5)]
        retries: u32,
        /// Speed limit in KiB/s (0 = unlimited)
        #[arg(short, long, default_value_t = 0)]
        limit: u64,
    },
    /// Join video and audio files (MPEG-TS, AAC, MP4) into one .mp4 without re-encoding
    Remux {
        /// Output .mp4
        output: PathBuf,
        /// Input files: video first, then a separate audio file if any
        #[arg(required = true)]
        inputs: Vec<PathBuf>,
    },
}

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("Mozilla/5.0 (compatible; downloaderx/", env!("CARGO_PKG_VERSION"), ")"))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()?)
}

#[cfg(windows)]
pub use win::shell_icon;
#[cfg(not(windows))]
pub fn shell_icon(_file: Option<&std::path::Path>, _ext: &str) -> Option<Vec<u8>> {
    None
}

fn main() -> Result<()> {
    #[cfg(windows)]
    if std::env::args_os().len() > 1 {
        win::attach_console();
    }
    let args = Args::parse();
    let rt = tokio::runtime::Runtime::new()?;
    match args.cmd {
        Some(Cmd::Serve { port, no_open }) => rt.block_on(serve(port, !no_open)),
        Some(Cmd::Get { url, output, connections, retries, limit }) => rt.block_on(get(url, output, connections, retries, limit)),
        Some(Cmd::Remux { output, inputs }) => {
            mux::remux(&inputs, &output)?;
            println!("saved {}", output.display());
            Ok(())
        }
        #[cfg(windows)]
        None => desktop(rt, args.hidden, args.setup_extension),
        #[cfg(not(windows))]
        None => rt.block_on(serve(DEFAULT_PORT, !args.hidden)),
    }
}

async fn start(port: u16, panel: Option<&str>) -> Result<Option<(Arc<queue::Manager>, std::net::TcpListener)>> {
    let listener = match std::net::TcpListener::bind(("127.0.0.1", port)) {
        Ok(l) => l,
        Err(e) if e.kind() == std::io::ErrorKind::AddrInUse => {
            // Already running: bring that instance to the front instead.
            let r = reqwest::Client::new()
                .post(format!("http://127.0.0.1:{port}/api/show"))
                .header("x-downloaderx", "1")
                .header("content-type", "application/json")
                .body(serde_json::json!({ "open": panel }).to_string())
                .send()
                .await;
            if r.is_ok() {
                return Ok(None);
            }
            return Err(e.into());
        }
        Err(e) => return Err(e.into()),
    };
    // DOWNLOADERX_DATA_DIR keeps a second (test) instance away from the real queue.
    let data_dir = std::env::var_os("DOWNLOADERX_DATA_DIR").map(PathBuf::from).unwrap_or_else(|| dirs::data_dir().unwrap_or_else(|| ".".into()).join("downloaderx"));
    let mgr = queue::Manager::new(client()?, &data_dir).await;
    tokio::spawn(mgr.clone().run());
    Ok(Some((mgr, listener)))
}

async fn serve(port: u16, open: bool) -> Result<()> {
    let Some((mgr, listener)) = start(port, None).await? else {
        println!("DownloaderX is already running");
        return Ok(());
    };
    if open {
        open_browser(&format!("http://127.0.0.1:{port}"));
    }
    server::serve(mgr, listener).await
}

/// Window + tray on the main thread, downloads and the local API on the tokio runtime.
#[cfg(windows)]
fn desktop(rt: tokio::runtime::Runtime, hidden: bool, setup_extension: bool) -> Result<()> {
    let panel = setup_extension.then_some("extension");
    let Some((mgr, listener)) = rt.block_on(start(DEFAULT_PORT, panel))? else { return Ok(()) };
    rt.spawn(server::serve(mgr.clone(), listener));
    gui::run(mgr, DEFAULT_PORT, hidden && !setup_extension, panel)
}

async fn get(url: String, output: Option<PathBuf>, connections: usize, retries: u32, limit: u64) -> Result<()> {
    let client = client()?;
    let progress = Arc::new(engine::Progress::default());
    let limiter = limiter::Limiter::new(limit * 1024);
    let cancel = CancellationToken::new();
    let target = match output {
        Some(p) => engine::Target::File(p),
        None => engine::Target::Dir { dir: ".".into(), name: None },
    };
    let opts = engine::Options { connections, retries, headers: Default::default() };

    let bar = ProgressBar::new(0);
    bar.set_style(
        ProgressStyle::with_template("[{bar:40.cyan/blue}] {bytes}/{total_bytes} {bytes_per_sec} ETA {eta}")?.progress_chars("=> "),
    );
    let ticker = {
        let (p, bar) = (progress.clone(), bar.clone());
        tokio::spawn(async move {
            loop {
                bar.set_length(p.total.load(Relaxed));
                bar.set_position(p.downloaded.load(Relaxed));
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
        })
    };
    {
        let cancel = cancel.clone();
        tokio::spawn(async move {
            tokio::signal::ctrl_c().await.ok();
            cancel.cancel();
        });
    }
    let result = engine::download(&client, &url, target, &opts, &progress, &limiter, &cancel).await;
    ticker.abort();
    bar.finish_and_clear();
    match result {
        Ok(p) => println!("saved {}", p.display()),
        Err(e) if e.is::<engine::Cancelled>() => {
            eprintln!("paused, run the same command again to resume");
            std::process::exit(1);
        }
        Err(e) => return Err(e),
    }
    Ok(())
}

fn open_browser(url: &str) {
    #[cfg(target_os = "windows")]
    let ok = win::shell_open(url);
    #[cfg(target_os = "macos")]
    let ok = std::process::Command::new("open").arg(url).spawn().is_ok();
    #[cfg(all(unix, not(target_os = "macos")))]
    let ok = std::process::Command::new("xdg-open").arg(url).spawn().is_ok();
    if !ok {
        println!("open {url} in your browser");
    }
}
