//! MP4 reader for plain files and fragmented MP4 (HLS fMP4, DASH, YouTube streams).
//! Only the sample tables are read; the sample data stays in the file and is copied later.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

use anyhow::{Context, Result, bail};

use super::{Media, Sample, Track};

fn u32_at(b: &[u8], i: usize) -> Option<u32> {
    Some(u32::from_be_bytes(b.get(i..i + 4)?.try_into().ok()?))
}
fn u64_at(b: &[u8], i: usize) -> Option<u64> {
    Some(u64::from_be_bytes(b.get(i..i + 8)?.try_into().ok()?))
}

/// Child boxes of a box body: (type, body, offset of the box inside `data`).
fn children(data: &[u8]) -> Vec<([u8; 4], &[u8], usize)> {
    let mut out = Vec::new();
    let mut i = 0;
    while i + 8 <= data.len() {
        let mut size = u32_at(data, i).unwrap_or(0) as usize;
        let kind: [u8; 4] = data[i + 4..i + 8].try_into().unwrap();
        let mut head = 8;
        if size == 1 {
            size = u64_at(data, i + 8).unwrap_or(0) as usize;
            head = 16;
        } else if size == 0 {
            size = data.len() - i;
        }
        if size < head || i + size > data.len() {
            break;
        }
        out.push((kind, &data[i + head..i + size], i));
        i += size;
    }
    out
}

fn child<'a>(data: &'a [u8], kind: &[u8; 4]) -> Option<&'a [u8]> {
    children(data).into_iter().find(|c| &c.0 == kind).map(|c| c.1)
}

fn path<'a>(data: &'a [u8], kinds: &[&[u8; 4]]) -> Option<&'a [u8]> {
    kinds.iter().try_fold(data, |d, k| child(d, k))
}

#[derive(Default, Clone, Copy)]
struct Defaults {
    duration: u32,
    size: u32,
    flags: u32,
}

struct Trak {
    id: u32,
    track: Track,
    defaults: Defaults,
    /// Decode time of the first sample, and of the next fragment's first sample.
    first_dts: Option<i64>,
    next_dts: Option<i64>,
    /// Edit list: empty time before the track (movie timescale) and the media time shown first.
    empty: u64,
    shown_from: i64,
}

fn parse_trak(t: &[u8]) -> Result<Option<Trak>> {
    let tkhd = child(t, b"tkhd").context("trak without tkhd")?;
    let v1 = tkhd.first() == Some(&1);
    let id = u32_at(tkhd, if v1 { 20 } else { 12 }).context("short tkhd")?;
    let (w, h) = (tkhd.len().checked_sub(8).and_then(|i| u32_at(tkhd, i)), tkhd.len().checked_sub(4).and_then(|i| u32_at(tkhd, i)));
    let mdia = child(t, b"mdia").context("trak without mdia")?;
    let mdhd = child(mdia, b"mdhd").context("no mdhd")?;
    let timescale = u32_at(mdhd, if mdhd.first() == Some(&1) { 20 } else { 12 }).context("short mdhd")?;
    let handler = child(mdia, b"hdlr").and_then(|h| h.get(8..12)).context("no hdlr")?;
    let media = match handler {
        b"vide" => Media::Video,
        b"soun" => Media::Audio,
        _ => return Ok(None),
    };
    let stbl = path(mdia, &[b"minf", b"stbl"]).context("no stbl")?;
    let stsd = child(stbl, b"stsd").context("no stsd")?;
    let size = u32_at(stsd, 8).context("empty stsd")? as usize;
    let entry = stsd.get(8..8 + size).context("short stsd")?.to_vec();
    if matches!(&entry[4..8], b"encv" | b"enca") {
        bail!("this stream is DRM protected and can't be remuxed");
    }
    // Empty edits delay the track; the first edit with media gives where presentation starts.
    let (mut empty, mut shown_from) = (0u64, 0i64);
    if let Some(elst) = path(t, &[b"edts", b"elst"]) {
        let v1 = elst.first() == Some(&1);
        let n = u32_at(elst, 4).unwrap_or(0) as usize;
        let step = if v1 { 20 } else { 12 };
        for k in 0..n {
            let at = 8 + k * step;
            let (dur, mt) = if v1 {
                (u64_at(elst, at), u64_at(elst, at + 8).map(|v| v as i64))
            } else {
                (u32_at(elst, at).map(u64::from), u32_at(elst, at + 4).map(|v| v as i32 as i64))
            };
            match (dur, mt) {
                (_, Some(m)) if m >= 0 => {
                    shown_from = m;
                    break;
                }
                (Some(d), Some(_)) => empty += d,
                _ => break,
            }
        }
    }
    let samples = table_samples(stbl).unwrap_or_default();
    let first_dts = (!samples.is_empty()).then_some(0);
    Ok(Some(Trak {
        id,
        track: Track { media, timescale, entry, width: w.unwrap_or(0) >> 16, height: h.unwrap_or(0) >> 16, samples, start: None, media_time: 0 },
        defaults: Defaults::default(),
        first_dts,
        next_dts: None,
        empty,
        shown_from,
    }))
}

/// Samples of a non-fragmented track, from its sample tables.
fn table_samples(stbl: &[u8]) -> Option<Vec<Sample>> {
    let stsz = child(stbl, b"stsz")?;
    let fixed = u32_at(stsz, 4)?;
    let count = u32_at(stsz, 8)? as usize;
    if count == 0 {
        return Some(vec![]);
    }
    let sizes: Vec<u32> = (0..count).map(|i| if fixed != 0 { Some(fixed) } else { u32_at(stsz, 12 + 4 * i) }).collect::<Option<_>>()?;
    let chunks: Vec<u64> = if let Some(co) = child(stbl, b"stco") {
        (0..u32_at(co, 4)? as usize).map(|i| u32_at(co, 8 + 4 * i).map(u64::from)).collect::<Option<_>>()?
    } else {
        let co = child(stbl, b"co64")?;
        (0..u32_at(co, 4)? as usize).map(|i| u64_at(co, 8 + 8 * i)).collect::<Option<_>>()?
    };
    let stsc = child(stbl, b"stsc")?;
    let runs: Vec<(u32, u32)> = (0..u32_at(stsc, 4)? as usize).map(|i| Some((u32_at(stsc, 8 + 12 * i)?, u32_at(stsc, 12 + 12 * i)?))).collect::<Option<_>>()?;
    let mut durations = Vec::with_capacity(count);
    let stts = child(stbl, b"stts")?;
    for i in 0..u32_at(stts, 4)? as usize {
        let (n, d) = (u32_at(stts, 8 + 8 * i)?, u32_at(stts, 12 + 8 * i)?);
        durations.extend(std::iter::repeat_n(d, n as usize));
    }
    let mut cts = Vec::new();
    if let Some(ctts) = child(stbl, b"ctts") {
        for i in 0..u32_at(ctts, 4)? as usize {
            let (n, o) = (u32_at(ctts, 8 + 8 * i)?, u32_at(ctts, 12 + 8 * i)? as i32);
            cts.extend(std::iter::repeat_n(o, n as usize));
        }
    }
    let sync: Option<Vec<u32>> = child(stbl, b"stss").and_then(|s| (0..u32_at(s, 4)? as usize).map(|i| u32_at(s, 8 + 4 * i)).collect());

    let mut out = Vec::with_capacity(count);
    let mut n = 0usize;
    for (ci, &base) in chunks.iter().enumerate() {
        let chunk_no = ci as u32 + 1;
        let per = runs.iter().rev().find(|r| r.0 <= chunk_no).map_or(0, |r| r.1);
        let mut off = base;
        for _ in 0..per {
            if n >= count {
                break;
            }
            out.push(Sample {
                src: 0,
                offset: off,
                size: sizes[n],
                duration: durations.get(n).copied().unwrap_or(0),
                cts: cts.get(n).copied().unwrap_or(0),
                sync: sync.as_ref().is_none_or(|s| s.binary_search(&(n as u32 + 1)).is_ok()),
            });
            off += sizes[n] as u64;
            n += 1;
        }
    }
    Some(out)
}

/// Adds the samples of one moof (which starts at `moof_at` in the file) to the tracks.
fn parse_moof(moof: &[u8], moof_at: u64, tracks: &mut HashMap<u32, Trak>) -> Option<()> {
    for (kind, traf, _) in children(moof) {
        if &kind != b"traf" {
            continue;
        }
        let tfhd = child(traf, b"tfhd")?;
        let flags = u32_at(tfhd, 0)? & 0xFF_FFFF;
        let id = u32_at(tfhd, 4)?;
        let Some(Trak { track, defaults: trex, first_dts, next_dts, .. }) = tracks.get_mut(&id) else { continue };
        let mut at = 8;
        let mut base = moof_at;
        if flags & 1 != 0 {
            base = u64_at(tfhd, at)?;
            at += 8;
        }
        if flags & 2 != 0 {
            at += 4;
        }
        let mut def = *trex;
        if flags & 8 != 0 {
            def.duration = u32_at(tfhd, at)?;
            at += 4;
        }
        if flags & 0x10 != 0 {
            def.size = u32_at(tfhd, at)?;
            at += 4;
        }
        if flags & 0x20 != 0 {
            def.flags = u32_at(tfhd, at)?;
        }
        let tfdt = child(traf, b"tfdt").and_then(|t| if t[0] == 1 { u64_at(t, 4).map(|v| v as i64) } else { u32_at(t, 4).map(i64::from) });
        let mut dts = tfdt.or(*next_dts).unwrap_or(0);
        first_dts.get_or_insert(dts);
        let mut cursor = base;
        for (kind, trun, _) in children(traf) {
            if &kind != b"trun" {
                continue;
            }
            let tf = u32_at(trun, 0)? & 0xFF_FFFF;
            let count = u32_at(trun, 4)?;
            let mut i = 8;
            let mut pos = cursor;
            if tf & 1 != 0 {
                pos = (base as i64 + u32_at(trun, i)? as i32 as i64) as u64;
                i += 4;
            }
            let mut first_flags = None;
            if tf & 4 != 0 {
                first_flags = Some(u32_at(trun, i)?);
                i += 4;
            }
            for k in 0..count {
                let mut field = |bit: u32, default: u32| -> Option<u32> {
                    if tf & bit != 0 {
                        let v = u32_at(trun, i)?;
                        i += 4;
                        Some(v)
                    } else {
                        Some(default)
                    }
                };
                let duration = field(0x100, def.duration)?;
                let size = field(0x200, def.size)?;
                let mut sflags = field(0x400, def.flags)?;
                let cts = field(0x800, 0)? as i32;
                if k == 0 && tf & 0x400 == 0 {
                    sflags = first_flags.unwrap_or(sflags);
                }
                track.samples.push(Sample { src: 0, offset: pos, size, duration, cts, sync: sflags & 0x1_0000 == 0 });
                pos += size as u64;
                dts += duration as i64;
            }
            cursor = pos;
        }
        *next_dts = Some(dts);
    }
    Some(())
}

pub fn read(file_path: &Path) -> Result<Vec<Track>> {
    let mut f = File::open(file_path)?;
    let len = f.metadata()?.len();
    let mut moov: Option<Vec<u8>> = None;
    let mut tracks: HashMap<u32, Trak> = HashMap::new();
    let mut movie_scale = 1000u32;
    let mut order = Vec::new();
    let mut pos = 0u64;
    while pos + 8 <= len {
        f.seek(SeekFrom::Start(pos))?;
        let mut h = [0u8; 16];
        f.read_exact(&mut h[..8])?;
        let mut size = u32::from_be_bytes(h[..4].try_into().unwrap()) as u64;
        let kind: [u8; 4] = h[4..8].try_into().unwrap();
        let mut head = 8;
        if size == 1 {
            f.read_exact(&mut h[8..16])?;
            size = u64::from_be_bytes(h[8..16].try_into().unwrap());
            head = 16;
        } else if size == 0 {
            size = len - pos;
        }
        if size < head {
            break;
        }
        let size = size.min(len - pos);
        match &kind {
            b"moov" | b"moof" => {
                let mut body = vec![0u8; (size - head) as usize];
                f.read_exact(&mut body)?;
                if &kind == b"moov" {
                    if moov.is_none() {
                        let mut trex = HashMap::new();
                        if let Some(mvex) = child(&body, b"mvex") {
                            for (k, b, _) in children(mvex) {
                                if &k == b"trex" {
                                    if let (Some(id), Some(d), Some(s), Some(fl)) = (u32_at(b, 4), u32_at(b, 12), u32_at(b, 16), u32_at(b, 20)) {
                                        trex.insert(id, Defaults { duration: d, size: s, flags: fl });
                                    }
                                }
                            }
                        }
                        for (k, t, _) in children(&body) {
                            if &k == b"trak" {
                                if let Some(mut tr) = parse_trak(t)? {
                                    tr.defaults = trex.get(&tr.id).copied().unwrap_or_default();
                                    order.push(tr.id);
                                    tracks.insert(tr.id, tr);
                                }
                            }
                        }
                        if let Some(m) = child(&body, b"mvhd") {
                            movie_scale = u32_at(m, if m[0] == 1 { 20 } else { 12 }).filter(|&v| v > 0).unwrap_or(1000);
                        }
                        moov = Some(body);
                    }
                } else if !tracks.is_empty() {
                    parse_moof(&body, pos, &mut tracks).context("bad movie fragment")?;
                }
            }
            _ => {}
        }
        pos += size;
    }
    if moov.is_none() {
        bail!("MP4 without a moov box");
    }
    // Media time X plays at empty + (X - shown_from) on the movie timeline.
    Ok(order
        .into_iter()
        .filter_map(|id| tracks.remove(&id))
        .map(|t| {
            let mut track = t.track;
            let d0 = t.first_dts.unwrap_or(0);
            let ts = track.timescale.max(1) as f64;
            track.media_time = (t.shown_from - d0).max(0);
            track.start = Some(t.empty as f64 / movie_scale as f64 + (d0.max(t.shown_from) - t.shown_from) as f64 / ts);
            track
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn walks_boxes() {
        let data = [0, 0, 0, 12, b'f', b'r', b'e', b'e', 1, 2, 3, 4, 0, 0, 0, 8, b'm', b'd', b'a', b't'];
        let c = children(&data);
        assert_eq!(c.len(), 2);
        assert_eq!((&c[0].0, c[0].1), (b"free", &[1u8, 2, 3, 4][..]));
        assert_eq!(c[1].2, 12);
    }
}
