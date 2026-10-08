// HARMONY_SIGNALING_URL=auto: every client is sent to MediaMTX on the host it
// used to reach this server, so a server known by a LAN address and a
// Tailscale address gives media to clients on either.
//
//   node --test server/test/signaling.test.js

import { after, before, describe, it } from 'node:test';
import assert from 'node:assert/strict';
import http from 'node:http';
import { spawn } from 'node:child_process';
import { fileURLToPath } from 'node:url';
import { dirname, resolve } from 'node:path';
import { mkdtempSync, rmSync } from 'node:fs';
import { tmpdir } from 'node:os';
import WebSocket from 'ws';

import { parseSignaling, signalingBaseFor } from '../src/config.js';

const here = dirname(fileURLToPath(import.meta.url));
const serverEntry = resolve(here, '..', 'src', 'index.js');

// Unique across the suite: node --test runs these files in PARALLEL.
const FAKE_MTX_PORT = 20005;
const HARMONY_PORT = 18089;

const LAN = '192.168.1.10:8080';
const TAILSCALE = '100.101.102.103:8080';

describe('parsing HARMONY_SIGNALING_URL', () => {
  it('leaves an explicit URL alone, whatever host the client used', () => {
    const s = parseSignaling('https://media.example.com/');
    assert.equal(s.auto, null);
    assert.equal(signalingBaseFor(s, LAN), 'https://media.example.com');
    assert.equal(parseSignaling(undefined).base, 'http://localhost:8889');
  });

  it('auto keeps the scheme, port and path and takes the host from the request', () => {
    const bare = parseSignaling('auto');
    assert.equal(signalingBaseFor(bare, LAN), 'http://192.168.1.10:8889');
    assert.equal(signalingBaseFor(bare, TAILSCALE), 'http://100.101.102.103:8889');
    assert.equal(signalingBaseFor(bare, '[fd7a:115c::1]:8080'), 'http://[fd7a:115c::1]:8889');
    assert.equal(signalingBaseFor(bare, 'pi.local'), 'http://pi.local:8889');

    const proxied = parseSignaling('https://auto/mtx/');
    assert.equal(signalingBaseFor(proxied, 'harmony.example.com'), 'https://harmony.example.com/mtx');
  });

  it('falls back to localhost for a missing or odd Host header', () => {
    const s = parseSignaling('auto');
    for (const host of [undefined, '', 'evil.example/path', 'user@host', 'a b']) {
      assert.equal(signalingBaseFor(s, host), 'http://localhost:8889', String(host));
    }
  });
});

let fakeMtx;
let child;
let dataDir;
let ownerKey = null;

function startFakeMediaMtx() {
  return new Promise((done) => {
    fakeMtx = http.createServer((req, res) => {
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
        HARMONY_POLL_INTERVAL_MS: '100',
        HARMONY_SIGNALING_URL: 'auto',
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

/** An HTTP request that claims to have come in through `host`. */
function request(method, path, host, body, token) {
  return new Promise((done, fail) => {
    const data = body ? JSON.stringify(body) : null;
    const req = http.request(
      {
        host: '127.0.0.1',
        port: HARMONY_PORT,
        method,
        path,
        headers: {
          Host: host,
          ...(data ? { 'Content-Type': 'application/json' } : {}),
          ...(token ? { Authorization: `Bearer ${token}` } : {}),
        },
      },
      (res) => {
        let text = '';
        res.on('data', (c) => { text += c; });
        res.on('end', () => done({ status: res.statusCode, body: text ? JSON.parse(text) : null }));
      },
    );
    req.on('error', fail);
    req.end(data);
  });
}

/** Opens a socket through `host`, says hello, and sends one request. */
async function overSocket(host, token, payload) {
  const ws = new WebSocket(`ws://127.0.0.1:${HARMONY_PORT}/ws`, { headers: { Host: host } });
  const replies = new Map();
  ws.on('message', (data) => {
    const msg = JSON.parse(String(data));
    replies.get(msg.rid)?.(msg);
  });
  await new Promise((done, fail) => { ws.once('open', done); ws.once('error', fail); });
  const ask = (body) => new Promise((done) => {
    const rid = Math.random().toString(36).slice(2);
    replies.set(rid, done);
    ws.send(JSON.stringify({ ...body, rid }));
  });
  assert.equal((await ask({ type: 'hello', token })).type, 'hello-ok');
  const reply = await ask(payload);
  ws.close();
  return reply;
}

describe('a server on auto', () => {
  before(async () => {
    dataDir = mkdtempSync(resolve(tmpdir(), 'harmony-sig-'));
    await startFakeMediaMtx();
    await startHarmony();
    await new Promise((r) => setTimeout(r, 300));
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

  it('reports a signaling base per address in /api/health', async () => {
    assert.equal((await request('GET', '/api/health', LAN)).body.signalingBase, 'http://192.168.1.10:8889');
    assert.equal((await request('GET', '/api/health', TAILSCALE)).body.signalingBase, 'http://100.101.102.103:8889');
  });

  it('hands out WHIP and WHEP URLs on the address the client used', async () => {
    const res = await request('POST', '/api/session', TAILSCALE, { username: 'carol' });
    assert.equal(res.status, 200);
    assert.match(res.body.whipUrl, /^http:\/\/100\.101\.102\.103:8889\/carol\/whip\?token=/);

    const streams = await request('GET', '/api/streams', LAN);
    assert.equal(streams.status, 200);
  });

  it('gives each socket channel URLs on its own address', async () => {
    const reg = await request('POST', '/api/accounts/register', LAN, { nickname: 'dave', password: 'hunter22', ownerKey });
    assert.equal(reg.status, 201, JSON.stringify(reg.body));
    const token = reg.body.token;
    const channels = (await request('GET', '/api/channels', LAN, null, token)).body.channels;
    const voice = channels.find((c) => c.kind === 'voice').id;

    const fromLan = await overSocket(LAN, token, { type: 'voice:join', channelId: voice });
    assert.equal(fromLan.type, 'voice:joined');
    assert.equal(fromLan.whepBase, 'http://192.168.1.10:8889');
    assert.match(fromLan.publish.voice, /^http:\/\/192\.168\.1\.10:8889\/vc-/);

    const fromTailscale = await overSocket(TAILSCALE, token, { type: 'voice:join', channelId: voice });
    assert.equal(fromTailscale.whepBase, 'http://100.101.102.103:8889');
    assert.match(fromTailscale.publish.cam, /^http:\/\/100\.101\.102\.103:8889\/vc-.*\/whip\?token=/);
  });
});
