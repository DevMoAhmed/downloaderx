// Sandboxed helper for YouTube (eval is allowed here, extension APIs are not):
// - op "solve": answers the player challenges (n and signature) with yt-dlp's EJS solver
//   (`jsc`). The player code is turned into a smaller "preprocessed" form once per version.
// - op "botguard": runs YouTube's BotGuard check and returns its answer; op "mint" then makes
//   a token (PO token) bound to an identifier, once Google has returned an integrity token.
const prepared = {};
let signals = null; // filled by BotGuard: [getMinter, ...]

function solve({ version, player, n, sig }) {
  if (!prepared[version] && !player) return { need: 'player' };
  const requests = [{ type: 'n', challenges: n || [] }, { type: 'sig', challenges: sig || [] }];
  const out = prepared[version]
    ? jsc({ type: 'preprocessed', preprocessed_player: prepared[version], requests })
    : jsc({ type: 'player', player, output_preprocessed: true, requests });
  if (out.preprocessed_player) prepared[version] = out.preprocessed_player;
  const [nr, sr] = out.responses;
  if (nr.type === 'error' && (n || []).length) throw new Error(nr.error);
  if (sr.type === 'error' && (sig || []).length) throw new Error(sr.error);
  return { n: nr.data || {}, sig: sr.data || {} };
}

async function botguard({ interpreter, program, globalName }) {
  (0, eval)(interpreter);
  const vm = globalThis[globalName];
  if (!vm) throw new Error('BotGuard did not load');
  const fns = await new Promise((resolve, reject) => {
    const timer = setTimeout(() => reject(new Error('BotGuard did not start')), 10000);
    vm.a(program, (snapshot, shutdown, passEvent, checkCamera) => { clearTimeout(timer); resolve({ snapshot }); }, true, undefined, () => {}, [[], []]);
  });
  signals = [];
  // Give it a moment to collect its signals, like a page that just loaded.
  await new Promise(r => setTimeout(r, 1500));
  const response = await new Promise(r => fns.snapshot(r, [undefined, undefined, signals, undefined]));
  return { response };
}

async function mint({ integrityToken, identifier }) {
  if (!signals?.[0]) throw new Error('BotGuard has not run');
  const bytes = s => Uint8Array.from(atob(s.replace(/-/g, '+').replace(/_/g, '/')), c => c.charCodeAt(0));
  const minter = await signals[0](bytes(integrityToken));
  const token = await minter(new TextEncoder().encode(identifier));
  return { pot: btoa(String.fromCharCode(...token)).replace(/\+/g, '-').replace(/\//g, '_') };
}

window.addEventListener('message', async e => {
  const { id, op = 'solve', ...args } = e.data || {};
  if (!id) return;
  let reply;
  try {
    reply = op === 'botguard' ? await botguard(args) : op === 'mint' ? await mint(args) : solve(args);
  } catch (err) {
    reply = { error: String(err && err.message || err).slice(0, 300) };
  }
  e.source.postMessage({ id, ...reply }, '*');
});
