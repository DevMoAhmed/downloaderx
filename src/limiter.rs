//! Global token-bucket speed limit shared by every download.

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use tokio::sync::Mutex;

pub struct Limiter {
    /// Bytes per second; 0 = unlimited.
    rate: AtomicU64,
    bucket: Mutex<(f64, Instant)>,
}

impl Limiter {
    pub fn new(rate: u64) -> Self {
        Self { rate: AtomicU64::new(rate), bucket: Mutex::new((0.0, Instant::now())) }
    }

    pub fn set_rate(&self, rate: u64) {
        self.rate.store(rate, Relaxed);
    }

    /// Wait until `n` bytes may pass. The bucket may go into debt, which turns into sleep.
    pub async fn take(&self, n: u64) {
        let rate = self.rate.load(Relaxed);
        if rate == 0 || n == 0 {
            return;
        }
        let wait = {
            let mut b = self.bucket.lock().await;
            let now = Instant::now();
            let rate = rate as f64;
            b.0 = (b.0 + now.duration_since(b.1).as_secs_f64() * rate).min(rate);
            b.1 = now;
            b.0 -= n as f64;
            if b.0 < 0.0 { -b.0 / rate } else { 0.0 }
        };
        if wait > 0.0 {
            tokio::time::sleep(Duration::from_secs_f64(wait)).await;
        }
    }
}
