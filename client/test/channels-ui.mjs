// The channels view, driven through the real client.
//
// Everything else that covers this layer talks to the server directly or
// imports a module in isolation. That is how 2.0.0 shipped a complete,
// well-tested accounts API that was `undefined` at runtime, and then shipped
// routes with no control in front of them at all: a passing test proved the
// server could do it, never that anybody could ask.
//
// So this clicks. It launches the packaged renderer against a real control
// server, signs in, and uses the sidebar, the chat pane, the avatar button
// and the admin controls the way a person would.
//
// MediaMTX is stubbed -- nothing here publishes or subscribes, and a stub
// keeps the run to a few seconds. The media path has its own suites
// (e2e.mjs, channel-video.mjs).
//
//   node client/test/channels-ui.mjs

import { spawn } from 'node:child_process';
import http from 'node:http';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join, resolve, dirname } from 'node:path';
import { fileURLToPath } from 'node:url';

import { attach, findPage, launchApp, reporter, sleep, waitFor } from './cdp.mjs';

const here = dirname(fileURLToPath(import.meta.url));
const repoRoot = resolve(here, '..', '..');
const serverEntry = resolve(repoRoot, 'server', 'src', 'index.js');

const HARMONY_PORT = 18130;
const FAKE_MTX_PORT = 20010;
const DEBUG_PORT = 9411;
const BASE = `http://127.0.0.1:${HARMONY_PORT}`;

const dataDir = mkdtempSync(join(tmpdir(), 'harmony-ui-'));
const userDataDir = mkdtempSync(join(tmpdir(), 'harmony-ui-profile-'));
const { check, summary } = reporter();
const procs = [];
let fakeMtx = null;
let serverLog = '';
let ownerKey = null;
let cdp = null;

function cleanup() {
  for (const p of procs) {
    try { p.kill(); } catch { /* already gone */ }
  }
  try { fakeMtx?.close(); } catch { /* already closed */ }
}
process.on('exit', cleanup);
process.on('SIGINT', () => process.exit(130));

// ---------------------------------------------------------------------------

function startFakeMediaMtx() {
  return new Promise((done) => {
    fakeMtx = http.createServer((_req, res) => {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ itemCount: 0, pageCount: 0, items: [] }));
    });
    fakeMtx.listen(FAKE_MTX_PORT, '127.0.0.1', done);
  });
}

function startHarmonyServer() {
  return new Promise((done, fail) => {
    const p = spawn(process.execPath, [serverEntry], {
      env: {
        ...process.env,
        HARMONY_PORT: String(HARMONY_PORT),
        HARMONY_HOST: '127.0.0.1',
        HARMONY_DATA_DIR: dataDir,
        HARMONY_MEDIAMTX_API: `http://127.0.0.1:${FAKE_MTX_PORT}`,
        HARMONY_SIGNALING_URL: 'http://127.0.0.1:8889',
        HARMONY_POLL_INTERVAL_MS: '500',
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

/**
 * The visible names in the sidebar.
 *
 * `.kind + span` rather than `span:not(.kind)`: a row is
 * [.kind][name][tag?][count?][.row-tools], and .row-tools is a span too, so
 * the looser selector returns the admin buttons alongside the name.
 */
const CHANNEL_NAMES =
  "return [...document.querySelectorAll('#channel-items li.channel-row')]"
  + ".map((li) => li.querySelector('.kind + span')?.textContent);";

/** Type into an input and fire the events the app listens for. */
const setInput = (id, value) =>
  `const i = document.getElementById(${JSON.stringify(id)}); i.value = ${JSON.stringify(value)}; `
  + `i.dispatchEvent(new Event('input', {bubbles:true})); `
  + `i.dispatchEvent(new Event('change', {bubbles:true}));`;

/**
 * window.prompt and window.confirm block a renderer forever under CDP.
 *
 * The admin controls use them deliberately -- a friends' server does not need
 * a modal framework -- so the test answers them instead of avoiding the code
 * paths that use them, which are the paths being tested.
 */
const stubDialogs = (answers) => `
  window.__asked = [];
  window.prompt = (q) => {
    window.__asked.push(q);
    const next = ${JSON.stringify(answers)}[window.__asked.length - 1];
    return next === undefined ? null : next;
  };
  window.confirm = (q) => { window.__asked.push(q); return true; };
  return true;
`;

// ---------------------------------------------------------------------------

async function run() {
  console.log('Starting the control server and the client…\n');
  await startFakeMediaMtx();
  await startHarmonyServer();

  const app = launchApp({ port: DEBUG_PORT, userDataDir });
  procs.push(app);
  cdp = await attach(await findPage(DEBUG_PORT));
  await sleep(1200);

  // --- registering as the owner, from the connect screen ----------------
  await cdp.evaluate(`${setInput('server-url', BASE)} return true;`);
  await sleep(1500);

  // No toggle: a server with accounts but nobody registered opens in REGISTER
  // mode already, and clicking the toggle would switch it to sign-in and fail
  // with "wrong nickname or password" -- which is what it did the first time.
  const opening = await cdp.evaluate(`
    return {
      button: document.getElementById('continue').textContent,
      ownerField: !document.getElementById('owner-key-field').hidden,
    };
  `);
  check(
    'a server with no accounts opens straight into "create account"',
    opening.button === 'Create account' && opening.ownerField === true,
    `${opening.button}, owner key ${opening.ownerField ? 'shown' : 'hidden'}`,
  );

  await cdp.evaluate(`${setInput('username', 'Pedro Lucas')} return true;`);
  await cdp.evaluate(`${setInput('account-password', 'hunter22')} return true;`);
  await cdp.evaluate(`${setInput('account-confirm', 'hunter22')} return true;`);
  await cdp.evaluate(`${setInput('owner-key', ownerKey)} return true;`);
  await cdp.evaluate("document.getElementById('continue').click(); return true;");

  const inChannels = await waitFor(
    cdp,
    "document.querySelector('.view[data-active]')?.id === 'view-channels'",
    { label: 'the channels view' },
  );
  check('registering with the owner key lands in the channels view', inChannels === true);

  const whoami = await cdp.evaluate(`
    return {
      who: document.getElementById('channels-who').textContent,
      role: document.getElementById('channels-role').textContent,
      // A name typed with a capital and a space is stored folded.
      avatar: document.getElementById('channels-avatar').textContent,
      addVisible: !document.getElementById('channel-add').hidden,
    };
  `);
  check(
    'the owner is shown, with the folded nickname and their initials',
    whoami.who === 'pedrolucas' && whoami.role === 'owner' && whoami.avatar === 'PE',
    `${whoami.who} / ${whoami.role} / "${whoami.avatar}"`,
  );
  check('an admin is offered the "+ New" button', whoami.addVisible === true, '');

  // --- the sidebar ------------------------------------------------------
  const sidebar = await cdp.evaluate(`
    const rows = [...document.querySelectorAll('#channel-items li.channel-row')];
    return rows.map((li) => ({
      // The span right after .kind: .row-tools is a span as well, so
      // span:not(.kind) would pick the admin buttons up with the name.
      name: li.querySelector('.kind + span')?.textContent,
      tools: li.querySelectorAll('.row-tools button').length,
      firstDisabled: li.querySelector('.row-tools button')?.disabled ?? null,
    }));
  `);
  check(
    'the seeded channels are listed',
    sidebar.length === 2 && sidebar[0].name === 'general' && sidebar[1].name === 'voice',
    sidebar.map((r) => r.name).join(', '),
  );
  check(
    'each row carries the four admin controls, with "up" disabled on the first',
    sidebar.every((r) => r.tools === 4) && sidebar[0].firstDisabled === true,
    `${sidebar[0].tools} buttons per row`,
  );

  // --- creating a channel -----------------------------------------------
  await cdp.evaluate(stubDialogs(['Game Night', '']));
  await cdp.evaluate("document.getElementById('channel-add').click(); return true;");
  // confirm() answers true, which the handler reads as "voice".
  await waitFor(cdp, "document.querySelectorAll('#channel-items li.channel-row').length === 3", {
    label: 'the new channel',
  });
  const names = await cdp.evaluate(
    CHANNEL_NAMES,
  );
  check(
    'an admin can create a channel, and the name is folded',
    names.length === 3 && names.includes('gamenight'),
    names.join(', '),
  );

  // --- reordering -------------------------------------------------------
  //
  // The button that had no caller at all until 2.1.0.
  await cdp.evaluate(`
    const rows = [...document.querySelectorAll('#channel-items li.channel-row')];
    rows[2].querySelector('.row-tools button').click();   // move up
    return true;
  `);
  await sleep(700);
  const reordered = await cdp.evaluate(
    CHANNEL_NAMES,
  );
  check(
    'moving a channel up reorders it for everybody',
    reordered[1] === 'gamenight',
    reordered.join(', '),
  );

  // --- a profile picture ------------------------------------------------
  //
  // The file input is driven directly rather than through the OS dialog:
  // everything after the picker -- the canvas downscale, the upload, the
  // route and the repaint -- is what was missing and what is being tested.
  const avatar = await cdp.evaluate(`
    const canvas = document.createElement('canvas');
    canvas.width = 600; canvas.height = 400;
    const ctx = canvas.getContext('2d');
    ctx.fillStyle = '#c0ffee'; ctx.fillRect(0, 0, 600, 400);
    ctx.fillStyle = '#300'; ctx.fillRect(100, 80, 300, 200);
    const blob = await new Promise((r) => canvas.toBlob(r, 'image/png'));
    const file = new File([blob], 'me.png', { type: 'image/png' });

    const input = document.getElementById('avatar-file');
    const dt = new DataTransfer();
    dt.items.add(file);
    input.files = dt.files;
    input.dispatchEvent(new Event('change', { bubbles: true }));

    // Wait for the round trip: upload, then the avatar route, then repaint.
    for (let i = 0; i < 60; i += 1) {
      const img = document.querySelector('#channels-avatar img');
      if (img) {
        await img.decode().catch(() => {});
        return {
          src: img.src,
          w: img.naturalWidth,
          h: img.naturalHeight,
          error: document.getElementById('channels-error').textContent,
        };
      }
      await new Promise((r) => setTimeout(r, 250));
    }
    return { src: null, error: document.getElementById('channels-error').textContent };
  `);
  check(
    'picking a picture uploads it and the bar repaints with it',
    typeof avatar.src === 'string' && avatar.src.startsWith('harmony://app/media/'),
    avatar.src ?? `no image; error: "${avatar.error}"`,
  );
  check(
    'it was downscaled to a 256px square before upload, and decodes from the cache',
    avatar.w === 256 && avatar.h === 256,
    `${avatar.w}x${avatar.h}`,
  );

  // --- the chat pane ----------------------------------------------------
  await cdp.evaluate(`
    const rows = [...document.querySelectorAll('#channel-items li.channel-row')];
    rows.find((li) => li.textContent.includes('general')).click();
    return true;
  `);
  await waitFor(cdp, "!document.getElementById('chat-active').hidden", { label: 'the chat pane' });

  await cdp.evaluate(`${setInput('chat-input', 'hello from the ui test')} return true;`);
  await cdp.evaluate("document.getElementById('chat-form').requestSubmit(); return true;");
  await waitFor(cdp, "document.querySelectorAll('#chat-log .chat-msg').length > 0", {
    label: 'the message appearing',
  });

  const row = await cdp.evaluate(`
    const msg = document.querySelector('#chat-log .chat-msg');
    const img = msg.querySelector('.who .avatar img');
    if (img) await img.decode().catch(() => {});
    return {
      text: msg.querySelector('.text').textContent,
      who: msg.querySelector('.who').textContent,
      hasAvatar: Boolean(img),
      avatarDecoded: img ? img.naturalWidth : 0,
      buttons: [...msg.querySelectorAll('button')].map((b) => b.textContent),
    };
  `);
  check(
    'a posted message comes back over the socket and renders',
    row.text === 'hello from the ui test' && row.who === 'pedrolucas',
    `${row.who}: ${row.text}`,
  );
  check(
    "the author's picture is drawn on the message, from the local cache",
    row.hasAvatar === true && row.avatarDecoded === 256,
    `avatar ${row.avatarDecoded}px`,
  );
  check(
    'a message you can delete offers Pin and Delete',
    row.buttons.includes('Pin') && row.buttons.includes('Delete'),
    row.buttons.join(', '),
  );

  // --- pinning ----------------------------------------------------------
  await cdp.evaluate(`
    const msg = document.querySelector('#chat-log .chat-msg');
    [...msg.querySelectorAll('button')].find((b) => b.textContent === 'Pin').click();
    return true;
  `);
  await sleep(800);
  const pinned = await cdp.evaluate(`
    return {
      stripShown: !document.getElementById('chat-pinned').hidden,
      strip: document.getElementById('chat-pinned').textContent,
      label: [...document.querySelectorAll('#chat-log .chat-msg button')]
        .map((b) => b.textContent).join(','),
    };
  `);
  check(
    'pinning a message puts it in the strip and flips the button',
    pinned.stripShown === true && pinned.label.includes('Unpin'),
    pinned.strip.trim(),
  );

  // --- searching --------------------------------------------------------
  await cdp.evaluate(`${setInput('chat-search', 'hello')} return true;`);
  await sleep(900);
  const searched = await cdp.evaluate(`
    return {
      note: document.getElementById('chat-note').textContent,
      hits: document.querySelectorAll('#chat-log .chat-msg').length,
    };
  `);
  check(
    'search finds the message and says which index answered',
    searched.hits === 1 && searched.note.includes('fts'),
    `${searched.hits} hit: "${searched.note}"`,
  );

  await cdp.evaluate("document.getElementById('chat-search-clear').click(); return true;");
  await sleep(700);

  // --- deleting ---------------------------------------------------------
  //
  // The route existed and was tested in 2.0.0. Nothing called it.
  await cdp.evaluate(stubDialogs([]));
  await cdp.evaluate(`
    const msg = document.querySelector('#chat-log .chat-msg');
    [...msg.querySelectorAll('button')].find((b) => b.textContent === 'Delete').click();
    return true;
  `);
  await sleep(900);
  const afterDelete = await cdp.evaluate(`
    return {
      rows: document.querySelectorAll('#chat-log .chat-msg').length,
      strip: document.getElementById('chat-pinned').hidden,
      error: document.getElementById('channels-error').textContent,
    };
  `);
  check(
    'deleting a message removes it, and takes it out of the pinned strip',
    afterDelete.rows === 0 && afterDelete.strip === true,
    `${afterDelete.rows} rows left; error: "${afterDelete.error}"`,
  );

  // --- the soundpad -----------------------------------------------------
  const clips = await cdp.evaluate(`
    const { harmony } = await import('./bridge.js');
    const server = ${JSON.stringify(BASE)};
    // A minimal Ogg: the server checks the declared type and the size, not
    // the bytes, and nothing here plays it.
    const bytes = new TextEncoder().encode('OggS\\u0000\\u0002ui test clip');
    for (const name of ['first', 'second']) {
      const up = await harmony.media.upload(server, bytes, 'audio/ogg');
      await harmony.api.addClip(server, { name, hash: up.hash + '' });
    }
    return true;
  `).catch((e) => ({ error: String(e) }));

  // Both clips share one hash (same bytes), so the second add must still work:
  // content addressing deduplicates the FILE, not the clip.
  await sleep(900);
  const pad = await cdp.evaluate(`
    return {
      shown: !document.getElementById('soundpad').hidden,
      cells: document.querySelectorAll('#soundpad-grid .clip-cell').length,
      names: [...document.querySelectorAll('#soundpad-grid .clip-cell > button:nth-child(2)')]
        .map((b) => b.textContent),
    };
  `);
  check(
    'soundpad clips appear with their reorder arrows for an admin',
    pad.shown === true && pad.cells === 2,
    `${pad.cells} clips: ${pad.names.join(', ')}${clips?.error ? ` (${clips.error})` : ''}`,
  );

  await cdp.evaluate(`
    const cells = [...document.querySelectorAll('#soundpad-grid .clip-cell')];
    cells[1].querySelector('button').click();   // the left arrow on the second
    return true;
  `);
  await sleep(900);
  const padOrder = await cdp.evaluate(
    // nth-child(2): an admin's cell is [left arrow][the clip][right arrow].
    "return [...document.querySelectorAll('#soundpad-grid .clip-cell > button:nth-child(2)')]"
    + ".map((b) => b.textContent);",
  );
  check(
    'an admin can reorder the soundpad',
    padOrder[0] === 'second',
    padOrder.join(', '),
  );

  // --- nothing threw along the way --------------------------------------
  const errors = await cdp.evaluate(
    "return document.getElementById('channels-error').textContent;",
  );
  check('the channels view reports no error after all of that', errors === '', `"${errors}"`);

  // --- "stay signed in" ---------------------------------------------------
  //
  // Restart the app against the SAME profile directory and it must come back
  // signed in, with nothing to type.
  //
  // This shipped broken in a way no test could see, because no test had ever
  // restarted the app: the token was saved and restored and handed to every
  // request, but the connect screen gates on `state.auth.user`, which only a
  // fresh sign-in ever set. So a saved session still demanded the password
  // and the setting looked like it simply was not saving.
  cdp.close();
  app.kill();
  await sleep(1500);

  const again = launchApp({ port: DEBUG_PORT + 1, userDataDir });
  procs.push(again);
  const cdp2 = await attach(await findPage(DEBUG_PORT + 1));
  await sleep(2500);

  const restored = await cdp2.evaluate(`
    return {
      view: document.querySelector('.view[data-active]')?.id,
      server: document.getElementById('server-url').value,
      username: document.getElementById('username').value,
      accountFieldsHidden: document.getElementById('account-fields').hidden,
      hint: document.getElementById('username-hint').textContent,
      toggle: document.getElementById('auth-mode-toggle').textContent,
      passwordTyped: document.getElementById('account-password').value,
    };
  `);
  check(
    'a restarted client comes back knowing who it is, with no password to type',
    restored.hint.includes('Signed in as pedrolucas') && restored.accountFieldsHidden === true
      && restored.passwordTyped === '',
    `hint="${restored.hint}", fields hidden=${restored.accountFieldsHidden}`,
  );
  check(
    'the server address and nickname come back too',
    restored.server === BASE && restored.username === 'pedrolucas',
    `${restored.server} as ${restored.username}`,
  );
  check(
    'and it offers a way out, since the fields are hidden',
    restored.toggle === 'Sign out',
    `toggle reads "${restored.toggle}"`,
  );

  // Pressing Continue has to get straight in, which is the whole point.
  await cdp2.evaluate("document.getElementById('continue').click(); return true;");
  const backIn = await waitFor(
    cdp2,
    "document.querySelector('.view[data-active]')?.id === 'view-channels'",
    { label: 'getting back in without a password' },
  ).catch(() => false);
  check(
    'Continue goes straight back into the channels, no password asked',
    Boolean(backIn),
    backIn ? 'signed in from the saved session' : 'was asked to log in again',
  );
}

run()
  .catch((err) => check('channels-ui run completed', false, err.message))
  .finally(async () => {
    cdp?.close();
    cleanup();
    await sleep(600);
    for (const dir of [dataDir, userDataDir]) {
      rmSync(dir, { recursive: true, force: true, maxRetries: 10, retryDelay: 150 });
    }
    const failed = summary();
    if (failed) console.error(`\n--- harmony server ---\n${serverLog.slice(-2500)}`);
    process.exit(failed ? 1 : 0);
  });
