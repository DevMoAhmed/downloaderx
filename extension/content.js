// Finds videos on the page (also inside shadow DOM and iframes, since this runs in every frame),
// reports them for the badge and popup, and shows a movable "Download" button above each video.
(() => {
  if (window.__downloaderx) return;
  window.__downloaderx = true;

  const alive = () => !!chrome.runtime?.id; // false once the extension was reloaded or removed
  const send = msg => new Promise(res => {
    if (!alive()) return res({ error: 'DownloaderX was updated. Reload this page.' });
    try { chrome.runtime.sendMessage(msg, r => res(r || { error: chrome.runtime.lastError?.message })); }
    catch (e) { res({ error: e.message }); }
  });
  const size = b => { if (!b) return ''; const u = ['B', 'KB', 'MB', 'GB']; let i = 0; while (b >= 1024 && i < 3) { b /= 1024; i++; } return b.toFixed(i ? 1 : 0) + ' ' + u[i]; };
  const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
  const title = () => (document.title || 'video').replace(/\s*[-|–]\s*[^-|–]+$/, '').trim() || 'video';
  const siteKey = location.hostname.replace(/^www\./, '');

  // ---------- settings (popup and the × menu change them) ----------
  let cfg = { floatButton: true, floatBlocked: [], floatPos: {} };
  const enabled = () => cfg.floatButton && !cfg.floatBlocked.includes(siteKey);
  chrome.storage.local.get(cfg).then(c => { cfg = c; if (!enabled()) hide(true); }).catch(() => {});
  chrome.storage.onChanged.addListener((ch, area) => {
    if (area !== 'local') return;
    for (const k of Object.keys(cfg)) if (ch[k]) cfg[k] = ch[k].newValue ?? cfg[k];
    if (!enabled()) hide(true);
  });

  // ---------- finding videos ----------
  // Players often hide <video> inside shadow roots, which querySelectorAll doesn't enter.
  function findVideos(root = document, out = []) {
    root.querySelectorAll('video').forEach(v => out.push(v));
    const walker = document.createTreeWalker(root, NodeFilter.SHOW_ELEMENT);
    for (let n = walker.nextNode(); n; n = walker.nextNode()) if (n.shadowRoot) findVideos(n.shadowRoot, out);
    return out;
  }
  let videos = [];
  const isReal = v => v.readyState >= 1 && (v.duration > 3 || v.duration === Infinity); // skip hover previews and blips
  const srcOf = v => v.currentSrc || v.src || '';

  // Tell the extension what this frame plays, so the badge and popup list it even when the
  // stream is assembled in the page (YouTube, Vimeo…) and the network only sees small chunks.
  let lastReport = '';
  function scan() {
    if (!alive()) return teardown();
    videos = findVideos();
    const real = videos.filter(isReal);
    const direct = [...new Set(real.map(srcOf).filter(s => /^https?:/.test(s)))];
    const streams = real.filter(v => !/^https?:/.test(srcOf(v))).length;
    const report = { type: 'pageVideos', url: location.href, title: title(), direct, streams, top: window === top };
    const key = JSON.stringify(report);
    if (key !== lastReport && (direct.length || streams)) { lastReport = key; send(report); }
    if (video && !video.isConnected) hide(true);
  }
  const scanTimer = setInterval(scan, 1500);
  document.addEventListener('loadedmetadata', scan, true);
  setTimeout(scan, 300);

  // ---------- the button ----------
  // Shadow DOM so the page's CSS can't break our button.
  const host = document.createElement('div');
  host.style.cssText = 'position:fixed;top:0;left:0;width:0;height:0;z-index:2147483647;';
  const root = host.attachShadow({ mode: 'closed' });
  root.innerHTML = `<style>
    * { box-sizing: border-box; font: 700 13px/1.3 "Segoe UI", system-ui, -apple-system, sans-serif; }
    /* DownloaderX palette: marigold block, ink outline, cream menu */
    .bar { position: fixed; display: none; align-items: stretch; border: 2px solid #2A1A12; border-radius: 10px; overflow: hidden; touch-action: none;
      background: #F0B23C; color: #2A1A12; box-shadow: 0 3px 0 #2A1A12; user-select: none; -webkit-user-select: none; }
    .bar.drag { cursor: grabbing; box-shadow: 0 6px 0 #2A1A12; }
    .go { display: flex; align-items: center; gap: 6px; padding: 5px 10px 5px 7px; cursor: pointer; font-weight: 800; }
    .go:hover, .x:hover { background: #FFC83D; }
    .go img { width: 18px; height: 18px; image-rendering: pixelated; }
    .x { display: flex; align-items: center; padding: 0 9px; border-left: 2px solid #2A1A12; cursor: pointer; font-size: 15px; font-weight: 900; }
    .menu { position: fixed; display: none; min-width: 240px; max-width: 360px; max-height: 360px; overflow: auto; padding: 6px;
      background: #FFF4E0; color: #2A1A12; border: 3px solid #2A1A12; border-radius: 12px; box-shadow: 0 5px 0 #2A1A12; }
    .menu .h { padding: 6px 8px; color: #5C6B73; font-size: 12px; font-weight: 800; }
    .menu button { display: flex; width: 100%; justify-content: space-between; gap: 12px; padding: 8px 10px; border: 0; border-radius: 8px;
      background: transparent; color: inherit; cursor: pointer; text-align: left; font-weight: 800; }
    .menu button:hover { background: #FFC83D; }
    .menu button span { color: #5C6B73; }
    .err { padding: 8px; color: #C9402F; }
    .ok { padding: 8px; color: #4F8A4B; font-weight: 900; }
  </style>
  <div class="bar" title="Download with DownloaderX (drag to move)">
    <div class="go"><img alt="" src="${chrome.runtime.getURL('icons/48.png')}">Download</div>
    <div class="x" title="Hide the Download button">×</div>
  </div>
  <div class="menu"></div>`;
  const bar = root.querySelector('.bar');
  const menu = root.querySelector('.menu');
  (document.documentElement || document.body).appendChild(host);

  let video = null;        // video the button belongs to
  let hideTimer = 0;
  let menuOpen = false;
  const dismissed = new WeakSet(); // "hide on this video"

  // Where the button goes, relative to the video's top-right corner. Default: just above the
  // video, right-aligned, so it never covers the picture. Dragging changes it (saved per site).
  function anchor() {
    const p = cfg.floatPos[siteKey];
    return p ? { dx: p.dx, dy: p.dy } : null;
  }
  function place() {
    if (!video) return;
    const r = video.getBoundingClientRect();
    const w = bar.offsetWidth || 130, h = bar.offsetHeight || 30;
    const a = anchor();
    let x = a ? r.right + a.dx : r.right - w;
    let y = a ? r.top + a.dy : r.top - h - 6;
    if (!a && y < 4) y = r.top + 8; // no room above (top of the window, fullscreen): tuck inside the corner
    x = Math.max(4, Math.min(innerWidth - w - 4, x));
    y = Math.max(4, Math.min(innerHeight - h - 4, y));
    bar.style.left = x + 'px'; bar.style.top = y + 'px';
    host.dataset.dxButton = JSON.stringify({ x: Math.round(x), y: Math.round(y), w, h }); // read by tests
    if (menuOpen) placeMenu();
  }
  function placeMenu() {
    const b = bar.getBoundingClientRect(), mh = menu.offsetHeight, mw = menu.offsetWidth;
    const below = b.bottom + 6 + mh < innerHeight;
    menu.style.top = (below ? b.bottom + 6 : Math.max(4, b.top - 6 - mh)) + 'px';
    menu.style.left = Math.max(4, Math.min(innerWidth - mw - 4, b.right - mw)) + 'px';
    // Menu rows for tests (the shadow root is closed).
    host.dataset.dxMenu = JSON.stringify([...menu.querySelectorAll('button')].map(e => {
      const r = e.getBoundingClientRect(); return { t: e.textContent.trim(), x: Math.round(r.left + r.width / 2), y: Math.round(r.top + r.height / 2) };
    }));
  }
  function show(v) {
    if (!enabled() || dismissed.has(v)) return;
    clearTimeout(hideTimer);
    if (video !== v) { video = v; closeMenu(); }
    bar.style.display = 'flex';
    place();
  }
  function hide(now = false) {
    clearTimeout(hideTimer);
    const go = () => { if (!menuOpen || now) { closeMenu(); bar.style.display = 'none'; video = null; delete host.dataset.dxButton; } };
    now ? go() : (hideTimer = setTimeout(go, 1800));
  }

  // Players put overlays on top of <video>, so test the pointer against video rectangles.
  let last = 0;
  document.addEventListener('mousemove', e => {
    if (dragging) return;
    const now = performance.now();
    if (now - last < 100) return;
    last = now;
    if (e.composedPath().includes(host)) { clearTimeout(hideTimer); return; }
    const v = videos.find(v => {
      const r = v.getBoundingClientRect();
      return r.width >= 160 && r.height >= 90 && e.clientX >= r.left && e.clientX <= r.right && e.clientY >= r.top && e.clientY <= r.bottom;
    });
    v ? show(v) : (video && hide());
  }, { passive: true, capture: true });
  addEventListener('scroll', place, { passive: true, capture: true });
  addEventListener('resize', place, { passive: true });
  document.addEventListener('fullscreenchange', () => {
    // Move into the fullscreen element so the button stays visible there.
    (document.fullscreenElement || document.documentElement).appendChild(host);
    place();
  });

  // Drag to move; a press without movement is a click.
  let dragging = false, press = null;
  bar.addEventListener('pointerdown', e => {
    if (e.button !== 0) return;
    const b = bar.getBoundingClientRect();
    press = { x: e.clientX, y: e.clientY, ox: e.clientX - b.left, oy: e.clientY - b.top, onX: !!e.composedPath().find(n => n.classList?.contains('x')) };
    bar.setPointerCapture(e.pointerId);
    e.preventDefault(); e.stopPropagation();
  });
  bar.addEventListener('pointermove', e => {
    if (!press) return;
    if (!dragging && Math.hypot(e.clientX - press.x, e.clientY - press.y) < 5) return;
    if (!dragging) { dragging = true; bar.classList.add('drag'); closeMenu(); }
    const w = bar.offsetWidth, h = bar.offsetHeight;
    bar.style.left = Math.max(0, Math.min(innerWidth - w, e.clientX - press.ox)) + 'px';
    bar.style.top = Math.max(0, Math.min(innerHeight - h, e.clientY - press.oy)) + 'px';
  });
  bar.addEventListener('pointerup', e => {
    if (!press) return;
    const wasX = press.onX; press = null;
    bar.releasePointerCapture(e.pointerId);
    if (dragging) {
      dragging = false; bar.classList.remove('drag');
      if (video) {
        const r = video.getBoundingClientRect(), b = bar.getBoundingClientRect();
        cfg.floatPos = { ...cfg.floatPos, [siteKey]: { dx: Math.round(b.left - r.right), dy: Math.round(b.top - r.top) } };
        chrome.storage.local.set({ floatPos: cfg.floatPos }).catch(() => {});
        place();
      }
      return;
    }
    wasX ? openHideMenu() : openDownloadMenu();
  });
  // Keep the page (player) from reacting to clicks on our button.
  for (const t of ['click', 'mousedown', 'mouseup', 'dblclick']) bar.addEventListener(t, e => { e.preventDefault(); e.stopPropagation(); });

  function closeMenu() { menuOpen = false; menu.style.display = 'none'; delete host.dataset.dxMenu; }
  function openMenu(html) { menuOpen = true; menu.innerHTML = html; menu.style.display = 'block'; placeMenu(); }
  document.addEventListener('mousedown', e => { if (menuOpen && !e.composedPath().includes(host)) closeMenu(); }, true);
  menu.addEventListener('mousedown', e => e.stopPropagation());

  function openHideMenu() {
    if (menuOpen && menu.dataset.kind === 'hide') return closeMenu();
    menu.dataset.kind = 'hide';
    openMenu(`<div class="h">Hide the Download button</div>
      <button data-a="video">On this video</button>
      <button data-a="site">Always on ${esc(siteKey)}</button>
      <button data-a="all">Everywhere<span>turn back on in the popup</span></button>
      ${cfg.floatPos[siteKey] ? '<button data-a="reset">Move it back above the video</button>' : ''}`);
    menu.querySelectorAll('button').forEach(b => b.onclick = ev => {
      ev.stopPropagation();
      const a = b.dataset.a;
      if (a === 'video') { dismissed.add(video); hide(true); }
      if (a === 'site') chrome.storage.local.set({ floatBlocked: [...new Set([...cfg.floatBlocked, siteKey])] });
      if (a === 'all') chrome.storage.local.set({ floatButton: false });
      if (a === 'reset') { const p = { ...cfg.floatPos }; delete p[siteKey]; cfg.floatPos = p; chrome.storage.local.set({ floatPos: p }); closeMenu(); place(); }
    });
  }

  async function openDownloadMenu() {
    if (menuOpen && menu.dataset.kind === 'dl') return closeMenu();
    menu.dataset.kind = 'dl';
    openMenu('<div class="h">Finding qualities…</div>');
    const v = video;
    try {
      render(await findChoices(v));
    } catch (err) {
      menu.innerHTML = `<div class="err">${esc(err.message)}</div>`;
    }
    placeMenu();
  }

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
    const seen = ((await send({ type: 'media' })).data || []).filter(m => m.type !== 'page');
    const streams = seen.filter(m => m.type === 'hls' || m.type === 'dash').map(m => m.url).reverse().slice(0, 6);
    for (const m of seen) if (m.type !== 'hls' && m.type !== 'dash') direct.add(m.url);

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

  function teardown() { clearInterval(scanTimer); host.remove(); }
})();
