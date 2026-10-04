// The channel mosaic, end to end.
//
// Starts a real MediaMTX and a real Harmony control server, registers two
// accounts, joins them both to a voice channel over the WebSocket, then
// publishes video into one member's channel path and subscribes to it from
// the other member's token. Nothing is mocked: this is the real WHIP/WHEP
// path through the real auth hook.
//
// It exists because the channel-scoped media paths are the one part of the
// design that cannot be checked without MediaMTX. The server tests prove the
// URLs are minted and the auth hook answers correctly; only this proves that
// frames actually arrive.
//
//   MEDIAMTX_BIN=/path/to/mediamtx node client/test/channel-video.mjs

import { spawn } from 'node:child_process';
import { mkdtempSync, existsSync, copyFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

import { attach, reporter, sleep, waitUntil } from './cdp.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..');
const serverEntry = resolve(repoRoot, 'server', 'src', 'index.js');
const mediamtxConfig = resolve(repoRoot, 'server', 'mediamtx.yml');
const ELECTRON = join(here, '..', 'node_modules', 'electron', 'dist',
  process.platform === 'win32' ? 'electron.exe' : 'electron');
const APP = join(here, 'channel-video');

const MEDIAMTX_BIN = process.env.MEDIAMTX_BIN;

// Its own ports. This runs alongside nothing, but the server suite and e2e.mjs
// both bind fixed ports and a collision here would look like a media failure.
const HARMONY_PORT = 18110;
const MTX_API_PORT = 9987;
const MTX_WEBRTC_PORT = 8879;
// The ICE host ports too, not just the signalling one: two MediaMTX instances
// on one machine collide on webrtcLocalUDPAddress long before they collide on
// anything visible, and the symptom is a subscription that signals fine and
// then never connects.
const MTX_ICE_PORT = 8179;
const DEBUG_PORT = 9336;
const SIGNALING = `http://127.0.0.1:${MTX_WEBRTC_PORT}`;

const dataDir = mkdtempSync(join(tmpdir(), 'harmony-chanvid-'));
const { check, summary } = reporter();
const procs = [];
let mtxLog = '';
let serverLog = '';
let ownerKey = null;
let cdp = null;
let electron = null;

function cleanup() {
  for (const p of procs) {
    try { p.kill(); } catch { /* already gone */ }
  }
}
process.on('exit', cleanup);
process.on('SIGINT', () => process.exit(130));

// ---------------------------------------------------------------------------

async function startMediaMtx() {
  if (!MEDIAMTX_BIN || !existsSync(MEDIAMTX_BIN)) {
    throw new Error('Set MEDIAMTX_BIN to a MediaMTX binary to run this test.');
  }
  const dir = mkdtempSync(join(tmpdir(), 'harmony-mtx-cv-'));
  const cfg = join(dir, 'mediamtx.yml');
  copyFileSync(mediamtxConfig, cfg);

  const p = spawn(MEDIAMTX_BIN, [cfg], {
    env: {
      ...process.env,
      MTX_AUTHHTTPADDRESS: `http://127.0.0.1:${HARMONY_PORT}/mediamtx/auth`,
      MTX_WEBRTCADDRESS: `:${MTX_WEBRTC_PORT}`,
      MTX_APIADDRESS: `127.0.0.1:${MTX_API_PORT}`,
      MTX_WEBRTCLOCALUDPADDRESS: `:${MTX_ICE_PORT}`,
      MTX_WEBRTCLOCALTCPADDRESS: `:${MTX_ICE_PORT}`,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  procs.push(p);
  p.stdout.on('data', (b) => (mtxLog += b.toString()));
  p.stderr.on('data', (b) => (mtxLog += b.toString()));

  await waitUntil(() => new RegExp(`\\[WebRTC\\].*:${MTX_WEBRTC_PORT}`).test(mtxLog), {
    label: 'MediaMTX startup',
  });
  return p;
}

async function startHarmonyServer() {
  const p = spawn(process.execPath, [serverEntry], {
    env: {
      ...process.env,
      HARMONY_PORT: String(HARMONY_PORT),
      HARMONY_HOST: '127.0.0.1',
      HARMONY_DATA_DIR: dataDir,
      HARMONY_MEDIAMTX_API: `http://127.0.0.1:${MTX_API_PORT}`,
      HARMONY_SIGNALING_URL: SIGNALING,
      HARMONY_POLL_INTERVAL_MS: '400',
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  procs.push(p);
  p.stdout.on('data', (b) => {
    serverLog += b.toString();
    const match = /^\s*│\s+([A-Za-z0-9_-]{20,})\s+│$/m.exec(serverLog);
    if (match) ownerKey = match[1];
  });
  p.stderr.on('data', (b) => (serverLog += b.toString()));

  await waitUntil(() => serverLog.includes('control server on'), {
    label: 'Harmony server startup',
  });
  return p;
}

const BASE = `http://127.0.0.1:${HARMONY_PORT}`;

async function api(path, { method = 'GET', body, token } = {}) {
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers: {
      ...(body ? { 'Content-Type': 'application/json' } : {}),
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    ...(body ? { body: JSON.stringify(body) } : {}),
  });
  const text = await res.text();
  let json = null;
  try { json = text ? JSON.parse(text) : null; } catch { /* non-JSON */ }
  if (!res.ok) throw new Error(`${path} -> ${res.status} ${json?.message ?? text.slice(0, 120)}`);
  return json;
}

/** One member: an account, a socket, a slot and its media URLs. */
async function joinMember(nickname, channelId, { ownerKey: key } = {}) {
  const { token } = await api('/api/accounts/register', {
    method: 'POST',
    body: { nickname, password: 'channelvideo', ...(key ? { ownerKey: key } : {}) },
  });

  const ws = new WebSocket(`${BASE.replace(/^http/, 'ws')}/ws`);
  const pending = new Map();
  const events = [];
  ws.addEventListener('message', (event) => {
    const msg = JSON.parse(event.data);
    events.push(msg);
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
  if (hello.type !== 'hello-ok') throw new Error(`hello failed: ${hello.error}`);

  const joined = await request({ type: 'voice:join', channelId });
  if (joined.type !== 'voice:joined') throw new Error(`join failed: ${joined.error}`);

  return { nickname, token, ws, request, events, ...joined };
}

const whep = (member, mid, kind) =>
  `${SIGNALING}/vc-${member.channelId.toString(36)}-${mid.toString(36)}-${kind}`
  + `/whep?token=${encodeURIComponent(member.token)}`;

const mtxPaths = async () =>
  (await (await fetch(`http://127.0.0.1:${MTX_API_PORT}/v3/paths/list`)).json()).items ?? [];

// ---------------------------------------------------------------------------

async function run() {
  console.log('Starting MediaMTX and the Harmony control server…\n');
  await startMediaMtx();
  await startHarmonyServer();
  check('MediaMTX and the control server are up', true);

  // --- the page that owns the peer connections --------------------------
  const env = { ...process.env };
  delete env.ELECTRON_RUN_AS_NODE;
  electron = spawn(ELECTRON, [APP, `--remote-debugging-port=${DEBUG_PORT}`], {
    env, stdio: 'ignore',
  });
  procs.push(electron);
  const page = await waitUntil(async () => {
    try {
      const list = await (await fetch(`http://127.0.0.1:${DEBUG_PORT}/json/list`)).json();
      return list.find((t) => t.type === 'page' && t.url.startsWith('file://'));
    } catch { return null; }
  }, { timeoutMs: 30_000, label: 'channel-video renderer' });
  cdp = await attach(page);
  await waitUntil(() => cdp.evaluate('return window.channelVideoReady === true;'), {
    label: 'page script',
  });

  // --- two members in one voice channel ---------------------------------
  const bootstrap = await api('/api/accounts/register', {
    method: 'POST', body: { nickname: 'chanowner', password: 'channelvideo', ownerKey },
  });
  const { channels } = await api('/api/channels', { token: bootstrap.token });
  const voice = channels.find((c) => c.kind === 'voice');
  check('the server seeded a voice channel', Boolean(voice), voice?.name);

  const alice = await joinMember('alice', voice.id);
  const bob = await joinMember('bob', voice.id);
  check(
    'two members joined and got different slots',
    alice.mid !== bob.mid,
    `alice=${alice.mid} bob=${bob.mid}`,
  );
  check(
    'each member was given a camera and a screen path of its own',
    alice.publish.cam.includes(`-${alice.mid.toString(36)}-c/whip`)
      && alice.publish.screen.includes(`-${alice.mid.toString(36)}-s/whip`),
    alice.publish.cam.replace(SIGNALING, ''),
  );

  // --- publishing a camera into the channel -----------------------------
  const published = await cdp.evaluate(
    `return window.publishVideo('alice-cam', ${JSON.stringify(alice.publish.cam)}, 'alice');`,
  );
  check(
    'a camera publishes to its channel path',
    published.ok === true,
    published.ok ? '201 from WHIP' : JSON.stringify(published),
  );

  const camPath = `vc-${voice.id.toString(36)}-${alice.mid.toString(36)}-c`;
  const seen = await waitUntil(async () => {
    const paths = await mtxPaths();
    return paths.find((p) => p.name === camPath && p.ready) ?? null;
  }, { timeoutMs: 15_000, label: 'the camera path going ready' }).catch(() => null);
  check(
    'MediaMTX reports the channel camera path as live',
    Boolean(seen),
    seen ? `tracks: ${seen.tracks?.join(', ')}` : `no ready ${camPath}`,
  );

  // --- the other member watches it --------------------------------------
  //
  // bob's token, alice's slot. The auth hook only compares the CHANNEL for a
  // read, which is exactly what makes a mosaic possible: every member can
  // watch every other member with the one token they were issued on join.
  const subscribed = await cdp.evaluate(
    `return window.subscribeVideo('bob-sees-alice', ${JSON.stringify(whep(bob, alice.mid, 'c'))});`,
  );
  check(
    "another member's token opens the subscription",
    subscribed.ok === true,
    subscribed.ok ? '201 from WHEP' : JSON.stringify(subscribed),
  );

  const decoding = await waitUntil(async () => {
    const s = await cdp.evaluate("return window.videoStats('bob-sees-alice');");
    // More than one: a single decoded frame is just the keyframe, which
    // arrives even when nothing is flowing afterwards.
    return s.framesDecoded >= 3 ? s : null;
  }, { timeoutMs: 20_000, label: 'frames arriving' }).catch(() => null);
  check(
    'video frames actually arrive at the other member',
    Boolean(decoding),
    decoding
      ? `${decoding.framesDecoded} frames, ${decoding.bytes} bytes, ${decoding.width}px`
      : 'nothing decoded',
  );

  // --- the roster is what tells everyone to subscribe -------------------
  await alice.request({ type: 'voice:publishing', channelId: voice.id, kind: 'c', on: true });
  await sleep(400);
  const roster = [...bob.events].reverse().find((m) => m.type === 'voice:roster');
  const aliceRow = roster?.roster?.find((m) => m.mid === alice.mid);
  check(
    'the roster tells other members a camera is publishing',
    aliceRow?.publishing?.includes('c') === true,
    JSON.stringify(aliceRow?.publishing ?? null),
  );

  // --- a screen share is a separate path, not a second track ------------
  const screened = await cdp.evaluate(
    `return window.publishVideo('bob-screen', ${JSON.stringify(bob.publish.screen)}, 'bob screen');`,
  );
  check(
    'a screen share publishes alongside the camera, on its own path',
    screened.ok === true,
    screened.ok ? '201 from WHIP' : JSON.stringify(screened),
  );

  const bothLive = await waitUntil(async () => {
    const names = (await mtxPaths()).filter((p) => p.ready).map((p) => p.name);
    const screenPath = `vc-${voice.id.toString(36)}-${bob.mid.toString(36)}-s`;
    return names.includes(camPath) && names.includes(screenPath) ? names : null;
  }, { timeoutMs: 15_000, label: 'both paths live' }).catch(() => null);
  check(
    "one member's camera and another's screen are live at the same time",
    Boolean(bothLive),
    bothLive ? bothLive.join(', ') : 'not both ready',
  );

  // --- and none of it leaks into the flat namespace ---------------------
  const { streams } = await api('/api/streams', { token: bob.token });
  check(
    'channel paths never appear in /api/streams',
    streams.every((s) => !s.username.startsWith('vc-')),
    `${streams.length} flat streams: ${streams.map((s) => s.username).join(', ') || '(none)'}`,
  );

  // --- a stranger with no slot in the channel cannot watch --------------
  //
  // The reason channel-scoped paths exist at all: a share inside a channel is
  // not reachable by someone who was never admitted to it.
  const stranger = await fetch(
    `${SIGNALING}/${camPath}/whep?token=not-a-real-token`,
    { method: 'POST', headers: { 'Content-Type': 'application/sdp' }, body: 'v=0\r\n' },
  );
  check(
    'a bad channel token cannot read a channel path',
    stranger.status === 401 || stranger.status === 403,
    `WHEP answered ${stranger.status}`,
  );

  // --- leaving stops the publish ----------------------------------------
  await cdp.evaluate('return window.teardownAll();');
  await alice.request({ type: 'voice:leave', channelId: voice.id });
  await sleep(500);
  const afterLeave = [...bob.events].reverse().find((m) => m.type === 'voice:roster');
  check(
    'leaving removes the member, and with them their tiles',
    afterLeave?.roster?.every((m) => m.mid !== alice.mid) === true,
    `${afterLeave?.roster?.length ?? '?'} left in the channel`,
  );

  alice.ws.close();
  bob.ws.close();
}

run()
  .catch((err) => check('channel-video run completed', false, err.message))
  .finally(async () => {
    try { await cdp?.evaluate('return window.teardownAll();'); } catch { /* going away */ }
    cdp?.close();
    cleanup();
    await sleep(500);
    rmSync(dataDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
    const failed = summary();
    if (failed) {
      console.error(`
--- mediamtx ---
${mtxLog.slice(-3000)}`);
      console.error(`
--- harmony server ---
${serverLog.slice(-2500)}`);
    }
    process.exit(failed ? 1 : 0);
  });
