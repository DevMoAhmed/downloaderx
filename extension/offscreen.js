// Relays requests from the background worker to the sandboxed helper frame (sandbox.js).
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
  if (msg.target !== 'offscreen') return false;
  const { target, ...args } = msg;
  ready.then(() => {
    const id = next++;
    waiting.set(id, reply);
    frame.contentWindow.postMessage({ id, ...args }, '*');
  });
  return true;
});
