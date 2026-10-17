// Floating "Download" button over videos, with a quality menu.
(() => {
  if (window.__downloaderx) return;
  window.__downloaderx = true;

  const send = msg => new Promise(res => chrome.runtime.sendMessage(msg, r => res(r || { error: chrome.runtime.lastError?.message })));
  const size = b => { if (!b) return ''; const u = ['B', 'KB', 'MB', 'GB']; let i = 0; while (b >= 1024 && i < 3) { b /= 1024; i++; } return b.toFixed(i ? 1 : 0) + ' ' + u[i]; };

  // Shadow DOM so the page's CSS can't break our button.
  const host = document.createElement('div');
  host.style.cssText = 'position:fixed;top:0;left:0;width:0;height:0;z-index:2147483647;';
  const root = host.attachShadow({ mode: 'closed' });
  root.innerHTML = `<style>
    * { box-sizing: border-box; font: 13px/1.3 system-ui, -apple-system, Segoe UI, sans-serif; }
    .btn { position: fixed; display: none; align-items: center; gap: 6px; padding: 6px 10px; border: 0; border-radius: 8px;
      background: rgba(47,111,237,.95); color: #fff; cursor: pointer; box-shadow: 0 2px 10px rgba(0,0,0,.35); font-weight: 600; }
    .btn:hover { background: #2559c9; }
    .btn svg { width: 14px; height: 14px; }
    .menu { position: fixed; display: none; min-width: 240px; max-width: 360px; max-height: 360px; overflow: auto; padding: 6px;
      background: #1c2028; color: #e6e9ef; border-radius: 10px; box-shadow: 0 8px 28px rgba(0,0,0,.45); }
    .menu .h { padding: 6px 8px; color: #8f97a8; font-size: 12px; }
    .menu button { display: flex; width: 100%; justify-content: space-between; gap: 12px; padding: 8px 10px; border: 0; border-radius: 6px;
      background: transparent; color: inherit; cursor: pointer; text-align: left; }
    .menu button:hover { background: #2b3446; }
    .menu button span { color: #8f97a8; }
    .err { padding: 8px; color: #ff8a8a; }
    .ok { padding: 8px; color: #6fdc9a; }
  </style>
  <button class="btn" title="Download with DownloaderX">
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.5" stroke-linecap="round"><path d="M12 4v11m0 0l-5-5m5 5l5-5M5 20h14"/></svg>Download
  </button>
  <div class="menu"></div>`;
  const btn = root.querySelector('.btn');
  const menu = root.querySelector('.menu');
  (document.body || document.documentElement).appendChild(host);

  let video = null;    // video the button is attached to
  let hideTimer = 0;
  let menuOpen = false;

  const visibleVideos = () => [...document.querySelectorAll('video')].filter(v => {
    const r = v.getBoundingClientRect();
    return r.width >= 200 && r.height >= 110 && r.bottom > 0 && r.top < innerHeight;
  });

  function place() {
    if (!video) return;
    const r = video.getBoundingClientRect();
    const w = btn.offsetWidth || 110;
    btn.style.top = Math.max(4, r.top + 10) + 'px';
    btn.style.left = Math.min(innerWidth - w - 4, r.right - w - 10) + 'px';
    if (menuOpen) {
      const b = btn.getBoundingClientRect();
      menu.style.top = b.bottom + 6 + 'px';
      menu.style.left = Math.max(4, b.right - menu.offsetWidth) + 'px';
    }
  }

  function show(v) {
    clearTimeout(hideTimer);
    if (video !== v) { video = v; closeMenu(); }
    btn.style.display = 'flex';
    place();
  }

  function scheduleHide() {
    clearTimeout(hideTimer);
    hideTimer = setTimeout(() => { if (!menuOpen) { btn.style.display = 'none'; video = null; } }, 1800);
  }

  // Players often put overlays on top of <video>, so test the pointer against video rectangles.
  let last = 0;
  document.addEventListener('mousemove', e => {
    const now = performance.now();
    if (now - last < 120) return;
    last = now;
    if (e.composedPath().includes(host)) { clearTimeout(hideTimer); return; }
    const v = visibleVideos().find(v => {
      const r = v.getBoundingClientRect();
      return e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
    });
    v ? show(v) : scheduleHide();
  }, { passive: true, capture: true });
  addEventListener('scroll', place, { passive: true, capture: true });
  addEventListener('resize', place, { passive: true });
  document.addEventListener('fullscreenchange', () => {
    // Move into the fullscreen element so the button stays visible there.
    (document.fullscreenElement || document.body || document.documentElement).appendChild(host);
    place();
  });

  function closeMenu() { menuOpen = false; menu.style.display = 'none'; }
  document.addEventListener('mousedown', e => { if (menuOpen && !e.composedPath().includes(host)) closeMenu(); }, true);

  btn.addEventListener('click', async e => {
    e.preventDefault(); e.stopPropagation();
    if (menuOpen) return closeMenu();
    menuOpen = true;
    menu.innerHTML = '<div class="h">Finding qualities…</div>';
    menu.style.display = 'block';
    place();
    const v = video;
    try {
      render(await findChoices(v));
    } catch (err) {
      menu.innerHTML = `<div class="err">${esc(err.message)}</div>`;
    }
    place();
  });

  const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const title = () => (document.title || 'video').replace(/\s*[-|–]\s*[^-|–]+$/, '').trim() || 'video';

  async function probe(url) {
    const r = await send({ type: 'probe', url, referer: location.href });
    if (r.error) throw new Error(r.error);
    return r.data;
  }

  // Collect download options: the video's own file, streams seen on this tab, or the page via yt-dlp.
  async function findChoices(v) {
    const direct = new Set();
    for (const s of [v?.currentSrc, v?.src, ...[...(v?.querySelectorAll('source') || [])].map(s => s.src)]) {
      if (s && /^https?:/.test(s)) direct.add(s);
    }
    const seen = (await send({ type: 'media' })).data || [];
    const streams = seen.filter(m => m.type === 'hls').map(m => m.url).reverse().slice(0, 6);
    for (const m of seen) if (m.type !== 'hls') direct.add(m.url);

    const choices = [];
    const bare = label => label.replace(/^\d+p · /, '');
    const results = await Promise.allSettled([...streams, ...[...direct].slice(-6)].map(u => probe(u)));
    const masters = results.filter(r => r.status === 'fulfilled' && r.value.choices.some(c => c.label !== 'Stream'));
    for (const r of results) {
      if (r.status !== 'fulfilled') continue;
      // A master playlist already lists every quality; skip its single-quality sub-playlists.
      if (masters.length && r.value.choices.every(c => c.label === 'Stream')) continue;
      for (const c of r.value.choices) {
        if (c.kind.type === 'direct' && v && c.url === v.currentSrc && v.videoHeight) c.label = `${v.videoHeight}p · ${c.label}`;
        // The <video> src and the URL seen on the network can differ only by a redirect,
        // so direct files also count as the same when name and size match.
        const same = x => JSON.stringify(x.kind) === JSON.stringify(c.kind) &&
          (x.url === c.url || (c.kind.type === 'direct' && c.size && x.size === c.size && bare(x.label) === bare(c.label)));
        if (!choices.some(same)) choices.push(c);
      }
    }
    if (!choices.length) {
      // e.g. YouTube: the video is assembled in the page (blob:), so ask yt-dlp about the page itself.
      const r = await probe(location.href);
      choices.push(...r.choices);
      if (r.title) choices.title = r.title;
    }
    if (!choices.length) throw new Error('No downloadable video found here.');
    return choices;
  }

  function render(choices) {
    menu.innerHTML = '<div class="h">Download with DownloaderX</div>' + choices.map((c, i) =>
      `<button data-i="${i}">${esc(c.label)}<span>${size(c.size)}</span></button>`).join('');
    menu.querySelectorAll('button').forEach(b => b.addEventListener('click', async ev => {
      ev.stopPropagation();
      const c = choices[b.dataset.i];
      const name = c.kind.type === 'direct' ? null : choices.title || title();
      const r = await send({ type: 'download', url: c.url, kind: c.kind, name, referer: location.href, description: c.kind.type === 'direct' ? null : c.label });
      menu.innerHTML = r.error ? `<div class="err">${esc(r.error)}</div>` : `<div class="ok">✓ Added: ${esc(c.label)}</div>`;
      setTimeout(closeMenu, r.error ? 4000 : 1500);
    }));
  }
})();
