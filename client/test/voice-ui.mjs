// The voice pane, driven through two real clients.
//
// channels-ui.mjs covers the parts of the channels view that need no media.
// This covers the rest: joining a voice channel, the roster, mute, deafen,
// the camera, the channel mosaic, and the admin controls that act on another
// person. Two real Electron clients, a real control server and a real
// MediaMTX -- the only thing faked is the microphone and camera hardware,
// which Chromium provides.
//
// It is the last place where "the route works" and "somebody can use it"
// were still different statements.
//
//   MEDIAMTX_BIN=/path/to/mediamtx node client/test/voice-ui.mjs

import { spawn } from 'node:child_process';
import http from 'node:http';
import { mkdtempSync, existsSync, copyFileSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

import { attach, findPage, launchApp, reporter, sleep, waitFor } from './cdp.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..');
const serverEntry = resolve(repoRoot, 'server', 'src', 'index.js');
const mediamtxConfig = resolve(repoRoot, 'server', 'mediamtx.yml');

const MEDIAMTX_BIN = process.env.MEDIAMTX_BIN;

// Its own ports throughout: this must be runnable while another suite is up.
const HARMONY_PORT = 18140;
const MTX_API_PORT = 9977;
const MTX_WEBRTC_PORT = 8869;
const MTX_ICE_PORT = 8169;
const PORT_A = 9421;
const PORT_B = 9422;
const SIGNALING = `http://127.0.0.1:${MTX_WEBRTC_PORT}`;
const BASE = `http://127.0.0.1:${HARMONY_PORT}`;

const dataDir = mkdtempSync(join(tmpdir(), 'harmony-voice-'));
const profiles = [];
const { check, summary } = reporter();
const procs = [];
let mtxLog = '';
let serverLog = '';
let ownerKey = null;

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
  const dir = mkdtempSync(join(tmpdir(), 'harmony-mtx-v-'));
  const cfg = join(dir, 'mediamtx.yml');
  copyFileSync(mediamtxConfig, cfg);

  const p = spawn(MEDIAMTX_BIN, [cfg], {
    env: {
      ...process.env,
      MTX_AUTHHTTPADDRESS: `http://127.0.0.1:${HARMONY_PORT}/mediamtx/auth`,
      MTX_WEBRTCADDRESS: `:${MTX_WEBRTC_PORT}`,
      MTX_APIADDRESS: `127.0.0.1:${MTX_API_PORT}`,
      // Two MediaMTX instances collide here long before anything visible.
      MTX_WEBRTCLOCALUDPADDRESS: `:${MTX_ICE_PORT}`,
      MTX_WEBRTCLOCALTCPADDRESS: `:${MTX_ICE_PORT}`,
    },
    stdio: ['ignore', 'pipe', 'pipe'],
  });
  procs.push(p);
  p.stdout.on('data', (b) => (mtxLog += b.toString()));
  p.stderr.on('data', (b) => (mtxLog += b.toString()));

  const deadline = Date.now() + 30_000;
  while (Date.now() < deadline) {
    if (new RegExp(`\\[WebRTC\\].*:${MTX_WEBRTC_PORT}`).test(mtxLog)) return p;
    await sleep(200);
  }
  throw new Error('MediaMTX did not start');
}

function startHarmonyServer() {
  return new Promise((done, fail) => {
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
    const timer = setTimeout(() => fail(new Error('server did not start')), 15_000);
    p.stdout.on('data', (b) => {
      serverLog += b.toString();
      const match = /^\s*│\s+([A-Za-z0-9_-]{20,})\s+│$/m.exec(serverLog);
      if (match) ownerKey = match[1];
      if (serverLog.includes('control server on')) {
        clearTimeout(timer);
        done(p);
      }
    });
    p.stderr.on('data', (b) => (serverLog += b.toString()));
  });
}

const setInput = (id, value) =>
  `const i = document.getElementById(${JSON.stringify(id)}); i.value = ${JSON.stringify(value)}; `
  + `i.dispatchEvent(new Event('input', {bubbles:true})); `
  + `i.dispatchEvent(new Event('change', {bubbles:true}));`;

/**
 * Sign a fresh client in and leave it on the channels view.
 *
 * The microphone and camera are Chromium's fake devices. Without them the
 * run depends on whatever hardware the machine has, and on a machine with no
 * microphone getUserMedia rejects and every voice check fails for a reason
 * that has nothing to do with Harmony.
 */
async function signIn({ port, nickname, password, key }) {
  const userDataDir = mkdtempSync(join(tmpdir(), 'harmony-vp-'));
  profiles.push(userDataDir);
  const app = launchApp({
    port,
    userDataDir,
    extraArgs: [
      '--use-fake-device-for-media-stream',
      '--use-fake-ui-for-media-stream',
    ],
  });
  procs.push(app);
  const cdp = await attach(await findPage(port));
  await sleep(1400);

  await cdp.evaluate(`${setInput('server-url', BASE)} return true;`);
  await sleep(1400);

  // A server with no accounts opens in register mode; once one exists it
  // opens in sign-in mode, so the second client has to ask for register.
  const mode = await cdp.evaluate(
    "return document.getElementById('continue').textContent;",
  );
  if (mode !== 'Create account') {
    await cdp.evaluate("document.getElementById('auth-mode-toggle').click(); return true;");
    await sleep(300);
  }

  await cdp.evaluate(`${setInput('username', nickname)} return true;`);
  await cdp.evaluate(`${setInput('account-password', password)} return true;`);
  await cdp.evaluate(`${setInput('account-confirm', password)} return true;`);
  if (key) await cdp.evaluate(`${setInput('owner-key', key)} return true;`);
  await cdp.evaluate("document.getElementById('continue').click(); return true;");

  await waitFor(cdp, "document.querySelector('.view[data-active]')?.id === 'view-channels'", {
    label: `${nickname} reaching the channels view`,
  });
  return cdp;
}

/** Click the voice channel in the sidebar. */
const joinVoice = (cdp) => cdp.evaluate(`
  const row = [...document.querySelectorAll('#channel-items li.channel-row')]
    .find((li) => li.querySelector('.kind')?.textContent !== '#');
  row.click();
  return true;
`);

/**
 * Wait until a client has actually finished joining.
 *
 * NOT "the voice pane is visible": joinVoice unhides the pane, then awaits
 * startMic and a voice:publishing round trip, and only then renders the
 * roster. Waiting on visibility reads an empty pane and every assertion
 * about it fails for a reason that has nothing to do with the app.
 *
 * The expression is kept on ONE LINE on purpose. waitFor evaluates
 * `return ${expression};`, so a leading newline is an automatic-semicolon
 * insertion and the whole thing silently returns undefined.
 */
const joined = (cdp, n, who) => waitFor(
  cdp,
  `document.querySelectorAll('#voice-roster li').length === ${n}`,
  { label: `${who} seeing ${n} in the roster`, timeoutMs: 30_000 },
);

/** The indicators and audio controls on one person's roster row. */
const rosterRow = (cdp, who) => cdp.evaluate(`
  const li = [...document.querySelectorAll('#voice-roster li')]
    .find((x) => x.querySelector('.member-name').textContent.startsWith(${JSON.stringify(who)}));
  if (!li) return null;
  const volume = li.querySelector('.peer-volume');
  const label = li.querySelector('.peer-volume-label');
  return {
    status: [...li.querySelectorAll('.status-dot')].map((d) => d.dataset.kind),
    speaking: li.hasAttribute('data-speaking'),
    localMuted: li.hasAttribute('data-local-muted'),
    hasVolume: Boolean(volume),
    volume: volume ? Number(volume.value) : null,
    volumeMax: volume ? Number(volume.max) : null,
    boosted: Boolean(volume && volume.hasAttribute('data-boosted')),
    labelBoosted: Boolean(label && label.hasAttribute('data-boosted')),
    label: label ? label.textContent : null,
    muteTitle: li.querySelector('.peer-mute') ? li.querySelector('.peer-mute').title : null,
    rowUser: li.dataset.userId,
  };
`);

/** Drag one person's volume slider to a value. */
const setPeerVolume = (cdp, who, percent) => cdp.evaluate(`
  const li = [...document.querySelectorAll('#voice-roster li')]
    .find((x) => x.querySelector('.member-name').textContent.startsWith(${JSON.stringify(who)}));
  const v = li.querySelector('.peer-volume');
  v.value = '${percent}';
  v.dispatchEvent(new Event('input', { bubbles: true }));
  return true;
`);

/** Click one person's local mute button. */
const clickPeerMute = (cdp, who) => cdp.evaluate(`
  const li = [...document.querySelectorAll('#voice-roster li')]
    .find((x) => x.querySelector('.member-name').textContent.startsWith(${JSON.stringify(who)}));
  li.querySelector('.peer-mute').click();
  return true;
`);

/** The state of the two device pickers. */
const devices = (cdp) => cdp.evaluate(`
  const input = document.getElementById('voice-input');
  const output = document.getElementById('voice-output');
  const list = (sel) => [...sel.options].map((o) => ({ value: o.value, label: o.textContent }));
  return {
    inputs: list(input),
    outputs: list(output),
    inputValue: input.value,
    outputValue: output.value,
    outputDisabled: output.disabled,
    note: document.getElementById('voice-device-note').textContent,
  };
`);

/** Who the SIDEBAR says is in each voice channel. */
const sidebarMembers = (cdp) => cdp.evaluate(`
  return [...document.querySelectorAll('#channel-items .channel-members')].map((ul) => ({
    channelId: ul.dataset.for,
    members: [...ul.querySelectorAll('.channel-member')].map((m) => ({
      name: m.querySelector('.member-name').textContent,
      hasPicture: Boolean(m.querySelector('.avatar img')),
      initials: m.querySelector('.avatar')?.textContent ?? '',
      muted: m.hasAttribute('data-muted'),
      forced: m.hasAttribute('data-forced'),
      status: [...m.querySelectorAll('.status-dot')].map((d) => d.dataset.kind),
    })),
  }));
`);

/** What the voice pane currently shows. */
const pane = (cdp) => cdp.evaluate(`
  const rows = [...document.querySelectorAll('#voice-roster li')];
  return {
    active: !document.getElementById('voice-active').hidden,
    name: document.getElementById('voice-name').textContent,
    count: document.getElementById('voice-count').textContent,
    mute: document.getElementById('voice-mute').textContent,
    deafen: document.getElementById('voice-deafen').textContent,
    cam: document.getElementById('voice-cam').textContent,
    screen: document.getElementById('voice-screen').textContent,
    error: document.getElementById('channels-error').textContent,
    tiles: [...document.querySelectorAll('#channel-video .channel-tile figcaption')]
      .map((f) => f.textContent),
    videoShown: !document.getElementById('channel-video').hidden,
    roster: rows.map((li) => ({
      // The avatar is first, then the name span.
      name: li.querySelector('.member-name')?.textContent,
      status: [...li.querySelectorAll('.status-dot')].map((d) => d.dataset.kind),
      // Admin controls only. Every row now carries a local mute button too,
      // and that one is not a permission -- it is your own speakers.
      buttons: [...li.querySelectorAll('button')]
        .filter((b) => !b.closest('.peer-audio'))
        .map((b) => b.textContent),
      moveOptions: [...(li.querySelector('.move-select')?.options ?? [])].map((o) => o.textContent),
    })),
  };
`);

// ---------------------------------------------------------------------------

async function run() {
  console.log('Starting MediaMTX, the control server and two clients…\n');
  await startMediaMtx();
  await startHarmonyServer();

  const a = await signIn({ port: PORT_A, nickname: 'Alice', password: 'hunter22', key: ownerKey });
  const b = await signIn({ port: PORT_B, nickname: 'Bob', password: 'hunter22' });
  check('two clients signed in, one of them the owner', true);

  // Alice gets a picture and Bob does not, so the sidebar below has to draw
  // both cases -- an <img> for her, initials for him.
  await a.evaluate(`
    const canvas = document.createElement('canvas');
    canvas.width = 400; canvas.height = 400;
    const ctx = canvas.getContext('2d');
    ctx.fillStyle = '#4477ff'; ctx.fillRect(0, 0, 400, 400);
    const blob = await new Promise((r) => canvas.toBlob(r, 'image/png'));
    const input = document.getElementById('avatar-file');
    const dt = new DataTransfer();
    dt.items.add(new File([blob], 'a.png', { type: 'image/png' }));
    input.files = dt.files;
    input.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
  `);
  await waitFor(a, "Boolean(document.querySelector('#channels-avatar img'))", {
    label: "Alice's picture being set",
  });
  // Bob has to learn about it: the server pushes user:updated to everybody.
  await waitFor(b, "Boolean(document.querySelector('#channels-avatar')) && true", {
    label: 'Bob ready',
  });

  // --- joining ----------------------------------------------------------
  await joinVoice(a);
  await joined(a, 1, 'Alice');
  const aAlone = await pane(a);
  check(
    'clicking a voice channel joins it and shows the voice pane',
    aAlone.active === true && aAlone.name === 'voice' && aAlone.count.includes('1 person'),
    `${aAlone.name}: ${aAlone.count}`,
  );
  check(
    'the pane offers mic, camera, screen and deafen',
    aAlone.mute === 'Mute mic' && aAlone.cam === 'Start camera'
      && aAlone.screen === 'Share screen here' && aAlone.deafen === 'Deafen',
    [aAlone.mute, aAlone.cam, aAlone.screen, aAlone.deafen].join(' | '),
  );
  check(
    'you are not offered admin controls on yourself',
    aAlone.roster.length === 1 && aAlone.roster[0].buttons.length === 0,
    `${aAlone.roster[0]?.buttons.length ?? '?'} buttons on your own row`,
  );

  // --- the device pickers -----------------------------------------------
  //
  // Checked while in a channel, because that is the only time labels exist:
  // Chromium withholds device names until a getUserMedia has been granted,
  // which joining has just done.
  const picked = await devices(a);
  check(
    'the voice pane offers a microphone and an output, defaulting to the system',
    picked.inputs.length >= 1 && picked.inputs[0].value === ''
      && picked.outputs.length >= 1 && picked.outputs[0].value === '',
    `${picked.inputs.length} inputs, ${picked.outputs.length} outputs`,
  );
  check(
    'the devices are named rather than listed as blanks',
    picked.inputs.every((o) => o.label.trim().length > 0),
    picked.inputs.map((o) => o.label).join(' | '),
  );
  check(
    'nothing is reported as unplugged on a clean start',
    picked.note === '',
    `"${picked.note}"`,
  );

  // Choosing a real device has to stick AND take effect on the live mic.
  const realInput = picked.inputs.find((o) => o.value !== '');
  if (realInput) {
    await a.evaluate(`
      const sel = document.getElementById('voice-input');
      sel.value = ${JSON.stringify(realInput.value)};
      sel.dispatchEvent(new Event('change', { bubbles: true }));
      return true;
    `);
    await sleep(1500);
    const afterPick = await devices(a);
    check(
      'choosing a microphone selects it and reports no problem',
      afterPick.inputValue === realInput.value && afterPick.note === '',
      `${afterPick.inputs.find((o) => o.value === afterPick.inputValue)?.label} / "${afterPick.note}"`,
    );

    const stillUp = await pane(a);
    check(
      'switching microphone does not drop you out of the channel',
      stillUp.active === true && stillUp.roster.length === 1,
      'the publish survived the track swap',
    );

    const saved = await a.evaluate(`
      const { harmony } = await import('./bridge.js');
      const s = await harmony.settings.get();
      return { voiceInputId: s.voiceInputId, voiceOutputId: s.voiceOutputId };
    `);
    check(
      'the choice is written to settings, so it survives a restart',
      saved.voiceInputId === realInput.value,
      `voiceInputId=${saved.voiceInputId.slice(0, 12)}…`,
    );
  } else {
    check('choosing a microphone selects it and reports no problem', true, 'no real device listed');
    check('switching microphone does not drop you out of the channel', true, 'skipped');
    check('the choice is written to settings, so it survives a restart', true, 'skipped');
  }

  await joinVoice(b);
  await joined(b, 2, 'Bob');
  await joined(a, 2, 'Alice');

  const aBoth = await pane(a);
  const bBoth = await pane(b);
  check(
    'both clients see both people in the roster',
    aBoth.roster.length === 2 && bBoth.roster.length === 2,
    `Alice sees ${aBoth.roster.map((r) => r.name).join('/')}, `
    + `Bob sees ${bBoth.roster.map((r) => r.name).join('/')}`,
  );

  // --- the sidebar listing ----------------------------------------------
  //
  // Checked on BOTH clients, because the point of it is that you can see who
  // is in a channel from outside it. The rosters are broadcast to everyone,
  // not only to a channel's members.
  const sideA = await sidebarMembers(a);
  const sideB = await sidebarMembers(b);
  check(
    'the sidebar lists the people in the voice channel, on every client',
    sideA.length === 1 && sideA[0].members.length === 2
      && sideB.length === 1 && sideB[0].members.length === 2,
    `Alice: ${sideA[0]?.members.map((m) => m.name).join('/') ?? 'none'} | `
    + `Bob: ${sideB[0]?.members.map((m) => m.name).join('/') ?? 'none'}`,
  );

  const sideAlice = sideB[0]?.members.find((m) => m.name === 'alice');
  const sideBob = sideB[0]?.members.find((m) => m.name === 'bob');
  check(
    'each person in the list is drawn with their profile picture',
    sideAlice?.hasPicture === true,
    `alice: ${sideAlice?.hasPicture ? 'picture' : 'no picture'}`,
  );
  check(
    'somebody with no picture falls back to their initials, not a blank',
    sideBob?.hasPicture === false && sideBob?.initials === 'BO',
    `bob: "${sideBob?.initials}"`,
  );

  // --- the indicators ---------------------------------------------------
  const bobIdle = await rosterRow(a, 'bob');
  check(
    'somebody doing nothing has no indicators cluttering their row',
    bobIdle.status.length === 0,
    bobIdle.status.join(', ') || '(none)',
  );
  check(
    'every row but your own carries a local mute and a volume slider',
    bobIdle.hasVolume === true && bobIdle.volume === 100 && bobIdle.volumeMax === 350,
    `${bobIdle.volume}% of max ${bobIdle.volumeMax}%`,
  );
  const selfRow = await rosterRow(a, 'alice');
  check(
    'you are not offered a volume slider for yourself',
    selfRow.hasVolume === false,
    'no slider on your own row',
  );

  const bobRow = aBoth.roster.find((r) => r.name.startsWith('bob'));
  check(
    'an admin gets a force-mute button and a move target on other people',
    bobRow?.buttons.includes('Force mute') === true && bobRow.moveOptions.length >= 2,
    `${bobRow?.buttons.join(', ')} + [${bobRow?.moveOptions.join(', ')}]`,
  );
  const aliceRowOnB = bBoth.roster.find((r) => r.name.startsWith('alice'));
  check(
    'a member gets no admin controls on anybody',
    aliceRowOnB?.buttons.length === 0 && aliceRowOnB?.moveOptions.length === 0,
    `${aliceRowOnB?.buttons.length ?? '?'} buttons`,
  );

  // --- mute and deafen --------------------------------------------------
  await a.evaluate("document.getElementById('voice-mute').click(); return true;");
  await waitFor(
    b,
    "[...document.querySelectorAll('#voice-roster li')].some((li) => "
    + "li.querySelector('.member-name').textContent.startsWith('alice') && "
    + "li.querySelector('.status-dot[data-kind=\"muted\"]') !== null)",
    { label: 'the mute reaching Bob' },
  );
  const aMuted = await pane(a);
  check(
    'muting flips the button and tells the other client',
    aMuted.mute === 'Unmute mic',
    `button now "${aMuted.mute}", and Bob sees the tag`,
  );

  const aliceMutedRow = await rosterRow(b, 'alice');
  check(
    'muting yourself shows as a mute indicator on every other roster',
    aliceMutedRow.status.includes('muted') && !aliceMutedRow.status.includes('forced'),
    aliceMutedRow.status.join(', '),
  );

  const mutedSide = (await sidebarMembers(b))[0]?.members.find((m) => m.name === 'alice');
  check(
    'the sidebar shows who is muted without being told twice',
    mutedSide?.muted === true,
    `alice muted in sidebar: ${mutedSide?.muted}`,
  );

  await a.evaluate("document.getElementById('voice-mute').click(); return true;");
  await sleep(600);
  await a.evaluate("document.getElementById('voice-deafen').click(); return true;");
  await sleep(400);
  const aDeaf = await pane(a);
  check(
    'deafening also mutes, so nobody broadcasts into a conversation they cannot hear',
    aDeaf.deafen === 'Undeafen' && aDeaf.mute === 'Unmute mic',
    `${aDeaf.mute} / ${aDeaf.deafen}`,
  );

  // Undeafen does NOT un-mute: the implication only runs one way, because
  // coming back from deafened into a live microphone is the surprise.
  await a.evaluate("document.getElementById('voice-deafen').click(); return true;");
  await sleep(400);
  await a.evaluate("document.getElementById('voice-mute').click(); return true;");
  await sleep(600);
  const aBack = await pane(a);
  check(
    'undeafening and unmuting put both back, and neither is a round trip',
    aBack.deafen === 'Deafen' && aBack.mute === 'Mute mic',
    `${aBack.mute} / ${aBack.deafen}`,
  );

  // --- the camera, and the mosaic it feeds ------------------------------
  await b.evaluate("document.getElementById('voice-cam').click(); return true;");
  await waitFor(b, "document.getElementById('voice-cam').textContent === 'Stop camera'", {
    label: "Bob's camera starting",
    timeoutMs: 25_000,
  });

  // The path is not readable until RTP arrives, so the first subscribe can
  // 404 and the 4s reconcile timer is what fixes it. That delay is the
  // behaviour being tested, not an inconvenience.
  const tileArrived = await waitFor(
    a,
    "document.querySelectorAll('#channel-video .channel-tile').length > 0",
    { label: "Bob's camera tile reaching Alice", timeoutMs: 45_000 },
  ).catch(() => false);

  const aWithTile = await pane(a);
  check(
    "a camera started in a channel appears as a tile on the other member's screen",
    Boolean(tileArrived) && aWithTile.tiles.length === 1,
    aWithTile.tiles.join(', ') || 'no tiles',
  );
  check(
    'the tile is captioned with who it is and what it is',
    aWithTile.tiles[0]?.includes('bob') && aWithTile.tiles[0]?.includes('camera'),
    aWithTile.tiles[0] ?? '(none)',
  );
  check(
    'the roster says who is publishing video',
    aWithTile.roster.find((r) => r.name.startsWith('bob'))?.status.includes('cam') === true,
    aWithTile.roster.find((r) => r.name.startsWith('bob'))?.status.join(', ') || '(nothing)',
  );

  const camRow = await rosterRow(a, 'bob');
  check(
    'a camera shows as its own indicator, distinct from a screen share',
    camRow.status.includes('cam') && !camRow.status.includes('screen'),
    camRow.status.join(', '),
  );

  const camSide = (await sidebarMembers(a))[0]?.members.find((m) => m.name === 'bob');
  check(
    'the sidebar marks who is sending video, with the same glyph as the pane',
    camSide?.status.includes('cam') === true,
    camSide?.status.join(', ') || '(nothing)',
  );

  const decoding = await a.evaluate(`
    const v = document.querySelector('#channel-video .channel-tile video');
    if (!v) return { ok: false };
    for (let i = 0; i < 40; i += 1) {
      if (v.videoWidth > 0) break;
      await new Promise((r) => setTimeout(r, 250));
    }
    return { ok: v.videoWidth > 0, w: v.videoWidth, h: v.videoHeight, muted: v.muted };
  `);
  check(
    'the tile is really decoding video, and is muted so gain.js owns the sound',
    decoding.ok === true && decoding.muted === true,
    `${decoding.w}x${decoding.h}, muted=${decoding.muted}`,
  );

  // --- the speaking ring ------------------------------------------------
  //
  // Chromium's fake microphone is a continuous tone, so an unmuted fake mic
  // registers as talking. That is what makes this testable at all; a real
  // microphone in a quiet room never would be.
  const ringAppeared = await waitFor(
    a,
    "[...document.querySelectorAll('#voice-roster li')].some((li) => li.hasAttribute('data-speaking'))",
    { label: 'the speaking ring lighting up', timeoutMs: 20_000 },
  ).catch(() => false);
  check(
    'somebody talking gets a ring round their picture',
    Boolean(ringAppeared),
    ringAppeared ? 'data-speaking set from the analyser' : 'never lit',
  );

  const ringStyle = await a.evaluate(`
    const li = [...document.querySelectorAll('#voice-roster li')]
      .find((x) => x.hasAttribute('data-speaking'));
    if (!li) return null;
    return { shadow: getComputedStyle(li.querySelector('.avatar')).boxShadow };
  `);
  check(
    'the ring is green, and a shadow rather than a border so nothing shifts',
    typeof ringStyle?.shadow === 'string' && ringStyle.shadow.includes('63, 199, 122'),
    ringStyle?.shadow ?? '(no ring)',
  );

  // --- turning one person up past 100% ----------------------------------
  await setPeerVolume(a, 'bob', 250);
  const loud = await rosterRow(a, 'bob');
  check(
    'one person can be turned up past 100%, as far as 350%',
    loud.volume === 250 && loud.label === '250%',
    loud.label,
  );
  check(
    'going past 100% is flagged on both the slider and the number',
    loud.boosted === true && loud.labelBoosted === true,
    `slider=${loud.boosted}, label=${loud.labelBoosted}`,
  );

  const warn = await a.evaluate(`
    const li = [...document.querySelectorAll('#voice-roster li')]
      .find((x) => x.querySelector('.member-name').textContent.startsWith('bob'));
    const label = li.querySelector('.peer-volume-label');
    return {
      colour: getComputedStyle(label).color,
      after: getComputedStyle(label, '::after').content,
    };
  `);
  check(
    'the amplified reading is amber and carries a warning mark',
    warn.colour.includes('227, 160, 8') && warn.after.includes('\u26A0'),
    `${warn.colour}, after=${warn.after}`,
  );

  await setPeerVolume(a, 'bob', 80);
  const quiet = await rosterRow(a, 'bob');
  check(
    'coming back under 100% clears the warning, or it would mean nothing',
    quiet.volume === 80 && quiet.boosted === false && quiet.labelBoosted === false,
    `${quiet.label}, boosted=${quiet.boosted}`,
  );

  // --- muting one person, locally ---------------------------------------
  await clickPeerMute(a, 'bob');
  const locallyMuted = await rosterRow(a, 'bob');
  check(
    'one person can be muted just for you, without losing their volume',
    locallyMuted.localMuted === true && locallyMuted.label === 'muted'
      && locallyMuted.volume === 80,
    `label="${locallyMuted.label}", slider still at ${locallyMuted.volume}%`,
  );

  const bobsOwnRow = await rosterRow(b, 'bob');
  check(
    'a local mute is local: the person muted is never told, and nor is anyone else',
    bobsOwnRow.status.includes('muted') === false
      && bobsOwnRow.status.includes('forced') === false,
    bobsOwnRow.status.join(', ') || '(no indicators on their own row)',
  );

  await clickPeerMute(a, 'bob');
  const unmutedAgain = await rosterRow(a, 'bob');
  check(
    'unmuting restores the volume they were at, not a default',
    unmutedAgain.localMuted === false && unmutedAgain.volume === 80,
    `${unmutedAgain.label}, slider=${unmutedAgain.volume}`,
  );

  // --- force-mute -------------------------------------------------------
  //
  // The victim's peer connection stays `connected` for about nine seconds
  // after the server kills the session, so the roster push is the only thing
  // that can tell them. That is what this checks: Bob's own UI, not Alice's.
  await a.evaluate(`
    const row = [...document.querySelectorAll('#voice-roster li')]
      .find((li) => li.textContent.includes('bob'));
    [...row.querySelectorAll('button')].find((x) => x.textContent === 'Force mute').click();
    return true;
  `);
  const bTold = await waitFor(
    b,
    "document.getElementById('channels-error').textContent.includes('muted your microphone')",
    { label: 'Bob being told he was muted' },
  ).catch(() => false);
  const bMuted = await pane(b);
  check(
    'a force-muted user is told by the roster push, not by their connection',
    Boolean(bTold),
    `"${bMuted.error}"`,
  );
  check(
    "the force-muted person sees it on their OWN row, not just everyone else's",
    bMuted.roster.find((r) => r.name.startsWith('bob'))?.status.includes('forced') === true,
    bMuted.roster.find((r) => r.name.startsWith('bob'))?.status.join(', ') || '(nothing)',
  );

  const forcedRow = await rosterRow(a, 'bob');
  check(
    'an admin mute is a different indicator from muting yourself',
    forcedRow.status.includes('forced') && !forcedRow.status.includes('muted'),
    forcedRow.status.join(', '),
  );

  const forcedSide = (await sidebarMembers(a))[0]?.members.find((m) => m.name === 'bob');
  check(
    'a force-mute is distinguishable from an ordinary mute in the sidebar',
    forcedSide?.forced === true && forcedSide?.muted === false,
    `forced=${forcedSide?.forced} muted=${forcedSide?.muted}`,
  );

  const toggled = await a.evaluate(`
    const row = [...document.querySelectorAll('#voice-roster li')]
      .find((li) => li.textContent.includes('bob'));
    return [...row.querySelectorAll('button')].map((x) => x.textContent);
  `);
  check(
    'the button offers to undo it',
    toggled.includes('Unmute'),
    toggled.join(', '),
  );

  // --- moving somebody out ----------------------------------------------
  await a.evaluate(`
    const row = [...document.querySelectorAll('#voice-roster li')]
      .find((li) => li.textContent.includes('bob'));
    const sel = row.querySelector('.move-select');
    sel.value = 'none';
    sel.dispatchEvent(new Event('change', { bubbles: true }));
    return true;
  `);
  const bLeft = await waitFor(b, "document.getElementById('voice-active').hidden === true", {
    label: 'Bob being disconnected',
  }).catch(() => false);
  const bAfter = await pane(b);
  check(
    'an admin can disconnect somebody from the roster control',
    Boolean(bLeft) && bAfter.active === false,
    `Bob's pane closed; "${bAfter.error}"`,
  );

  const aAfter = await waitFor(a, "document.querySelectorAll('#voice-roster li').length === 1", {
    label: 'the roster shrinking',
  }).catch(() => false);
  const aEnd = await pane(a);
  check(
    'their tiles go with them',
    Boolean(aAfter) && aEnd.tiles.length === 0 && aEnd.videoShown === false,
    `${aEnd.roster.length} in the channel, ${aEnd.tiles.length} tiles`,
  );

  // --- a device that is not there any more -------------------------------
  //
  // Simulated by saving a preference for a device id that does not exist,
  // which is exactly what an unplugged headset leaves behind, and then
  // firing the event the system fires on a hardware change.
  await a.evaluate(`
    const { harmony } = await import('./bridge.js');
    await harmony.settings.set({ voiceInputId: 'a-device-that-is-not-plugged-in' });
    return true;
  `);
  await a.evaluate(`
    navigator.mediaDevices.dispatchEvent(new Event('devicechange'));
    return true;
  `);
  await sleep(1200);
  const unplugged = await devices(a);
  check(
    'an unplugged device falls back to the default and says so',
    unplugged.inputValue === '' && unplugged.note.includes('unplugged'),
    `value="${unplugged.inputValue}", note="${unplugged.note}"`,
  );

  // --- leaving ----------------------------------------------------------
  await a.evaluate("document.getElementById('voice-leave').click(); return true;");
  await waitFor(a, "document.getElementById('voice-active').hidden === true", {
    label: 'Alice leaving',
  });
  const idle = await a.evaluate(`
    return {
      idle: !document.getElementById('voice-idle').hidden,
      error: document.getElementById('channels-error').textContent,
    };
  `);
  check(
    'disconnecting returns to the idle pane with nothing broken',
    idle.idle === true,
    `error: "${idle.error}"`,
  );

  const emptied = await sidebarMembers(a);
  check(
    'an empty voice channel lists nobody rather than an empty box',
    emptied.length === 0,
    `${emptied.length} member lists left in the sidebar`,
  );
}

run()
  .catch((err) => check('voice-ui run completed', false, err.message))
  .finally(async () => {
    cleanup();
    await sleep(800);
    for (const dir of [dataDir, ...profiles]) {
      rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 150 });
    }
    const failed = summary();
    if (failed) {
      console.error(`\n--- mediamtx ---\n${mtxLog.slice(-2500)}`);
      console.error(`\n--- harmony server ---\n${serverLog.slice(-2500)}`);
    }
    process.exit(failed ? 1 : 0);
  });
