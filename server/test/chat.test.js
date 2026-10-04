// Messages, pins, uploads and search.
//
//   node --test server/test/chat.test.js

import { after, before, describe, it } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';

import { ftsPhrase, mediaTypeOf, MAX_UPLOAD_BYTES } from '../src/chat.js';

const here = dirname(fileURLToPath(import.meta.url));
const serverEntry = resolve(here, '..', 'src', 'index.js');

// Unique across the suite: node --test runs these files in PARALLEL.
const FAKE_MTX_PORT = 20002;
const HARMONY_PORT = 18085;
const BASE = `http://127.0.0.1:${HARMONY_PORT}`;

let fakeMtx;
let child;
let dataDir;
let ownerKey = null;
let ownerToken;
let memberToken;
let textChannelId;
let lockedChannelId;

function startFakeMediaMtx() {
  return new Promise((done) => {
    fakeMtx = http.createServer((_req, res) => {
      res.writeHead(200, { 'Content-Type': 'application/json' });
      res.end(JSON.stringify({ itemCount: 0, pageCount: 0, items: [] }));
    });
    fakeMtx.listen(FAKE_MTX_PORT, '127.0.0.1', done);
  });
}

function startHarmony(extraEnv = {}) {
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
        ...extraEnv,
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

function stopHarmony() {
  return new Promise((done) => {
    if (!child || child.exitCode !== null) return done();
    child.once('exit', () => done());
    child.kill('SIGTERM');
    setTimeout(() => { child.kill('SIGKILL'); done(); }, 4000).unref();
  });
}

const api = async (path, { method = 'GET', body, token, raw, contentType } = {}) => {
  const res = await fetch(`${BASE}${path}`, {
    method,
    headers: {
      ...(body ? { 'Content-Type': 'application/json' } : {}),
      ...(raw ? { 'Content-Type': contentType } : {}),
      ...(token ? { Authorization: `Bearer ${token}` } : {}),
    },
    ...(body ? { body: JSON.stringify(body) } : {}),
    ...(raw ? { body: raw } : {}),
  });
  const text = await res.text();
  let json = null;
  try { json = JSON.parse(text); } catch { /* binary or empty */ }
  return { status: res.status, body: json, text, headers: res.headers };
};

/** A tiny but genuinely valid PNG. */
const PNG = Buffer.from(
  'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==',
  'base64',
);

before(async () => {
  dataDir = mkdtempSync(resolve(tmpdir(), 'harmony-chat-'));
  await startFakeMediaMtx();
  await startHarmony();

  ownerToken = (await api('/api/accounts/register', {
    method: 'POST', body: { nickname: 'boss', password: 'hunter22', ownerKey },
  })).body.token;
  memberToken = (await api('/api/accounts/register', {
    method: 'POST', body: { nickname: 'member', password: 'hunter22' },
  })).body.token;

  const channels = (await api('/api/channels', { token: ownerToken })).body.channels;
  textChannelId = channels.find((c) => c.kind === 'text').id;

  lockedChannelId = (await api('/api/channels', {
    method: 'POST',
    body: { kind: 'text', name: 'secrets', password: 'letmein' },
    token: ownerToken,
  })).body.channel.id;
});

after(async () => {
  await stopHarmony();
  await new Promise((done) => fakeMtx.close(done));
  rmSync(dataDir, { recursive: true, force: true, maxRetries: 10, retryDelay: 100 });
});

// ---------------------------------------------------------------------------

describe('the FTS5 phrase escaper', () => {
  // Every one of these throws "unterminated string" or is silently parsed as
  // query syntax if passed to MATCH raw.
  it('neutralises query syntax and quotes', () => {
    assert.equal(ftsPhrase('a"b(c'), '"a""b(c"');
    assert.equal(ftsPhrase('NOT bob'), '"NOT bob"');
    assert.equal(ftsPhrase('*'), '"*"');
  });
});

describe('the upload allowlist', () => {
  it('maps allowed types to a media category', () => {
    assert.equal(mediaTypeOf('image/png'), 'image');
    assert.equal(mediaTypeOf('IMAGE/PNG'), 'image');
    assert.equal(mediaTypeOf('video/mp4'), 'video');
    assert.equal(mediaTypeOf('text/html'), null, 'html must never be storable');
    assert.equal(mediaTypeOf('application/javascript'), null);
  });
});

describe('messages', () => {
  it('posts and reads back', async () => {
    const posted = await api(`/api/channels/${textChannelId}/messages`, {
      method: 'POST', body: { body: 'hello everyone' }, token: memberToken,
    });
    assert.equal(posted.status, 201);
    assert.equal(posted.body.message.nickname, 'member');
    assert.equal(posted.body.message.body, 'hello everyone');

    const history = await api(`/api/channels/${textChannelId}/messages`, { token: ownerToken });
    assert.equal(history.status, 200);
    assert.ok(history.body.messages.some((m) => m.body === 'hello everyone'));
  });

  it('refuses an empty message with no attachment', async () => {
    const res = await api(`/api/channels/${textChannelId}/messages`, {
      method: 'POST', body: { body: '   ' }, token: memberToken,
    });
    assert.equal(res.status, 400);
    assert.equal(res.body.error, 'empty_message');
  });

  it('hides a locked channel from somebody without a grant', async () => {
    const res = await api(`/api/channels/${lockedChannelId}/messages`, { token: memberToken });
    assert.equal(res.status, 404, 'no grant means the channel does not exist to you');
  });

  it('pins and unpins', async () => {
    const posted = await api(`/api/channels/${textChannelId}/messages`, {
      method: 'POST', body: { body: 'pin me' }, token: memberToken,
    });
    const id = posted.body.message.id;

    await api(`/api/messages/${id}/pin`, { method: 'POST', body: { pinned: true }, token: ownerToken });
    let history = await api(`/api/channels/${textChannelId}/messages`, { token: ownerToken });
    assert.ok(history.body.pinned.some((m) => m.id === id));

    await api(`/api/messages/${id}/pin`, { method: 'POST', body: { pinned: false }, token: ownerToken });
    history = await api(`/api/channels/${textChannelId}/messages`, { token: ownerToken });
    assert.ok(!history.body.pinned.some((m) => m.id === id));
  });

  it("lets you delete your own but not someone else's", async () => {
    const mine = await api(`/api/channels/${textChannelId}/messages`, {
      method: 'POST', body: { body: 'my message' }, token: memberToken,
    });
    const theirs = await api(`/api/channels/${textChannelId}/messages`, {
      method: 'POST', body: { body: 'boss message' }, token: ownerToken,
    });

    const forbidden = await api(`/api/messages/${theirs.body.message.id}/delete`, {
      method: 'POST', token: memberToken,
    });
    assert.equal(forbidden.status, 403);

    const own = await api(`/api/messages/${mine.body.message.id}/delete`, {
      method: 'POST', token: memberToken,
    });
    assert.equal(own.status, 200);

    // ...and an admin may delete anybody's.
    const byAdmin = await api(`/api/messages/${theirs.body.message.id}/delete`, {
      method: 'POST', token: ownerToken,
    });
    assert.equal(byAdmin.status, 200);
  });
});

describe('uploads', () => {
  let hash;

  it('accepts a PNG and returns its hash', async () => {
    const res = await api('/api/uploads', {
      method: 'POST', raw: PNG, contentType: 'image/png', token: memberToken,
    });
    assert.equal(res.status, 201);
    assert.match(res.body.hash, /^[0-9a-f]{64}$/);
    assert.equal(res.body.bytes, PNG.length);
    hash = res.body.hash;
  });

  it('deduplicates the same bytes', async () => {
    const res = await api('/api/uploads', {
      method: 'POST', raw: PNG, contentType: 'image/png', token: memberToken,
    });
    assert.equal(res.status, 201);
    assert.equal(res.body.hash, hash);
    assert.equal(res.body.deduplicated, true);
  });

  it('refuses a type that is not on the allowlist', async () => {
    const res = await api('/api/uploads', {
      method: 'POST', raw: Buffer.from('<script>alert(1)</script>'),
      contentType: 'text/html', token: memberToken,
    });
    assert.equal(res.status, 400);
    assert.equal(res.body.error, 'empty_file',
      'express.raw does not even parse a type outside the allowlist');
  });

  it('serves the file back with its stored type and nosniff', async () => {
    const res = await fetch(`${BASE}/api/uploads/${hash}`, {
      headers: { Authorization: `Bearer ${memberToken}` },
    });
    assert.equal(res.status, 200);
    assert.equal(res.headers.get('content-type'), 'image/png');
    assert.equal(res.headers.get('x-content-type-options'), 'nosniff');
    const bytes = Buffer.from(await res.arrayBuffer());
    assert.deepEqual(bytes, PNG, 'what comes back must be what went in');
  });

  it('refuses an unauthenticated download', async () => {
    const res = await fetch(`${BASE}/api/uploads/${hash}`);
    assert.equal(res.status, 401);
  });

  it('refuses a hash that is not a hash', async () => {
    const res = await api('/api/uploads/..%2F..%2Fetc%2Fpasswd', { token: memberToken });
    assert.equal(res.status, 400);
  });

  it('attaches to a message and records its media type', async () => {
    const res = await api(`/api/channels/${textChannelId}/messages`, {
      method: 'POST', body: { body: 'look at this', attachmentHash: hash }, token: memberToken,
    });
    assert.equal(res.status, 201);
    assert.equal(res.body.message.mediaType, 'image');
    assert.equal(res.body.message.attachmentHash, hash);
  });

  it('refuses an attachment that was never uploaded', async () => {
    const res = await api(`/api/channels/${textChannelId}/messages`, {
      method: 'POST', body: { body: 'nope', attachmentHash: 'f'.repeat(64) }, token: memberToken,
    });
    assert.equal(res.status, 400);
    assert.equal(res.body.error, 'no_such_upload');
  });

  it('still refuses an oversized JSON body on a normal route', async () => {
    // Pinned so nobody "fixes" the upload route by loosening the global limit.
    const res = await api(`/api/channels/${textChannelId}/messages`, {
      method: 'POST', body: { body: 'x'.repeat(20_000) }, token: memberToken,
    });
    assert.equal(res.status, 413);
  });

  it('has a per-file cap well under the disk quota', () => {
    assert.ok(MAX_UPLOAD_BYTES <= 25 * 1024 * 1024);
  });
});

describe('search', () => {
  before(async () => {
    for (const body of ['the quick brown fox', 'a screenshot of the build', 'unrelated chatter']) {
      await api(`/api/channels/${textChannelId}/messages`, {
        method: 'POST', body: { body }, token: memberToken,
      });
    }
  });

  it('matches a substring, not just a whole word', async () => {
    const res = await api(`/api/channels/${textChannelId}/search?q=creensho`, { token: memberToken });
    assert.equal(res.status, 200);
    assert.equal(res.body.mode, 'fts');
    assert.ok(res.body.results.some((m) => m.body.includes('screenshot')),
      'trigram search is what makes "creensho" find "screenshot"');
  });

  it('ignores case', async () => {
    const res = await api(`/api/channels/${textChannelId}/search?q=QUICK`, { token: memberToken });
    assert.ok(res.body.results.some((m) => m.body.includes('quick')));
  });

  it('finds by author', async () => {
    const res = await api(`/api/channels/${textChannelId}/search?q=member`, { token: memberToken });
    assert.ok(res.body.results.length > 0);
  });

  it('finds by media type', async () => {
    const res = await api(`/api/channels/${textChannelId}/search?q=image`, { token: memberToken });
    assert.ok(res.body.results.some((m) => m.mediaType === 'image'));
  });

  it('falls back to LIKE under three characters, instead of silently finding nothing', async () => {
    // The trap: trigram returns ZERO rows for a 1-2 character query and does
    // not error, so without the fallback this looks like "no results".
    const res = await api(`/api/channels/${textChannelId}/search?q=fo`, { token: memberToken });
    assert.equal(res.body.mode, 'like');
    assert.ok(res.body.results.some((m) => m.body.includes('fox')));
  });

  it('survives input that is FTS5 query syntax', async () => {
    for (const q of ['a"b(c', '100%', '*', 'NOT bob', '^he']) {
      const res = await api(
        `/api/channels/${textChannelId}/search?q=${encodeURIComponent(q)}`,
        { token: memberToken },
      );
      assert.equal(res.status, 200, `query ${JSON.stringify(q)} must not 500`);
    }
  });
});

describe('the disk quota', () => {
  it('refuses an upload that would exceed it', async () => {
    // Restart with a quota smaller than the file we are about to send.
    await stopHarmony();
    await startHarmony({ HARMONY_MAX_DISK_BYTES: '100' });

    const token = (await api('/api/accounts/login', {
      method: 'POST', body: { nickname: 'member', password: 'hunter22' },
    })).body.token;

    const big = Buffer.alloc(5000, 1);
    const res = await api('/api/uploads', {
      method: 'POST', raw: big, contentType: 'image/png', token,
    });
    assert.equal(res.status, 507, 'a full disk stops every SQLite write, so refuse early');
    assert.equal(res.body.error, 'server_full');
  });
});
