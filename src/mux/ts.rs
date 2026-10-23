//! MPEG-TS demuxer (H.264 / H.265 video, AAC audio) and raw ADTS reader. Sample data is
//! rewritten into temp files (length-prefixed NAL units, AAC without ADTS headers) that
//! the MP4 writer then copies from.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use super::codec::{self, Adts};
use super::{Media, Sample, Track};

const PTS_WRAP: i64 = 1 << 33;
const SECOND: i64 = 90_000;

pub fn looks_like(head: &[u8]) -> bool {
    !head.is_empty() && head[0] == 0x47 && (head.len() <= 188 || head[188] == 0x47)
}

#[derive(Clone, Copy, PartialEq)]
enum Codec {
    H264,
    H265,
    Aac,
}

struct Raw {
    offset: u64,
    size: u32,
    dts: i64,
    pts: i64,
    sync: bool,
}

/// Writes one track's rewritten samples to its own temp file.
struct Sink {
    out: BufWriter<File>,
    pos: u64,
}

impl Sink {
    fn new(path: &Path) -> Result<Self> {
        Ok(Self { out: BufWriter::with_capacity(1 << 20, File::create(path)?), pos: 0 })
    }
    fn put(&mut self, b: &[u8]) -> Result<()> {
        self.out.write_all(b)?;
        self.pos += b.len() as u64;
        Ok(())
    }
}

/// Undo the 33-bit timestamp wrap-around so times keep increasing.
#[derive(Default)]
struct Unwrap {
    last: Option<i64>,
    offset: i64,
}

impl Unwrap {
    fn fix(&mut self, raw: i64) -> i64 {
        if let Some(last) = self.last {
            if raw + self.offset < last - PTS_WRAP / 2 {
                self.offset += PTS_WRAP;
            }
        }
        let v = raw + self.offset;
        self.last = Some(v);
        v
    }
}

struct Video {
    codec: Codec,
    sink: Sink,
    vps: Vec<Vec<u8>>,
    sps: Vec<Vec<u8>>,
    pps: Vec<Vec<u8>>,
    samples: Vec<Raw>,
    clock: Unwrap,
}

impl Video {
    fn pes(&mut self, pts: Option<i64>, dts: Option<i64>, payload: &[u8]) -> Result<()> {
        let mut nals = Vec::new();
        let mut sync = false;
        for n in codec::nal_units(payload) {
            let (kind, set) = match self.codec {
                Codec::H264 => {
                    let t = n[0] & 0x1F;
                    sync |= t == 5;
                    (t, match t {
                        7 => Some(&mut self.sps),
                        8 => Some(&mut self.pps),
                        _ => None,
                    })
                }
                _ => {
                    let t = (n[0] >> 1) & 0x3F;
                    sync |= (16..=21).contains(&t);
                    (t, match t {
                        32 => Some(&mut self.vps),
                        33 => Some(&mut self.sps),
                        34 => Some(&mut self.pps),
                        _ => None,
                    })
                }
            };
            if let Some(set) = set {
                // Parameter sets go in the sample entry, not in the samples.
                if set.len() < 8 && !set.iter().any(|s| s == n) {
                    set.push(n.to_vec());
                }
                continue;
            }
            let aud = match self.codec {
                Codec::H264 => kind == 9,
                _ => kind == 35,
            };
            if !aud {
                nals.push(n);
            }
        }
        if nals.is_empty() {
            return Ok(());
        }
        let Some(pts) = pts else {
            // A PES without a time continues the previous frame.
            if let Some(last) = self.samples.last_mut() {
                if last.offset + last.size as u64 == self.sink.pos {
                    for n in nals {
                        self.sink.put(&(n.len() as u32).to_be_bytes())?;
                        self.sink.put(n)?;
                        last.size += 4 + n.len() as u32;
                    }
                }
            }
            return Ok(());
        };
        // Frames before the first keyframe can't be decoded.
        if self.samples.is_empty() && (!sync || self.sps.is_empty()) {
            return Ok(());
        }
        let dts = self.clock.fix(dts.unwrap_or(pts));
        let mut pts = pts;
        while pts < dts - PTS_WRAP / 2 {
            pts += PTS_WRAP;
        }
        let offset = self.sink.pos;
        for n in &nals {
            self.sink.put(&(n.len() as u32).to_be_bytes())?;
            self.sink.put(n)?;
        }
        self.samples.push(Raw { offset, size: (self.sink.pos - offset) as u32, dts, pts, sync });
        Ok(())
    }

    fn track(mut self) -> Result<Track> {
        self.sink.out.flush()?;
        let (entry, info) = match self.codec {
            Codec::H264 => codec::avc1(&self.sps, &self.pps)?,
            _ => codec::hvc1(&self.vps, &self.sps, &self.pps)?,
        };
        let raw = &self.samples;
        let mut good = 3003i64; // ~29.97 fps until a real frame step is seen
        let mut samples = Vec::with_capacity(raw.len());
        for (i, r) in raw.iter().enumerate() {
            let step = raw.get(i + 1).map(|n| n.dts - r.dts);
            // Jumps (stream discontinuities) keep the previous frame step.
            if let Some(d) = step.filter(|&d| d > 0 && d < 5 * SECOND) {
                good = d;
            }
            let cts = r.pts - r.dts;
            samples.push(Sample {
                src: 0,
                offset: r.offset,
                size: r.size,
                duration: step.filter(|&d| d > 0 && d < 5 * SECOND).unwrap_or(good) as u32,
                cts: if cts.abs() < 5 * SECOND { cts as i32 } else { 0 },
                sync: r.sync,
            });
        }
        let first_dts = raw.first().map_or(0, |r| r.dts);
        let first_pts = raw.iter().take(32).map(|r| r.pts).min().unwrap_or(first_dts);
        Ok(Track {
            media: Media::Video,
            timescale: SECOND as u32,
            entry,
            width: info.width,
            height: info.height,
            samples,
            start: Some(first_dts.max(first_pts) as f64 / SECOND as f64),
            media_time: (first_pts - first_dts).clamp(0, 5 * SECOND),
        })
    }
}

/// Splits AAC frames out of ADTS data that can be cut anywhere between packets.
struct Audio {
    sink: Sink,
    buf: Vec<u8>,
    /// Stream times (90 kHz) and the buffer position from which each applies.
    marks: Vec<(usize, i64)>,
    first: Option<Adts>,
    start: Option<i64>,
    /// Whether any stream time was seen (raw ADTS files may have none).
    timed: bool,
    /// Total duration so far, in samples at the AAC rate.
    elapsed: u64,
    samples: Vec<Sample>,
    clock: Unwrap,
}

impl Audio {
    fn new(sink: Sink) -> Self {
        Self { sink, buf: Vec::new(), marks: Vec::new(), first: None, start: None, timed: false, elapsed: 0, samples: Vec::new(), clock: Unwrap::default() }
    }

    fn feed(&mut self, pts: Option<i64>, data: &[u8]) -> Result<()> {
        if let Some(p) = pts {
            let p = self.clock.fix(p);
            self.marks.push((self.buf.len(), p));
        }
        self.buf.extend_from_slice(data);
        let mut pos = 0;
        loop {
            let rest = &self.buf[pos..];
            if rest.starts_with(b"ID3") && rest.len() >= 10 {
                let n = codec::skip_id3(rest);
                if rest.len() < n {
                    break;
                }
                if let Some(t) = id3_time(&rest[..n]) {
                    let t = self.clock.fix(t);
                    self.marks.push((pos + n, t));
                }
                pos += n;
                continue;
            }
            let Some(h) = codec::parse_adts(rest) else {
                if rest.len() < 7 {
                    break;
                }
                pos += 1; // lost sync: look for the next frame
                continue;
            };
            if rest.len() < h.frame_len {
                break;
            }
            let rate = h.rate() as u64;
            // The newest stream time at or before this frame.
            let mark = self.marks.iter().rev().find(|(at, _)| *at <= pos).map(|m| m.1);
            if let (Some(t), Some(start)) = (mark, self.start) {
                let expected = start + (self.elapsed * SECOND as u64 / rate) as i64;
                let gap = t - expected;
                // A forward jump (missing audio) becomes a longer previous frame.
                if gap > SECOND / 10 && gap < 30 * SECOND {
                    if let Some(last) = self.samples.last_mut() {
                        let extra = (gap as u64 * rate / SECOND as u64) as u32;
                        last.duration += extra;
                        self.elapsed += extra as u64;
                    }
                }
            }
            if self.start.is_none() {
                self.start = Some(mark.unwrap_or(0));
                self.timed = mark.is_some();
            }
            self.marks.retain(|(at, _)| *at > pos);
            let offset = self.sink.pos;
            self.sink.put(&rest[h.header_len..h.frame_len])?;
            self.samples.push(Sample { src: 0, offset, size: (h.frame_len - h.header_len) as u32, duration: 1024, cts: 0, sync: true });
            self.elapsed += 1024;
            pos += h.frame_len;
            self.first.get_or_insert(h);
        }
        self.buf.drain(..pos);
        for m in &mut self.marks {
            m.0 -= pos.min(m.0);
        }
        Ok(())
    }

    fn track(mut self) -> Result<Option<Track>> {
        self.sink.out.flush()?;
        let Some(h) = self.first else { return Ok(None) };
        let rate = h.rate();
        let start = self.start.filter(|_| self.timed);
        Ok(Some(Track {
            media: Media::Audio,
            timescale: rate,
            entry: codec::mp4a(&h)?,
            width: 0,
            height: 0,
            samples: self.samples,
            start: start.map(|s| s as f64 / SECOND as f64),
            media_time: 0,
        }))
    }
}

/// HLS audio segments carry their start time in an ID3 PRIV frame.
fn id3_time(tag: &[u8]) -> Option<i64> {
    const OWNER: &[u8] = b"com.apple.streaming.transportStreamTimestamp\0";
    let at = tag.windows(OWNER.len()).position(|w| w == OWNER)? + OWNER.len();
    let b: [u8; 8] = tag.get(at..at + 8)?.try_into().ok()?;
    Some((u64::from_be_bytes(b) & (PTS_WRAP as u64 - 1)) as i64)
}

fn read_pts(b: &[u8]) -> i64 {
    (((b[0] as i64 >> 1) & 7) << 30) | ((b[1] as i64) << 22) | ((b[2] as i64 >> 1) << 15) | ((b[3] as i64) << 7) | (b[4] as i64 >> 1)
}

/// Returns (pts, dts, payload) of a complete PES packet.
fn parse_pes(p: &[u8]) -> Option<(Option<i64>, Option<i64>, &[u8])> {
    if p.len() < 9 || p[..3] != [0, 0, 1] {
        return None;
    }
    let flags = p[7] >> 6;
    let start = 9 + p[8] as usize;
    let pts = (flags & 2 != 0 && p.len() >= 14).then(|| read_pts(&p[9..14]));
    let dts = (flags == 3 && p.len() >= 19).then(|| read_pts(&p[14..19]));
    Some((pts, dts, p.get(start..)?))
}

pub fn demux(path: &Path, temp: &Path) -> Result<(Vec<Track>, Vec<PathBuf>)> {
    let vpath = temp.with_extension("v.tmp");
    let apath = temp.with_extension("a.tmp");
    let mut input = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut pmt_pids = Vec::new();
    let mut streams: HashMap<u16, Codec> = HashMap::new();
    let (mut vpid, mut apid) = (None, None);
    let mut video: Option<Video> = None;
    let mut audio: Option<Audio> = None;
    let mut pes: HashMap<u16, Vec<u8>> = HashMap::new();
    let mut unsupported = None;

    let mut pkt = [0u8; 188];
    loop {
        match input.read_exact(&mut pkt[..1]) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(e.into()),
        }
        if pkt[0] != 0x47 {
            continue; // resync byte by byte
        }
        if input.read_exact(&mut pkt[1..]).is_err() {
            break;
        }
        let start = pkt[1] & 0x40 != 0;
        let pid = (((pkt[1] & 0x1F) as u16) << 8) | pkt[2] as u16;
        let afc = (pkt[3] >> 4) & 3;
        let mut at = 4;
        if afc & 2 != 0 {
            at += 1 + pkt[4] as usize;
        }
        if afc & 1 == 0 || at >= 188 {
            continue;
        }
        let payload = &pkt[at..];

        if pid == 0 || pmt_pids.contains(&pid) {
            if !start {
                continue;
            }
            let sec = payload.get(1 + payload[0] as usize..).unwrap_or(&[]);
            if sec.len() < 8 {
                continue;
            }
            let len = (((sec[1] & 0x0F) as usize) << 8 | sec[2] as usize).min(sec.len().saturating_sub(3));
            let end = (3 + len).saturating_sub(4);
            if pid == 0 && sec[0] == 0 {
                let mut i = 8;
                while i + 4 <= end {
                    let program = u16::from_be_bytes([sec[i], sec[i + 1]]);
                    let p = (((sec[i + 2] & 0x1F) as u16) << 8) | sec[i + 3] as u16;
                    if program != 0 && !pmt_pids.contains(&p) {
                        pmt_pids.push(p);
                    }
                    i += 4;
                }
            } else if sec[0] == 2 && sec.len() >= 12 {
                let info = (((sec[10] & 0x0F) as usize) << 8) | sec[11] as usize;
                let mut i = 12 + info;
                while i + 5 <= end {
                    let kind = sec[i];
                    let p = (((sec[i + 1] & 0x1F) as u16) << 8) | sec[i + 2] as u16;
                    let es = (((sec[i + 3] & 0x0F) as usize) << 8) | sec[i + 4] as usize;
                    let codec = match kind {
                        0x1B => Some(Codec::H264),
                        0x24 => Some(Codec::H265),
                        0x0F => Some(Codec::Aac),
                        _ => None,
                    };
                    match codec {
                        Some(c) => {
                            streams.insert(p, c);
                        }
                        None if matches!(kind, 0x02 | 0x03 | 0x04 | 0x11 | 0x81 | 0x87 | 0xC1 | 0xC2 | 0xCF | 0xDB) => unsupported = Some(kind),
                        None => {}
                    }
                    if c_is_video(codec) && vpid.is_none() {
                        vpid = Some(p);
                        video = Some(Video { codec: codec.unwrap(), sink: Sink::new(&vpath)?, vps: vec![], sps: vec![], pps: vec![], samples: vec![], clock: Unwrap::default() });
                    }
                    if codec == Some(Codec::Aac) && apid.is_none() {
                        apid = Some(p);
                        audio = Some(Audio::new(Sink::new(&apath)?));
                    }
                    i += 5 + es;
                }
            }
            continue;
        }
        if Some(pid) != vpid && Some(pid) != apid {
            continue;
        }
        if start {
            if let Some(old) = pes.insert(pid, payload.to_vec()) {
                flush(pid, old, (vpid, apid), &mut video, &mut audio)?;
            }
        } else if let Some(buf) = pes.get_mut(&pid) {
            buf.extend_from_slice(payload);
        }
    }
    for (pid, data) in std::mem::take(&mut pes) {
        flush(pid, data, (vpid, apid), &mut video, &mut audio)?;
    }

    let mut tracks = Vec::new();
    let mut files = Vec::new();
    if let Some(v) = video.filter(|v| !v.samples.is_empty()) {
        tracks.push(v.track()?);
        files.push(vpath.clone());
    }
    if let Some(t) = audio.map(Audio::track).transpose()?.flatten() {
        let src = files.len();
        let mut t = t;
        t.samples.iter_mut().for_each(|s| s.src = src);
        tracks.push(t);
        files.push(apath.clone());
    }
    if tracks.is_empty() {
        std::fs::remove_file(&vpath).ok();
        std::fs::remove_file(&apath).ok();
        match unsupported {
            Some(k) => bail!("unsupported stream type 0x{k:02X} in MPEG-TS"),
            None => bail!("no H.264, H.265 or AAC stream in MPEG-TS"),
        }
    }
    if !files.contains(&vpath) {
        std::fs::remove_file(&vpath).ok();
    }
    if !files.contains(&apath) {
        std::fs::remove_file(&apath).ok();
    }
    Ok((tracks, files))
}

fn flush(pid: u16, data: Vec<u8>, (vpid, apid): (Option<u16>, Option<u16>), video: &mut Option<Video>, audio: &mut Option<Audio>) -> Result<()> {
    let Some((pts, dts, payload)) = parse_pes(&data) else { return Ok(()) };
    if Some(pid) == vpid {
        if let Some(v) = video {
            v.pes(pts, dts, payload)?;
        }
    } else if Some(pid) == apid {
        if let Some(a) = audio {
            a.feed(pts, payload)?;
        }
    }
    Ok(())
}

fn c_is_video(c: Option<Codec>) -> bool {
    matches!(c, Some(Codec::H264 | Codec::H265))
}

/// A raw .aac (ADTS) file, as used by some HLS audio renditions.
pub fn demux_adts(path: &Path, temp: &Path) -> Result<(Vec<Track>, Vec<PathBuf>)> {
    let apath = temp.with_extension("a.tmp");
    let mut a = Audio::new(Sink::new(&apath)?);
    let mut input = BufReader::with_capacity(1 << 20, File::open(path)?);
    let mut chunk = vec![0u8; 1 << 20];
    loop {
        let n = input.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        a.feed(None, &chunk[..n])?;
    }
    let t = a.track()?.context("no AAC frames found")?;
    Ok((vec![t], vec![apath]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unwraps_timestamps() {
        let mut u = Unwrap::default();
        assert_eq!(u.fix(PTS_WRAP - 10), PTS_WRAP - 10);
        assert_eq!(u.fix(5), PTS_WRAP + 5);
    }

    #[test]
    fn pts_bytes() {
        // PTS 0x1_2345_6789 encoded with marker bits
        let v: i64 = 0x1_2345_6789;
        let b = [
            0x21 | (((v >> 30) & 7) << 1) as u8,
            (v >> 22) as u8,
            (((v >> 15) & 0x7F) << 1) as u8 | 1,
            (v >> 7) as u8,
            ((v & 0x7F) << 1) as u8 | 1,
        ];
        assert_eq!(read_pts(&b), v);
    }
}
