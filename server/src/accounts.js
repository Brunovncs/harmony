// Accounts: registration, login, session tokens and roles.
//
// The shared HARMONY_PASSWORD is unchanged and still the outer door -- accounts
// sit behind it. A stranger cannot reach these endpoints at all, let alone
// enumerate who has an account.
//
// Passwords are hashed with scrypt rather than encrypted with an AES key from
// the environment. That is both simpler and stronger: there is nothing to
// decrypt, no key to lose or rotate, and a stolen database reveals nothing.

import { randomBytes, createHash, timingSafeEqual, scrypt as scryptCb } from 'node:crypto';
import { promisify } from 'node:util';

import { isSystemName, normalizeName } from './rooms.js';

const scrypt = promisify(scryptCb);

/**
 * scrypt cost.
 *
 * N=16384 is the ceiling under Node's DEFAULT maxmem: scrypt needs 128*N*r
 * bytes = 16 MiB here, and N=32768 would need 32 MiB and fail outright against
 * the 32 MiB default unless maxmem is passed explicitly. Measured at ~20 ms on
 * a desktop, which is roughly 100-150 ms on a Pi 4 -- about right for a login
 * on the weakest host Harmony targets.
 *
 * Stored self-describing (scrypt$N$r$p$salt$hash) so these can be raised later
 * without a migration: an old hash still carries the parameters it was made
 * with.
 */
const SCRYPT = { N: 16384, r: 8, p: 1, keylen: 32 };

const NICKNAME_RE = /^[a-z0-9][a-z0-9_-]{1,19}$/;

/** Roles, most privileged first. */
export const ROLES = ['owner', 'admin', 'member'];

/**
 * "Pedro Lucas" is accepted and stored as `pedrolucas` -- the nickname becomes
 * a MediaMTX path and part of a URL, so it cannot keep the capitals or the
 * space, but there is no reason to make someone discover that by being
 * rejected. The 20-character cap applies to the NORMALISED form, and is 20
 * rather than 24 because a nickname has to leave room for the `-cam` suffix its
 * webcam path appends.
 *
 * The system-namespace ban is imported rather than repeated: two copies of this
 * rule is how one of them ends up not being updated, which is exactly the bug
 * the shared validator in rooms.js was split to prevent.
 */
export function normalizeNickname(raw) {
  const value = normalizeName(raw);
  if (!NICKNAME_RE.test(value)) return null;
  return isSystemName(value) ? null : value;
}

/**
 * Hash a password. Always async.
 *
 * NEVER use scryptSync on a request path. This same process serves
 * /mediamtx/auth, which MediaMTX calls on every publish and every read; a
 * 150 ms event-loop stall there is a visible stream failure, not a slow login.
 */
export async function hashPassword(password, params = SCRYPT) {
  const salt = randomBytes(16);
  const key = await scrypt(password, salt, params.keylen, {
    N: params.N, r: params.r, p: params.p,
  });
  return [
    'scrypt', params.N, params.r, params.p,
    salt.toString('base64url'), key.toString('base64url'),
  ].join('$');
}

/**
 * Verify a password against a stored hash.
 *
 * Returns false rather than throwing on a malformed hash: a corrupted row
 * should lock that one account out, not 500 the login endpoint.
 */
export async function verifyPassword(password, stored) {
  try {
    const [scheme, N, r, p, saltB64, hashB64] = String(stored ?? '').split('$');
    if (scheme !== 'scrypt') return false;

    const salt = Buffer.from(saltB64, 'base64url');
    const expected = Buffer.from(hashB64, 'base64url');
    const actual = await scrypt(password, salt, expected.length, {
      N: Number(N), r: Number(r), p: Number(p),
    });
    // Equal length by construction (we asked for expected.length), so
    // timingSafeEqual cannot throw and leak a length.
    return timingSafeEqual(actual, expected);
  } catch {
    return false;
  }
}

/**
 * Session tokens are hashed with sha256, not scrypt.
 *
 * A 256-bit uniform random value has no guessable structure, so a KDF buys
 * nothing against a stolen database and would cost 20 ms on every
 * authenticated request. Same reasoning as the publish tokens in rooms.js.
 */
const tokenHash = (token) => createHash('sha256').update(String(token)).digest('hex');

export class Accounts {
  #db;
  #q;

  constructor(db) {
    this.#db = db;
    this.#q = {
      byNickname: db.prepare('SELECT * FROM users WHERE nickname = ?'),
      byId: db.prepare('SELECT * FROM users WHERE id = ?'),
      insert: db.prepare(
        'INSERT INTO users (nickname, password_hash, role, created_at) VALUES (?, ?, ?, ?)',
      ),
      count: db.prepare('SELECT COUNT(*) AS n FROM users'),
      countOwners: db.prepare("SELECT COUNT(*) AS n FROM users WHERE role = 'owner'"),
      setRole: db.prepare('UPDATE users SET role = ? WHERE id = ?'),
      setAvatar: db.prepare('UPDATE users SET avatar_hash = ? WHERE id = ?'),
      list: db.prepare('SELECT id, nickname, role, avatar_hash, created_at FROM users ORDER BY nickname'),

      addSession: db.prepare(
        'INSERT INTO sessions (token_hash, user_id, created_at, last_seen) VALUES (?, ?, ?, ?)',
      ),
      findSession: db.prepare(
        'SELECT u.*, s.token_hash FROM sessions s JOIN users u ON u.id = s.user_id '
        + 'WHERE s.token_hash = ?',
      ),
      touchSession: db.prepare('UPDATE sessions SET last_seen = ? WHERE token_hash = ?'),
      dropSession: db.prepare('DELETE FROM sessions WHERE token_hash = ?'),
      dropUserSessions: db.prepare('DELETE FROM sessions WHERE user_id = ?'),
      expireSessions: db.prepare('DELETE FROM sessions WHERE last_seen < ?'),
    };
  }

  get isEmpty() {
    return this.#q.count.get().n === 0;
  }

  get hasOwner() {
    return this.#q.countOwners.get().n > 0;
  }

  find(nickname) {
    return this.#q.byNickname.get(nickname) ?? null;
  }

  list() {
    return this.#q.list.all();
  }

  /**
   * Create an account.
   *
   * @returns {Promise<{ok: true, user: object} | {ok: false, error: string}>}
   */
  async register(nickname, password, { role = 'member' } = {}) {
    const name = normalizeNickname(nickname);
    if (!name) return { ok: false, error: 'invalid_nickname' };
    if (password == null || String(password).length < 6) {
      return { ok: false, error: 'weak_password' };
    }
    if (this.find(name)) return { ok: false, error: 'nickname_taken' };

    const hash = await hashPassword(password);
    try {
      this.#q.insert.run(name, hash, role, Date.now());
    } catch (err) {
      // UNIQUE violation: someone registered the same name between the check
      // above and here. The constraint is the real guard; the check is only for
      // a nicer error.
      if (/UNIQUE/i.test(err.message)) return { ok: false, error: 'nickname_taken' };
      throw err;
    }
    return { ok: true, user: this.find(name) };
  }

  /**
   * Check a password. Returns the user row or null.
   *
   * Deliberately does nothing about rate limiting -- the caller owns that, so
   * the same LoginLimiter can be keyed by both nickname and IP.
   */
  async verify(nickname, password) {
    const name = normalizeNickname(nickname);
    if (!name) return null;
    const user = this.find(name);
    if (!user) {
      // Hash anyway, so "no such account" and "wrong password" take the same
      // time. Otherwise this endpoint is a user-enumeration oracle.
      await hashPassword(String(password ?? ''));
      return null;
    }
    return (await verifyPassword(String(password ?? ''), user.password_hash)) ? user : null;
  }

  async changePassword(userId, password) {
    if (password == null || String(password).length < 6) {
      return { ok: false, error: 'weak_password' };
    }
    const hash = await hashPassword(password);
    this.#db.prepare('UPDATE users SET password_hash = ? WHERE id = ?').run(hash, userId);
    // Every other device holding a session for this account loses it, which is
    // the point of changing a password.
    this.#q.dropUserSessions.run(userId);
    return { ok: true };
  }

  /** Issue a bearer token. The plaintext is returned once and never stored. */
  startSession(userId) {
    const token = randomBytes(32).toString('base64url');
    const now = Date.now();
    this.#q.addSession.run(tokenHash(token), userId, now, now);
    return token;
  }

  /** Resolve a bearer token to a user, refreshing its last_seen. */
  resolveSession(token) {
    if (!token) return null;
    const row = this.#q.findSession.get(tokenHash(token));
    if (!row) return null;
    this.#q.touchSession.run(Date.now(), row.token_hash);
    return row;
  }

  endSession(token) {
    if (!token) return false;
    this.#q.dropSession.run(tokenHash(token));
    return true;
  }

  /** Drop sessions nobody has used in a while. Called on a timer. */
  expireSessions(maxIdleMs) {
    this.#q.expireSessions.run(Date.now() - maxIdleMs);
  }

  setRole(userId, role) {
    if (!ROLES.includes(role)) return { ok: false, error: 'invalid_role' };
    const user = this.#q.byId.get(userId);
    if (!user) return { ok: false, error: 'no_such_user' };
    // The last owner cannot demote themselves: a server with no owner has no
    // way back short of editing the database by hand.
    if (user.role === 'owner' && role !== 'owner' && this.#q.countOwners.get().n <= 1) {
      return { ok: false, error: 'last_owner' };
    }
    this.#q.setRole.run(role, userId);
    return { ok: true, user: this.#q.byId.get(userId) };
  }

  /**
   * Point this account at a stored file, or at nothing.
   *
   * Returns the hash it was pointing at before, because the caller owns the
   * reference counting: setting a new picture has to give the old file's
   * reference back or the uploads directory only ever grows.
   */
  setAvatar(userId, hash) {
    const user = this.#q.byId.get(userId);
    if (!user) return { ok: false, error: 'no_such_user' };
    this.#q.setAvatar.run(hash ?? null, userId);
    return { ok: true, previous: user.avatar_hash ?? null, user: this.#q.byId.get(userId) };
  }
}

/** Shape a user row for the wire. Never leaks password_hash. */
export const publicUser = (u) => (u ? {
  id: u.id,
  nickname: u.nickname,
  role: u.role,
  avatarHash: u.avatar_hash ?? null,
  createdAt: u.created_at,
} : null);

/**
 * The owner bootstrap string.
 *
 * On a server with no owner, generate one and print it. stdout is the only
 * place a Docker operator can see it -- there is no console, no email and no
 * config file they are already editing. The first account to present it becomes
 * owner and the string is consumed.
 *
 * Not regenerated once an owner exists, so restarting the server does not spray
 * fresh admin credentials into the logs.
 */
export function ensureOwnerToken(accounts, metaStore) {
  if (accounts.hasOwner) {
    metaStore.delete('owner_token');
    return null;
  }
  let token = metaStore.get('owner_token');
  if (!token) {
    token = randomBytes(16).toString('base64url');
    metaStore.set('owner_token', token);
  }
  console.log(`
  ┌──────────────────────────────────────────────────────────────┐
  │  This server has no owner yet.                               │
  │                                                              │
  │  Register in the client and paste this string into the       │
  │  "owner key" box to claim ownership:                         │
  │                                                              │
  │      ${token.padEnd(56)}│
  │                                                              │
  │  It works once. Anyone who reads this log can use it, so     │
  │  claim it now.                                               │
  └──────────────────────────────────────────────────────────────┘
`);
  return token;
}

/**
 * Consume the owner string. Timing-safe because it is a 128-bit secret sitting
 * behind an endpoint anyone past the door password can reach.
 */
export function claimOwner(accounts, metaStore, userId, offered) {
  const expected = metaStore.get('owner_token');
  if (!expected || !offered) return false;
  const a = createHash('sha256').update(String(offered)).digest();
  const b = createHash('sha256').update(expected).digest();
  if (!timingSafeEqual(a, b)) return false;
  accounts.setRole(userId, 'owner');
  metaStore.delete('owner_token');
  console.log('[accounts] owner claimed');
  return true;
}
