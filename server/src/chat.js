// Text and media channels: messages, pins, attachments and search.
//
// Uploads are the first thing Harmony stores that grows without bound, and the
// README is explicit that "writing to disk is the one thing that would give the
// server a cost that scales". So the limits here are not defensive padding,
// they are the feature: a per-file cap, a content-type allowlist, and a total
// quota checked before anything is written. The reference host is a Pi with an
// SD card.

import { createHash } from 'node:crypto';
import { mkdirSync, writeFileSync, renameSync, unlinkSync, statSync } from 'node:fs';
import { join } from 'node:path';

/**
 * What may be uploaded, and what it is called in search.
 *
 * An allowlist rather than a blocklist, and the client's claimed type is not
 * trusted for anything but this check -- the file is served back with the type
 * recorded here, so an .html smuggled in as image/png would still be served as
 * an image and never execute.
 */
const ALLOWED_TYPES = new Map([
  ['image/png', 'image'],
  ['image/jpeg', 'image'],
  ['image/gif', 'image'],
  ['image/webp', 'image'],
  ['video/mp4', 'video'],
  ['video/webm', 'video'],
  ['audio/mpeg', 'audio'],
  ['audio/ogg', 'audio'],
  ['audio/wav', 'audio'],
  ['audio/webm', 'audio'],
  ['application/pdf', 'file'],
  ['text/plain', 'file'],
]);

export const mediaTypeOf = (contentType) => ALLOWED_TYPES.get(String(contentType).toLowerCase()) ?? null;
export const allowedTypes = () => [...ALLOWED_TYPES.keys()];

/** Hard ceiling per file, regardless of the total quota. */
export const MAX_UPLOAD_BYTES = 25 * 1024 * 1024;

/** Messages returned by one history request. */
const PAGE_SIZE = 50;

/**
 * Turn arbitrary text into one FTS5 phrase.
 *
 * Verified necessary: `a"b(c` passed to MATCH raw throws "unterminated
 * string", and `*`, `^he` and `NOT bob` are all parsed as query syntax rather
 * than as the text somebody typed. Wrapping in quotes and doubling any
 * internal quote makes every one of them a literal, non-throwing search.
 */
export const ftsPhrase = (query) => `"${String(query).replaceAll('"', '""')}"`;

export class Chat {
  #db;
  #dir;
  #maxBytes;
  #q;

  constructor(db, { dataDir, maxDiskBytes }) {
    this.#db = db;
    this.#dir = join(dataDir, 'uploads');
    this.#maxBytes = maxDiskBytes;
    mkdirSync(this.#dir, { recursive: true });

    this.#q = {
      insertMessage: db.prepare(
        'INSERT INTO messages (channel_id, user_id, body, attachment_hash, media_type, created_at) '
        + 'VALUES (?, ?, ?, ?, ?, ?)',
      ),
      insertFts: db.prepare(
        'INSERT INTO messages_fts (rowid, body, nickname, media_type) VALUES (?, ?, ?, ?)',
      ),
      deleteFts: db.prepare('DELETE FROM messages_fts WHERE rowid = ?'),
      deleteMessage: db.prepare('DELETE FROM messages WHERE id = ?'),
      messageById: db.prepare(`
        SELECT m.*, u.nickname FROM messages m JOIN users u ON u.id = m.user_id
        WHERE m.id = ?
      `),
      history: db.prepare(`
        SELECT m.*, u.nickname FROM messages m JOIN users u ON u.id = m.user_id
        WHERE m.channel_id = ? AND m.id < ?
        ORDER BY m.id DESC LIMIT ?
      `),
      pinned: db.prepare(`
        SELECT m.*, u.nickname FROM messages m JOIN users u ON u.id = m.user_id
        WHERE m.channel_id = ? AND m.pinned = 1
        ORDER BY m.id DESC
      `),
      setPinned: db.prepare('UPDATE messages SET pinned = ? WHERE id = ?'),

      searchFts: db.prepare(`
        SELECT m.*, u.nickname FROM messages_fts f
        JOIN messages m ON m.id = f.rowid
        JOIN users u ON u.id = m.user_id
        WHERE messages_fts MATCH ? AND m.channel_id = ?
        ORDER BY m.id DESC LIMIT ?
      `),
      searchLike: db.prepare(`
        SELECT m.*, u.nickname FROM messages m JOIN users u ON u.id = m.user_id
        WHERE m.channel_id = ?
          AND (m.body LIKE ? OR u.nickname LIKE ? OR IFNULL(m.media_type, '') LIKE ?)
        ORDER BY m.id DESC LIMIT ?
      `),

      upload: db.prepare('SELECT * FROM uploads WHERE hash = ?'),
      insertUpload: db.prepare(
        'INSERT INTO uploads (hash, content_type, bytes, created_at, refs) VALUES (?, ?, ?, ?, 0) '
        + 'ON CONFLICT(hash) DO NOTHING',
      ),
      addRef: db.prepare('UPDATE uploads SET refs = refs + 1 WHERE hash = ?'),
      dropRef: db.prepare('UPDATE uploads SET refs = MAX(0, refs - 1) WHERE hash = ?'),
      totalBytes: db.prepare('SELECT COALESCE(SUM(bytes), 0) AS n FROM uploads'),
      orphans: db.prepare('SELECT hash FROM uploads WHERE refs <= 0 ORDER BY created_at LIMIT 200'),
      deleteUpload: db.prepare('DELETE FROM uploads WHERE hash = ?'),
    };
  }

  // ----------------------------------------------------------------- files

  get usedBytes() {
    return this.#q.totalBytes.get().n;
  }

  get quotaBytes() {
    return this.#maxBytes;
  }

  /** Sharded, so one directory never holds tens of thousands of entries. */
  pathFor(hash) {
    return join(this.#dir, hash.slice(0, 2), hash.slice(2, 4), hash);
  }

  /**
   * Store a file, keyed by the hash of its contents.
   *
   * Content addressing means uploading the same image twice costs nothing the
   * second time, and that a file's name can never collide with or escape into
   * another's -- the hash is the only thing that reaches the filesystem.
   *
   * @returns {{ok: true, upload: object} | {ok: false, error: string}}
   */
  store(buffer, contentType) {
    const mediaType = mediaTypeOf(contentType);
    if (!mediaType) return { ok: false, error: 'type_not_allowed' };
    if (!buffer?.length) return { ok: false, error: 'empty_file' };
    if (buffer.length > MAX_UPLOAD_BYTES) return { ok: false, error: 'file_too_large' };

    const hash = createHash('sha256').update(buffer).digest('hex');
    const existing = this.#q.upload.get(hash);
    if (existing) return { ok: true, upload: existing, deduplicated: true };

    // Checked BEFORE writing, and it is not only about uploads: once the
    // filesystem is full every SQLite write throws too, so an unbounded
    // uploads directory does not degrade the server, it stops it.
    if (this.usedBytes + buffer.length > this.#maxBytes) {
      this.#evictOrphans(buffer.length);
      if (this.usedBytes + buffer.length > this.#maxBytes) {
        return { ok: false, error: 'server_full' };
      }
    }

    const target = this.pathFor(hash);
    mkdirSync(join(target, '..'), { recursive: true });
    // Write then rename, so a crash mid-write cannot leave a truncated file
    // sitting at the name its hash promises.
    const temp = `${target}.part`;
    writeFileSync(temp, buffer);
    renameSync(temp, target);

    this.#q.insertUpload.run(hash, String(contentType).toLowerCase(), buffer.length, Date.now());
    return { ok: true, upload: this.#q.upload.get(hash), deduplicated: false };
  }

  /** Delete unreferenced files until there is room, oldest first. */
  #evictOrphans(needed) {
    for (const { hash } of this.#q.orphans.all()) {
      try {
        unlinkSync(this.pathFor(hash));
      } catch { /* already gone; drop the row anyway */ }
      this.#q.deleteUpload.run(hash);
      if (this.usedBytes + needed <= this.#maxBytes) return;
    }
  }

  fileInfo(hash) {
    const row = this.#q.upload.get(hash);
    if (!row) return null;
    try {
      statSync(this.pathFor(hash));
    } catch {
      return null; // row without a file: treat as missing rather than 500
    }
    return { ...row, path: this.pathFor(hash) };
  }

  // -------------------------------------------------------------- messages

  /**
   * Post a message. The FTS row is written here and nowhere else.
   *
   * One write path rather than triggers: a trigger that silently stops firing
   * produces a search index that is quietly wrong, which is far harder to
   * notice than a function that throws.
   */
  post({ channelId, user, body = '', attachmentHash = null }) {
    const text = String(body ?? '').slice(0, 4000);
    if (!text.trim() && !attachmentHash) return { ok: false, error: 'empty_message' };

    let mediaType = null;
    if (attachmentHash) {
      const upload = this.#q.upload.get(attachmentHash);
      if (!upload) return { ok: false, error: 'no_such_upload' };
      mediaType = mediaTypeOf(upload.content_type);
    }

    this.#db.exec('BEGIN');
    try {
      const info = this.#q.insertMessage.run(
        channelId, user.id, text, attachmentHash, mediaType, Date.now(),
      );
      const id = Number(info.lastInsertRowid);
      this.#q.insertFts.run(id, text, user.nickname, mediaType ?? '');
      if (attachmentHash) this.#q.addRef.run(attachmentHash);
      this.#db.exec('COMMIT');
      return { ok: true, message: this.#q.messageById.get(id) };
    } catch (err) {
      this.#db.exec('ROLLBACK');
      throw err;
    }
  }

  get(id) {
    return this.#q.messageById.get(id) ?? null;
  }

  remove(id) {
    const message = this.#q.messageById.get(id);
    if (!message) return { ok: false, error: 'no_such_message' };

    this.#db.exec('BEGIN');
    try {
      this.#q.deleteFts.run(id);
      this.#q.deleteMessage.run(id);
      if (message.attachment_hash) this.#q.dropRef.run(message.attachment_hash);
      this.#db.exec('COMMIT');
    } catch (err) {
      this.#db.exec('ROLLBACK');
      throw err;
    }
    return { ok: true, message };
  }

  history(channelId, { before = Number.MAX_SAFE_INTEGER, limit = PAGE_SIZE } = {}) {
    return this.#q.history
      .all(channelId, before, Math.min(limit, PAGE_SIZE))
      .reverse(); // oldest first, which is the order a chat pane renders
  }

  pinned(channelId) {
    return this.#q.pinned.all(channelId);
  }

  setPinned(id, pinned) {
    const message = this.#q.messageById.get(id);
    if (!message) return { ok: false, error: 'no_such_message' };
    this.#q.setPinned.run(pinned ? 1 : 0, id);
    return { ok: true, message: this.#q.messageById.get(id) };
  }

  /**
   * Search one channel by text, author or media type.
   *
   * Returns the mode it used, because the two differ in a way a user can
   * notice: trigram cannot match fewer than 3 characters at all, so short
   * queries silently fall back to LIKE. Surfacing it makes that testable
   * rather than mysterious.
   */
  search(channelId, query, { limit = PAGE_SIZE } = {}) {
    const text = String(query ?? '').trim();
    if (!text) return { mode: 'none', results: [] };

    if (text.length < 3) {
      const like = `%${text.replaceAll('%', '\\%').replaceAll('_', '\\_')}%`;
      return {
        mode: 'like',
        results: this.#q.searchLike.all(channelId, like, like, like, Math.min(limit, PAGE_SIZE)),
      };
    }

    return {
      mode: 'fts',
      results: this.#q.searchFts.all(ftsPhrase(text), channelId, Math.min(limit, PAGE_SIZE)),
    };
  }
}

/**
 * Soundpad clips.
 *
 * Only the registry lives here -- the audio itself is an ordinary upload, so
 * it inherits the allowlist, the per-file cap and the disk quota for free.
 *
 * Playback is NOT mixed into anyone's microphone. The server broadcasts "play
 * clip X" and every client plays its own cached copy, for four reasons: the
 * mixed alternative re-encodes music through a 32 kbps speech codec, the
 * clicker's echo cancellation ducks it, only people subscribed to that one
 * person would hear it, and a force-muted user's soundpad would go silent too.
 */
export class Soundpad {
  #db;
  #q;

  constructor(db) {
    this.#db = db;
    this.#q = {
      all: db.prepare(`
        SELECT c.*, u.nickname AS uploader FROM soundpad_clips c
        LEFT JOIN users u ON u.id = c.uploaded_by
        ORDER BY c.position, c.id
      `),
      // Joined, so a freshly created clip has the same shape as a listed one.
      // Without the join the 201 response says uploader: null and the list
      // says the nickname, which is the kind of inconsistency a client ends up
      // working around rather than reporting.
      byId: db.prepare(`
        SELECT c.*, u.nickname AS uploader FROM soundpad_clips c
        LEFT JOIN users u ON u.id = c.uploaded_by
        WHERE c.id = ?
      `),
      insert: db.prepare(
        'INSERT INTO soundpad_clips (name, file_hash, uploaded_by, position, created_at) '
        + 'VALUES (?, ?, ?, ?, ?)',
      ),
      nextPosition: db.prepare('SELECT COALESCE(MAX(position), -1) + 1 AS p FROM soundpad_clips'),
      remove: db.prepare('DELETE FROM soundpad_clips WHERE id = ?'),
      addRef: db.prepare('UPDATE uploads SET refs = refs + 1 WHERE hash = ?'),
      dropRef: db.prepare('UPDATE uploads SET refs = MAX(0, refs - 1) WHERE hash = ?'),
      upload: db.prepare('SELECT * FROM uploads WHERE hash = ?'),
    };
  }

  list() {
    return this.#q.all.all();
  }

  add({ name, fileHash, userId }) {
    const clean = String(name ?? '').trim().slice(0, 32);
    if (!clean) return { ok: false, error: 'invalid_name' };

    const upload = this.#q.upload.get(fileHash);
    if (!upload) return { ok: false, error: 'no_such_upload' };
    if (mediaTypeOf(upload.content_type) !== 'audio') {
      return { ok: false, error: 'not_audio' };
    }

    this.#db.exec('BEGIN');
    try {
      const info = this.#q.insert.run(
        clean, fileHash, userId, this.#q.nextPosition.get().p, Date.now(),
      );
      // Referenced, so cache eviction and orphan cleanup both leave it alone.
      this.#q.addRef.run(fileHash);
      this.#db.exec('COMMIT');
      return { ok: true, clip: this.#q.byId.get(Number(info.lastInsertRowid)) };
    } catch (err) {
      this.#db.exec('ROLLBACK');
      throw err;
    }
  }

  remove(id) {
    const clip = this.#q.byId.get(id);
    if (!clip) return { ok: false, error: 'no_such_clip' };
    this.#db.exec('BEGIN');
    try {
      this.#q.remove.run(id);
      this.#q.dropRef.run(clip.file_hash);
      this.#db.exec('COMMIT');
    } catch (err) {
      this.#db.exec('ROLLBACK');
      throw err;
    }
    return { ok: true, clip };
  }

  get(id) {
    return this.#q.byId.get(id) ?? null;
  }
}

export const publicClip = (c) => (c ? {
  id: c.id,
  name: c.name,
  hash: c.file_hash,
  uploader: c.uploader ?? null,
  createdAt: c.created_at,
} : null);

export const publicMessage = (m) => (m ? {
  id: m.id,
  channelId: m.channel_id,
  userId: m.user_id,
  nickname: m.nickname,
  body: m.body,
  attachmentHash: m.attachment_hash ?? null,
  mediaType: m.media_type ?? null,
  pinned: Boolean(m.pinned),
  createdAt: m.created_at,
} : null);
