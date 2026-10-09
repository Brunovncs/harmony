// Calls between the two people in a private conversation.
//
// Two layers, kept apart for the same reason Channels and VoiceRooms are:
//
//   the call   -- ringing, answered, over. A small state machine per
//                 conversation, live only, never written to disk: a restart
//                 hangs up every call, which is what a restart is.
//   the media  -- who holds which slot, muted or not, publishing what. That
//                 is exactly what a voice channel already tracks, so it is a
//                 VoiceRooms of its own, keyed by conversation id instead of
//                 channel id. Same slots, same rosters, same mute rules.
//
// The media itself is end-to-end encrypted by the clients, frame by frame,
// under a key the caller makes and seals to the conversation. The server
// passes that sealed key along and can no more open it than a message. What
// it does know -- that a call happened, when, for how long -- it writes into
// the conversation as a plain line, because that is all it is.

import { VoiceRooms } from './channels.js';

/** How long a call rings before it counts as missed. */
export const RING_MS = 45_000;

/**
 * How long a call under way waits for somebody whose connection dropped.
 *
 * A Wi-Fi hiccup closes the socket; the client is back within seconds and
 * takes the same slot. Ending the call on the spot would turn every blip
 * into "call ended" for both people, so it waits this long first.
 */
export const GRACE_MS = 20_000;

export class Calls {
  /** @type {Map<number, object>} by conversation id */
  #calls = new Map();
  /** @type {Map<number, NodeJS.Timeout>} by user id: who dropped, and when they run out */
  #grace = new Map();
  #notify;
  #record;
  #ringMs;
  #graceMs;
  #now;

  /** Media slots for calls, exactly as a voice channel keeps them. */
  voice = new VoiceRooms();

  /**
   * @param {object} deps
   * @param {(userIds: number[], payload: object) => void} deps.notify
   *   send to every socket of each of these people
   * @param {(conversationId: number, callerId: number, meta: object) => void} deps.record
   *   write the call's line into the conversation (and tell both people)
   */
  constructor({ notify, record, ringMs = RING_MS, graceMs = GRACE_MS, now = Date.now }) {
    this.#notify = notify;
    this.#record = record;
    this.#ringMs = ringMs;
    this.#graceMs = graceMs;
    this.#now = now;
  }

  get(conversationId) {
    return this.#calls.get(Number(conversationId)) ?? null;
  }

  /** Every call this person is part of: ringing them, ringing from them, or under way. */
  forUser(userId) {
    return [...this.#calls.values()].filter((c) => c.callerId === userId || c.calleeId === userId);
  }

  static isParty(call, userId) {
    return call.callerId === userId || call.calleeId === userId;
  }

  /**
   * Ring the other person.
   *
   * Two people calling each other at the same moment is one call, not two:
   * the second ring answers the first. Ringing somebody who has no socket
   * open is refused straight away -- there is nobody to hear it -- but still
   * leaves "missed call" in the conversation, so they see it when they are
   * back.
   */
  start({ conversationId, callerId, calleeId, sealedKey, senderKey, recipientKey, calleeOnline }) {
    const existing = this.get(conversationId);
    if (existing) {
      if (existing.state === 'ringing' && existing.calleeId === callerId) {
        return this.answer(conversationId, callerId);
      }
      return { ok: true, call: existing };
    }
    if (!calleeOnline) {
      this.#record(conversationId, callerId, { outcome: 'missed' });
      return { ok: false, error: 'peer_offline' };
    }

    const call = {
      conversationId: Number(conversationId),
      callerId,
      calleeId,
      state: 'ringing',
      startedAt: this.#now(),
      answeredAt: null,
      sealedKey,
      senderKey: Number(senderKey),
      recipientKey: Number(recipientKey),
      timer: null,
    };
    call.timer = setTimeout(() => this.#end(call.conversationId, 'missed'), this.#ringMs);
    call.timer.unref?.();
    this.#calls.set(call.conversationId, call);
    this.#announce(call);
    return { ok: true, call };
  }

  /** The person being rung picks up. */
  answer(conversationId, userId) {
    const call = this.get(conversationId);
    if (!call || call.calleeId !== userId) return { ok: false, error: 'no_such_call' };
    if (call.state !== 'ringing') return { ok: true, call };
    clearTimeout(call.timer);
    call.state = 'active';
    call.answeredAt = this.#now();
    this.#announce(call);
    return { ok: true, call };
  }

  /**
   * Either person ends it. Before it was answered that is the caller giving
   * up or the callee saying no; after, it is a hang-up, and a one-to-one call
   * with one person left in it is over for both.
   */
  hangUp(conversationId, userId) {
    const call = this.get(conversationId);
    if (!call || !Calls.isParty(call, userId)) return { ok: false, error: 'no_such_call' };
    let outcome = 'ended';
    if (call.state === 'ringing') outcome = userId === call.calleeId ? 'declined' : 'missed';
    this.#end(call.conversationId, outcome);
    return { ok: true };
  }

  /** Whatever call this person is in is over, now. */
  leaveUser(userId) {
    this.#forgive(userId);
    for (const call of this.forUser(userId)) this.hangUp(call.conversationId, userId);
  }

  /**
   * Somebody's last socket closed. A call still ringing ends at once -- a
   * caller who vanished is not calling any more, and a ring nobody can hear
   * is a missed call -- but one under way waits GRACE_MS for them to come
   * back, keeping their slot so the media resumes where it was.
   */
  disconnected(userId) {
    const calls = this.forUser(userId);
    for (const call of calls.filter((c) => c.state === 'ringing')) this.hangUp(call.conversationId, userId);
    if (!calls.some((c) => c.state === 'active') || this.#grace.has(userId)) return;
    const timer = setTimeout(() => this.leaveUser(userId), this.#graceMs);
    timer.unref?.();
    this.#grace.set(userId, timer);
  }

  /** They are back in time: the call goes on. */
  reconnected(userId) {
    this.#forgive(userId);
  }

  #forgive(userId) {
    clearTimeout(this.#grace.get(userId));
    this.#grace.delete(userId);
  }

  #end(conversationId, outcome) {
    const call = this.#calls.get(conversationId);
    if (!call) return;
    clearTimeout(call.timer);
    this.#calls.delete(conversationId);
    for (const id of [call.callerId, call.calleeId]) this.voice.leave(conversationId, id);

    const meta = { outcome: call.answeredAt ? 'ended' : outcome };
    if (call.answeredAt) meta.durationMs = this.#now() - call.answeredAt;
    this.#record(conversationId, call.callerId, meta);
    this.#notify([call.callerId, call.calleeId], {
      type: 'call:state',
      call: { ...publicCall(call), state: 'ended', outcome: meta.outcome },
    });
    this.#notify([call.callerId, call.calleeId], {
      type: 'call:roster', conversationId, roster: [],
    });
  }

  #announce(call) {
    this.#notify([call.callerId, call.calleeId], { type: 'call:state', call: publicCall(call) });
  }

  /** Push the media roster of a call to its two people, and nobody else. */
  broadcastRoster(conversationId) {
    const call = this.get(conversationId);
    if (!call) return;
    this.#notify([call.callerId, call.calleeId], {
      type: 'call:roster',
      conversationId: call.conversationId,
      roster: this.voice.roster(call.conversationId),
    });
  }

  /** Stop every ring timer, for shutdown and tests. */
  close() {
    for (const call of this.#calls.values()) clearTimeout(call.timer);
    for (const timer of this.#grace.values()) clearTimeout(timer);
    this.#calls.clear();
    this.#grace.clear();
  }
}

/**
 * A call as its two people see it. The sealed key rides along on every
 * state push, so a client that reconnects mid-call -- or a second device
 * that answers -- has what it needs to hear the other side.
 */
export const publicCall = (c) => (c ? {
  conversationId: c.conversationId,
  callerId: c.callerId,
  calleeId: c.calleeId,
  state: c.state,
  startedAt: c.startedAt,
  answeredAt: c.answeredAt,
  sealedKey: c.sealedKey,
  senderKey: c.senderKey,
  recipientKey: c.recipientKey,
} : null);
