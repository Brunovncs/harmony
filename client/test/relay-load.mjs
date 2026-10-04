// Relay load test: how much does a voice channel actually cost the server?
//
//   HARMONY_PASSWORD=... node client/test/relay-load.mjs \
//     --server https://stream.example.com:8444 \
//     --media  http://192.168.1.50:8889 \
//     --members 8
//
// The relay cannot mix audio, so every member subscribes to every other member
// individually and the cost at the server is N*(N-1) concurrent WebRTC
// sessions: 4 people is 12, 8 is 56, 16 is 240. That quadratic is structural,
// not a tuning choice, which is why the member cap is a real constraint and
// why this number has to be measured on the actual host rather than assumed.
//
// The Phase 0 spike already measured the CLIENT side (sixteen subscriptions
// for 7.3% of one core, zero loss). This measures the SERVER.
//
// --media rewrites the signaling host for the WHIP/WHEP URLs the server hands
// out. Point it at the relay's LAN address so the test measures the relay
// rather than your internet link.

import { spawn } from 'node:child_process';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

import { attach, sleep, waitUntil } from './cdp.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const ELECTRON = join(here, '..', 'node_modules', 'electron', 'dist',
  process.platform === 'win32' ? 'electron.exe' : 'electron');
const APP = join(here, 'relay-load');
const PORT = 9335;

const arg = (name, fallback) => {
  const i = process.argv.indexOf(`--${name}`);
  return i >= 0 ? process.argv[i + 1] : fallback;
};

const SERVER = (arg('server') ?? '').replace(/\/+$/, '');
const MEDIA = (arg('media') ?? '').replace(/\/+$/, '');
const MEMBERS = Number.parseInt(arg('members', '8'), 10);
const HOLD_SEC = Number.parseInt(arg('hold', '20'), 10);
const PASSWORD = process.env.HARMONY_PASSWORD ?? '';
const PREFIX = arg('prefix', 'loadtest');

if (!SERVER) {
  console.error('usage: node relay-load.mjs --server <url> [--media <url>] [--members N] [--hold S]');
  process.exit(2);
}

const headers = (token) => ({
  'Content-Type': 'application/json',
  ...(PASSWORD ? { 'X-Harmony-Password': PASSWORD } : {}),
  ...(token ? { Authorization: `Bearer ${token}` } : {}),
});

async function api(path, { method = 'GET', body, token } = {}) {
  const res = await fetch(`${SERVER}${path}`, {
    method,
    headers: headers(token),
    ...(body ? { body: JSON.stringify(body) } : {}),
  });
  const text = await res.text();
  let json = null;
  try { json = text ? JSON.parse(text) : null; } catch { /* non-JSON */ }
  if (!res.ok) throw new Error(`${path} -> ${res.status} ${json?.message ?? text.slice(0, 120)}`);
  return json;
}

/** Swap the signaling host so media stays on the LAN. */
const toMedia = (url) => (MEDIA ? url.replace(/^https?:\/\/[^/]+/, MEDIA) : url);

/** One simulated member: an account, a socket, a slot and its tokens. */
async function joinMember(index, channelId) {
  const nickname = `${PREFIX}${index}`;
  const password = 'loadtest-password';

  let token;
  try {
    token = (await api('/api/accounts/register', {
      method: 'POST', body: { nickname, password },
    })).token;
  } catch (err) {
    if (!/nickname_taken|already registered/i.test(err.message)) throw err;
    token = (await api('/api/accounts/login', {
      method: 'POST', body: { nickname, password },
    })).token;
  }

  const ws = new WebSocket(`${SERVER.replace(/^http/, 'ws')}/ws`);
  const pending = new Map();
  ws.addEventListener('message', (event) => {
    const msg = JSON.parse(event.data);
    const slot = pending.get(msg.rid);
    if (slot) { pending.delete(msg.rid); slot(msg); }
  });
  await new Promise((done, fail) => {
    ws.addEventListener('open', done, { once: true });
    ws.addEventListener('error', () => fail(new Error(`socket failed for ${nickname}`)), { once: true });
  });

  let rid = 0;
  const request = (payload) => new Promise((done, fail) => {
    const id = `r${rid += 1}`;
    const timer = setTimeout(() => fail(new Error(`${payload.type} timed out`)), 15_000);
    pending.set(id, (msg) => { clearTimeout(timer); done(msg); });
    ws.send(JSON.stringify({ ...payload, rid: id }));
  });

  const hello = await request({ type: 'hello', token });
  if (hello.type !== 'hello-ok') throw new Error(`hello failed for ${nickname}: ${hello.error}`);

  const joined = await request({ type: 'voice:join', channelId });
  if (joined.type !== 'voice:joined') {
    throw new Error(`join failed for ${nickname}: ${joined.error}`);
  }

  return { nickname, ws, request, mid: joined.mid, token: joined.token, publish: joined.publish };
}

// ---------------------------------------------------------------------------

let electron = null;
let cdp = null;
const joined = [];

async function main() {
  console.log(`relay load: ${MEMBERS} members -> ${MEMBERS * (MEMBERS - 1)} subscriptions`);
  console.log(`server ${SERVER}${MEDIA ? `   media ${MEDIA}` : ''}\n`);

  const { channels } = await api('/api/channels', {
    token: (await api('/api/accounts/login', {
      method: 'POST', body: { nickname: `${PREFIX}1`, password: 'loadtest-password' },
    }).catch(() => api('/api/accounts/register', {
      method: 'POST', body: { nickname: `${PREFIX}1`, password: 'loadtest-password' },
    }))).token,
  });
  const voice = channels.find((c) => c.kind === 'voice' && !c.locked);
  if (!voice) throw new Error('no open voice channel on this server');
  console.log(`using voice channel #${voice.id} "${voice.name}"`);

  // --- Electron ---------------------------------------------------------
  const env = { ...process.env };
  delete env.ELECTRON_RUN_AS_NODE;
  electron = spawn(ELECTRON, [APP, `--remote-debugging-port=${PORT}`], { env, stdio: 'ignore' });
  const page = await waitUntil(async () => {
    try {
      const list = await (await fetch(`http://127.0.0.1:${PORT}/json/list`)).json();
      return list.find((t) => t.type === 'page' && t.url.startsWith('file://'));
    } catch { return null; }
  }, { timeoutMs: 30_000, label: 'load-test renderer' });
  cdp = await attach(page);
  await waitUntil(() => cdp.evaluate('return window.loadReady === true;'), { label: 'page script' });
  await cdp.evaluate('return window.initMic();');

  // --- join -------------------------------------------------------------
  for (let i = 1; i <= MEMBERS; i += 1) {
    joined.push(await joinMember(i, voice.id));
    process.stdout.write(`\r  joined ${joined.length}/${MEMBERS}`);
  }
  console.log('');

  // --- publish ----------------------------------------------------------
  for (const member of joined) {
    const url = JSON.stringify(toMedia(member.publish.voice));
    const res = await cdp.evaluate(`return window.publishMember(${member.mid}, ${url});`);
    if (!res.ok) throw new Error(`publish failed for ${member.nickname}: ${JSON.stringify(res)}`);
    await member.request({ type: 'voice:publishing', channelId: voice.id, kind: 'v', on: true });
  }
  console.log(`  published ${joined.length} microphones`);

  // --- subscribe --------------------------------------------------------
  // Staggered, because the N-th joiner opening N-1 sessions at once is the
  // join storm the costing called out as the third thing to break.
  const t0 = Date.now();
  let opened = 0;
  let failed = 0;
  for (const member of joined) {
    for (const peer of joined) {
      if (peer.mid === member.mid) continue;
      const path = `vc-${voice.id.toString(36)}-${peer.mid.toString(36)}-v`;
      const url = JSON.stringify(
        `${toMedia(SERVER)}/${path}/whep?token=${encodeURIComponent(member.token)}`,
      );
      const res = await cdp.evaluate(
        `return window.subscribePeer(${member.mid}, ${peer.mid}, ${url});`,
      );
      if (res.ok) opened += 1;
      else { failed += 1; if (failed <= 3) console.log(`    sub failed: ${JSON.stringify(res)}`); }
      process.stdout.write(`\r  subscriptions ${opened}/${MEMBERS * (MEMBERS - 1)}`);
      await sleep(75);
    }
  }
  console.log(`\n  opened in ${((Date.now() - t0) / 1000).toFixed(1)}s (${failed} failed)\n`);

  // --- hold and measure --------------------------------------------------
  console.log(`holding ${HOLD_SEC}s ...`);
  const first = await cdp.evaluate('return window.sample();');
  await sleep(HOLD_SEC * 1000);
  const last = await cdp.evaluate('return window.sample();');

  const deltaBytes = last.bytes - first.bytes;
  const deltaPackets = last.packets - first.packets;
  const lossPct = last.packets ? (last.lost / (last.lost + last.packets)) * 100 : 0;
  const concealPct = last.samplesTotal ? (last.concealed / last.samplesTotal) * 100 : 0;

  console.log('');
  console.log(`  subscriptions connected : ${last.connected}/${last.total}`);
  console.log(`  downstream throughput   : ${((deltaBytes * 8) / HOLD_SEC / 1e6).toFixed(2)} Mbps`);
  console.log(`  packets/sec received    : ${Math.round(deltaPackets / HOLD_SEC)}`);
  console.log(`  packet loss             : ${lossPct.toFixed(3)}%`);
  console.log(`  concealed audio         : ${concealPct.toFixed(3)}%`);
  console.log(`  max RTT                 : ${(last.maxRtt * 1000).toFixed(1)} ms`);
  console.log('');
  console.log('Relay-side CPU is the number that matters; sample it on the host while this runs.');
}

main()
  .catch((err) => {
    console.error(`\nFAILED: ${err.message}`);
    process.exitCode = 1;
  })
  .finally(async () => {
    try { await cdp?.evaluate('return window.teardown();'); } catch { /* going away */ }
    for (const member of joined) {
      try { member.ws.close(); } catch { /* already closed */ }
    }
    cdp?.close();
    electron?.kill();
    await sleep(500);
  });
