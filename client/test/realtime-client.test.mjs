// The reconnect behaviour of the main-process WebSocket client.
//
// Everything else that touches realtime.js drives it against a healthy
// server for a few seconds. The two bugs this file exists for both needed a
// connection that stays up for a while, or one whose close arrives late --
// neither of which any suite had ever produced:
//
//   1. the watchdog tore down HEALTHY sockets, because it was fed only by
//      application messages and a quiet server sends none;
//   2. a stale socket's close handler nulled its own replacement, leaving
//      the UI stuck on "Reconnecting..." over a working connection.
//
// It runs against a tiny WebSocket server of its own rather than the real
// one: the point is the client's state machine, and a stub can be made to
// go quiet or to stall a close on demand.
//
//   node --test client/test/realtime-client.test.mjs

import { after, before, describe, it } from 'node:test';
import assert from 'node:assert/strict';
import { createServer } from 'node:http';
import { createRequire } from 'node:module';

const require = createRequire(import.meta.url);
// `ws` is a SERVER dependency; the client deliberately has none, because
// Node's built-in WebSocket is a client only. Reaching across for the stub
// keeps it that way.
const { WebSocketServer } = require('../../server/node_modules/ws');
const { RealtimeClient } = require('../src/main/realtime.js');

const PORT = 18150;
const BASE = `http://127.0.0.1:${PORT}`;

let http;
let wss;

/** Sockets the stub has accepted, newest last. */
const accepted = [];
/** Set true to make the stub ignore `ping`, so the watchdog is reachable. */
let answerPings = true;
/** Set true to make the stub accept a hello but never close politely. */
let stallClose = false;

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/**
 * Hang up on everything and start the socket list over.
 *
 * The stub is shared, and a client from the previous test can still have a
 * reconnect in flight when the next one begins -- which lands in `accepted`
 * and makes `accepted[0]` somebody else's socket. Clearing the array is not
 * enough; the sockets have to actually go.
 */
async function reset() {
  for (const ws of accepted) {
    try { ws.terminate(); } catch { /* gone */ }
  }
  accepted.length = 0;
  answerPings = true;
  await sleep(400);
  accepted.length = 0;
}

before(async () => {
  http = createServer();
  wss = new WebSocketServer({ noServer: true });

  http.on('upgrade', (req, socket, head) => {
    if (new URL(req.url, BASE).pathname !== '/ws') return socket.destroy();
    return wss.handleUpgrade(req, socket, head, (ws) => {
      accepted.push(ws);
      ws.on('message', (data) => {
        const msg = JSON.parse(String(data));
        if (msg.type === 'hello') {
          ws.send(JSON.stringify({ type: 'hello-ok', rid: msg.rid, user: { nickname: 'x' } }));
          return;
        }
        if (msg.type === 'ping' && answerPings) {
          ws.send(JSON.stringify({ type: 'pong', rid: msg.rid }));
        }
        // Anything else: deliberate silence, like a real quiet channel.
      });
      ws.on('error', () => {});
    });
  });

  await new Promise((done) => http.listen(PORT, '127.0.0.1', done));
});

after(async () => {
  for (const ws of accepted) {
    try { ws.terminate(); } catch { /* gone */ }
  }
  wss.close();
  await new Promise((done) => http.close(done));
});

// ---------------------------------------------------------------------------

describe('staying connected while nothing happens', () => {
  it('survives far longer than the watchdog without reconnecting', async () => {
    await reset();

    const client = new RealtimeClient();
    const events = [];
    client.on('event', (e) => events.push(e.type));

    await client.connect(BASE, 'token');
    assert.equal(accepted.length, 1, 'one socket after connecting');

    /*
     * Forty seconds of silence. The watchdog is 35 s and the heartbeat 15 s,
     * so this covers two heartbeats and would have tripped the old watchdog
     * at least once -- which is exactly what put "Reconnecting..." on screen
     * every half minute in a channel where nobody was saying anything.
     */
    await sleep(40_000);

    assert.equal(accepted.length, 1,
      `the connection was rebuilt ${accepted.length - 1} time(s) for no reason`);
    assert.ok(!events.includes('realtime:down'),
      `saw ${JSON.stringify(events)}`);
    assert.equal(client.connected, true);

    client.disconnect();
  });

  it('still gives up on a socket that genuinely stops answering', async () => {
    await reset();
    answerPings = false; // the socket is open, and nothing comes back

    const client = new RealtimeClient();
    const downs = [];
    client.on('event', (e) => { if (e.type === 'realtime:down') downs.push(Date.now()); });

    await client.connect(BASE, 'token');
    assert.equal(accepted.length, 1);

    // The watchdog is 35 s; allow it a moment past that.
    await sleep(40_000);

    assert.ok(downs.length >= 1, 'a dead socket was never noticed');
    assert.ok(accepted.length >= 2,
      `it noticed but never reconnected (${accepted.length} sockets)`);

    client.disconnect();
    answerPings = true;
  });
});

describe('a stale socket finishing its close', () => {
  /**
   * The stuck-on-"Reconnecting" bug, reproduced directly.
   *
   * A socket is abandoned, a replacement connects, and only then does the
   * old one's close event arrive. The old handler used to null `this.#ws`
   * -- the NEW socket -- fail its pending work and emit `realtime:down`, so
   * the UI sat on "Reconnecting..." over a connection that was working, with
   * the roster still updating underneath.
   */
  it('does not let the old socket tear down its replacement', async () => {
    await reset();

    const client = new RealtimeClient();
    const events = [];
    client.on('event', (e) => events.push(e.type));

    await client.connect(BASE, 'token');
    assert.equal(accepted.length, 1, 'another test left a socket behind');
    const first = accepted[0];

    // Drop the connection the way a network does: no close handshake, so the
    // client notices, reconnects, and the original's close can land late.
    first.terminate();

    await new Promise((done) => {
      const timer = setInterval(() => {
        if (accepted.length >= 2 && client.connected) {
          clearInterval(timer);
          done();
        }
      }, 100);
    });

    const upAt = events.lastIndexOf('realtime:up');
    assert.ok(upAt >= 0, `never came back up: ${JSON.stringify(events)}`);

    // Give any straggling close event from the first socket time to arrive.
    await sleep(1500);

    assert.equal(client.connected, true,
      'the replacement was torn down by the old socket');
    assert.equal(events.lastIndexOf('realtime:down') < upAt, true,
      `a stale down arrived after the reconnect: ${JSON.stringify(events)}`);

    client.disconnect();
  });
});
