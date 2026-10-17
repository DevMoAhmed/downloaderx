// DownloaderX extension service worker: finds media, talks to the local app.

const APP = 'http://127.0.0.1:6801/api';
const HEADERS = { 'x-downloaderx': '1', 'content-type': 'application/json' };

// Media files and stream playlists. Pieces of streams (.ts, .m4s, range requests) are skipped.
const MEDIA_EXT = /\.(m3u8|mp4|webm|mkv|mov|m4v|mp3|m4a|flac|ogg|wav)(\?|#|$)/i;
const SKIP = /\.(ts|m4s|aac|vtt|srt|key|js|css)(\?|#|$)|[?&](range|bytes)=/i;
const MIN_SIZE = 300 * 1024;

async function app(path, body) {
  const ctrl = new AbortController();
  const t = setTimeout(() => ctrl.abort(), path === '/probe' ? 60000 : 3000);
  try {
    const r = await fetch(APP + path, {
      method: body ? 'POST' : 'GET', headers: HEADERS, body: body && JSON.stringify(body), signal: ctrl.signal,
    });
    const j = await r.json().catch(() => ({}));
    if (!r.ok) throw new Error(j.error || `app error ${r.status}`);
    return j;
  } catch (e) {
    if (e.name === 'AbortError' || e instanceof TypeError) throw new Error('DownloaderX app is not running. Start it first.');
    throw e;
  } finally {
    clearTimeout(t);
  }
}

// Cookies + referer so downloads that need a login still work in the app.
async function headersFor(url, referer) {
  const h = [['User-Agent', navigator.userAgent]];
  if (referer) h.push(['Referer', referer]);
  try {
    const cs = await chrome.cookies.getAll({ url });
    if (cs.length) h.push(['Cookie', cs.map(c => `${c.name}=${c.value}`).join('; ')]);
  } catch {}
  return h;
}

// --- media found per tab (kept in session storage so it survives worker restarts) ---
const key = tab => `media_${tab}`;
async function getMedia(tab) {
  return (await chrome.storage.session.get(key(tab)))[key(tab)] || [];
}
async function setMedia(tab, list) {
  await chrome.storage.session.set({ [key(tab)]: list });
  showCount(tab, list.length);
}
function showCount(tab, n) {
  chrome.action.setBadgeText({ tabId: tab, text: n ? String(n) : '' }).catch(() => {});
  chrome.action.setBadgeBackgroundColor({ tabId: tab, color: '#2f6fed' }).catch(() => {});
}
// Many media responses arrive at once; run each tab's read-modify-write one at a time
// or they overwrite each other and the badge undercounts.
const pending = {};
function updateMedia(tab, fn) {
  const p = (pending[tab] || Promise.resolve()).then(async () => {
    const list = await fn(await getMedia(tab));
    if (list) await setMedia(tab, list);
  });
  pending[tab] = p.catch(() => {});
  return p;
}

chrome.webRequest.onHeadersReceived.addListener(
  d => {
    if (d.tabId < 0 || d.statusCode >= 400 || SKIP.test(d.url) || !/^https?:/.test(d.url)) return;
    const h = {};
    for (const x of d.responseHeaders || []) h[x.name.toLowerCase()] = x.value || '';
    const ct = (h['content-type'] || '').toLowerCase();
    const hls = ct.includes('mpegurl') || /\.m3u8(\?|#|$)/i.test(d.url);
    const media = hls || ct.startsWith('video/') || ct.startsWith('audio/') || (MEDIA_EXT.test(d.url) && !ct.includes('text/'));
    if (!media) return;
    const size = +(h['content-range'] || '').split('/')[1] || +h['content-length'] || 0;
    if (!hls && size && size < MIN_SIZE) return; // ads, previews, sound effects
    updateMedia(d.tabId, list => {
      if (list.some(m => m.url === d.url)) return;
      list.push({ url: d.url, type: hls ? 'hls' : ct.split(';')[0] || 'media', size, time: Date.now() });
      return list.slice(-30);
    });
  },
  { urls: ['<all_urls>'], types: ['media', 'xmlhttprequest', 'other', 'object'] },
  ['responseHeaders'],
);

chrome.tabs.onUpdated.addListener((tab, info) => {
  if (info.status === 'loading' && info.url) updateMedia(tab, () => []);
});
chrome.tabs.onRemoved.addListener(tab => {
  delete pending[tab];
  chrome.storage.session.remove(key(tab));
});

// --- actions ---
async function probe(url, referer) {
  return app('/probe', { url, headers: await headersFor(url, referer) });
}

async function download({ url, kind, name, referer, description }) {
  return app('/add', { url, kind: kind || { type: 'direct' }, name: name || null, description: description || null, headers: await headersFor(url, referer) });
}

/// Probe and add the best choice (used by the context menu).
async function downloadBest(url, referer, name) {
  const r = await probe(url, referer);
  const c = r.choices[0];
  const direct = c.kind.type === 'direct';
  await download({ url: c.url, kind: c.kind, name: direct ? null : name || r.title, referer, description: direct ? null : c.label });
  return c.label;
}

// Flash the result on the tab's badge, then put the media count back. The badge is
// per tab: a global badge would be hidden behind the count on tabs that found media.
async function notify(tab, ok, message) {
  if (tab == null) return console.log(message);
  await chrome.action.setBadgeText({ tabId: tab, text: ok ? '✓' : '!' }).catch(() => {});
  await chrome.action.setBadgeBackgroundColor({ tabId: tab, color: ok ? '#1f9d55' : '#d64545' }).catch(() => {});
  await chrome.action.setTitle({ tabId: tab, title: `DownloaderX: ${message}` }).catch(() => {});
  setTimeout(async () => {
    showCount(tab, (await getMedia(tab)).length);
    chrome.action.setTitle({ tabId: tab, title: 'DownloaderX' }).catch(() => {});
  }, ok ? 1500 : 5000);
}

chrome.runtime.onMessage.addListener((msg, sender, reply) => {
  const tab = msg.tabId ?? sender.tab?.id;
  const run = async () => {
    switch (msg.type) {
      case 'media': return getMedia(tab);
      case 'probe': return probe(msg.url, msg.referer);
      case 'download': return download(msg);
      case 'status': return app('/status');
      case 'show': return app('/show', {});
      case 'settings': return chrome.storage.local.get({ intercept: true });
      case 'setSettings': return chrome.storage.local.set(msg.settings);
    }
  };
  run().then(data => reply({ data }), e => reply({ error: e.message }));
  return true; // reply asynchronously
});

// --- right-click menu ---
chrome.runtime.onInstalled.addListener(() => {
  chrome.contextMenus.create({ id: 'link', title: 'Download with DownloaderX', contexts: ['link', 'video', 'audio', 'image'] });
  chrome.contextMenus.create({ id: 'page', title: 'Download video from this page', contexts: ['page'] });
});

chrome.contextMenus.onClicked.addListener((info, tab) => {
  const url = info.menuItemId === 'page' ? info.pageUrl : info.linkUrl || info.srcUrl;
  if (!url || !/^https?:/.test(url)) return;
  downloadBest(url, info.pageUrl, tab?.title).then(
    label => notify(tab?.id, true, `added ${label}`),
    e => notify(tab?.id, false, e.message),
  );
});

// --- take over normal browser downloads ---
chrome.downloads.onCreated.addListener(async item => {
  const { intercept } = await chrome.storage.local.get({ intercept: true });
  const url = item.finalUrl || item.url;
  if (!intercept || item.byExtensionId || !/^https?:/.test(url)) return;
  if (item.totalBytes > 0 && item.totalBytes < MIN_SIZE) return; // small files: let the browser do it
  try {
    await app('/status');
  } catch {
    return; // app not running: keep the browser download
  }
  await chrome.downloads.cancel(item.id).catch(() => {});
  chrome.downloads.erase({ id: item.id }).catch(() => {});
  const name = item.filename ? item.filename.split(/[\\/]/).pop() : null;
  download({ url, kind: { type: 'direct' }, name, referer: item.referrer }).catch(e => console.warn(e.message));
});
