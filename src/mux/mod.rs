//! Built-in remuxer, so no ffmpeg is needed: MPEG-TS (H.264 / H.265 + AAC), raw ADTS
//! audio and MP4 / fragmented MP4 inputs are joined into one ordinary .mp4. Streams are
//! copied, never re-encoded.

mod bits;
mod codec;
mod mp4r;
mod mp4w;
mod ts;

use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Media {
    Video,
    Audio,
}

#[derive(Clone, Debug)]
pub struct Sample {
    /// Index into the list of files the sample data lives in.
    pub src: usize,
    pub offset: u64,
    pub size: u32,
    pub duration: u32,
    /// Composition (presentation) time minus decode time.
    pub cts: i32,
    pub sync: bool,
}

#[derive(Clone, Debug)]
pub struct Track {
    pub media: Media,
    pub timescale: u32,
    /// The complete sample entry box (avc1, hvc1, mp4a, av01, ...).
    pub entry: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub samples: Vec<Sample>,
    /// When the track's first shown sample plays on the timeline shared by all tracks
    /// of an input, in seconds; `None` when the input has no timestamps.
    pub start: Option<f64>,
    /// Media time, counted from the first sample, where presentation starts (skips the
    /// B-frame reorder delay and AAC priming).
    pub media_time: i64,
}

/// Tracks read from one input, plus the files their sample data is in.
struct Input {
    tracks: Vec<Track>,
    sources: Vec<PathBuf>,
    temps: Vec<PathBuf>,
}

/// Join `inputs` into `out`: the first video track found and the audio track from the
/// last input that has one (so a separate audio download wins over audio muxed in the video).
pub fn remux(inputs: &[PathBuf], out: &Path) -> Result<()> {
    let mut sources = Vec::new();
    let mut temps = Vec::new();
    let mut video = None;
    let mut audio = None;
    let result = (|| {
        for (i, path) in inputs.iter().enumerate() {
            let input = read_input(path, &out.with_extension(format!("mux{i}.tmp"))).with_context(|| format!("reading {}", path.display()))?;
            temps.extend(input.temps);
            let base = sources.len();
            sources.extend(input.sources);
            let mut audio_here = false;
            for mut t in input.tracks {
                t.samples.iter_mut().for_each(|s| s.src += base);
                match t.media {
                    Media::Video if video.is_none() => video = Some(t),
                    Media::Audio if !audio_here => {
                        audio = Some(t);
                        audio_here = true;
                    }
                    _ => {}
                }
            }
        }
        let tracks: Vec<Track> = video.take().into_iter().chain(audio.take()).filter(|t| !t.samples.is_empty()).collect();
        if tracks.is_empty() {
            bail!("no audio or video found");
        }
        mp4w::write(&tracks, &sources, out)
    })();
    for t in &temps {
        std::fs::remove_file(t).ok();
    }
    if result.is_err() {
        std::fs::remove_file(out).ok();
    }
    result
}

/// [`remux`] on a blocking thread.
pub async fn remux_async(inputs: Vec<PathBuf>, out: PathBuf) -> Result<()> {
    tokio::task::spawn_blocking(move || remux(&inputs, &out)).await?
}

fn read_input(path: &Path, temp: &Path) -> Result<Input> {
    let mut head = [0u8; 400];
    let n = File::open(path)?.read(&mut head)?;
    let head = &head[..n];
    if ts::looks_like(head) {
        let (tracks, files) = ts::demux(path, temp)?;
        return Ok(Input { tracks, sources: files.clone(), temps: files });
    }
    if head.len() >= 8 && matches!(&head[4..8], b"ftyp" | b"styp" | b"moov" | b"moof" | b"sidx" | b"free" | b"mdat") {
        return Ok(Input { tracks: mp4r::read(path)?, sources: vec![path.to_path_buf()], temps: vec![] });
    }
    if codec::looks_like_adts(head) {
        let (tracks, files) = ts::demux_adts(path, temp)?;
        return Ok(Input { tracks, sources: files.clone(), temps: files });
    }
    bail!("unknown container")
}

// --- box writing ---

pub(crate) fn boxed(kind: &[u8; 4], body: &[u8]) -> Vec<u8> {
    let mut b = Vec::with_capacity(body.len() + 8);
    b.extend_from_slice(&(body.len() as u32 + 8).to_be_bytes());
    b.extend_from_slice(kind);
    b.extend_from_slice(body);
    b
}

/// A box built by a closure that appends its body.
pub(crate) fn bx(kind: &[u8; 4], f: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut body = Vec::new();
    f(&mut body);
    boxed(kind, &body)
}

pub(crate) trait Put {
    fn u8(&mut self, v: u8);
    fn u16(&mut self, v: u16);
    fn u32(&mut self, v: u32);
    fn u64(&mut self, v: u64);
}

impl Put for Vec<u8> {
    fn u8(&mut self, v: u8) {
        self.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.extend_from_slice(&v.to_be_bytes());
    }
}
