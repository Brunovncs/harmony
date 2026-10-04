// The database, and the only place that knows SQL schema.
//
// Harmony stored nothing on disk until this file existed. That was enforced,
// not incidental -- `read_only: true` in compose, `ProtectSystem=strict` in
// systemd -- so introducing a writable directory is a deployment change as much
// as a code change. The boot check below exists because the expected first
// experience of an un-upgraded deployment is EROFS, and an unexplained EROFS at
// 2am is an hour of someone's evening.
//
// `node:sqlite` rather than better-sqlite3: zero npm dependencies and zero
// native compilation. The Docker `deps` stage runs on $BUILDPLATFORM with no
// build toolchain, so a native SQLite module would silently produce binaries
// for the wrong architecture on every cross-build for a Pi.

import { DatabaseSync } from 'node:sqlite';
import { mkdirSync, unlinkSync, writeFileSync } from 'node:fs';
import { join } from 'node:path';

/**
 * Schema migrations, applied in order. The array index is the version the
 * migration produces: migrations[0] takes an empty database to user_version 1.
 *
 * Never edit a migration that has shipped -- add a new one. A deployed server
 * has already run the old text, and SQLite has no way to notice it changed.
 */
const MIGRATIONS = [
  // v1 -- identity.
  (db) => {
    db.exec(`
      CREATE TABLE users (
        id            INTEGER PRIMARY KEY,
        nickname      TEXT    NOT NULL UNIQUE,
        password_hash TEXT    NOT NULL,
        role          TEXT    NOT NULL DEFAULT 'member',
        avatar_hash   TEXT,
        created_at    INTEGER NOT NULL
      );

      CREATE TABLE sessions (
        token_hash TEXT    PRIMARY KEY,
        user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
        created_at INTEGER NOT NULL,
        last_seen  INTEGER NOT NULL
      );
      CREATE INDEX sessions_user ON sessions(user_id);

      CREATE TABLE server_meta (
        key   TEXT PRIMARY KEY,
        value TEXT NOT NULL
      );
    `);
  },

  // v2 -- channels.
  (db) => {
    db.exec(`
      CREATE TABLE channels (
        id            INTEGER PRIMARY KEY,
        kind          TEXT    NOT NULL,
        name          TEXT    NOT NULL,
        position      INTEGER NOT NULL,
        password_hash TEXT,
        created_at    INTEGER NOT NULL
      );
      CREATE INDEX channels_order ON channels(position, id);

      -- Entering a channel password grants lasting access, so it is asked once
      -- rather than on every join. Revoking is deleting the row.
      CREATE TABLE channel_grants (
        channel_id INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
        user_id    INTEGER NOT NULL REFERENCES users(id)    ON DELETE CASCADE,
        granted_at INTEGER NOT NULL,
        PRIMARY KEY (channel_id, user_id)
      );
    `);

    // A server with no channels has nowhere to go, and an owner who has to
    // create one before anything works is a worse first run than a lobby that
    // already exists. Both are deletable.
    const now = Date.now();
    const insert = db.prepare(
      'INSERT INTO channels (kind, name, position, created_at) VALUES (?, ?, ?, ?)',
    );
    insert.run('text', 'general', 0, now);
    insert.run('voice', 'voice', 1, now);
  },

  // v3 -- chat, uploads and search.
  (db) => {
    db.exec(`
      CREATE TABLE uploads (
        hash         TEXT    PRIMARY KEY,
        content_type TEXT    NOT NULL,
        bytes        INTEGER NOT NULL,
        created_at   INTEGER NOT NULL,
        refs         INTEGER NOT NULL DEFAULT 0
      );

      CREATE TABLE messages (
        id              INTEGER PRIMARY KEY,
        channel_id      INTEGER NOT NULL REFERENCES channels(id) ON DELETE CASCADE,
        user_id         INTEGER NOT NULL REFERENCES users(id),
        body            TEXT    NOT NULL DEFAULT '',
        attachment_hash TEXT    REFERENCES uploads(hash),
        media_type      TEXT,
        pinned          INTEGER NOT NULL DEFAULT 0,
        created_at      INTEGER NOT NULL
      );
      CREATE INDEX messages_channel ON messages(channel_id, id DESC);
      CREATE INDEX messages_pinned  ON messages(channel_id) WHERE pinned = 1;
    `);

    /*
     * Search.
     *
     * `trigram` rather than the default tokenizer because "fuzzy" in the
     * request means substring: typing "creen" should find "screenshot", which
     * a word tokenizer will never do. Verified behaviour, both parts of which
     * the query layer has to handle and neither of which is an error:
     *
     *   - a query shorter than 3 characters matches NOTHING, silently. Hence
     *     the mandatory LIKE fallback in chat.js.
     *   - raw user input passed to MATCH throws (a"b(c -> "unterminated
     *     string"), so every query is wrapped as one escaped phrase.
     *
     * columnsize=0 drops per-column length data we never rank on. A trigram
     * index costs roughly 2-3x the indexed text, which for a friends' server
     * is kilobytes.
     *
     * nickname is denormalised in so that "everything by bob" needs no join.
     * Kept in sync by chat.js's single write path rather than by triggers --
     * one place to look, nothing hidden.
     */
    db.exec(`
      CREATE VIRTUAL TABLE messages_fts USING fts5(
        body, nickname, media_type,
        tokenize = 'trigram',
        columnsize = 0
      );
    `);
  },

  // v4 -- the soundpad.
  (db) => {
    db.exec(`
      CREATE TABLE soundpad_clips (
        id          INTEGER PRIMARY KEY,
        name        TEXT    NOT NULL,
        file_hash   TEXT    NOT NULL REFERENCES uploads(hash),
        uploaded_by INTEGER REFERENCES users(id),
        position    INTEGER NOT NULL DEFAULT 0,
        created_at  INTEGER NOT NULL
      );
      CREATE INDEX soundpad_order ON soundpad_clips(position, id);
    `);
  },

  /*
   * v5 -- a display name.
   *
   * SEPARATE from the nickname, not a relaxing of it. The nickname is an
   * identity: it is folded, it is unique, it is what you log in with, it is
   * what a MediaMTX path is built from and what the auth hook parses. A
   * display name is a label, so it may be anything -- capitals, spaces,
   * accents, emoji -- and two people may pick the same one.
   *
   * NULL means "no preference", which renders as the nickname. Storing the
   * nickname into it instead would mean a later change of nickname quietly
   * failing to show up.
   */
  (db) => {
    db.exec('ALTER TABLE users ADD COLUMN display_name TEXT');
  },
];

export const SCHEMA_VERSION = MIGRATIONS.length;

/**
 * Create the data directory, or explain exactly what to do and stop.
 *
 * Exiting is deliberate. A server that silently runs without persistence would
 * accept registrations, look healthy, and lose every account on restart -- a
 * far worse failure than refusing to start.
 */
export function ensureDataDir(dataDir) {
  try {
    mkdirSync(dataDir, { recursive: true, mode: 0o750 });
    // mkdirSync succeeds on a directory that already exists without telling us
    // anything about whether it can be written to, and a read-only bind mount
    // is exactly that case. So actually write something.
    const probe = join(dataDir, '.writable');
    writeFileSync(probe, '');
    unlinkSync(probe);
    return dataDir;
  } catch (err) {
    const why = err.code === 'EROFS'
      ? 'that path is on a read-only filesystem'
      : err.code === 'EACCES'
        ? `this process (uid ${typeof process.getuid === 'function' ? process.getuid() : '?'}) cannot write there`
        : err.message;

    console.error(`
[harmony] FATAL: cannot use the data directory ${dataDir}
[harmony]        ${why}

Harmony now stores accounts and channels in SQLite, so it needs one writable
directory. Nothing else about the hardening has to change.

  docker compose:  add a volume and keep read_only: true

      services:
        harmony:
          volumes:
            - harmony-data:/var/lib/harmony
      volumes:
        harmony-data:

  docker run:      add  -v harmony-data:/var/lib/harmony   (keep --read-only)

  bare metal:      the systemd unit sets StateDirectory=harmony-server, which
                   creates and owns /var/lib/harmony-server for you

A BIND mount keeps the host's ownership rather than the image's, so a bind-
mounted directory must be chown'd to uid 1000 yourself. A NAMED volume inherits
the image's ownership and needs nothing.

Set HARMONY_DATA_DIR to put it somewhere else.
`);
    process.exit(1);
  }
}

/**
 * Open the database and bring it up to the current schema version.
 *
 * @param {{dataDir: string, file?: string}} opts
 * @returns {DatabaseSync}
 */
export function openDatabase({ dataDir, file = 'harmony.db' }) {
  const db = new DatabaseSync(join(dataDir, file));

  // WAL lets the MediaMTX auth hook read while a registration is writing.
  // Without it every write blocks every read, and the auth hook is on the path
  // of every publish and every read in the system.
  db.exec('PRAGMA journal_mode = WAL');

  // NORMAL rather than FULL: a crash can lose the last transaction, which for a
  // chat message on a friends' server is the right trade against an fsync per
  // write on an SD card.
  db.exec('PRAGMA synchronous = NORMAL');

  // Already the default in node:sqlite, unlike stock SQLite. Set anyway, so the
  // schema's ON DELETE CASCADE cannot quietly stop working if that changes.
  db.exec('PRAGMA foreign_keys = ON');

  migrate(db);
  return db;
}

/** Run any migrations this database has not seen yet. */
export function migrate(db) {
  const [{ user_version: current }] = db.prepare('PRAGMA user_version').all();

  if (current > MIGRATIONS.length) {
    throw new Error(
      `database is at schema v${current} but this server only knows v${MIGRATIONS.length}. `
      + 'Downgrading is not supported -- restore a backup or run the newer server.',
    );
  }

  for (let v = current; v < MIGRATIONS.length; v += 1) {
    // Each migration is its own transaction: a failure half way leaves the
    // version untouched, so the next boot retries cleanly rather than starting
    // from a half-applied schema.
    db.exec('BEGIN');
    try {
      MIGRATIONS[v](db);
      // Not a bound parameter: PRAGMA does not accept one. v is a loop index
      // over a literal array, so there is nothing user-controlled here.
      db.exec(`PRAGMA user_version = ${v + 1}`);
      db.exec('COMMIT');
      console.log(`[db] migrated to schema v${v + 1}`);
    } catch (err) {
      db.exec('ROLLBACK');
      throw new Error(`migration to v${v + 1} failed: ${err.message}`);
    }
  }
}

/** Small key/value store for things with exactly one row. */
export function meta(db) {
  const get = db.prepare('SELECT value FROM server_meta WHERE key = ?');
  const set = db.prepare(
    'INSERT INTO server_meta (key, value) VALUES (?, ?) '
    + 'ON CONFLICT(key) DO UPDATE SET value = excluded.value',
  );
  const del = db.prepare('DELETE FROM server_meta WHERE key = ?');
  return {
    get: (key) => get.get(key)?.value ?? null,
    set: (key, value) => { set.run(key, String(value)); },
    delete: (key) => { del.run(key); },
  };
}
