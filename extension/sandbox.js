// Solves YouTube's player challenges (n and signature) with yt-dlp's EJS solver (`jsc`).
// The player code is turned into a smaller "preprocessed" form once per player version.
const prepared = {};

window.addEventListener('message', e => {
  const { id, version, player, n, sig } = e.data || {};
  if (!id) return;
  let reply;
  try {
    if (!prepared[version] && !player) {
      reply = { need: 'player' };
    } else {
      const requests = [{ type: 'n', challenges: n || [] }, { type: 'sig', challenges: sig || [] }];
      const out = prepared[version]
        ? jsc({ type: 'preprocessed', preprocessed_player: prepared[version], requests })
        : jsc({ type: 'player', player, output_preprocessed: true, requests });
      if (out.preprocessed_player) prepared[version] = out.preprocessed_player;
      const [nr, sr] = out.responses;
      if (nr.type === 'error' && (n || []).length) throw new Error(nr.error);
      if (sr.type === 'error' && (sig || []).length) throw new Error(sr.error);
      reply = { n: nr.data || {}, sig: sr.data || {} };
    }
  } catch (err) {
    reply = { error: String(err && err.message || err).slice(0, 300) };
  }
  e.source.postMessage({ id, ...reply }, '*');
});
