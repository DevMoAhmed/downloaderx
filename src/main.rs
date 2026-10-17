mod engine;
mod hls;
mod limiter;
mod queue;
mod server;
mod ytdlp;

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
}

#[derive(Subcommand)]
enum Cmd {
    /// Run the download manager (dashboard + browser extension API). Default.
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
}

pub fn client() -> Result<Client> {
    Ok(Client::builder()
        .user_agent(concat!("Mozilla/5.0 (compatible; downloaderx/", env!("CARGO_PKG_VERSION"), ")"))
        .connect_timeout(Duration::from_secs(15))
        .read_timeout(Duration::from_secs(30))
        .build()?)
}

#[tokio::main]
async fn main() -> Result<()> {
    match Args::parse().cmd.unwrap_or(Cmd::Serve { port: DEFAULT_PORT, no_open: false }) {
        Cmd::Serve { port, no_open } => serve(port, !no_open).await,
        Cmd::Get { url, output, connections, retries, limit } => get(url, output, connections, retries, limit).await,
    }
}

async fn serve(port: u16, open: bool) -> Result<()> {
    let data_dir = dirs::data_dir().unwrap_or_else(|| ".".into()).join("downloaderx");
    let mgr = queue::Manager::new(client()?, &data_dir).await;
    if !mgr.ytdlp {
        eprintln!("note: yt-dlp not found, site videos (YouTube etc.) won't be available");
    }
    if !mgr.ffmpeg {
        eprintln!("note: ffmpeg not found, HLS videos are saved as .ts and qualities above 720p on some sites may lack audio");
    }
    tokio::spawn(mgr.clone().run());
    if open {
        open_browser(&format!("http://127.0.0.1:{port}"));
    }
    server::serve(mgr, port).await
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
    let r = std::process::Command::new("cmd").args(["/C", "start", "", url]).spawn();
    #[cfg(target_os = "macos")]
    let r = std::process::Command::new("open").arg(url).spawn();
    #[cfg(all(unix, not(target_os = "macos")))]
    let r = std::process::Command::new("xdg-open").arg(url).spawn();
    if r.is_err() {
        println!("open {url} in your browser");
    }
}
