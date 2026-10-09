// Private conversations and private calls: keys, sealed messages, sealed
// files, reactions, read marks, blocking, ringing -- and above all, that
// nobody outside a conversation can touch any of it. Every route is tried by
// a third person, the owner included, and must look exactly like a
// conversation that does not exist.
//
//   node --test server/test/dm.test.js

import { after, before, describe, it } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { randomBytes } from 'node:crypto';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { DatabaseSync } from 'node:sqlite';

import { callPath, channelPath, mintChannelToken, parseCallPath } from '../src/channels.js';
import { isPublicKey, isSealedText } from '../src/dms.js';
import { Throttle } from '../src/throttle.js';
import { Calls } from '../src/calls.js';

const here = dirname(fileURLToPath(import.meta.url));
const serverEntry = resolve(here, '..', 'src', 'index.js');

// Unique across the suite: node --test runs these files in PARALLEL.
const FAKE_MTX_PORT = 20011;
const HARMONY_PORT = 18095;
const BASE = `http://127.0.0.1:${HARMONY_PORT}`;
const WS_URL = `ws://127.0.0.1:${HARMONY_PORT}/ws`;

let fakeMtx;
let child;
let dataDir;
let ownerKey = null;
const tokens = {};
const ids = {};

function startFakeMediaMtx() {
  return new Promise((done) => {
    fakeMtx = http.createServer((_req, res) => {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ itemCount: 0, pageCount: 0, items: [] }));
    });
    fakeMtx.listen(FAKE_MTX_PORT, '127.0.0.1', done);
  });
}

function startHarmony() {
  return new Promise((done, fail) => {
    child = spawn(process.execPath, [serverEntry], {
      env: {
        ...process.env,
        HARMONY_PORT: String(HARMONY_PORT),
        HARMONY_HOST: '127.0.0.1',
        HARMONY_DATA_DIR: dataDir,
        HARMONY_MEDIAMTX_API: `http://127.0.0.1:${FAKE_MTX_PORT}`,
        HARMONY_POLL_INTERVAL_MS: '500',
        HARMONY_SIGNALING_URL: 'http://media.test:8889',
        HARMONY_CALL_GRACE_MS: '400',
      },
      stdio: ['ignore', 'pipe', 'pipe'],
    });
    const timer = setTimeout(() => fail(new Error('server did not start')), 10_000);
    let out = '';
    child.stdout.on('data', (b) => {
      out += b.toString();
      const match = /^\s*│\s+([A-Za-z0-9_-]{20,})\s+│$/m.exec(out);
      if (match) ownerKey = match[1];
      if (out.includes('control server on')) {
        clearTimeout(timer);
        done();
      }
    });
    child.stderr.on('data', (b) => process.stderr.write(`[server] ${b}`));
  });
}

const api = async (path, { method = 'GET', body, token, raw, contentType } = {}) => {
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers: {
      ...(body ? { 'Content-Type': 'application/json' } : {}),
      ...(raw ? { 'Content-Type': contentType ?? 'application/octet-stream' } : {}),
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    ...(body ? { body: JSON.stringify(body) } : {}),
    ...(raw ? { body: raw } : {}),
  });
  const buffer = Buffer.from(await res.arrayBuffer());
  let json = null;
  try { json = JSON.parse(buffer.toString()); } catch { /* binary or empty */ }
  return { status: res.status, body: json, buffer, headers: res.headers };
};

const b64 = (n) => randomBytes(n).toString('base64url');
const fakeKey = () => ({ publicKey: b64(32), wrapped: b64(73) });
const sealed = (n = 64) => b64(n);

function connect(token) {
  const ws = new WebSocket(WS_URL);
  const inbox = [];
  const waiters = [];
  ws.addEventListener('message', (event) => {
    const msg = JSON.parse(event.data);
    inbox.push(msg);
    for (let i = waiters.length - 1; i >= 0; i -= 1) {
      if (waiters[i].match(msg)) {
        waiters[i].resolve(msg);
        waiters.splice(i, 1);
      }
    }
  });
  const client = {
    ws,
    inbox,
    next(match, { timeoutMs = 5000 } = {}) {
      const found = inbox.find(match);
      if (found) return Promise.resolve(found);
      return new Promise((done, fail) => {
        const timer = setTimeout(
          () => fail(new Error(`timed out; saw ${JSON.stringify(inbox.map((m) => m.type))}`)),
          timeoutMs,
        );
        waiters.push({ match, resolve: (m) => { clearTimeout(timer); done(m); } });
      });
    },
    async request(payload) {
      const rid = Math.random().toString(36).slice(2);
      ws.send(JSON.stringify({ ...payload, rid }));
      return this.next((m) => m.rid === rid);
    },
    async hello() {
      if (ws.readyState !== WebSocket.OPEN) {
        await new Promise((done) => ws.addEventListener('open', done, { once: true }));
      }
      return this.request({ type: 'hello', token });
    },
    close() {
      ws.close();
      return new Promise((done) => ws.addEventListener('close', done, { once: true }));
    },
  };
  return client;
}

/** Nothing matching arrives within `ms`. */
async function silent(client, match, ms = 300) {
  await new Promise((done) => setTimeout(done, ms));
  assert.equal(client.inbox.find(match), undefined, 'that should not have reached this socket');
}

const keyOf = {};

async function publishKey(who) {
  const r = await api('/api/keys', { method: 'POST', body: fakeKey(), token: tokens[who] });
  assert.equal(r.status, 201);
  keyOf[who] = r.body.key.id;
  return r.body.key;
}

async function open(who, peer) {
  const r = await api('/api/dms', { method: 'POST', body: { userId: ids[peer] }, token: tokens[who] });
  assert.ok([200, 201].includes(r.status), JSON.stringify(r.body));
  return r.body.conversation.id;
}

async function send(who, peer, conversationId, extra = {}) {
  return api(`/api/dms/${conversationId}/messages`, {
    method: 'POST',
    token: tokens[who],
    body: { sealed: sealed(), senderKey: keyOf[who], recipientKey: keyOf[peer], ...extra },
  });
}

before(async () => {
  dataDir = mkdtempSync(resolve(tmpdir(), 'harmony-dm-'));
  await startFakeMediaMtx();
  await startHarmony();
  for (const nick of ['boss', 'alice', 'bob', 'eve', 'carol']) {
    const r = await api('/api/accounts/register', {
      method: 'POST',
      body: { nickname: nick, password: 'hunter22', ...(nick === 'boss' ? { ownerKey } : {}) },
    });
    tokens[nick] = r.body.token;
    ids[nick] = r.body.user.id;
  }
});

after(async () => {
  await new Promise((done) => {
    if (!child || child.exitCode !== null) return done();
    child.once('exit', done);
    child.kill('SIGTERM');
    setTimeout(() => { child.kill('SIGKILL'); done(); }, 4000).unref();
  });
  await new Promise((done) => fakeMtx.close(done));
  rmSync(dataDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
});

// ---------------------------------------------------------------------------

describe('pieces', () => {
  it('knows a public key and a sealed string when it sees one', () => {
    assert.ok(isPublicKey(b64(32)));
    assert.ok(!isPublicKey(b64(31)));
    assert.ok(!isPublicKey(`${b64(32).slice(0, 42)}=`));
    assert.ok(isSealedText(b64(100)));
    assert.ok(!isSealedText('has spaces in it'));
    assert.ok(!isSealedText(''));
  });

  it('names call paths outside every nickname, and parses them back', () => {
    const path = callPath(41, 2, 'screen');
    assert.equal(path, 'dm.15.2.s');
    assert.deepEqual(parseCallPath(path), { conversationId: 41, mid: 2, kind: 's' });
    assert.equal(parseCallPath(channelPath(41, 2, 'screen')), null);
    assert.equal(parseCallPath('dm-15-2-s'), null);
  });

  it('throttles a flood but lets a burst through, and refills', () => {
    let now = 0;
    const t = new Throttle({ burst: 3, perSecond: 1, now: () => now });
    assert.ok(t.take('a').allowed && t.take('a').allowed && t.take('a').allowed);
    const refused = t.take('a');
    assert.equal(refused.allowed, false);
    assert.equal(refused.retryAfterSec, 1);
    assert.ok(t.take('b').allowed, 'one person flooding does not slow anybody else');
    now = 1000;
    assert.ok(t.take('a').allowed);
  });

  it('rings, times out as missed, and leaves a line', async () => {
    const lines = [];
    const sent = [];
    const calls = new Calls({
      notify: (to, p) => sent.push([to, p.type]),
      record: (cid, caller, meta) => lines.push({ cid, caller, meta }),
      ringMs: 20,
    });
    const r = calls.start({
      conversationId: 7, callerId: 1, calleeId: 2, sealedKey: b64(60), senderKey: 1, recipientKey: 2,
      calleeOnline: true,
    });
    assert.ok(r.ok);
    await new Promise((done) => setTimeout(done, 60));
    assert.equal(calls.get(7), null);
    assert.deepEqual(lines, [{ cid: 7, caller: 1, meta: { outcome: 'missed' } }]);
    assert.ok(sent.some(([to, type]) => type === 'call:state' && to.includes(2)));
    calls.close();
  });

  it('turns two people calling each other into one answered call', () => {
    const calls = new Calls({ notify: () => {}, record: () => {} });
    const base = { conversationId: 3, sealedKey: b64(60), senderKey: 1, recipientKey: 2, calleeOnline: true };
    calls.start({ ...base, callerId: 1, calleeId: 2 });
    const second = calls.start({ ...base, callerId: 2, calleeId: 1 });
    assert.equal(second.call.state, 'active');
    assert.equal(second.call.callerId, 1);
    calls.close();
  });
});

describe('keys', () => {
  it('publishes a key and shows the wrapped half only to its owner', async () => {
    const mine = await publishKey('alice');
    assert.ok(mine.wrapped, 'your own key comes back with the wrapped private half');

    const asAlice = await api('/api/keys', { token: tokens.alice });
    assert.equal(asAlice.body.mine.id, mine.id);
    assert.equal(asAlice.body.mine.wrapped, mine.wrapped);

    const asEve = await api('/api/keys', { token: tokens.eve });
    assert.equal(asEve.body.mine, null);
    const listed = asEve.body.keys.find((k) => k.userId === ids.alice);
    assert.equal(listed.publicKey, mine.publicKey);
    assert.equal(listed.wrapped, undefined, 'nobody else ever sees a wrapped key');
  });

  it('refuses things that are not keys', async () => {
    for (const body of [
      { publicKey: 'short', wrapped: b64(73) },
      { publicKey: b64(32), wrapped: 'not base64url!' },
      { publicKey: b64(32), wrapped: b64(1000) },
      {},
    ]) {
      const r = await api('/api/keys', { method: 'POST', body, token: tokens.carol });
      assert.equal(r.status, 400, JSON.stringify(body));
      assert.equal(r.body.error, 'bad_key');
    }
  });

  it('tells everybody when a key changes, and keeps the old one for history', async () => {
    const watcher = connect(tokens.eve);
    await watcher.hello();
    await publishKey('bob');
    const first = keyOf.bob;
    const second = await publishKey('bob');
    const push = await watcher.next((m) => m.type === 'keys:changed' && m.key.id === second.id);
    assert.equal(push.key.userId, ids.bob);
    assert.equal(push.key.wrapped, undefined);

    const looked = await api(`/api/keys/lookup?ids=${first},${second.id}`, { token: tokens.eve });
    assert.deepEqual(looked.body.keys.map((k) => k.id).sort(), [first, second.id].sort());
    await watcher.close();
  });

  it('re-wraps only your current key', async () => {
    const stale = await api('/api/keys/wrap', {
      method: 'POST', body: { keyId: keyOf.bob - 1, wrapped: b64(73) }, token: tokens.bob,
    });
    assert.equal(stale.status, 409);
    const ok = await api('/api/keys/wrap', {
      method: 'POST', body: { keyId: keyOf.bob, wrapped: b64(73) }, token: tokens.bob,
    });
    assert.equal(ok.status, 200);
    const other = await api('/api/keys/wrap', {
      method: 'POST', body: { keyId: keyOf.bob, wrapped: b64(73) }, token: tokens.alice,
    });
    assert.equal(other.status, 409, "somebody else's key is never yours to re-wrap");
  });
});

describe('conversations and messages', () => {
  let convo;

  it('opens one conversation per pair, from either side', async () => {
    convo = await open('alice', 'bob');
    assert.equal(await open('bob', 'alice'), convo);
    const self = await api('/api/dms', { method: 'POST', body: { userId: ids.alice }, token: tokens.alice });
    assert.equal(self.status, 400);
    const ghost = await api('/api/dms', { method: 'POST', body: { userId: 99999 }, token: tokens.alice });
    assert.equal(ghost.status, 404);
  });

  it('does not list a conversation nobody wrote in', async () => {
    const list = await api('/api/dms', { token: tokens.bob });
    assert.equal(list.body.conversations.length, 0);
  });

  it('delivers a sealed message to both people and to nobody else', async () => {
    const a = connect(tokens.alice);
    const b = connect(tokens.bob);
    const e = connect(tokens.eve);
    const o = connect(tokens.boss);
    await Promise.all([a.hello(), b.hello(), e.hello(), o.hello()]);

    const body = sealed(200);
    const r = await send('alice', 'bob', convo, { sealed: body });
    assert.equal(r.status, 201, JSON.stringify(r.body));
    assert.equal(r.body.message.sealed, body, 'the server stores and returns exactly what it was given');

    const got = await b.next((m) => m.type === 'dm:message' && m.message.id === r.body.message.id);
    assert.equal(got.message.sealed, body);
    assert.deepEqual(got.conversation.userIds.sort(), [ids.alice, ids.bob].sort());
    await a.next((m) => m.type === 'dm:message' && m.message.id === r.body.message.id);
    await silent(e, (m) => m.type === 'dm:message');
    await silent(o, (m) => m.type === 'dm:message');
    await Promise.all([a.close(), b.close(), e.close(), o.close()]);
  });

  it('counts unread for the reader and clears it with a read mark', async () => {
    let list = await api('/api/dms', { token: tokens.bob });
    assert.equal(list.body.conversations[0].unread, 1);
    assert.equal(list.body.conversations[0].last.senderKey, keyOf.alice);
    assert.ok(list.body.keys.some((k) => k.id === keyOf.alice), 'the keys to open the preview come along');

    const mine = await api('/api/dms', { token: tokens.alice });
    assert.equal(mine.body.conversations[0].unread, 0, 'your own message is never unread to you');

    const lastId = list.body.conversations[0].last.id;
    const read = await api(`/api/dms/${convo}/read`, { method: 'POST', body: { upTo: lastId + 1000 }, token: tokens.bob });
    assert.equal(read.body.lastReadId, lastId, 'a read mark never runs past the last message');
    list = await api('/api/dms', { token: tokens.bob });
    assert.equal(list.body.conversations[0].unread, 0);
  });

  it('refuses a message sealed to a key that has been replaced', async () => {
    const r = await send('alice', 'bob', convo, { recipientKey: keyOf.bob - 1 });
    assert.equal(r.status, 409);
    assert.equal(r.body.error, 'stale_key');
    assert.ok(r.body.keys.some((k) => k.id === keyOf.bob), 'and says what the current keys are');
  });

  it('refuses to send to somebody with no key yet', async () => {
    const c = await open('alice', 'carol');
    const r = await send('alice', 'carol', c, { recipientKey: 1 });
    assert.equal(r.status, 409);
    assert.equal(r.body.error, 'peer_has_no_key');
  });

  it('refuses what is not sealed text', async () => {
    const r = await send('alice', 'bob', convo, { sealed: 'hello bob, in plain text' });
    assert.equal(r.status, 400);
    assert.equal(r.body.error, 'bad_message');
  });

  it('takes a long sealed message that the ordinary JSON limit would refuse', async () => {
    const r = await send('alice', 'bob', convo, { sealed: b64(30_000) });
    assert.equal(r.status, 201);
  });

  it('pages history, oldest first, with the keys needed to open it', async () => {
    const page = await api(`/api/dms/${convo}/messages`, { token: tokens.bob });
    assert.equal(page.status, 200);
    assert.ok(page.body.messages.length >= 2);
    const ids_ = page.body.messages.map((m) => m.id);
    assert.deepEqual(ids_, [...ids_].sort((x, y) => x - y));
    assert.ok(page.body.keys.some((k) => k.id === keyOf.bob));
  });

  it('lets only the author edit or delete -- not the other person, not the owner', async () => {
    const r = await send('alice', 'bob', convo);
    const id = r.body.message.id;
    const editBody = { sealed: sealed(), senderKey: keyOf.bob, recipientKey: keyOf.alice };
    assert.equal((await api(`/api/dm-messages/${id}/edit`, { method: 'POST', body: editBody, token: tokens.bob })).status, 403);
    assert.equal((await api(`/api/dm-messages/${id}/delete`, { method: 'POST', token: tokens.bob })).status, 403);

    const asOwner = await api(`/api/dm-messages/${id}/delete`, { method: 'POST', token: tokens.boss });
    assert.equal(asOwner.status, 404, 'to the owner it does not exist');

    const edited = await api(`/api/dm-messages/${id}/edit`, {
      method: 'POST', token: tokens.alice, body: { sealed: sealed(), senderKey: keyOf.alice, recipientKey: keyOf.bob },
    });
    assert.equal(edited.status, 200);
    assert.ok(edited.body.message.editedAt);
    assert.equal((await api(`/api/dm-messages/${id}/delete`, { method: 'POST', token: tokens.alice })).status, 200);
  });

  it('keeps one sealed reaction blob per person, and clears it', async () => {
    const r = await send('alice', 'bob', convo);
    const id = r.body.message.id;
    const react = (who, peer, blob) => api(`/api/dm-messages/${id}/react`, {
      method: 'POST', token: tokens[who], body: { sealed: blob, senderKey: keyOf[who], recipientKey: keyOf[peer] },
    });
    let out = await react('bob', 'alice', sealed(40));
    assert.equal(out.body.reactions.length, 1);
    out = await react('bob', 'alice', sealed(40));
    assert.equal(out.body.reactions.length, 1, 'a second reaction replaces the blob, it does not add one');
    out = await react('alice', 'bob', sealed(40));
    assert.equal(out.body.reactions.length, 2);
    out = await react('bob', 'alice', null);
    assert.deepEqual(out.body.reactions.map((x) => x.userId), [ids.alice]);
  });

  it('is invisible to a third person on every route', async () => {
    const { body: { messages: [m] } } = await api(`/api/dms/${convo}/messages`, { token: tokens.alice });
    for (const who of ['eve', 'boss']) {
      const t = tokens[who];
      const tries = [
        api(`/api/dms/${convo}`, { token: t }),
        api(`/api/dms/${convo}/messages`, { token: t }),
        api(`/api/dms/${convo}/messages`, { method: 'POST', token: t, body: { sealed: sealed(), senderKey: 1, recipientKey: 1 } }),
        api(`/api/dms/${convo}/read`, { method: 'POST', token: t, body: { upTo: 1 } }),
        api(`/api/dms/${convo}/uploads`, { method: 'POST', token: t, raw: randomBytes(64) }),
        api(`/api/dms/${convo}/uploads/${'a'.repeat(64)}`, { token: t }),
        api(`/api/dm-messages/${m.id}/edit`, { method: 'POST', token: t, body: { sealed: sealed() } }),
        api(`/api/dm-messages/${m.id}/delete`, { method: 'POST', token: t }),
        api(`/api/dm-messages/${m.id}/react`, { method: 'POST', token: t, body: { sealed: null } }),
      ];
      for (const r of await Promise.all(tries)) assert.equal(r.status, 404, `${who}: ${JSON.stringify(r.body)}`);
    }
    const unauthenticated = await api(`/api/dms/${convo}/messages`);
    assert.equal(unauthenticated.status, 401);
  });
});

describe('sealed files', () => {
  let convo;
  let hash;

  before(async () => {
    convo = await open('alice', 'bob');
  });

  it('stores ciphertext and serves it only through its own conversation', async () => {
    const bytes = randomBytes(2048);
    const up = await api(`/api/dms/${convo}/uploads`, { method: 'POST', token: tokens.alice, raw: bytes });
    assert.equal(up.status, 201, JSON.stringify(up.body));
    hash = up.body.hash;

    // Uploaded but not sent: nobody can fetch it yet, not even the uploader.
    assert.equal((await api(`/api/dms/${convo}/uploads/${hash}`, { token: tokens.alice })).status, 404);

    const r = await send('alice', 'bob', convo, { attachmentHash: hash });
    assert.equal(r.status, 201);

    const got = await api(`/api/dms/${convo}/uploads/${hash}`, { token: tokens.bob });
    assert.equal(got.status, 200);
    assert.ok(got.buffer.equals(bytes));
    assert.equal(got.headers.get('content-type'), 'application/octet-stream');
    assert.match(got.headers.get('cache-control'), /private/);
  });

  it('never serves a sealed file on the public route, to anyone', async () => {
    for (const who of ['alice', 'bob', 'boss']) {
      assert.equal((await api(`/api/uploads/${hash}`, { token: tokens[who] })).status, 404);
    }
  });

  it('cannot be fetched through another conversation, even by somebody in both', async () => {
    const other = await open('alice', 'eve');
    assert.equal((await api(`/api/dms/${other}/uploads/${hash}`, { token: tokens.alice })).status, 404);
  });

  it('cannot be attached to a channel message', async () => {
    const channels = (await api('/api/channels', { token: tokens.alice })).body.channels;
    const text = channels.find((c) => c.kind === 'text').id;
    const r = await api(`/api/channels/${text}/messages`, {
      method: 'POST', token: tokens.alice, body: { body: 'look', attachmentHash: hash },
    });
    assert.equal(r.status, 400);
    assert.equal(r.body.error, 'no_such_upload');
  });

  it('refuses a public upload as a private attachment', async () => {
    const png = Buffer.from('iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==', 'base64');
    const pub = await api('/api/uploads', { method: 'POST', token: tokens.alice, raw: png, contentType: 'image/png' });
    const r = await send('alice', 'bob', convo, { attachmentHash: pub.body.hash });
    assert.equal(r.status, 400);
  });
});

describe('blocking', () => {
  let convo;

  before(async () => {
    await publishKey('eve');
    convo = await open('eve', 'alice');
  });

  it('stops messages both ways while it holds, and only then', async () => {
    assert.equal((await send('eve', 'alice', convo)).status, 201);
    const blocked = await api('/api/dms/blocks', { method: 'POST', token: tokens.alice, body: { userId: ids.eve } });
    assert.deepEqual(blocked.body.blocked, [ids.eve]);

    for (const [who, peer] of [['eve', 'alice'], ['alice', 'eve']]) {
      const r = await send(who, peer, convo);
      assert.equal(r.status, 403);
      assert.equal(r.body.error, 'blocked');
    }
    const listed = await api('/api/dms', { token: tokens.alice });
    assert.deepEqual(listed.body.blocked, [ids.eve]);

    await api('/api/dms/blocks', { method: 'POST', token: tokens.alice, body: { userId: ids.eve, blocked: false } });
    assert.equal((await send('eve', 'alice', convo)).status, 201);
  });

  it('refuses a call while blocked', async () => {
    await api('/api/dms/blocks', { method: 'POST', token: tokens.alice, body: { userId: ids.eve } });
    const e = connect(tokens.eve);
    await e.hello();
    const r = await e.request({
      type: 'call:start', conversationId: convo, sealedKey: b64(60), senderKey: keyOf.eve, recipientKey: keyOf.alice,
    });
    assert.equal(r.error, 'blocked');
    await e.close();
    await api('/api/dms/blocks', { method: 'POST', token: tokens.alice, body: { userId: ids.eve, blocked: false } });
  });
});

describe('throttling', () => {
  it('lets a burst of messages through and refuses a flood', async () => {
    const convo = await open('bob', 'alice');
    const results = [];
    for (let i = 0; i < 30; i += 1) results.push((await send('bob', 'alice', convo)).status);
    assert.ok(results.slice(0, 15).every((s) => s === 201), results.join(','));
    assert.ok(results.includes(429), 'thirty in a row is a flood');
  });
});

describe('calls', () => {
  let convo;
  let alice;
  let bob;

  before(async () => {
    convo = await open('alice', 'bob');
    alice = connect(tokens.alice);
    bob = connect(tokens.bob);
    await Promise.all([alice.hello(), bob.hello()]);
  });

  after(async () => {
    await Promise.all([alice.close(), bob.close()]);
  });

  const start = (client, who, peer) => client.request({
    type: 'call:start', conversationId: convo, sealedKey: b64(60), senderKey: keyOf[who], recipientKey: keyOf[peer],
  });

  it('rings the other person, with the sealed key', async () => {
    const r = await start(alice, 'alice', 'bob');
    assert.equal(r.type, 'call:ok', JSON.stringify(r));
    const ring = await bob.next((m) => m.type === 'call:state' && m.call.state === 'ringing');
    assert.equal(ring.call.callerId, ids.alice);
    assert.equal(ring.call.sealedKey, r.call.sealedKey);
  });

  it('lets the caller take a slot while it rings, but not the callee', async () => {
    const early = await bob.request({ type: 'call:join', conversationId: convo });
    assert.equal(early.error, 'not_answered');
    const joined = await alice.request({ type: 'call:join', conversationId: convo, muted: true });
    assert.equal(joined.type, 'call:joined');
    assert.match(joined.publish.voice, /\/dm\.[0-9a-z]+\.1\.v\/whip\?token=/);
    assert.equal(joined.roster[0].muted, true);
  });

  it('answers, joins and pushes the roster to the two of them only', async () => {
    const eve = connect(tokens.eve);
    await eve.hello();
    const answered = await bob.request({ type: 'call:answer', conversationId: convo });
    assert.equal(answered.call.state, 'active');
    await alice.next((m) => m.type === 'call:state' && m.call.state === 'active');
    const joined = await bob.request({ type: 'call:join', conversationId: convo });
    assert.equal(joined.type, 'call:joined');
    const roster = await alice.next((m) => m.type === 'call:roster' && m.roster.length === 2);
    assert.equal(roster.conversationId, convo);
    await silent(eve, (m) => m.type === 'call:roster' || m.type === 'call:state');

    const muted = await bob.request({ type: 'voice:mute', conversationId: convo, muted: true, deafened: false });
    assert.equal(muted.type, 'voice:ok');
    const refreshed = await bob.request({ type: 'voice:refresh', conversationId: convo });
    assert.equal(refreshed.type, 'voice:tokens');
    assert.equal(refreshed.conversationId, convo);
    await eve.close();
  });

  it('opens the relay to call tokens only, and keeps channel tokens out', async () => {
    const joined = await bob.request({ type: 'voice:refresh', conversationId: convo });
    const token = new URL(joined.publish.voice).searchParams.get('token');
    const hook = (path, t, action = 'read') => api('/mediamtx/auth', {
      method: 'POST', body: { action, path, query: `token=${encodeURIComponent(t)}` },
    });
    assert.equal((await hook(callPath(convo, 1, 'voice'), token)).status, 204, 'reading the other side');
    assert.equal((await hook(callPath(convo, 2, 'voice'), token, 'publish')).status, 204, 'publishing your own slot');
    assert.equal((await hook(callPath(convo, 1, 'voice'), token, 'publish')).status, 401, 'not the other slot');
    assert.equal((await hook(callPath(convo + 1, 1, 'voice'), token)).status, 401, 'not another call');
    assert.equal((await hook(channelPath(convo, 1, 'voice'), token)).status, 401, 'not the channel with the same id');
  });

  it('hangs up for both and writes how long it lasted', async () => {
    const r = await alice.request({ type: 'call:hang-up', conversationId: convo });
    assert.equal(r.type, 'call:ok');
    const ended = await bob.next((m) => m.type === 'call:state' && m.call.state === 'ended');
    assert.equal(ended.call.outcome, 'ended');
    const line = await bob.next((m) => m.type === 'dm:message' && m.message.kind === 'call' && m.message.meta.outcome === 'ended');
    assert.equal(typeof line.message.meta.durationMs, 'number');
    assert.equal(line.message.sealed, null);
  });

  it('records a missed call for somebody who is not online', async () => {
    const c = await open('alice', 'carol');
    await publishKey('carol');
    const r = await alice.request({
      type: 'call:start', conversationId: c, sealedKey: b64(60), senderKey: keyOf.alice, recipientKey: keyOf.carol,
    });
    assert.equal(r.error, 'peer_offline');
    const page = await api(`/api/dms/${c}/messages`, { token: tokens.carol });
    assert.equal(page.body.messages.at(-1).meta.outcome, 'missed');
  });

  it('declines as declined', async () => {
    await start(alice, 'alice', 'bob');
    await bob.request({ type: 'call:hang-up', conversationId: convo });
    const line = await alice.next((m) => m.type === 'dm:message' && m.message.meta?.outcome === 'declined');
    assert.equal(line.message.userId, ids.alice, 'the line is the caller\'s call');
  });

  it('ends the call when the other person goes offline, not when one window closes', async () => {
    await start(alice, 'alice', 'bob');
    await bob.request({ type: 'call:answer', conversationId: convo });
    const endings = () => alice.inbox.filter((m) => m.type === 'call:state' && m.call.state === 'ended').length;
    const before = endings();

    const second = connect(tokens.bob);
    await second.hello();
    await bob.close();
    await new Promise((done) => setTimeout(done, 200));
    assert.equal(endings(), before, 'bob still has a window open, so the call goes on');

    await second.close();
    await new Promise((done) => setTimeout(done, 200));
    assert.equal(endings(), before, 'a dropped connection gets a moment to come back');
    await new Promise((done) => setTimeout(done, 500));
    assert.equal(endings(), before + 1, 'and then the call is over');
    bob = connect(tokens.bob);
    await bob.hello();
  });
});

describe('a dropped connection', () => {
  it('keeps the call going for whoever comes back in time', async () => {
    const convo = await open('alice', 'bob');
    const alice = connect(tokens.alice);
    let bob = connect(tokens.bob);
    await Promise.all([alice.hello(), bob.hello()]);
    await alice.request({
      type: 'call:start', conversationId: convo, sealedKey: b64(60), senderKey: keyOf.alice, recipientKey: keyOf.bob,
    });
    await bob.request({ type: 'call:answer', conversationId: convo });
    const first = await bob.request({ type: 'call:join', conversationId: convo });

    await bob.close();
    bob = connect(tokens.bob);
    const hello = await bob.hello();
    assert.equal(hello.calls[0].state, 'active', 'the call is still there on reconnect');
    const again = await bob.request({ type: 'call:join', conversationId: convo });
    assert.equal(again.mid, first.mid, 'and so is the slot');
    await new Promise((done) => setTimeout(done, 600));
    assert.ok(!alice.inbox.some((m) => m.type === 'call:state' && m.call.state === 'ended'));

    await alice.request({ type: 'call:hang-up', conversationId: convo });
    await Promise.all([alice.close(), bob.close()]);
  });
});

describe('removing an account', () => {
  it('takes their conversations and frees their files', async () => {
    // Carol published hers during the calls; key changes are throttled hard.
    if (!keyOf.carol) await publishKey('carol');
    const convo = await open('carol', 'alice');
    const up = await api(`/api/dms/${convo}/uploads`, { method: 'POST', token: tokens.carol, raw: randomBytes(512) });
    assert.equal((await send('carol', 'alice', convo, { attachmentHash: up.body.hash })).status, 201);

    const removed = await api(`/api/accounts/${ids.carol}/delete`, { method: 'POST', token: tokens.boss });
    assert.equal(removed.status, 200);

    assert.equal((await api(`/api/dms/${convo}/messages`, { token: tokens.alice })).status, 404);
    const list = await api('/api/dms', { token: tokens.alice });
    assert.ok(!list.body.conversations.some((c) => c.id === convo));
    const keys = await api('/api/keys', { token: tokens.alice });
    assert.ok(!keys.body.keys.some((k) => k.userId === ids.carol));

    // The file is still on disk but nothing holds it, so the next upload that needs room may sweep it.
    const db = new DatabaseSync(resolve(dataDir, 'harmony.db'), { readOnly: true });
    try {
      assert.equal(db.prepare('SELECT refs FROM uploads WHERE hash = ?').get(up.body.hash).refs, 0);
      assert.equal(db.prepare('SELECT COUNT(*) AS n FROM dm_messages WHERE conversation_id = ?').get(convo).n, 0);
    } finally {
      db.close();
    }
  });
});

describe('tokens', () => {
  it('cannot be turned from one kind into the other', () => {
    // The flag is inside the signature: changing it breaks the token.
    const t = mintChannelToken('secret', { cid: 5, mid: 1, flags: 'rw' });
    const forged = t.replace('.rw.', '.dm.');
    assert.notEqual(t, forged);
  });
});
