/*
 * Media keys: a way to load an upload without headers.
 *
 * The native desktop client fetches every upload itself, so it can send the
 * door password and the session token as headers. A browser or the Android
 * app cannot: an <img>, <video> or <audio> tag sends no headers at all. So
 * those clients ask for a media key once (GET /api/media-key, behind the
 * door and a login) and put it in the URL instead:
 * GET /api/media/<hash>?k=<key>.
 *
 * A key is per account and per week, and accepted for the week it was made
 * and the one before. Stable for a week, because it is part of every media
 * URL and a URL that changed on each request would defeat the browser's
 * cache; short-lived, because a URL ends up in places a header does not
 * (history, logs, a copied link).
 *
 * The signing secret is the channel-token secret, and the "m1" prefix keeps
 * the two kinds of token from ever being mistaken for each other.
 *
 * A key opens exactly what GET /api/uploads opens and nothing more: a private
 * conversation's sealed files are refused there whoever asks, and so here.
 */

import { createHmac, timingSafeEqual } from 'node:crypto';

const WEEK_MS = 7 * 24 * 60 * 60 * 1000;

const sign = (secret, body) =>
  createHmac('sha256', secret).update(body).digest('base64url').slice(0, 22);

const weekOf = (now) => Math.floor(now / WEEK_MS);

/** A key for this account, this week. */
export function mintMediaKey(secret, userId, now = Date.now()) {
  const body = `m1.${Number(userId).toString(36)}.${weekOf(now).toString(36)}`;
  return `${body}.${sign(secret, body)}`;
}

/** The account id a key was made for, or null if it is not a current key. */
export function verifyMediaKey(secret, key, now = Date.now()) {
  const parts = String(key ?? '').split('.');
  if (parts.length !== 4 || parts[0] !== 'm1') return null;
  const [, uid36, week36, sig] = parts;

  const expected = Buffer.from(sign(secret, `m1.${uid36}.${week36}`));
  const given = Buffer.from(sig);
  if (given.length !== expected.length || !timingSafeEqual(given, expected)) return null;

  const week = parseInt(week36, 36);
  const current = weekOf(now);
  if (!(week === current || week === current - 1)) return null;

  const userId = parseInt(uid36, 36);
  return Number.isSafeInteger(userId) && userId > 0 ? userId : null;
}

/**
 * The account a key says it was made for, NOT checked.
 *
 * Only for finding the account whose secret the key has to be verified
 * with, when the secret is per account; never an answer on its own.
 */
export function claimedMediaKeyAccount(key) {
  const parts = String(key ?? '').split('.');
  if (parts.length !== 4 || parts[0] !== 'm1') return null;
  const userId = parseInt(parts[1], 36);
  return Number.isSafeInteger(userId) && userId > 0 ? userId : null;
}
