//! MPEG-DASH (.mpd) support without yt-dlp: list the qualities of a manifest and turn
//! a representation into the list of segment URLs to download.

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use reqwest::Client;
use reqwest::header::HeaderMap;
use tokio_util::sync::CancellationToken;
use url::Url;

use crate::engine::{Cancelled, Progress, with_suffix};
use crate::hls::{self, Seg};
use crate::limiter::Limiter;

// --- a small XML reader, enough for MPD manifests ---

#[derive(Debug, Default)]
pub struct El {
    pub name: String,
    pub attrs: HashMap<String, String>,
    pub kids: Vec<El>,
    pub text: String,
}

impl El {
    fn kid(&self, name: &str) -> Option<&El> {
        self.kids.iter().find(|k| k.name == name)
    }
    fn kids<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a El> + 'a {
        self.kids.iter().filter(move |k| k.name == name)
    }
    fn attr(&self, k: &str) -> Option<&str> {
        self.attrs.get(k).map(String::as_str)
    }
}

fn unescape(s: &str) -> String {
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&amp;", "&")
}

/// Element name without its namespace prefix.
fn local(name: &str) -> &str {
    name.rsplit(':').next().unwrap_or(name)
}

pub fn parse_xml(src: &str) -> Result<El> {
    let mut stack: Vec<El> = vec![El::default()];
    let mut rest = src;
    while let Some(lt) = rest.find('<') {
        let text = &rest[..lt];
        if let Some(top) = stack.last_mut() {
            top.text.push_str(&unescape(text.trim()));
        }
        rest = &rest[lt..];
        if let Some(r) = rest.strip_prefix("<!--") {
            rest = r.split_once("-->").map_or("", |x| x.1);
        } else if rest.starts_with("<?") || rest.starts_with("<!") {
            rest = rest.split_once('>').map_or("", |x| x.1);
        } else if let Some(r) = rest.strip_prefix("</") {
            rest = r.split_once('>').map_or("", |x| x.1);
            if stack.len() > 1 {
                let el = stack.pop().unwrap();
                stack.last_mut().unwrap().kids.push(el);
            }
        } else {
            // Find the end of the tag, skipping '>' inside quoted attribute values.
            let mut quote = None;
            let mut end = None;
            for (i, c) in rest.char_indices().skip(1) {
                match (quote, c) {
                    (None, '"' | '\'') => quote = Some(c),
                    (Some(q), c) if c == q => quote = None,
                    (None, '>') => {
                        end = Some(i);
                        break;
                    }
                    _ => {}
                }
            }
            let end = end.context("unterminated XML tag")?;
            let tag = &rest[1..end];
            rest = &rest[end + 1..];
            let closed = tag.ends_with('/');
            let tag = tag.trim_end_matches('/');
            let (name, mut a) = tag.split_once(char::is_whitespace).unwrap_or((tag, ""));
            let mut el = El { name: local(name).to_string(), ..Default::default() };
            while let Some((k, v)) = a.split_once('=') {
                let v = v.trim_start();
                let q = v.chars().next().unwrap_or('"');
                let Some(body) = v.get(1..) else { break };
                let Some(close) = body.find(q) else { break };
                el.attrs.insert(local(k.trim()).to_string(), unescape(&body[..close]));
                a = &body[close + 1..];
            }
            if closed {
                stack.last_mut().unwrap().kids.push(el);
            } else {
                stack.push(el);
            }
        }
    }
    while stack.len() > 1 {
        let el = stack.pop().unwrap();
        stack.last_mut().unwrap().kids.push(el);
    }
    stack.pop().and_then(|root| root.kids.into_iter().next()).context("empty XML")
}

// --- MPD ---

/// ISO 8601 duration such as PT1H2M3.5S, in seconds.
fn duration(s: &str) -> Option<f64> {
    let s = s.strip_prefix('P')?;
    let (date, time) = s.split_once('T').unwrap_or((s, ""));
    let mut total = 0.0;
    let mut num = String::new();
    for (part, units) in [(date, &[('Y', 31_536_000.0), ('M', 2_592_000.0), ('W', 604_800.0), ('D', 86_400.0)][..]), (time, &[('H', 3600.0), ('M', 60.0), ('S', 1.0)][..])] {
        for c in part.chars() {
            if c.is_ascii_digit() || c == '.' {
                num.push(c);
            } else {
                let mult = units.iter().find(|u| u.0 == c)?.1;
                total += num.parse::<f64>().ok()? * mult;
                num.clear();
            }
        }
    }
    Some(total)
}

#[derive(Clone, Debug, PartialEq)]
pub enum Kind {
    Video,
    Audio,
}

#[derive(Debug)]
pub struct Rep {
    pub id: String,
    pub kind: Kind,
    pub bandwidth: u64,
    pub height: Option<u32>,
    pub codecs: String,
    pub label: String,
    init: Option<Seg>,
    segs: Vec<Seg>,
}

impl Rep {
    pub fn segments(self) -> (Option<Seg>, Vec<Seg>) {
        (self.init, self.segs)
    }
}

/// Template with $RepresentationID$, $Number$, $Time$, $Bandwidth$ (and %0Nd widths).
fn fill(t: &str, id: &str, bandwidth: u64, number: u64, time: u64) -> String {
    let mut out = String::new();
    let mut parts = t.split('$');
    if let Some(first) = parts.next() {
        out.push_str(first);
    }
    let mut var = true;
    for p in parts {
        if var {
            let (name, fmt) = p.split_once('%').unwrap_or((p, ""));
            let width: usize = fmt.trim_start_matches('0').trim_end_matches('d').parse().unwrap_or(0);
            let v = match name {
                "" => "$".to_string(),
                "RepresentationID" => id.to_string(),
                "Number" => format!("{number:0width$}"),
                "Time" => format!("{time:0width$}"),
                "Bandwidth" => format!("{bandwidth:0width$}"),
                other => format!("${other}$"),
            };
            out.push_str(&v);
        } else {
            out.push_str(p);
        }
        var = !var;
    }
    out
}

fn byte_range(r: &str) -> Option<(u64, u64)> {
    let (a, b) = r.split_once('-')?;
    let (a, b): (u64, u64) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
    (b >= a).then_some((a, b - a + 1))
}

fn base_of(el: &El, base: &Url) -> Url {
    el.kid("BaseURL").and_then(|b| base.join(b.text.trim()).ok()).unwrap_or_else(|| base.clone())
}

/// Every downloadable representation of the first period.
pub fn parse(text: &str, manifest_url: &Url) -> Result<Vec<Rep>> {
    let mpd = parse_xml(text)?;
    if mpd.name != "MPD" {
        bail!("not a DASH manifest");
    }
    if mpd.attr("type") == Some("dynamic") {
        bail!("live streams are not supported yet");
    }
    let period = mpd.kid("Period").context("manifest without a period")?;
    let total = period.attr("duration").or(mpd.attr("mediaPresentationDuration")).and_then(duration).unwrap_or(0.0);
    let base = base_of(period, &base_of(&mpd, manifest_url));
    let mut reps = Vec::new();
    for set in period.kids("AdaptationSet") {
        if set.kid("ContentProtection").is_some() {
            continue; // DRM
        }
        let set_base = base_of(set, &base);
        for rep in set.kids("Representation") {
            if rep.kid("ContentProtection").is_some() {
                continue;
            }
            let get = |k: &str| rep.attr(k).or(set.attr(k)).unwrap_or("");
            let mime = get("mimeType");
            let ctype = get("contentType");
            let kind = if mime.starts_with("video") || ctype == "video" || !get("width").is_empty() {
                Kind::Video
            } else if mime.starts_with("audio") || ctype == "audio" {
                Kind::Audio
            } else {
                continue;
            };
            let id = get("id").to_string();
            let bandwidth: u64 = get("bandwidth").parse().unwrap_or(0);
            let height: Option<u32> = get("height").parse().ok();
            let rep_base = base_of(rep, &set_base);
            let template = rep.kid("SegmentTemplate").or(set.kid("SegmentTemplate"));
            let (init, segs) = if let Some(t) = template {
                // Representation-level attributes override the adaptation set's.
                let outer = set.kid("SegmentTemplate");
                let ta = |k: &str| t.attr(k).or(outer.and_then(|o| o.attr(k)));
                let timescale: u64 = ta("timescale").and_then(|v| v.parse().ok()).unwrap_or(1);
                let start: u64 = ta("startNumber").and_then(|v| v.parse().ok()).unwrap_or(1);
                let init = ta("initialization").and_then(|i| rep_base.join(&fill(i, &id, bandwidth, 0, 0)).ok()).map(|url| Seg::new(url, None));
                let media = ta("media").context("segment template without media")?;
                let mut segs = Vec::new();
                let timeline = t.kid("SegmentTimeline").or(outer.and_then(|o| o.kid("SegmentTimeline")));
                if let Some(tl) = timeline {
                    let (mut time, mut number) = (0u64, start);
                    let entries: Vec<&El> = tl.kids("S").collect();
                    for (i, s) in entries.iter().enumerate() {
                        if let Some(t) = s.attr("t").and_then(|v| v.parse().ok()) {
                            time = t;
                        }
                        let d: u64 = s.attr("d").and_then(|v| v.parse().ok()).context("timeline entry without duration")?;
                        let mut repeat: i64 = s.attr("r").and_then(|v| v.parse().ok()).unwrap_or(0);
                        if repeat < 0 {
                            // Repeat until the next entry, or the end of the period.
                            let until = entries.get(i + 1).and_then(|n| n.attr("t")).and_then(|v| v.parse::<u64>().ok()).unwrap_or((total * timescale as f64) as u64);
                            repeat = (until.saturating_sub(time) / d.max(1)) as i64 - 1;
                        }
                        for _ in 0..=repeat.max(0) {
                            segs.push(Seg::new(rep_base.join(&fill(media, &id, bandwidth, number, time))?, None));
                            time += d;
                            number += 1;
                        }
                    }
                } else {
                    let d: f64 = ta("duration").and_then(|v| v.parse().ok()).context("segment template without duration")?;
                    let count = (total * timescale as f64 / d).ceil().max(1.0) as u64;
                    for n in 0..count {
                        segs.push(Seg::new(rep_base.join(&fill(media, &id, bandwidth, start + n, (n as f64 * d) as u64))?, None));
                    }
                }
                (init, segs)
            } else if let Some(list) = rep.kid("SegmentList").or(set.kid("SegmentList")) {
                let init = list.kid("Initialization").map(|i| {
                    let url = i.attr("sourceURL").and_then(|u| rep_base.join(u).ok()).unwrap_or_else(|| rep_base.clone());
                    Seg::new(url, i.attr("range").and_then(byte_range))
                });
                let segs = list
                    .kids("SegmentURL")
                    .map(|s| {
                        let url = s.attr("media").and_then(|u| rep_base.join(u).ok()).unwrap_or_else(|| rep_base.clone());
                        Seg::new(url, s.attr("mediaRange").and_then(byte_range))
                    })
                    .collect();
                (init, segs)
            } else {
                // SegmentBase or a bare BaseURL: one whole file.
                (None, vec![Seg::new(rep_base.clone(), None)])
            };
            if segs.is_empty() {
                continue;
            }
            let rate = if bandwidth >= 1_000_000 { format!("{:.1} Mbps", bandwidth as f64 / 1e6) } else { format!("{} kbps", bandwidth / 1000) };
            let label = match (&kind, height) {
                (Kind::Video, Some(h)) => format!("{h}p · {rate}"),
                (Kind::Video, None) => rate,
                (Kind::Audio, _) => format!("Audio · {rate}"),
            };
            reps.push(Rep { id, kind, bandwidth, height, codecs: get("codecs").to_string(), label, init, segs });
        }
    }
    if reps.is_empty() {
        bail!("no downloadable video or audio in this manifest (it may be DRM protected)");
    }
    reps.sort_by(|a, b| b.height.cmp(&a.height).then(b.bandwidth.cmp(&a.bandwidth)));
    Ok(reps)
}

/// The audio to pair with video: AAC first (it fits .mp4 everywhere), then the best rate.
pub fn best_audio(reps: &[Rep]) -> Option<&Rep> {
    reps.iter().filter(|r| r.kind == Kind::Audio).max_by_key(|r| (r.codecs.starts_with("mp4a"), r.bandwidth))
}

/// Download the chosen representations of a manifest and join them into one file
/// (.mp4, or .m4a when there's only audio). Missing ids fall back to the best quality.
#[allow(clippy::too_many_arguments)]
pub async fn download(
    client: &Client,
    url: &str,
    video: Option<&str>,
    audio: Option<&str>,
    out: PathBuf,
    headers: &HeaderMap,
    conns: usize,
    progress: &Progress,
    limiter: &Limiter,
    cancel: &CancellationToken,
) -> Result<PathBuf> {
    let work = async {
        let (text, base) = hls::fetch_text(client, url, headers).await?;
        let mut reps = parse(&text, &base)?;
        let pick = |reps: &mut Vec<Rep>, id: Option<&str>, kind: Kind| -> Option<Rep> {
            let i = match id {
                Some(id) => reps.iter().position(|r| r.id == id && r.kind == kind),
                None => None,
            };
            i.map(|i| reps.remove(i))
        };
        let v = match video {
            Some(id) => Some(pick(&mut reps, Some(id), Kind::Video).or_else(|| pick_first(&mut reps, Kind::Video)).context("video quality not in the manifest any more")?),
            None => None,
        };
        let a = match audio {
            Some(id) => pick(&mut reps, Some(id), Kind::Audio).or_else(|| {
                let best = best_audio(&reps)?.id.clone();
                pick(&mut reps, Some(&best), Kind::Audio)
            }),
            None => None,
        };
        if v.is_none() && a.is_none() {
            bail!("nothing to download");
        }
        let out = out.with_extension(if v.is_some() { "mp4" } else { "m4a" });
        if let Some(dir) = out.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        *progress.path.lock().unwrap() = Some(out.clone());
        progress.downloaded.store(0, std::sync::atomic::Ordering::Relaxed);
        let mut parts = Vec::new();
        for (rep, suffix) in [(v, ".video.part"), (a, ".audio.part")] {
            let Some(rep) = rep else { continue };
            let part = with_suffix(&out, suffix);
            let (init, segs) = rep.segments();
            hls::download_segments(client, init.as_ref(), &segs, &part, headers, conns, progress, limiter).await?;
            parts.push(part);
        }
        crate::mux::remux_async(parts.clone(), out.clone()).await?;
        for p in parts {
            tokio::fs::remove_file(p).await.ok();
        }
        Ok(out)
    };
    tokio::select! {
        r = work => r,
        _ = cancel.cancelled() => Err(Cancelled.into()),
    }
}

fn pick_first(reps: &mut Vec<Rep>, kind: Kind) -> Option<Rep> {
    let i = reps.iter().position(|r| r.kind == kind)?;
    Some(reps.remove(i))
}

#[cfg(test)]
mod tests {
    use super::*;

    const MPD: &str = r#"<?xml version="1.0"?>
<MPD xmlns="urn:mpeg:dash:schema:mpd:2011" type="static" mediaPresentationDuration="PT10.5S">
  <!-- comment -->
  <BaseURL>https://cdn.example.com/v/</BaseURL>
  <Period>
    <AdaptationSet mimeType="video/mp4" codecs="avc1.64001f">
      <SegmentTemplate timescale="1000" duration="4000" startNumber="1" initialization="$RepresentationID$/init.mp4" media="$RepresentationID$/seg-$Number%03d$.m4s"/>
      <Representation id="720" bandwidth="2500000" width="1280" height="720"/>
      <Representation id="360" bandwidth="800000" width="640" height="360"/>
    </AdaptationSet>
    <AdaptationSet mimeType="audio/mp4" codecs="mp4a.40.2">
      <Representation id="a1" bandwidth="128000">
        <SegmentTemplate timescale="48000" initialization="a/init.mp4" media="a/$Time$.m4s">
          <SegmentTimeline><S t="0" d="96000" r="2"/><S d="48000"/></SegmentTimeline>
        </SegmentTemplate>
      </Representation>
    </AdaptationSet>
    <AdaptationSet mimeType="video/mp4"><ContentProtection schemeIdUri="x"/><Representation id="drm" bandwidth="1"/></AdaptationSet>
  </Period>
</MPD>"#;

    #[test]
    fn reads_manifest() {
        let reps = parse(MPD, &Url::parse("https://example.com/x.mpd").unwrap()).unwrap();
        assert_eq!(reps.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["720", "360", "a1"]);
        assert_eq!(reps[0].label, "720p · 2.5 Mbps");
        let (init, segs) = reps.into_iter().next().unwrap().segments();
        assert_eq!(init.unwrap().url.as_str(), "https://cdn.example.com/v/720/init.mp4");
        assert_eq!(segs.len(), 3);
        assert_eq!(segs[2].url.as_str(), "https://cdn.example.com/v/720/seg-003.m4s");
    }

    #[test]
    fn timeline_times() {
        let reps = parse(MPD, &Url::parse("https://example.com/x.mpd").unwrap()).unwrap();
        let a = reps.into_iter().find(|r| r.id == "a1").unwrap();
        let urls: Vec<String> = a.segments().1.iter().map(|s| s.url.path().to_string()).collect();
        assert_eq!(urls, ["/v/a/0.m4s", "/v/a/96000.m4s", "/v/a/192000.m4s", "/v/a/288000.m4s"]);
    }

    #[test]
    fn iso_durations() {
        assert_eq!(duration("PT1H2M3.5S"), Some(3723.5));
        assert_eq!(duration("P1DT1S"), Some(86401.0));
    }
}
