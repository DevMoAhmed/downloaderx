// Runs in TikTok's own page world (not the extension's isolated one), where the feed's post
// data lives in React props. Answers content.js with the post a player belongs to: its
// qualities and links, caption, author and sound, without any extra request to TikTok.
window.addEventListener('message', e => {
  if (e.source !== window || e.data?.dx !== 'tiktok-item') return;
  const { id } = e.data;
  let item = null;
  const start = document.querySelector(`[id^="xgwrapper-"][id$="-${id}"]`);
  const find = (o, depth, seen) => {
    if (!o || typeof o !== 'object' || depth > 6 || seen.has(o)) return null;
    seen.add(o);
    if (o.id === id && o.video && (o.video.bitrateInfo || o.video.playAddr)) return o;
    for (const k of Object.keys(o)) {
      if (k === 'children' || k === '_owner' || k.startsWith('__')) continue;
      const r = find(o[k], depth + 1, seen);
      if (r) return r;
    }
    return null;
  };
  for (let el = start, i = 0; el && i < 30 && !item; el = el.parentElement, i++) {
    const key = Object.keys(el).find(k => k.startsWith('__reactFiber$'));
    for (let f = key && el[key], j = 0; f && j < 40 && !item; f = f.return, j++) item = find(f.memoizedProps, 0, new Set());
  }
  const reply = item && {
    id: item.id,
    desc: item.desc || '',
    author: item.author?.uniqueId || (typeof item.author === 'string' ? item.author : ''),
    video: {
      playAddr: item.video.playAddr || null,
      bitrateInfo: (item.video.bitrateInfo || []).map(b => ({ CodecType: b.CodecType, Bitrate: b.Bitrate, PlayAddr: { Height: b.PlayAddr?.Height, DataSize: b.PlayAddr?.DataSize, UrlList: b.PlayAddr?.UrlList || [] } })),
    },
    music: { playUrl: item.music?.playUrl || null },
  };
  window.postMessage({ dx: 'tiktok-item-reply', id, item: reply }, '*');
});
