//! Codec headers: H.264 / H.265 parameter sets and AAC (ADTS), turned into MP4 sample entries.

use anyhow::{Context, Result, bail};

use super::bits::{Bits, unescape};
use super::{Put, boxed, bx};

/// Split an Annex-B byte stream (00 00 01 start codes) into NAL units.
pub fn nal_units(data: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut start = None;
    let mut i = 0;
    while i + 2 < data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            if let Some(s) = start {
                out.push(trim_zeros(&data[s..i]));
            }
            i += 3;
            start = Some(i);
        } else {
            i += 1;
        }
    }
    if let Some(s) = start {
        out.push(trim_zeros(&data[s..]));
    }
    out.retain(|n| !n.is_empty());
    out
}

/// Drop the zero byte of a 4-byte start code that belongs to the next NAL.
fn trim_zeros(n: &[u8]) -> &[u8] {
    let mut end = n.len();
    while end > 0 && n[end - 1] == 0 {
        end -= 1;
    }
    &n[..end]
}

pub struct VideoInfo {
    pub width: u32,
    pub height: u32,
}

/// Width and height plus the avcC fields, from an H.264 SPS NAL.
pub struct AvcSps {
    pub info: VideoInfo,
    profile: u8,
    compat: u8,
    level: u8,
    chroma: u32,
    depth_luma: u32,
    depth_chroma: u32,
}

pub fn parse_avc_sps(nal: &[u8]) -> Option<AvcSps> {
    let raw = unescape(nal);
    let mut b = Bits::new(raw.get(1..)?);
    let profile = b.bits(8)? as u8;
    let compat = b.bits(8)? as u8;
    let level = b.bits(8)? as u8;
    b.ue()?;
    let (mut chroma, mut depth_luma, mut depth_chroma) = (1, 0, 0);
    if matches!(profile, 100 | 110 | 122 | 244 | 44 | 83 | 86 | 118 | 128 | 138 | 139 | 134 | 135) {
        chroma = b.ue()?;
        if chroma == 3 {
            b.bit()?;
        }
        depth_luma = b.ue()?;
        depth_chroma = b.ue()?;
        b.bit()?;
        if b.bit()? == 1 {
            for i in 0..if chroma == 3 { 12 } else { 8 } {
                if b.bit()? == 1 {
                    let size = if i < 6 { 16 } else { 64 };
                    let (mut last, mut next) = (8i32, 8i32);
                    for _ in 0..size {
                        if next != 0 {
                            next = (last + b.se()? + 256) % 256;
                        }
                        if next != 0 {
                            last = next;
                        }
                    }
                }
            }
        }
    }
    b.ue()?;
    match b.ue()? {
        0 => {
            b.ue()?;
        }
        1 => {
            b.bit()?;
            b.se()?;
            b.se()?;
            for _ in 0..b.ue()? {
                b.se()?;
            }
        }
        _ => {}
    }
    b.ue()?;
    b.bit()?;
    let w_mbs = b.ue()? + 1;
    let h_units = b.ue()? + 1;
    let frame_mbs_only = b.bit()?;
    if frame_mbs_only == 0 {
        b.bit()?;
    }
    b.bit()?;
    let (mut cl, mut cr, mut ct, mut cb) = (0, 0, 0, 0);
    if b.bit()? == 1 {
        (cl, cr, ct, cb) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
    }
    let (sub_w, sub_h) = match chroma {
        1 => (2, 2),
        2 => (2, 1),
        _ => (1, 1),
    };
    let (unit_x, unit_y) = if chroma == 0 { (1, 2 - frame_mbs_only) } else { (sub_w, sub_h * (2 - frame_mbs_only)) };
    let width = (w_mbs * 16).checked_sub((cl + cr) * unit_x)?;
    let height = ((2 - frame_mbs_only) * h_units * 16).checked_sub((ct + cb) * unit_y)?;
    Some(AvcSps { info: VideoInfo { width, height }, profile, compat, level, chroma, depth_luma, depth_chroma })
}

/// avc1 sample entry with its avcC box.
pub fn avc1(sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Result<(Vec<u8>, VideoInfo)> {
    let first = sps.first().context("H.264 stream without SPS")?;
    let s = parse_avc_sps(first).context("unreadable H.264 SPS")?;
    let mut c = vec![1, s.profile, s.compat, s.level, 0xFF, 0xE0 | sps.len() as u8];
    for n in sps {
        c.u16(n.len() as u16);
        c.extend_from_slice(n);
    }
    c.u8(pps.len() as u8);
    for n in pps {
        c.u16(n.len() as u16);
        c.extend_from_slice(n);
    }
    if !matches!(s.profile, 66 | 77 | 88) {
        c.extend_from_slice(&[0xFC | s.chroma as u8, 0xF8 | s.depth_luma as u8, 0xF8 | s.depth_chroma as u8, 0]);
    }
    let entry = visual_entry(b"avc1", s.info.width, s.info.height, &boxed(b"avcC", &c));
    Ok((entry, s.info))
}

/// Width, height and the hvcC fields from an H.265 SPS NAL.
struct HevcSps {
    info: VideoInfo,
    ptl: [u8; 12],
    chroma: u32,
    depth_luma: u32,
    depth_chroma: u32,
}

fn parse_hevc_sps(nal: &[u8]) -> Option<HevcSps> {
    let raw = unescape(nal);
    let mut b = Bits::new(raw.get(2..)?);
    b.bits(4)?;
    let sub_layers = b.bits(3)?;
    b.bit()?;
    let mut ptl = [0u8; 12];
    for p in &mut ptl {
        *p = b.bits(8)? as u8;
    }
    let mut present = Vec::new();
    for _ in 0..sub_layers {
        present.push((b.bit()?, b.bit()?));
    }
    if sub_layers > 0 {
        for _ in sub_layers..8 {
            b.bits(2)?;
        }
    }
    for (profile, level) in present {
        if profile == 1 {
            b.skip(88)?;
        }
        if level == 1 {
            b.skip(8)?;
        }
    }
    b.ue()?;
    let chroma = b.ue()?;
    if chroma == 3 {
        b.bit()?;
    }
    let mut width = b.ue()?;
    let mut height = b.ue()?;
    if b.bit()? == 1 {
        let (sub_w, sub_h) = match chroma {
            1 => (2, 2),
            2 => (2, 1),
            _ => (1, 1),
        };
        let (l, r, t, bo) = (b.ue()?, b.ue()?, b.ue()?, b.ue()?);
        width = width.checked_sub((l + r) * sub_w)?;
        height = height.checked_sub((t + bo) * sub_h)?;
    }
    let depth_luma = b.ue()?;
    let depth_chroma = b.ue()?;
    Some(HevcSps { info: VideoInfo { width, height }, ptl, chroma, depth_luma, depth_chroma })
}

/// hvc1 sample entry with its hvcC box, from the VPS, SPS and PPS NAL units.
pub fn hvc1(vps: &[Vec<u8>], sps: &[Vec<u8>], pps: &[Vec<u8>]) -> Result<(Vec<u8>, VideoInfo)> {
    let first = sps.first().context("H.265 stream without SPS")?;
    let s = parse_hevc_sps(first).context("unreadable H.265 SPS")?;
    let mut c = vec![1];
    c.extend_from_slice(&s.ptl);
    c.u16(0xF000);
    c.u8(0xFC);
    c.u8(0xFC | s.chroma as u8);
    c.u8(0xF8 | s.depth_luma as u8);
    c.u8(0xF8 | s.depth_chroma as u8);
    c.u16(0);
    c.u8(0x0F); // 1 temporal layer, nested, 4-byte NAL lengths
    let arrays = [(32u8, vps), (33, sps), (34, pps)];
    c.u8(arrays.iter().filter(|(_, v)| !v.is_empty()).count() as u8);
    for (kind, nals) in arrays {
        if nals.is_empty() {
            continue;
        }
        c.u8(0x80 | kind);
        c.u16(nals.len() as u16);
        for n in nals {
            c.u16(n.len() as u16);
            c.extend_from_slice(n);
        }
    }
    let entry = visual_entry(b"hvc1", s.info.width, s.info.height, &boxed(b"hvcC", &c));
    Ok((entry, s.info))
}

fn visual_entry(kind: &[u8; 4], width: u32, height: u32, config: &[u8]) -> Vec<u8> {
    bx(kind, |b| {
        b.extend_from_slice(&[0; 6]);
        b.u16(1); // data reference index
        b.extend_from_slice(&[0; 16]);
        b.u16(width as u16);
        b.u16(height as u16);
        b.u32(0x0048_0000);
        b.u32(0x0048_0000);
        b.u32(0);
        b.u16(1); // frame count
        b.extend_from_slice(&[0; 32]); // compressor name
        b.u16(0x0018);
        b.u16(0xFFFF);
        b.extend_from_slice(config);
    })
}

// --- AAC ---

const RATES: [u32; 13] = [96000, 88200, 64000, 48000, 44100, 32000, 24000, 22050, 16000, 12000, 11025, 8000, 7350];

pub struct Adts {
    pub header_len: usize,
    pub frame_len: usize,
    pub object_type: u8,
    pub rate_index: u8,
    pub channels: u8,
}

impl Adts {
    pub fn rate(&self) -> u32 {
        RATES[self.rate_index as usize]
    }
}

pub fn parse_adts(b: &[u8]) -> Option<Adts> {
    if b.len() < 7 || b[0] != 0xFF || b[1] & 0xF6 != 0xF0 {
        return None;
    }
    let protection_absent = b[1] & 1 == 1;
    let object_type = (b[2] >> 6) + 1;
    let rate_index = (b[2] >> 2) & 0x0F;
    let channels = ((b[2] & 1) << 2) | (b[3] >> 6);
    let frame_len = (((b[3] & 3) as usize) << 11) | ((b[4] as usize) << 3) | ((b[5] >> 5) as usize);
    let header_len = if protection_absent { 7 } else { 9 };
    if rate_index as usize >= RATES.len() || frame_len < header_len {
        return None;
    }
    Some(Adts { header_len, frame_len, object_type, rate_index, channels })
}

/// Raw .aac files from HLS audio renditions start with an ID3 tag, then ADTS frames.
pub fn looks_like_adts(head: &[u8]) -> bool {
    parse_adts(&head[skip_id3(head).min(head.len())..]).is_some() || head.starts_with(b"ID3")
}

pub fn skip_id3(b: &[u8]) -> usize {
    if b.len() >= 10 && b.starts_with(b"ID3") {
        let size = b[6..10].iter().fold(0usize, |a, &x| (a << 7) | (x & 0x7F) as usize);
        10 + size + if b[5] & 0x10 != 0 { 10 } else { 0 }
    } else {
        0
    }
}

/// mp4a sample entry for an AAC stream.
pub fn mp4a(h: &Adts) -> Result<Vec<u8>> {
    if h.channels == 0 {
        bail!("AAC with in-band channel layout is not supported");
    }
    let asc = [(h.object_type << 3) | (h.rate_index >> 1), ((h.rate_index & 1) << 7) | (h.channels << 3)];
    let desc = |tag: u8, body: &[u8]| -> Vec<u8> {
        let mut d = vec![tag, 0x80, 0x80, 0x80, body.len() as u8];
        d.extend_from_slice(body);
        d
    };
    let dsi = desc(5, &asc);
    let mut dcd = vec![0x40, 0x15, 0, 0, 0];
    dcd.u32(0);
    dcd.u32(0);
    dcd.extend_from_slice(&dsi);
    let mut es = vec![0, 1, 0];
    es.extend_from_slice(&desc(4, &dcd));
    es.extend_from_slice(&desc(6, &[2]));
    let esds = bx(b"esds", |b| {
        b.u32(0);
        b.extend_from_slice(&desc(3, &es));
    });
    Ok(bx(b"mp4a", |b| {
        b.extend_from_slice(&[0; 6]);
        b.u16(1);
        b.extend_from_slice(&[0; 8]);
        b.u16(h.channels as u16);
        b.u16(16);
        b.u32(0);
        b.u32(h.rate().min(65535) << 16);
        b.extend_from_slice(&esds);
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn splits_nal_units() {
        let s = [0, 0, 0, 1, 9, 0xF0, 0, 0, 1, 0x67, 1, 2, 0, 0, 0, 1, 0x68, 3];
        assert_eq!(nal_units(&s), vec![&[9, 0xF0][..], &[0x67, 1, 2], &[0x68, 3]]);
    }

    #[test]
    fn reads_adts_header() {
        // AAC-LC, 44.1 kHz, stereo, 100-byte frame
        let h = parse_adts(&[0xFF, 0xF1, 0x50, 0x80, 0x0C, 0x9F, 0xFC]).unwrap();
        assert_eq!((h.object_type, h.rate(), h.channels, h.frame_len, h.header_len), (2, 44100, 2, 100, 7));
    }
}
