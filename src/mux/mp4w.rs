//! MP4 writer: copies the samples of up to one video and one audio track into a single
//! file (ftyp, mdat, moov), interleaved in half-second chunks so playback and seeking
//! don't jump around the file.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use anyhow::Result;

use super::{Media, Put, Track, bx};

const MOVIE_SCALE: u64 = 1000;
const MATRIX: [u32; 9] = [0x0001_0000, 0, 0, 0, 0x0001_0000, 0, 0, 0, 0x4000_0000];

struct Chunk {
    track: usize,
    first: usize,
    count: usize,
    time: f64,
}

pub fn write(tracks: &[Track], sources: &[PathBuf], out: &Path) -> Result<()> {
    // Tracks without a known start line up with the earliest one.
    let start = tracks.iter().filter_map(|t| t.start).fold(f64::INFINITY, f64::min);
    let delays: Vec<f64> = tracks.iter().map(|t| t.start.map_or(0.0, |s| (s - start).max(0.0))).collect();

    let mut chunks = Vec::new();
    for (ti, t) in tracks.iter().enumerate() {
        let (mut first, mut dts, mut chunk_start, mut bytes) = (0usize, 0u64, 0u64, 0u64);
        for (i, s) in t.samples.iter().enumerate() {
            if i > first && (dts - chunk_start >= t.timescale as u64 / 2 || bytes >= 1 << 20) {
                chunks.push(Chunk { track: ti, first, count: i - first, time: delays[ti] + chunk_start as f64 / t.timescale as f64 });
                (first, chunk_start, bytes) = (i, dts, 0);
            }
            dts += s.duration as u64;
            bytes += s.size as u64;
        }
        if first < t.samples.len() {
            chunks.push(Chunk { track: ti, first, count: t.samples.len() - first, time: delays[ti] + chunk_start as f64 / t.timescale as f64 });
        }
    }
    chunks.sort_by(|a, b| a.time.total_cmp(&b.time));

    let mut f = BufWriter::with_capacity(1 << 20, File::create(out)?);
    let ftyp = bx(b"ftyp", |b| {
        b.extend_from_slice(b"isom");
        b.u32(0x200);
        b.extend_from_slice(b"isomiso2avc1mp41");
    });
    f.write_all(&ftyp)?;
    let mdat_at = ftyp.len() as u64;
    f.write_all(&[0, 0, 0, 1, b'm', b'd', b'a', b't', 0, 0, 0, 0, 0, 0, 0, 0])?;
    let mut pos = mdat_at + 16;

    let mut files: Vec<Option<File>> = sources.iter().map(|_| None).collect();
    let mut offsets: Vec<Vec<u64>> = vec![Vec::new(); tracks.len()];
    let mut buf = vec![0u8; 1 << 20];
    for c in &chunks {
        offsets[c.track].push(pos);
        let samples = &tracks[c.track].samples[c.first..c.first + c.count];
        let mut i = 0;
        while i < samples.len() {
            // Copy runs of samples that sit next to each other in the source in one go.
            let s = &samples[i];
            let mut end = s.offset + s.size as u64;
            let mut j = i + 1;
            while j < samples.len() && samples[j].src == s.src && samples[j].offset == end {
                end += samples[j].size as u64;
                j += 1;
            }
            let src = match &mut files[s.src] {
                Some(file) => file,
                slot => slot.insert(File::open(&sources[s.src])?),
            };
            src.seek(SeekFrom::Start(s.offset))?;
            let mut left = end - s.offset;
            while left > 0 {
                let n = left.min(buf.len() as u64) as usize;
                src.read_exact(&mut buf[..n])?;
                f.write_all(&buf[..n])?;
                left -= n as u64;
            }
            pos += end - s.offset;
            i = j;
        }
    }
    let mut file = f.into_inner().map_err(|e| e.into_error())?;
    file.seek(SeekFrom::Start(mdat_at + 8))?;
    file.write_all(&(pos - mdat_at).to_be_bytes())?;
    file.seek(SeekFrom::Start(pos))?;

    let mut per_track_chunks: Vec<Vec<usize>> = vec![Vec::new(); tracks.len()];
    for c in &chunks {
        per_track_chunks[c.track].push(c.count);
    }
    let mut traks = Vec::new();
    let mut movie_duration = 0u64;
    for (i, t) in tracks.iter().enumerate() {
        let media_dur: u64 = t.samples.iter().map(|s| s.duration as u64).sum();
        // Shown until the last frame in presentation order ends (B-frames reorder).
        let mut dts = 0i64;
        let mut end = 0i64;
        for x in &t.samples {
            end = end.max(dts + x.cts as i64 + x.duration as i64);
            dts += x.duration as i64;
        }
        let shown = (end - t.media_time).max(0) as u64;
        let delay = (delays[i] * MOVIE_SCALE as f64).round() as u64;
        let total = delay + shown * MOVIE_SCALE / t.timescale as u64;
        movie_duration = movie_duration.max(total);
        traks.push(trak(t, i as u32 + 1, media_dur, delay, total, &per_track_chunks[i], &offsets[i]));
    }
    let moov = bx(b"moov", |b| {
        b.extend_from_slice(&bx(b"mvhd", |m| {
            m.u32(1 << 24);
            m.u64(0);
            m.u64(0);
            m.u32(MOVIE_SCALE as u32);
            m.u64(movie_duration);
            m.u32(0x0001_0000);
            m.u16(0x0100);
            m.extend_from_slice(&[0; 10]);
            MATRIX.iter().for_each(|v| m.u32(*v));
            m.extend_from_slice(&[0; 24]);
            m.u32(tracks.len() as u32 + 1);
        }));
        traks.iter().for_each(|t| b.extend_from_slice(t));
    });
    file.write_all(&moov)?;
    file.flush()?;
    Ok(())
}

fn trak(t: &Track, id: u32, media_dur: u64, delay: u64, total: u64, chunk_sizes: &[usize], offsets: &[u64]) -> Vec<u8> {
    let video = t.media == Media::Video;
    bx(b"trak", |b| {
        b.extend_from_slice(&bx(b"tkhd", |h| {
            h.u32((1 << 24) | 3);
            h.u64(0);
            h.u64(0);
            h.u32(id);
            h.u32(0);
            h.u64(total);
            h.extend_from_slice(&[0; 8]);
            h.u16(0);
            h.u16(if video { 0 } else { 1 });
            h.u16(if video { 0 } else { 0x0100 });
            h.u16(0);
            MATRIX.iter().for_each(|v| h.u32(*v));
            h.u32(t.width << 16);
            h.u32(t.height << 16);
        }));
        if delay > 0 || t.media_time != 0 {
            b.extend_from_slice(&bx(b"edts", |e| {
                e.extend_from_slice(&bx(b"elst", |l| {
                    l.u32(1 << 24);
                    l.u32(if delay > 0 { 2 } else { 1 });
                    if delay > 0 {
                        l.u64(delay);
                        l.u64(u64::MAX); // -1: empty edit
                        l.u32(0x0001_0000);
                    }
                    l.u64(total - delay);
                    l.u64(t.media_time as u64);
                    l.u32(0x0001_0000);
                }));
            }));
        }
        b.extend_from_slice(&bx(b"mdia", |m| {
            m.extend_from_slice(&bx(b"mdhd", |h| {
                h.u32(1 << 24);
                h.u64(0);
                h.u64(0);
                h.u32(t.timescale);
                h.u64(media_dur);
                h.u16(0x55C4); // "und"
                h.u16(0);
            }));
            m.extend_from_slice(&bx(b"hdlr", |h| {
                h.u32(0);
                h.u32(0);
                h.extend_from_slice(if video { b"vide" } else { b"soun" });
                h.extend_from_slice(&[0; 12]);
                h.extend_from_slice(if video { b"VideoHandler\0" } else { b"SoundHandler\0" });
            }));
            m.extend_from_slice(&bx(b"minf", |n| {
                if video {
                    n.extend_from_slice(&bx(b"vmhd", |v| {
                        v.u32(1);
                        v.extend_from_slice(&[0; 8]);
                    }));
                } else {
                    n.extend_from_slice(&bx(b"smhd", |s| {
                        s.u32(0);
                        s.u32(0);
                    }));
                }
                n.extend_from_slice(&bx(b"dinf", |d| {
                    d.extend_from_slice(&bx(b"dref", |r| {
                        r.u32(0);
                        r.u32(1);
                        r.extend_from_slice(&bx(b"url ", |u| u.u32(1)));
                    }));
                }));
                n.extend_from_slice(&stbl(t, chunk_sizes, offsets));
            }));
        }));
    })
}

fn stbl(t: &Track, chunk_sizes: &[usize], offsets: &[u64]) -> Vec<u8> {
    let s = &t.samples;
    bx(b"stbl", |b| {
        b.extend_from_slice(&bx(b"stsd", |d| {
            d.u32(0);
            d.u32(1);
            d.extend_from_slice(&t.entry);
        }));
        b.extend_from_slice(&bx(b"stts", |d| {
            let runs = runs(s.iter().map(|x| x.duration));
            d.u32(0);
            d.u32(runs.len() as u32);
            runs.iter().for_each(|(n, v)| {
                d.u32(*n);
                d.u32(*v);
            });
        }));
        if s.iter().any(|x| x.cts != 0) {
            b.extend_from_slice(&bx(b"ctts", |d| {
                let runs = runs(s.iter().map(|x| x.cts as u32));
                d.u32(if s.iter().any(|x| x.cts < 0) { 1 << 24 } else { 0 });
                d.u32(runs.len() as u32);
                runs.iter().for_each(|(n, v)| {
                    d.u32(*n);
                    d.u32(*v);
                });
            }));
        }
        if !s.iter().all(|x| x.sync) {
            b.extend_from_slice(&bx(b"stss", |d| {
                let keys: Vec<u32> = s.iter().enumerate().filter(|(_, x)| x.sync).map(|(i, _)| i as u32 + 1).collect();
                d.u32(0);
                d.u32(keys.len() as u32);
                keys.iter().for_each(|k| d.u32(*k));
            }));
        }
        b.extend_from_slice(&bx(b"stsc", |d| {
            let mut entries: Vec<(u32, u32)> = Vec::new();
            for (i, &n) in chunk_sizes.iter().enumerate() {
                if entries.last().is_none_or(|e| e.1 != n as u32) {
                    entries.push((i as u32 + 1, n as u32));
                }
            }
            d.u32(0);
            d.u32(entries.len() as u32);
            entries.iter().for_each(|(first, n)| {
                d.u32(*first);
                d.u32(*n);
                d.u32(1);
            });
        }));
        b.extend_from_slice(&bx(b"stsz", |d| {
            d.u32(0);
            d.u32(0);
            d.u32(s.len() as u32);
            s.iter().for_each(|x| d.u32(x.size));
        }));
        b.extend_from_slice(&bx(b"co64", |d| {
            d.u32(0);
            d.u32(offsets.len() as u32);
            offsets.iter().for_each(|o| d.u64(*o));
        }));
    })
}

/// Run-length encode (count, value).
fn runs(values: impl Iterator<Item = u32>) -> Vec<(u32, u32)> {
    let mut out: Vec<(u32, u32)> = Vec::new();
    for v in values {
        match out.last_mut() {
            Some(last) if last.1 == v => last.0 += 1,
            _ => out.push((1, v)),
        }
    }
    out
}
