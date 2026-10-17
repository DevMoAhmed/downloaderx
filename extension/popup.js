const send = msg => new Promise(res => chrome.runtime.sendMessage(msg, r => res(r || {})));
const $ = id => document.getElementById(id);
const esc = s => String(s ?? '').replace(/[&<>"]/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;' }[c]));
const size = b => { if (!b) return ''; const u = ['B', 'KB', 'MB', 'GB']; let i = 0; while (b >= 1024 && i < 3) { b /= 1024; i++; } return b.toFixed(i ? 1 : 0) + ' ' + u[i]; };

(async () => {
  const [tab] = await chrome.tabs.query({ active: true, currentWindow: true });

  const st = await send({ type: 'status' });
  $('st').innerHTML = st.error ? '<span class="off">● app not running</span>'
    : `<span class="on">● connected</span>${st.data.ytdlp ? '' : ' · <span title="Install yt-dlp for YouTube and similar sites">no yt-dlp</span>'}`;

  const { data: s } = await send({ type: 'settings' });
  $('intercept').checked = s?.intercept ?? true;
  $('intercept').onchange = e => send({ type: 'setSettings', settings: { intercept: e.target.checked } });

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
        const n = c.kind.type === 'direct' ? null : name || r.data.title || tab.title;
        const d = await send({ type: 'download', url: c.url, kind: c.kind, name: n, referer: tab.url });
        box.innerHTML = d.error ? `<span class="err">${esc(d.error)}</span>` : '<span class="on">✓ Added</span>';
      };
      box.appendChild(b);
    }
  }

  $('page').onclick = () => choose(tab.url, $('pageQ'), tab.title);

  const { data: media = [] } = await send({ type: 'media', tabId: tab.id });
  if (media.length) {
    $('list').innerHTML = '';
    for (const m of [...media].reverse()) {
      const row = document.createElement('div');
      const name = decodeURIComponent(new URL(m.url).pathname.split('/').pop() || m.url);
      row.innerHTML = `<div class="m"><span class="n" title="${esc(m.url)}">${esc(name)}</span><small>${m.type === 'hls' ? 'stream' : size(m.size)}</small><button>Get</button></div><div class="q"></div>`;
      row.querySelector('button').onclick = () => choose(m.url, row.querySelector('.q'), tab.title);
      $('list').appendChild(row);
    }
  }
})();
