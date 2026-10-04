// Harmony control server.
//
// Two jobs, nothing more:
//   * /api/*          -- the client asks "is this username free?" and gets back
//                        either a publish token or a watch URL.
//   * /mediamtx/auth  -- MediaMTX asks "may this connection publish/read?".
//
// No media touches this process.

import express from 'express';
import { config, iceServers, whepUrl, whipUrl } from './config.js';
import { LoginLimiter, secretsMatch } from './auth.js';
import { MediaMtxMonitor } from './mediamtx-api.js';
import { Rooms, normalizeUsername, normalizePath } from './rooms.js';
import { ensureDataDir, openDatabase, meta } from './db.js';
import { Accounts, claimOwner, ensureOwnerToken, publicUser } from './accounts.js';
import {
  Channels,
  VoiceRooms,
  channelPath,
  channelSecret,
  mintChannelToken,
  parseChannelPath,
  publicChannel,
  readChannelToken,
  MEDIA_KINDS,
} from './channels.js';
import { Realtime } from './realtime.js';
import {
  Chat, Soundpad, publicMessage, publicClip, allowedTypes, mediaTypeOf,
  MAX_UPLOAD_BYTES, MAX_AVATAR_BYTES, MAX_CLIP_BYTES,
} from './chat.js';

const app = express();
app.disable('x-powered-by');

/**
 * Trust only a proxy on this machine -- Caddy, or a Cloudflare Tunnel connector.
 *
 * `true` would trust the whole X-Forwarded-For chain, and every entry in that
 * header except the one our own proxy appends is written by the client. With a
 * password to guess that is not academic: the rate limiter is keyed on the
 * address, so a spoofable address is a rate limiter that can be stepped around
 * by changing one header. 'loopback' makes Express walk back only through
 * proxies it actually trusts, landing on the address Caddy observed.
 */
app.set('trust proxy', 'loopback');
app.use(express.json({ limit: '16kb' }));

const db = openDatabase({ dataDir: ensureDataDir(config.dataDir) });
const metaStore = meta(db);
const accounts = new Accounts(db);
ensureOwnerToken(accounts, metaStore);

const channels = new Channels(db);
const voice = new VoiceRooms();
const mediaSecret = channelSecret(metaStore);
const chat = new Chat(db, { dataDir: config.dataDir, maxDiskBytes: config.maxDiskBytes });
const soundpad = new Soundpad(db);

const rooms = new Rooms({ claimTtlMs: config.claimTtlMs });
const limiter = new LoginLimiter({
  maxAttempts: config.maxLoginAttempts,
  lockoutMinutes: config.lockoutMinutes,
});

/**
 * A second limiter for account logins, keyed by nickname rather than address.
 *
 * The shared-password limiter is keyed by IP, which is the right control for
 * "someone is guessing the door key". It is the wrong one here: everybody
 * behind one NAT shares an address, so one person fat-fingering their password
 * would lock out the household. Keying by nickname also means an attacker who
 * rotates addresses still cannot grind a single account.
 */
const loginLimiter = new LoginLimiter({
  maxAttempts: config.maxLoginAttempts,
  lockoutMinutes: config.lockoutMinutes,
});
const monitor = new MediaMtxMonitor({
  apiUrl: config.mediamtxApi,
  intervalMs: config.pollIntervalMs,
});

monitor.on('paths', (paths) => rooms.syncFromPaths(paths));
monitor.on('down', (err) => console.warn(`[mediamtx] control API unreachable: ${err.message}`));
monitor.on('up', () => console.log('[mediamtx] control API connected'));

// The desktop client is not served from a browser origin, but allowing CORS
// keeps a plain browser usable for the read-only stream list.
app.use((req, res, next) => {
  res.set('Access-Control-Allow-Origin', '*');
  res.set('Access-Control-Allow-Headers', 'Content-Type, X-Harmony-Password, Authorization');
  res.set('Access-Control-Allow-Methods', 'GET,POST,OPTIONS');
  if (req.method === 'OPTIONS') return res.sendStatus(204);
  next();
});

/**
 * Gate for everything except /api/health.
 *
 * On an open server this is a no-op, so the un-passworded deployment keeps
 * exactly the behaviour it had. The password may arrive as a header or in the
 * JSON body; the header is what the client uses, the body form is there so the
 * endpoint can be exercised with curl.
 */
function requirePassword(req, res, next) {
  if (!config.password) return next();

  const key = req.ip ?? 'unknown';
  const gate = limiter.check(key);
  if (!gate.allowed) {
    res.set('Retry-After', String(gate.retryAfterSec));
    return res.status(429).json({
      error: 'locked_out',
      retryAfterSec: gate.retryAfterSec,
      message: `Too many wrong passwords. Try again in ${formatWait(gate.retryAfterSec)}.`,
    });
  }

  const offered = req.get('x-harmony-password') ?? req.body?.password;

  // Offering nothing is not a guess, and must not burn an attempt. The client
  // asks for the stream list as soon as it opens, before the user has typed
  // anything -- counting that would let someone lock themselves out of their
  // own server in three refreshes without ever getting a password wrong.
  if (!offered) {
    return res.status(401).json({
      error: 'password_required',
      message: 'This server needs a password.',
    });
  }

  if (secretsMatch(offered, config.password)) {
    limiter.succeed(key);
    return next();
  }

  const result = limiter.fail(key);
  console.warn(
    `[auth] wrong password from ${key}` +
      (result.locked ? ` -- locked out for ${formatWait(result.retryAfterSec)}` : ''),
  );
  if (result.locked) {
    res.set('Retry-After', String(result.retryAfterSec));
    return res.status(429).json({
      error: 'locked_out',
      retryAfterSec: result.retryAfterSec,
      message: `Too many wrong passwords. Try again in ${formatWait(result.retryAfterSec)}.`,
    });
  }
  return res.status(401).json({
    error: 'bad_password',
    attemptsLeft: result.attemptsLeft,
    message: `Wrong password. ${result.attemptsLeft} ${result.attemptsLeft === 1 ? 'try' : 'tries'} left before a lockout.`,
  });
}

function formatWait(seconds) {
  if (seconds < 90) return `${seconds} seconds`;
  return `${Math.ceil(seconds / 60)} minutes`;
}

// ---------------------------------------------------------------------------
// Client API
// ---------------------------------------------------------------------------

/**
 * Deliberately outside the password gate: the client has to be able to ask
 * "does this server want a password?" before it can sensibly prompt for one.
 * It gives away nothing but that answer until the caller authenticates.
 */
app.get('/api/health', (req, res) => {
  // Checking the password here used to be free: no rate limiting, so
  // `authenticated` flipped to true on a correct guess without ever burning an
  // attempt -- an unmetered oracle sitting next to a metered one. Offering
  // nothing is still free, because the client probes this endpoint before the
  // user has typed anything.
  let authed = !config.password;
  if (config.password) {
    const offered = req.get('x-harmony-password');
    const key = req.ip ?? 'unknown';
    const gate = limiter.check(key);
    if (offered && !gate.allowed) {
      res.set('Retry-After', String(gate.retryAfterSec));
      return res.status(429).json({
        error: 'locked_out',
        retryAfterSec: gate.retryAfterSec,
        message: `Too many wrong passwords. Try again in ${formatWait(gate.retryAfterSec)}.`,
      });
    }
    if (offered) {
      authed = secretsMatch(offered, config.password);
      if (authed) limiter.succeed(key);
      else limiter.fail(key);
    }
  }
  res.json({
    ok: monitor.reachable,
    mediamtx: monitor.reachable ? 'up' : 'down',
    passwordRequired: Boolean(config.password),
    authenticated: authed,
    ...(authed
      ? {
          signalingBase: config.signalingBase,
          liveStreams: rooms.listLive().length,
          // Lets the client show "create the first account" rather than a
          // login form on a brand new server.
          hasAccounts: !accounts.isEmpty,
          needsOwner: !accounts.hasOwner,
        }
      : {}),
  });
});

app.use('/api', requirePassword);

// ---------------------------------------------------------------------------
// Accounts
//
// These sit BEHIND the shared password: it is still the door, accounts are the
// rooms. A stranger cannot reach any of this, let alone enumerate nicknames.
// ---------------------------------------------------------------------------

/**
 * Resolve `Authorization: Bearer <token>` to req.user, or leave it undefined.
 *
 * Never rejects on its own -- plenty of routes are usable logged out, and the
 * ones that are not say so themselves. That keeps "who may do this" next to the
 * thing being done rather than in a middleware chain.
 */
app.use('/api', (req, _res, next) => {
  const header = req.get('authorization') ?? '';
  const token = header.startsWith('Bearer ') ? header.slice(7) : null;
  if (token) {
    const user = accounts.resolveSession(token);
    if (user) {
      req.user = user;
      req.sessionToken = token;
    }
  }
  next();
});

const requireLogin = (req, res, next) => {
  if (!req.user) {
    return res.status(401).json({ error: 'login_required', message: 'Log in first.' });
  }
  return next();
};

const requireRole = (...roles) => (req, res, next) => {
  if (!req.user) {
    return res.status(401).json({ error: 'login_required', message: 'Log in first.' });
  }
  if (!roles.includes(req.user.role)) {
    return res.status(403).json({ error: 'forbidden', message: 'You do not have permission.' });
  }
  return next();
};

app.post('/api/accounts/register', async (req, res) => {
  const result = await accounts.register(req.body?.nickname, req.body?.password);
  if (!result.ok) {
    const messages = {
      invalid_nickname:
        '2-20 characters: letters, digits, hyphen or underscore, starting with a letter or '
        + 'digit. Cannot start with "vc-" or end with "-cam".',
      weak_password: 'Use at least 6 characters.',
      nickname_taken: 'That nickname is already registered.',
    };
    return res.status(400).json({ error: result.error, message: messages[result.error] });
  }

  // The owner key is offered at registration so claiming ownership is one step
  // rather than "register, then find the other box".
  const becameOwner = req.body?.ownerKey
    ? claimOwner(accounts, metaStore, result.user.id, req.body.ownerKey)
    : false;

  const user = accounts.find(result.user.nickname);
  const token = accounts.startSession(user.id);
  return res.status(201).json({
    user: publicUser(user),
    token,
    ownerClaimed: becameOwner,
    // Tell them the key was wrong rather than silently making them a member.
    ownerKeyRejected: Boolean(req.body?.ownerKey) && !becameOwner,
  });
});

app.post('/api/accounts/login', async (req, res) => {
  const nickname = String(req.body?.nickname ?? '').trim().toLowerCase();
  const gate = loginLimiter.check(nickname);
  if (!gate.allowed) {
    res.set('Retry-After', String(gate.retryAfterSec));
    return res.status(429).json({
      error: 'locked_out',
      retryAfterSec: gate.retryAfterSec,
      message: `Too many wrong passwords for "${nickname}". Try again in ${formatWait(gate.retryAfterSec)}.`,
    });
  }

  const user = await accounts.verify(nickname, req.body?.password);
  if (!user) {
    const result = loginLimiter.fail(nickname);
    if (result.locked) {
      res.set('Retry-After', String(result.retryAfterSec));
      return res.status(429).json({
        error: 'locked_out',
        retryAfterSec: result.retryAfterSec,
        message: `Too many wrong passwords. Try again in ${formatWait(result.retryAfterSec)}.`,
      });
    }
    // One message for "no such account" and "wrong password" -- the pair would
    // otherwise tell an attacker which nicknames exist.
    return res.status(401).json({
      error: 'bad_credentials',
      attemptsLeft: result.attemptsLeft,
      message: 'Wrong nickname or password.',
    });
  }
  loginLimiter.succeed(nickname);

  const becameOwner = req.body?.ownerKey
    ? claimOwner(accounts, metaStore, user.id, req.body.ownerKey)
    : false;

  const token = accounts.startSession(user.id);
  return res.json({
    user: publicUser(accounts.find(user.nickname)),
    token,
    ownerClaimed: becameOwner,
    ownerKeyRejected: Boolean(req.body?.ownerKey) && !becameOwner,
  });
});

app.post('/api/accounts/logout', (req, res) => {
  if (req.sessionToken) accounts.endSession(req.sessionToken);
  res.json({ ok: true });
});

app.get('/api/accounts/me', requireLogin, (req, res) => {
  res.json({ user: publicUser(req.user) });
});

app.post('/api/accounts/password', requireLogin, async (req, res) => {
  const ok = await accounts.verify(req.user.nickname, req.body?.current);
  if (!ok) {
    return res.status(401).json({ error: 'bad_credentials', message: 'Current password is wrong.' });
  }
  const result = await accounts.changePassword(req.user.id, req.body?.password);
  if (!result.ok) {
    return res.status(400).json({ error: result.error, message: 'Use at least 6 characters.' });
  }
  // changePassword drops every session for this account, including this one.
  return res.json({ ok: true, token: accounts.startSession(req.user.id) });
});

/**
 * Set or clear your profile picture.
 *
 * Two steps on purpose: the picture is uploaded through /api/uploads like any
 * other file, and this only points the account at it. That means avatars
 * inherit the content-type allowlist, the disk quota and the content-addressed
 * storage for free, and two people who pick the same picture cost one copy.
 *
 * The reference juggling is the part that matters. An avatar nobody references
 * is an orphan the moment the next upload needs room, so the new file is
 * retained BEFORE the old one is released -- re-setting the same picture must
 * not momentarily drop it to zero.
 */
app.post('/api/accounts/avatar', requireLogin, (req, res) => {
  const hash = req.body?.hash == null ? null : String(req.body.hash);

  if (hash !== null) {
    if (!/^[0-9a-f]{64}$/.test(hash)) {
      return res.status(400).json({ error: 'bad_hash', message: 'That is not an uploaded file.' });
    }
    const upload = chat.fileInfo(hash);
    if (!upload) {
      return res.status(400).json({ error: 'no_such_upload', message: 'Upload the picture first.' });
    }
    if (mediaTypeOf(upload.content_type) !== 'image') {
      return res.status(400).json({
        error: 'not_an_image',
        message: 'A profile picture has to be an image.',
      });
    }
    if (upload.bytes > MAX_AVATAR_BYTES) {
      return res.status(400).json({
        error: 'avatar_too_large',
        message: `Profile pictures are limited to ${Math.round(MAX_AVATAR_BYTES / 1024)} KB.`,
      });
    }
    chat.retain(hash);
  }

  const result = accounts.setAvatar(req.user.id, hash);
  if (!result.ok) return res.status(404).json({ error: result.error });
  if (result.previous && result.previous !== hash) chat.release(result.previous);

  const user = publicUser(result.user);
  // Everyone draws everyone else's picture, so everyone needs to know.
  realtime?.broadcast({ type: 'user:updated', user });
  return res.json({ user });
});

/** Anyone logged in may see the roster; it is a friends' server, not a forum. */
app.get('/api/accounts', requireLogin, (_req, res) => {
  res.json({ users: accounts.list().map((u) => publicUser(u)) });
});

app.post('/api/accounts/:id/role', requireRole('owner'), (req, res) => {
  const id = Number.parseInt(req.params.id, 10);
  const result = accounts.setRole(id, String(req.body?.role ?? ''));
  if (!result.ok) {
    const messages = {
      invalid_role: 'Role must be owner, admin or member.',
      no_such_user: 'No such user.',
      last_owner: 'There has to be at least one owner.',
    };
    return res.status(400).json({ error: result.error, message: messages[result.error] });
  }
  const user = publicUser(result.user);
  realtime?.broadcast({ type: 'user:updated', user });
  return res.json({ user });
});

/**
 * Everything needed to watch, without reserving anything.
 *
 * The mosaic opens several streams at once, and going through /api/session for
 * each would be a trap: a name that stops being live between the listing and
 * the call comes back as "free", and the viewer would silently claim someone
 * else's username. Watching is public, so the watch URL belongs here.
 */
app.get('/api/streams', (_req, res) => {
  res.json({
    streams: rooms.listLive().map((stream) => ({ ...stream, whepUrl: whepUrl(stream.username) })),
    iceServers: iceServers(),
  });
});

// The single entry point behind the username box. Always answers with a role.
app.post('/api/session', (req, res) => {
  /**
   * Who is allowed to claim what.
   *
   * This endpoint used to let anyone past the door password claim any free
   * name. Once nicknames are permanent identities that is impersonation: log in
   * as `bob`, claim the stream name `alice`, and your screen share appears
   * under her name to everyone watching. So an authenticated caller streams
   * under their own nickname and the body's `username` is ignored outright.
   *
   * The anonymous path survives only while no account exists, so an existing
   * 1.0.0 deployment keeps working right up until someone registers. After
   * that, logging in is required -- leaving it open would leave the hole open.
   */
  /*
   * `kind: 'camera'` claims the caller's OWN derived webcam path.
   *
   * It has to be a flag rather than a username, for the same reason the body's
   * username is ignored above: letting the client name the path is exactly the
   * impersonation this endpoint was fixed to prevent. The `-cam` suffix is
   * appended here, server-side, from the authenticated nickname -- and
   * normalizeUsername refuses anything ending in `-cam` as INPUT, so the
   * namespace cannot be squatted.
   */
  const wantsCamera = req.body?.kind === 'camera';
  if (wantsCamera && !req.user) {
    return res.status(401).json({
      error: 'login_required',
      message: 'Sign in before starting your camera.',
    });
  }

  const username = req.user
    ? `${req.user.nickname}${wantsCamera ? '-cam' : ''}`
    : normalizeUsername(req.body?.username);

  if (!req.user && !accounts.isEmpty) {
    return res.status(401).json({
      error: 'login_required',
      message: 'This server has accounts. Log in to stream under your own name.',
    });
  }

  if (!username) {
    return res.status(400).json({
      error: 'invalid_username',
      message: '2-24 characters: letters, digits, hyphen or underscore, starting with a letter or digit.',
    });
  }

  if (!monitor.reachable) {
    return res.status(503).json({
      error: 'media_server_down',
      message: 'The media server is not responding. Try again in a moment.',
    });
  }

  const result = rooms.claim(username, { token: req.body?.token, ip: req.ip });

  if (result.role === 'broadcaster') {
    return res.json({
      role: 'broadcaster',
      username,
      token: result.token,
      whipUrl: whipUrl(username, result.token),
      iceServers: iceServers(),
      heartbeatMs: Math.floor(config.claimTtlMs / 3),
    });
  }

  return res.json({
    role: 'viewer',
    username,
    pending: result.pending,
    whepUrl: whepUrl(username),
    iceServers: iceServers(),
  });
});

app.post('/api/session/heartbeat', (req, res) => {
  const username = normalizeUsername(req.body?.username);
  const ok = username && rooms.heartbeat(username, req.body?.token);
  res.status(ok ? 200 : 404).json({ ok: Boolean(ok), live: username ? rooms.isLive(username) : false });
});

app.post('/api/session/release', (req, res) => {
  const username = normalizeUsername(req.body?.username);
  const ok = username && rooms.release(username, req.body?.token);
  res.json({ ok: Boolean(ok) });
});

// ---------------------------------------------------------------------------
// Channels
// ---------------------------------------------------------------------------

const requireAdmin = (req, res, next) => {
  if (!req.user) return res.status(401).json({ error: 'login_required', message: 'Log in first.' });
  if (req.user.role !== 'owner' && req.user.role !== 'admin') {
    return res.status(403).json({ error: 'forbidden', message: 'Admins only.' });
  }
  return next();
};

app.get('/api/channels', requireLogin, (req, res) => {
  res.json({
    channels: channels.list().map((c) => ({
      ...publicChannel(c),
      // So the client can show "you will not be asked again" rather than a
      // padlock on a channel this person already unlocked.
      unlocked: !c.password_hash || channels.hasGrant(c.id, req.user.id),
    })),
    occupancy: voice.occupancy(),
  });
});

app.post('/api/channels', requireAdmin, async (req, res) => {
  const result = await channels.create({
    kind: req.body?.kind,
    name: req.body?.name,
    password: req.body?.password,
  });
  if (!result.ok) {
    const messages = {
      invalid_kind: 'A channel is either voice or text.',
      invalid_name: 'Give it a name of 1-32 characters.',
    };
    return res.status(400).json({ error: result.error, message: messages[result.error] });
  }
  realtime?.broadcastChannels();
  return res.status(201).json({ channel: publicChannel(result.channel) });
});

// Registered BEFORE /api/channels/:id on purpose. Express matches routes in
// registration order, so with these the other way round a POST to
// /api/channels/reorder is matched by /:id with id="reorder", parsed as NaN,
// and answered "no such channel" -- a 400 that looks like a validation bug and
// is really a routing one.
app.post('/api/channels/reorder', requireAdmin, (req, res) => {
  const result = channels.reorder(Array.isArray(req.body?.ids) ? req.body.ids : []);
  if (!result.ok) {
    return res.status(400).json({
      error: result.error,
      message: 'Send every channel id exactly once, in the order you want.',
    });
  }
  realtime?.broadcastChannels();
  return res.json({ channels: result.channels.map(publicChannel) });
});

app.post('/api/channels/:id', requireAdmin, async (req, res) => {
  const result = await channels.update(Number.parseInt(req.params.id, 10), {
    ...(req.body?.name !== undefined ? { name: req.body.name } : {}),
    ...(req.body?.password !== undefined ? { password: req.body.password } : {}),
  });
  if (!result.ok) return res.status(400).json({ error: result.error });
  realtime?.broadcastChannels();
  return res.json({ channel: publicChannel(result.channel) });
});

app.post('/api/channels/:id/delete', requireAdmin, (req, res) => {
  const id = Number.parseInt(req.params.id, 10);
  const result = channels.remove(id);
  if (!result.ok) return res.status(404).json({ error: result.error });
  // Anyone sitting in it has nowhere to be any more.
  for (const { channelId } of voice.leaveAll(-1)) void channelId;
  realtime?.broadcastChannels();
  return res.json({ ok: true });
});

// ---------------------------------------------------------------------------
// Chat: messages, pins, search and uploads
// ---------------------------------------------------------------------------

/** A channel the caller is allowed to read, or null. */
function readableChannel(req, id) {
  const channel = channels.get(id);
  if (!channel) return null;
  if (channel.password_hash && !channels.hasGrant(id, req.user.id)) return null;
  return channel;
}

app.get('/api/channels/:id/messages', requireLogin, (req, res) => {
  const id = Number.parseInt(req.params.id, 10);
  const channel = readableChannel(req, id);
  if (!channel) return res.status(404).json({ error: 'no_such_channel' });

  const before = Number.parseInt(req.query.before ?? '', 10);
  return res.json({
    messages: chat
      .history(id, { before: Number.isFinite(before) ? before : undefined })
      .map(publicMessage),
    pinned: chat.pinned(id).map(publicMessage),
  });
});

app.post('/api/channels/:id/messages', requireLogin, (req, res) => {
  const id = Number.parseInt(req.params.id, 10);
  const channel = readableChannel(req, id);
  if (!channel) return res.status(404).json({ error: 'no_such_channel' });

  const result = chat.post({
    channelId: id,
    user: req.user,
    body: req.body?.body,
    attachmentHash: req.body?.attachmentHash ?? null,
  });
  if (!result.ok) return res.status(400).json({ error: result.error });

  const message = publicMessage(result.message);
  realtime?.broadcast({ type: 'message', message });
  return res.status(201).json({ message });
});

app.post('/api/messages/:id/pin', requireLogin, (req, res) => {
  const id = Number.parseInt(req.params.id, 10);
  const result = chat.setPinned(id, req.body?.pinned !== false);
  if (!result.ok) return res.status(404).json({ error: result.error });
  const message = publicMessage(result.message);
  realtime?.broadcast({ type: 'message:updated', message });
  return res.json({ message });
});

app.post('/api/messages/:id/delete', requireLogin, (req, res) => {
  const id = Number.parseInt(req.params.id, 10);

  // Checked BEFORE deleting, obviously -- the first draft of this removed the
  // row and then decided whether it was allowed to.
  const existing = chat.get(id);
  if (!existing) return res.status(404).json({ error: 'no_such_message' });

  const isOwn = existing.user_id === req.user.id;
  if (!isOwn && req.user.role !== 'owner' && req.user.role !== 'admin') {
    return res.status(403).json({ error: 'forbidden', message: 'Not your message.' });
  }

  const result = chat.remove(id);
  if (!result.ok) return res.status(404).json({ error: result.error });
  realtime?.broadcast({ type: 'message:deleted', id, channelId: existing.channel_id });
  return res.json({ ok: true });
});

app.get('/api/channels/:id/search', requireLogin, (req, res) => {
  const id = Number.parseInt(req.params.id, 10);
  const channel = readableChannel(req, id);
  if (!channel) return res.status(404).json({ error: 'no_such_channel' });

  const { mode, results } = chat.search(id, req.query.q);
  return res.json({ mode, results: results.map(publicMessage) });
});

/**
 * Upload a file.
 *
 * express.raw rather than the global express.json: that parser is
 * content-type gated, so an image/png body passes straight through it
 * untouched and arrives here unparsed. Verified -- the 16 kb JSON limit does
 * NOT apply to this route and does not need reordering.
 */
app.post(
  '/api/uploads',
  requireLogin,
  express.raw({ type: allowedTypes(), limit: MAX_UPLOAD_BYTES }),
  (req, res) => {
    if (!Buffer.isBuffer(req.body) || req.body.length === 0) {
      return res.status(400).json({
        error: 'empty_file',
        message: `Send the file as a raw body with one of: ${allowedTypes().join(', ')}`,
      });
    }

    const result = chat.store(req.body, req.get('content-type')?.split(';')[0]?.trim());
    if (!result.ok) {
      const messages = {
        type_not_allowed: `Allowed types: ${allowedTypes().join(', ')}`,
        file_too_large: `Files are limited to ${Math.round(MAX_UPLOAD_BYTES / 1024 / 1024)} MB.`,
        server_full: 'The server has run out of space for uploads.',
      };
      return res.status(result.error === 'server_full' ? 507 : 400).json({
        error: result.error,
        message: messages[result.error] ?? 'Upload refused.',
      });
    }

    return res.status(201).json({
      hash: result.upload.hash,
      contentType: result.upload.content_type,
      bytes: result.upload.bytes,
      deduplicated: result.deduplicated,
      usedBytes: chat.usedBytes,
      quotaBytes: chat.quotaBytes,
    });
  },
);

/**
 * Serve a stored file.
 *
 * Always with the content type recorded at upload, never one the request
 * asks for, and always as an attachment-safe type: a file smuggled in as
 * image/png is served as image/png and cannot execute. X-Content-Type-Options
 * stops a browser sniffing its way to a different conclusion.
 */
app.get('/api/uploads/:hash', requireLogin, (req, res) => {
  const hash = String(req.params.hash);
  if (!/^[0-9a-f]{64}$/.test(hash)) return res.status(400).json({ error: 'bad_hash' });

  const info = chat.fileInfo(hash);
  if (!info) return res.status(404).json({ error: 'no_such_file' });

  res.set('Content-Type', info.content_type);
  res.set('X-Content-Type-Options', 'nosniff');
  res.set('Content-Security-Policy', "default-src 'none'; sandbox");
  // Content-addressed, so it can never change: cache it forever.
  res.set('Cache-Control', 'public, max-age=31536000, immutable');
  return res.sendFile(info.path);
});

// ---------------------------------------------------------------------------
// Soundpad
// ---------------------------------------------------------------------------

app.get('/api/soundpad', requireLogin, (_req, res) => {
  res.json({ clips: soundpad.list().map(publicClip) });
});

app.post('/api/soundpad', requireAdmin, (req, res) => {
  const result = soundpad.add({
    name: req.body?.name,
    fileHash: req.body?.hash,
    userId: req.user.id,
  });
  if (!result.ok) {
    const messages = {
      invalid_name: 'Give the clip a name.',
      no_such_upload: 'Upload the audio first.',
      not_audio: 'Soundpad clips have to be audio.',
      clip_too_large:
        `Soundpad clips are limited to ${Math.round(MAX_CLIP_BYTES / 1024 / 1024)} MB -- `
        + 'every client downloads every clip.',
    };
    return res.status(400).json({ error: result.error, message: messages[result.error] });
  }
  realtime?.broadcast({ type: 'soundpad', clips: soundpad.list().map(publicClip) });
  return res.status(201).json({ clip: publicClip(result.clip) });
});

// Before /api/soundpad/:id/delete only by habit -- they cannot collide, since
// that one has a second path segment. The ordering rule still applies to the
// next person who adds /api/soundpad/:id, so it stays up here.
app.post('/api/soundpad/reorder', requireAdmin, (req, res) => {
  const result = soundpad.reorder(Array.isArray(req.body?.ids) ? req.body.ids : []);
  if (!result.ok) {
    return res.status(400).json({
      error: result.error,
      message: 'Send every clip id exactly once, in the order you want.',
    });
  }
  const clips = result.clips.map(publicClip);
  realtime?.broadcast({ type: 'soundpad', clips });
  return res.json({ clips });
});

app.post('/api/soundpad/:id/delete', requireAdmin, (req, res) => {
  const result = soundpad.remove(Number.parseInt(req.params.id, 10));
  if (!result.ok) return res.status(404).json({ error: result.error });
  realtime?.broadcast({ type: 'soundpad', clips: soundpad.list().map(publicClip) });
  return res.json({ ok: true });
});

// ---------------------------------------------------------------------------
// MediaMTX auth hook
//
// MediaMTX POSTs { user, password, token, ip, action, path, protocol, id,
// query, userAgent } and reads only the status code: 2xx allows, anything else
// denies.
// ---------------------------------------------------------------------------

app.post('/mediamtx/auth', (req, res) => {
  const { action, path, query } = req.body ?? {};

  // api / metrics / pprof are already excluded in mediamtx.yml, but MediaMTX
  // will ask if that config is ever edited. Loopback only.
  if (action === 'api' || action === 'metrics' || action === 'pprof') {
    return res.sendStatus(204);
  }

  /**
   * CHANNEL PATHS ARE HANDLED FIRST, AND THAT ORDERING IS SECURITY-CRITICAL.
   *
   * Two things below this point would otherwise defeat channel passwords
   * entirely:
   *
   *   1. the legacy read branch accepts the single process-wide `mediaToken`,
   *      which every client gets just for knowing the server password -- so a
   *      legacy client could listen to a password-protected voice channel;
   *   2. the open-server shortcut (`if (!config.mediaToken) return 204`) lets
   *      ANY read through on a server with no HARMONY_PASSWORD -- so a channel
   *      password would mean nothing at all there.
   *
   * Neither is a bug in those branches; they are correct for the flat username
   * namespace. They are simply the wrong answer for `vc-...`, and the only
   * thing keeping them from being asked is this early return. Do not "simplify"
   * it downwards.
   */
  const channelTarget = parseChannelPath(path);
  if (channelTarget) {
    const token = new URLSearchParams(query ?? '').get('token');
    const claim = readChannelToken(mediaSecret, token);

    if (!claim || claim.cid !== channelTarget.cid) {
      console.warn(`[auth] channel ${action} REJECTED for "${path}" (bad or expired token)`);
      return res.sendStatus(401);
    }

    if (action === 'read' || action === 'playback') {
      // Membership of the channel is the whole check: any member may watch any
      // other member. The hook never consults live presence, which keeps it
      // stateless, restart-proof, and off any lock during a join storm.
      return res.sendStatus(204);
    }

    if (action === 'publish') {
      // Publishing additionally requires the SLOT to match, or one member
      // could publish into another member's path.
      if (claim.mid !== channelTarget.mid) {
        console.warn(`[auth] channel publish REJECTED for "${path}" (slot mismatch)`);
        return res.sendStatus(401);
      }
      // The enforcing half of force-mute. The Phase 0 spike showed that
      // kicking the live session alone does nothing lasting -- the path is
      // immediately re-publishable -- so this refusal is what makes a mute
      // stick across the reconnect that follows.
      if (channelTarget.kind === MEDIA_KINDS.voice
          && voice.isForceMuted(channelTarget.cid, channelTarget.mid)) {
        console.warn(`[auth] channel publish REFUSED for "${path}" (force-muted)`);
        return res.sendStatus(401);
      }
      return res.sendStatus(204);
    }

    return res.sendStatus(401);
  }

  // normalizePath, NOT normalizeUsername: `bob-cam` is a system path that is
  // refused as user input but is legitimate here. Using the user-input
  // validator in the auth hook is what would make the two namespaces collide.
  const username = normalizePath(path);
  if (!username) return res.sendStatus(401);

  // On an open server, watching is open to anyone who knows the name -- that is
  // the design. With a password configured it must not be: MediaMTX listens on
  // its own port, so a reader who never touched the control server would other-
  // wise walk straight past the password. The watch URLs handed out by
  // /api/streams carry the token that proves the holder got them from us.
  if (action === 'read' || action === 'playback') {
    if (!config.mediaToken) return res.sendStatus(204);
    const token = new URLSearchParams(query ?? '').get('token');
    if (secretsMatch(token, config.mediaToken)) return res.sendStatus(204);
    console.warn(`[auth] read REJECTED for "${username}" (missing or stale watch token)`);
    return res.sendStatus(401);
  }

  if (action === 'publish') {
    // MediaMTX forwards the raw query string from the WHIP URL.
    const token = new URLSearchParams(query ?? '').get('token');
    if (rooms.mayPublish(username, token)) {
      console.log(`[auth] publish accepted for "${username}"`);
      return res.sendStatus(204);
    }
    console.warn(`[auth] publish REJECTED for "${username}" (bad or expired token)`);
    return res.sendStatus(401);
  }

  return res.sendStatus(401);
});

// ---------------------------------------------------------------------------

/**
 * The media URLs and tokens a member needs for one channel.
 *
 * One token covers reading and publishing; see channels.js for why that is
 * safe. The client builds every peer's WHEP URL from `readToken` and its own
 * WHIP URLs from the paths here.
 */
function issueTokens(channelId, userId, mid) {
  const token = mintChannelToken(mediaSecret, { cid: channelId, mid });
  const url = (kind) =>
    `${config.signalingBase}/${channelPath(channelId, mid, kind)}`;
  return {
    token,
    publish: {
      voice: `${url('voice')}/whip?token=${encodeURIComponent(token)}`,
      cam: `${url('cam')}/whip?token=${encodeURIComponent(token)}`,
      screen: `${url('screen')}/whip?token=${encodeURIComponent(token)}`,
    },
    // A template rather than a list: the roster changes constantly and the
    // client already knows every member's slot from it.
    whepBase: config.signalingBase,
    expiresInMs: TOKEN_LIFETIME_HINT_MS,
  };
}

const TOKEN_LIFETIME_HINT_MS = 10 * 60 * 1000;

monitor.start();

// Sweep idle sessions hourly. Cheap, and it keeps a long-lived server from
// accumulating a row per login forever.
const sessionSweeper = setInterval(
  () => accounts.expireSessions(config.sessionIdleMs),
  60 * 60 * 1000,
);
sessionSweeper.unref();

let realtime = null;

const server = app.listen(config.port, config.host, () => {
  console.log(`[harmony] control server on http://${config.host}:${config.port}`);
  console.log(`[harmony] clients will be sent to ${config.signalingBase}`);
  console.log(
    config.password
      ? `[harmony] password required — ${config.maxLoginAttempts} tries, then ${config.lockoutMinutes.join('/')} minute lockouts`
      : '[harmony] NO PASSWORD SET — anyone who can reach this server can use it',
  );
});

realtime = new Realtime({
  server,
  accounts,
  channels,
  voice,
  soundpad,
  issueTokens,
  kickMember: (channelId, mid) => monitor.kickPath(channelPath(channelId, mid, 'voice')),
});

/**
 * Push the live stream list instead of having every client poll for it.
 *
 * Only on CHANGE: the monitor ticks once a second and the list is usually
 * identical, so broadcasting every tick would be the same poll with extra
 * steps. The client keeps a slow reconciliation poll as a safety net.
 */
let lastStreamsJson = '';
monitor.on('paths', () => {
  const streams = rooms.listLive().map((s) => ({ ...s, whepUrl: whepUrl(s.username) }));
  const json = JSON.stringify(streams);
  if (json === lastStreamsJson) return;
  lastStreamsJson = json;
  realtime.broadcastStreams(streams);
});

for (const signal of ['SIGINT', 'SIGTERM']) {
  process.on(signal, () => {
    console.log(`\n[harmony] ${signal} -- shutting down`);
    monitor.stop();
    realtime?.close();
    clearInterval(sessionSweeper);
    server.close(() => {
      // Closing checkpoints the WAL, so the next start does not have to replay
      // it. Skipping this is survivable but leaves -wal/-shm files behind.
      db.close();
      process.exit(0);
    });
  });
}
