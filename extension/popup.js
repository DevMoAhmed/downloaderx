const send = msg => new Promise(res => chrome.runtime.sendMessage(msg, r => res(r || {})));
const $ = id => document.getElementById(id);
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const size = b => { if (!b) return ''; const u = ['B', 'KB', 'MB', 'GB']; let i = 0; while (b >= 1024 && i < 3) { b /= 1024; i++; } return b.toFixed(i ? 1 : 0) + ' ' + u[i]; };

(async () => {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });

  const st = await send({ type: 'status' });
  $('st').innerHTML = st.error ? '<span class="off">● app not running</span>'
    : '<span class="on">● connected</span>';

  const { data: s = {} } = await send({ type: 'settings' });
  $('intercept').checked = s.intercept ?? true;
  $('intercept').onchange = e => send({ type: 'setSettings', settings: { intercept: e.target.checked } });

  // Download button on videos: everywhere, and per site.
  let site = '';
  try { site = new URL(tab.url).hostname.replace(/^www\./, ''); } catch {}
  let blocked = s.floatBlocked || [];
  $('site').textContent = site || 'this site';
  $('float').checked = s.floatButton ?? true;
  $('floatSite').checked = !blocked.includes(site);
  const syncSite = () => { $('floatSite').disabled = !$('float').checked || !site; $('floatSite').parentElement.classList.toggle('dis', $('floatSite').disabled); };
  syncSite();
  $('float').onchange = e => { send({ type: 'setSettings', settings: { floatButton: e.target.checked } }); syncSite(); };
  $('floatSite').onchange = e => {
    blocked = e.target.checked ? blocked.filter(h => h !== site) : [...new Set([...blocked, site])];
    send({ type: 'setSettings', settings: { floatBlocked: blocked } });
  };

  // Probe a URL and show its qualities as buttons inside `box`.
  async function choose(url, box, name) {
    box.innerHTML = '<span class="msg">Finding qualities…</span>';
    const r = await send({ type: 'probe', url, referer: tab.url });
    if (r.error) return (box.innerHTML = `<span class="err">${esc(r.error)}</span>`);
    box.innerHTML = '';
    for (const c of r.data.choices) {
      const b = document.createElement('button');
      b.textContent = c.label + (c.size ? ` · ${size(c.size)}` : '');
      b.onclick = async () => {
        // Prefer the site's own title over the tab title with its " - YouTube" suffix.
        const n = c.name || (c.kind.type === 'direct' ? null : r.data.title || name || tab.title);
        const d = await send({ type: 'download', url: c.url, kind: c.kind, name: n, referer: tab.url, description: c.kind.type === 'direct' ? null : c.label, headers: c.headers });
        box.innerHTML = d.error ? `<span class="err">${esc(d.error)}</span>` : '<span class="on">✓ Added</span>';
      };
      box.appendChild(b);
    }
  }

  $('page').onclick = () => choose(tab.url, $('pageQ'), tab.title);

  // Bring up the DownloaderX window; fall back to the web dashboard if the app runs headless.
  $('dash').onclick = async e => {
    e.preventDefault();
    const r = await send({ type: 'show' });
    if (r.error) chrome.tabs.create({ url: 'http://127.0.0.1:6801' });
    window.close();
  };

  const { data: media = [] } = await send({ type: 'media', tabId: tab.id });
  if (media.length) {
    $('list').innerHTML = '';
    for (const m of [...media].reverse()) {
      const row = document.createElement('div');
      // "page": a player that builds the stream itself (YouTube, Vimeo…); the app reads the page.
      const name = m.type === 'page' ? (m.title || new URL(m.url).hostname) : decodeURIComponent(new URL(m.url).pathname.split('/').pop() || m.url);
      const kind = m.type === 'page' ? 'video on page' : m.type === 'hls' || m.type === 'dash' ? 'stream' : size(m.size) || 'video';
      row.innerHTML = `<div class="m"><span class="n" title="${esc(m.url)}">${esc(name)}</span><small>${esc(kind)}</small><button>Get</button></div><div class="q"></div>`;
      row.querySelector('button').onclick = () => choose(m.url, row.querySelector('.q'), tab.title);
      $('list').appendChild(row);
    }
  }
})();
