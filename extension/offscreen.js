// Relays solve requests from the background worker to the sandboxed solver frame.
const frame = document.getElementById('solver');
const ready = new Promise(r => frame.addEventListener('load', r, { once: true }));
const waiting = new Map();
let next = 1;

window.addEventListener('message', e => {
  const done = waiting.get(e.data?.id);
  if (!done) return;
  waiting.delete(e.data.id);
  const { id, ...result } = e.data;
  done(result);
});

chrome.runtime.onMessage.addListener((msg, _sender, reply) => {
  if (msg.target !== 'offscreen' || msg.type !== 'solve') return false;
  ready.then(() => {
    const id = next++;
    waiting.set(id, reply);
    frame.contentWindow.postMessage({ id, version: msg.version, player: msg.player, n: msg.n, sig: msg.sig }, '*');
  });
  return true;
});
