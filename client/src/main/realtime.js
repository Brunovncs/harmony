// The WebSocket client, in the main process.
//
// It lives here for the same reason every other network call does: the
// renderer is served over a custom scheme with `default-src 'self'` and no
// `connect-src`, so it cannot open a socket at all. Main owns the connection
// and forwards frames over IPC.
//
// Node's global WebSocket (Node 22+, and Electron 44 bundles Node 24) means
// this needs no dependency. It is browser-shaped, which has one consequence
// worth stating because it drives the protocol: it cannot set request headers,
// so the session token cannot travel in an Authorization header. It goes in a
// `hello` frame instead and the server accepts nothing else until it arrives.

const { EventEmitter } = require('node:events');

/** Backoff schedule. The last value repeats. */
const BACKOFF_MS = [500, 1000, 2000, 5000, 10_000, 15_000];

/**
 * A socket that has produced no traffic at all in this long is presumed dead.
 *
 * The classic LAN failure is not an error event -- it is a socket that stays
 * open forever and simply stops delivering, after a laptop sleeps or a Wi-Fi
 * roam drops the NAT entry. Nothing fires, so without this the client sits
 * looking connected and silently misses every roster change. The server pings
 * every 15 s, so 35 s is two missed pings plus slack.
 */
const WATCHDOG_MS = 35_000;

const REQUEST_TIMEOUT_MS = 10_000;

class RealtimeClient extends EventEmitter {
  #url = '';
  #token = '';
  #ws = null;
  #pending = new Map();
  #nextRid = 1;
  #attempt = 0;
  #reconnectTimer = null;
  #watchdog = null;
  /** Set when the server rejected our credentials; stops all retrying. */
  #rejected = false;
  #wanted = false;

  get connected() {
    return this.#ws?.readyState === 1; // OPEN
  }

  /**
   * Connect, and keep connecting. Resolves on the first successful hello.
   */
  connect(serverUrl, token) {
    this.#url = toWebSocketUrl(serverUrl);
    this.#token = String(token ?? '');
    this.#rejected = false;
    this.#wanted = true;
    this.#attempt = 0;

    return new Promise((resolve, reject) => {
      this.once('hello', resolve);
      this.once('give-up', reject);
      this.#open();
    });
  }

  disconnect() {
    this.#wanted = false;
    clearTimeout(this.#reconnectTimer);
    clearTimeout(this.#watchdog);
    this.#reconnectTimer = null;
    this.#failPending(new Error('disconnected'));
    try { this.#ws?.close(1000, 'client closing'); } catch { /* already gone */ }
    this.#ws = null;
  }

  #open() {
    if (!this.#wanted || this.#rejected) return;

    let ws;
    try {
      ws = new WebSocket(this.#url);
    } catch (err) {
      return this.#scheduleReconnect(err);
    }
    this.#ws = ws;

    ws.addEventListener('open', () => {
      this.#armWatchdog();
      // Authenticate before announcing anything: a socket that is open but has
      // not said hello cannot do anything, so it is not yet "connected" as far
      // as the rest of the app is concerned.
      this.request('hello', { token: this.#token })
        .then((reply) => {
          if (reply.type !== 'hello-ok') throw new Error(reply.error ?? 'hello failed');
          this.#attempt = 0;
          this.emit('hello', reply);
          this.emit('event', { type: 'realtime:up', ...reply });
        })
        .catch((err) => {
          // A hello that fails on credentials closes the socket server-side
          // with 4401, which #onClose turns into a permanent give-up.
          if (!this.#rejected) this.emit('event', { type: 'realtime:error', error: err.message });
        });
    });

    ws.addEventListener('message', (event) => {
      this.#armWatchdog();
      let msg;
      try {
        msg = JSON.parse(event.data);
      } catch {
        return;
      }

      const slot = msg.rid != null ? this.#pending.get(msg.rid) : null;
      if (slot) {
        this.#pending.delete(msg.rid);
        clearTimeout(slot.timer);
        slot.resolve(msg);
        return;
      }
      this.emit('event', msg);
    });

    ws.addEventListener('close', (event) => this.#onClose(event));
    ws.addEventListener('error', () => { /* 'close' always follows */ });
  }

  #onClose(event) {
    clearTimeout(this.#watchdog);
    this.#failPending(new Error('socket closed'));
    this.#ws = null;

    // 4401 is the server saying the token is no good. Retrying that is not
    // just useless, it is harmful: every attempt is another failed credential
    // against the login limiter, so a backgrounded client could lock its own
    // account out while nobody is watching.
    if (event?.code === 4401) {
      this.#rejected = true;
      this.#wanted = false;
      this.emit('event', { type: 'realtime:rejected' });
      this.emit('give-up', new Error('the server rejected this session'));
      return;
    }

    this.emit('event', { type: 'realtime:down' });
    this.#scheduleReconnect();
  }

  #scheduleReconnect() {
    if (!this.#wanted || this.#rejected || this.#reconnectTimer) return;
    const base = BACKOFF_MS[Math.min(this.#attempt, BACKOFF_MS.length - 1)];
    this.#attempt += 1;
    // Jitter, so a server restart does not bring every client back in the same
    // millisecond and knock it over again.
    const delay = base + Math.floor(Math.random() * base * 0.5);
    this.#reconnectTimer = setTimeout(() => {
      this.#reconnectTimer = null;
      this.#open();
    }, delay);
    this.#reconnectTimer.unref?.();
  }

  #armWatchdog() {
    clearTimeout(this.#watchdog);
    this.#watchdog = setTimeout(() => {
      // Terminate rather than close: the point is that this socket is not
      // actually carrying anything, so a polite close may never complete.
      try { this.#ws?.close(4000, 'watchdog'); } catch { /* already gone */ }
      this.#ws = null;
      this.emit('event', { type: 'realtime:down' });
      this.#scheduleReconnect();
    }, WATCHDOG_MS);
    this.#watchdog.unref?.();
  }

  #failPending(err) {
    for (const slot of this.#pending.values()) {
      clearTimeout(slot.timer);
      slot.reject(err);
    }
    this.#pending.clear();
  }

  /**
   * One envelope for everything.
   *
   * Every channel operation goes through this rather than its own IPC channel
   * and HTTP route: one correlation scheme, one timeout, one error path.
   */
  request(type, payload = {}) {
    if (this.#ws?.readyState !== 1) {
      return Promise.reject(new Error('Not connected to the server.'));
    }
    const rid = String(this.#nextRid++);
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => {
        this.#pending.delete(rid);
        reject(new Error(`The server did not answer "${type}" in time.`));
      }, REQUEST_TIMEOUT_MS);
      timer.unref?.();
      this.#pending.set(rid, { resolve, reject, timer });
      try {
        this.#ws.send(JSON.stringify({ ...payload, type, rid }));
      } catch (err) {
        this.#pending.delete(rid);
        clearTimeout(timer);
        reject(err);
      }
    });
  }
}

/** http://host:8080 -> ws://host:8080/ws, https -> wss. */
function toWebSocketUrl(serverUrl) {
  const trimmed = String(serverUrl ?? '').trim();
  const withScheme = /^https?:\/\//i.test(trimmed) ? trimmed : `http://${trimmed}`;
  const url = new URL(withScheme);
  url.protocol = url.protocol === 'https:' ? 'wss:' : 'ws:';
  url.pathname = '/ws';
  url.search = '';
  return url.toString();
}

module.exports = { RealtimeClient, toWebSocketUrl };
