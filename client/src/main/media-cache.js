// Local cache for server-hosted files: avatars, chat attachments, soundpad
// clips.
//
// This exists for two reasons that happen to want the same thing.
//
// The CSP one: index.html sets `default-src 'self'` with `img-src 'self' data:`
// and `media-src 'self' blob:`. A server-hosted <img src="http://pi:8080/...">
// is blocked outright, and loosening the CSP to allow it would undo the reason
// the renderer has no network access at all. Serving the file from our own
// `harmony://` scheme makes it same-origin and the CSP never has to change.
//
// The product one: "all files stored locally, no CDN". Which is this.
//
// The route is `harmony://app/media/<hash>` and NOT `harmony://media/<hash>`.
// The scheme is registered `{standard: true}`, so the host is part of the
// origin -- `harmony://media` would be a DIFFERENT origin that `'self'` does
// not cover, and every image would be blocked by the CSP it was meant to
// satisfy.

const { createHash } = require('node:crypto');
const fs = require('node:fs');
const fsp = require('node:fs/promises');
const path = require('node:path');

const HASH_RE = /^[0-9a-f]{64}$/;

/** Written back at most this often, however many files arrive. */
const INDEX_DEBOUNCE_MS = 2000;

class MediaCache {
  #dir;
  #indexPath;
  /** @type {Map<string, {contentType: string, bytes: number, usedAt: number}>} */
  #index = new Map();
  #indexTimer = null;
  /** Single-flight: one download per hash, however many callers ask. */
  #inflight = new Map();
  #budgetBytes;
  #keep = new Set();

  constructor({ dir, budgetBytes = 512 * 1024 * 1024 }) {
    this.#dir = dir;
    this.#indexPath = path.join(dir, 'index.json');
    this.#budgetBytes = budgetBytes;
    fs.mkdirSync(dir, { recursive: true });
    this.#loadIndex();
  }

  #loadIndex() {
    try {
      const raw = JSON.parse(fs.readFileSync(this.#indexPath, 'utf8'));
      for (const [hash, meta] of Object.entries(raw)) this.#index.set(hash, meta);
    } catch {
      // No index yet, or a corrupt one. Either way the files on disk are still
      // valid -- they are content-addressed -- so this only loses MIME types,
      // which the next fetch restores.
    }
  }

  #saveIndexSoon() {
    if (this.#indexTimer) return;
    this.#indexTimer = setTimeout(() => {
      this.#indexTimer = null;
      try {
        fs.writeFileSync(this.#indexPath, JSON.stringify(Object.fromEntries(this.#index)));
      } catch (err) {
        console.warn('[media] could not write the cache index:', err.message);
      }
    }, INDEX_DEBOUNCE_MS);
    this.#indexTimer.unref?.();
  }

  /** Sharded two levels, so no directory holds tens of thousands of entries. */
  pathFor(hash) {
    return path.join(this.#dir, hash.slice(0, 2), hash.slice(2, 4), hash);
  }

  has(hash) {
    return this.#index.has(hash) && fs.existsSync(this.pathFor(hash));
  }

  /**
   * Things that must never be evicted: avatars, soundpad clips, pinned
   * attachments. Replaced wholesale rather than added to, so a clip that is
   * deleted server-side stops being protected here too.
   */
  setKeepSet(hashes) {
    this.#keep = new Set(hashes);
  }

  /**
   * Return a local path for this hash, downloading it if necessary.
   *
   * @param {string} hash
   * @param {(hash: string) => Promise<{buffer: Buffer, contentType: string}>} fetcher
   */
  async get(hash, fetcher) {
    if (!HASH_RE.test(hash)) throw new Error('not a content hash');

    const file = this.pathFor(hash);
    if (this.has(hash)) {
      const meta = this.#index.get(hash);
      meta.usedAt = Date.now();
      this.#saveIndexSoon();
      return { path: file, contentType: meta.contentType };
    }

    // Single-flight. A chat pane rendering twenty copies of the same emoji
    // would otherwise start twenty identical downloads.
    if (this.#inflight.has(hash)) return this.#inflight.get(hash);

    const job = this.#download(hash, file, fetcher).finally(() => this.#inflight.delete(hash));
    this.#inflight.set(hash, job);
    return job;
  }

  async #download(hash, file, fetcher) {
    const { buffer, contentType } = await fetcher(hash);

    // The name promises the content. Check it, or a corrupted transfer gets
    // cached forever under a hash it does not match.
    const actual = createHash('sha256').update(buffer).digest('hex');
    if (actual !== hash) throw new Error(`downloaded file does not match its hash (${hash})`);

    await fsp.mkdir(path.dirname(file), { recursive: true });
    // Write to a temporary name and rename into place: rename is atomic, so a
    // reader can never see a half-written file at the real path.
    const temp = `${file}.${process.pid}.part`;
    await fsp.writeFile(temp, buffer);
    await fsp.rename(temp, file);

    this.#index.set(hash, { contentType, bytes: buffer.length, usedAt: Date.now() });
    this.#saveIndexSoon();
    this.#evictIfNeeded();
    return { path: file, contentType };
  }

  /** Least recently used first, never touching the keep-set. */
  #evictIfNeeded() {
    let total = 0;
    for (const meta of this.#index.values()) total += meta.bytes;
    if (total <= this.#budgetBytes) return;

    const candidates = [...this.#index.entries()]
      .filter(([hash]) => !this.#keep.has(hash))
      .sort((a, b) => a[1].usedAt - b[1].usedAt);

    for (const [hash, meta] of candidates) {
      if (total <= this.#budgetBytes) break;
      try {
        fs.unlinkSync(this.pathFor(hash));
      } catch { /* already gone */ }
      this.#index.delete(hash);
      total -= meta.bytes;
    }
    this.#saveIndexSoon();
  }

  stats() {
    let bytes = 0;
    for (const meta of this.#index.values()) bytes += meta.bytes;
    return { files: this.#index.size, bytes, budgetBytes: this.#budgetBytes };
  }

  clear() {
    for (const hash of this.#index.keys()) {
      try {
        fs.unlinkSync(this.pathFor(hash));
      } catch { /* already gone */ }
    }
    this.#index.clear();
    this.#saveIndexSoon();
  }
}

module.exports = { MediaCache, HASH_RE };
