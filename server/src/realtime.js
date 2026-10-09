// The push side: one WebSocket per client, mounted on the existing HTTP server.
//
// Harmony polled for everything until this existed. Polling is fine for "who is
// live" at 3-second granularity; it is not fine for "who just spoke", "you have
// been moved" or "somebody muted you" -- the last of which has a hard
// requirement behind it. The Phase 0 spike measured that a force-muted client's
// RTCPeerConnection stays `connected` for about nine seconds after its session
// is killed, so a roster push is the ONLY thing that can tell that person their
// microphone stopped working. Connection state will not, in time to matter.
//
// Every message is {type, ...}. Requests carry an `rid` and get exactly one
// reply with the same `rid`; everything else is a server-initiated event.

import { WebSocketServer } from 'ws';

import { publicChannel, publicGroup, VOICE_HARD_CAP } from './channels.js';
import { publicUser } from './accounts.js';
import { Calls, publicCall } from './calls.js';
import { DirectMessages } from './dms.js';

/** A socket that has not answered a ping in this long is assumed dead. */
const HEARTBEAT_MS = 15_000;

export class Realtime {
  #wss;
  #deps;
  /** @type {Map<import('ws').WebSocket, {user: object, alive: boolean, host: string}>} */
  #clients = new Map();
  #timer = null;

  /**
   * @param {object} deps
   * @param {import('node:http').Server} deps.server
   * @param {import('./accounts.js').Accounts} deps.accounts
   * @param {import('./channels.js').Channels} deps.channels
   * @param {import('./channels.js').VoiceRooms} deps.voice
   * @param {(channelId: number, userId: number, mid: number, host: string) => object} deps.issueTokens
   * @param {(conversationId: number, userId: number, mid: number, host: string) => object} deps.issueCallTokens
   * @param {import('./dms.js').DirectMessages} deps.dms
   * @param {import('./calls.js').Calls} deps.calls
   * @param {(channelId: number, mid: number) => Promise<void>} deps.kickMember
   */
  constructor(deps) {
    this.#deps = deps;

    // noServer: Express owns the HTTP server, and we only want to take over
    // sockets asking for /ws. Letting ws attach to the server directly would
    // have it answer upgrades on every path.
    this.#wss = new WebSocketServer({ noServer: true });

    deps.server.on('upgrade', (req, socket, head) => {
      const { pathname } = new URL(req.url, 'http://localhost');
      if (pathname !== '/ws') {
        socket.destroy();
        return;
      }
      this.#wss.handleUpgrade(req, socket, head, (ws) => this.#onConnection(ws, req.headers.host));
    });

    this.#timer = setInterval(() => this.#sweep(), HEARTBEAT_MS);
    this.#timer.unref();
  }

  // -------------------------------------------------------------------------

  /** `host` is the Host header the socket came in with: see issueTokens. */
  #onConnection(ws, host) {
    // Unauthenticated until the first frame. Node's global WebSocket client --
    // which is what the Electron main process uses -- is browser-shaped and
    // cannot set request headers, so the token cannot travel in an
    // Authorization header. It arrives in a `hello` frame instead, and nothing
    // else is accepted until it does.
    this.#clients.set(ws, { user: null, alive: true, host });

    ws.on('pong', () => {
      const client = this.#clients.get(ws);
      if (client) client.alive = true;
    });

    ws.on('message', (data) => {
      /*
       * Any frame at all proves this client is alive.
       *
       * The sweep below relies on pong frames, which assumes the client's
       * WebSocket answers protocol pings. Every browser-shaped one does, but
       * leaning on it is an unnecessary bet: a client that talks to us is
       * plainly not dead, whatever its ping handling looks like. Without
       * this a quiet-but-chatty client could be terminated on a technicality.
       */
      const alive = this.#clients.get(ws);
      if (alive) alive.alive = true;

      let msg;
      try {
        msg = JSON.parse(String(data));
      } catch {
        return this.#send(ws, { type: 'error', error: 'bad_json' });
      }
      this.#onMessage(ws, msg).catch((err) => {
        console.warn(`[ws] handler failed: ${err.message}`);
        this.#send(ws, { type: 'error', rid: msg?.rid, error: 'server_error' });
      });
    });

    ws.on('close', () => this.#onClose(ws));
    ws.on('error', () => this.#onClose(ws));

    // A client that never says hello is a port scanner or a bug, either way not
    // worth a socket.
    setTimeout(() => {
      if (this.#clients.get(ws)?.user == null) ws.close(4401, 'no hello');
    }, 10_000).unref();
  }

  #onClose(ws) {
    const client = this.#clients.get(ws);
    this.#clients.delete(ws);
    if (!client?.user) return;

    // Presence is derived from this socket existing, so closing it IS leaving.
    // Nothing has to be reconciled, and a server restart rebuilds the whole
    // picture from whoever reconnects.
    const stillOnline = this.#isOnline(client.user.id);
    if (stillOnline) return;

    for (const { channelId } of this.#deps.voice.leaveAll(client.user.id)) {
      this.broadcastRoster(channelId);
    }
    // Ringing ends; a call under way waits a little for them to come back.
    this.#deps.calls?.disconnected(client.user.id);
    // After leaveAll, so a client redrawing on this gets a roster that has
    // already lost them rather than one that still has them in a channel.
    this.broadcastPresence();
  }

  /**
   * Who has a socket open, by user id.
   *
   * Derived, never stored -- the same rule as voice presence, and for the
   * same reason: a restart rebuilds the whole picture from whoever
   * reconnects instead of leaving a stale table to be reconciled with
   * reality, which is the bug every "who is online" table eventually has.
   *
   * A Set, so two windows count once.
   */
  onlineUserIds() {
    const out = new Set();
    for (const client of this.#clients.values()) {
      if (client.user) out.add(client.user.id);
    }
    return [...out];
  }

  /** Tell everybody who is on. Sent whenever that set changes. */
  broadcastPresence() {
    this.broadcast({ type: 'presence', online: this.onlineUserIds() });
  }

  /** True if this user has another socket open (a second window, say). */
  #isOnline(userId) {
    for (const client of this.#clients.values()) {
      if (client.user?.id === userId) return true;
    }
    return false;
  }

  #sweep() {
    for (const [ws, client] of this.#clients) {
      if (!client.alive) {
        ws.terminate();
        continue;
      }
      client.alive = false;
      try { ws.ping(); } catch { /* closing */ }
    }
  }

  // -------------------------------------------------------------------------

  async #onMessage(ws, msg) {
    const client = this.#clients.get(ws);
    if (!client) return;

    /*
     * An application-level liveness check, answered before the hello gate.
     *
     * The client cannot feed its own watchdog from protocol pings -- those
     * are answered by the WebSocket implementation and never surface as a
     * message -- and a quiet server sends nothing for minutes at a time,
     * because rosters and channel lists are only pushed on change. So the
     * client asks, and this is the answer. It is deliberately the cheapest
     * handler here and deliberately needs no session: proving the socket
     * works is not privileged.
     */
    if (msg.type === 'ping') {
      return this.#send(ws, { type: 'pong', rid: msg.rid });
    }

    if (msg.type === 'hello') {
      const user = this.#deps.accounts.resolveSession(msg.token);
      if (!user) {
        // Close rather than reply-and-wait: the client must NOT retry a
        // credential rejection, and a close code says so unambiguously.
        this.#send(ws, { type: 'hello-failed', rid: msg.rid, error: 'bad_token' });
        return ws.close(4401, 'bad token');
      }
      client.user = user;
      this.#deps.calls?.reconnected(user.id);
      // Before the reply, so the arriving client's own hello already
      // contains them -- otherwise the one person guaranteed to be online
      // is missing from the list until somebody else connects.
      this.broadcastPresence();
      return this.#send(ws, {
        type: 'hello-ok',
        rid: msg.rid,
        user: publicUser(user),
        channels: this.#deps.channels.list().map(publicChannel),
        groups: this.#deps.channels.groups().map(publicGroup),
        occupancy: this.#deps.voice.occupancy(),
        // Who is in each channel right now. Without this a client that just
        // connected shows empty voice channels until somebody happens to
        // join or leave one.
        rosters: this.#deps.voice.allRosters(),
        online: this.onlineUserIds(),
        voiceCap: VOICE_HARD_CAP,
        // A call ringing for them, or one they were in before the socket
        // dropped: without it a reconnect mid-call would forget the call.
        calls: (this.#deps.calls?.forUser(user.id) ?? []).map((c) => ({
          ...publicCall(c),
          roster: this.#deps.calls.voice.roster(c.conversationId),
        })),
      });
    }

    if (!client.user) return this.#send(ws, { type: 'error', rid: msg.rid, error: 'no_hello' });

    const handler = this.#handlers[msg.type];
    if (!handler) return this.#send(ws, { type: 'error', rid: msg.rid, error: 'unknown_type' });

    const reply = await handler.call(this, client.user, msg, ws);
    if (reply) this.#send(ws, { ...reply, rid: msg.rid });
    return undefined;
  }

  get #handlers() {
    return {
      'voice:join': this.#voiceJoin,
      'voice:leave': this.#voiceLeave,
      'voice:mute': this.#voiceMute,
      'voice:publishing': this.#voicePublishing,
      'voice:refresh': this.#voiceRefresh,
      'call:start': this.#callStart,
      'call:answer': this.#callAnswer,
      'call:hang-up': this.#callHangUp,
      'call:join': this.#callJoin,
      'soundpad:play': this.#soundpadPlay,
      'admin:force-mute': this.#adminForceMute,
      'admin:move': this.#adminMove,
    };
  }

  // ------------------------------------------------------------- voice

  async #voiceJoin(user, msg, ws) {
    const channelId = Number(msg.channelId);
    const channel = this.#deps.channels.get(channelId);
    if (!channel || channel.kind !== 'voice') return { type: 'voice:error', error: 'no_such_channel' };

    const verdict = await this.#deps.channels.admit(channelId, user.id, msg.password);
    if (verdict !== 'ok') return { type: 'voice:error', error: verdict };

    // Leaving the previous channel first keeps "one voice channel at a time"
    // true without the client having to sequence two requests. A private call
    // counts as somewhere too.
    for (const { channelId: left } of this.#deps.voice.leaveAll(user.id)) {
      if (left !== channelId) this.broadcastRoster(left);
    }
    this.#deps.calls?.leaveUser(user.id);

    const joined = this.#deps.voice.join(channelId, user);
    if (!joined.ok) {
      return { type: 'voice:error', error: joined.error, cap: VOICE_HARD_CAP };
    }
    // Arriving muted is said in the join itself, so the room never sees a live mic flash by.
    if (msg.muted !== undefined) {
      this.#deps.voice.setMuted(channelId, user.id, msg.muted, msg.deafened);
    }

    this.broadcastRoster(channelId);
    return {
      type: 'voice:joined',
      channelId,
      mid: joined.mid,
      ...this.#deps.issueTokens(channelId, user.id, joined.mid, this.#clients.get(ws)?.host),
      roster: this.#deps.voice.roster(channelId),
    };
  }

  /**
   * Where a voice request is about: a channel, or -- with `conversationId`
   * instead of `channelId` -- a private call. The two keep their slots in
   * separate VoiceRooms, so the same mute, publishing and refresh handlers
   * serve both, and each tells its own audience about a change: a channel
   * everybody, a call only its two people.
   */
  #place(msg) {
    if (msg.conversationId != null && this.#deps.calls) {
      const id = Number(msg.conversationId);
      return {
        id,
        rooms: this.#deps.calls.voice,
        announce: () => this.#deps.calls.broadcastRoster(id),
        tokens: this.#deps.issueCallTokens,
        echo: { conversationId: id },
      };
    }
    const id = Number(msg.channelId);
    return {
      id,
      rooms: this.#deps.voice,
      announce: () => this.broadcastRoster(id),
      tokens: this.#deps.issueTokens,
      echo: { channelId: id },
    };
  }

  #voiceLeave(user, msg) {
    const place = this.#place(msg);
    const mid = place.rooms.leave(place.id, user.id);
    if (mid === false) return { type: 'voice:error', error: 'not_in_channel' };
    place.announce();
    return { type: 'voice:left', ...place.echo };
  }

  #voiceMute(user, msg) {
    const place = this.#place(msg);
    if (!place.rooms.setMuted(place.id, user.id, msg.muted, msg.deafened)) {
      return { type: 'voice:error', error: 'not_in_channel' };
    }
    place.announce();
    return { type: 'voice:ok' };
  }

  /** The client telling us which of its paths are actually publishing. */
  #voicePublishing(user, msg) {
    const place = this.#place(msg);
    const found = place.rooms.find(place.id, user.id);
    if (!found) return { type: 'voice:error', error: 'not_in_channel' };
    place.rooms.trackPublish(place.id, found.mid, String(msg.kind), Boolean(msg.on));
    place.announce();
    return { type: 'voice:ok' };
  }

  /**
   * Re-issue media tokens before they expire.
   *
   * Tokens last 10 minutes and MediaMTX only consults the auth hook at session
   * SETUP, so an expired token never drops a live stream -- it only stops a new
   * subscription being opened. Which is exactly what happens when somebody new
   * joins the channel an hour in, hence this.
   */
  #voiceRefresh(user, msg, ws) {
    const place = this.#place(msg);
    const found = place.rooms.find(place.id, user.id);
    if (!found) return { type: 'voice:error', error: 'not_in_channel' };
    return {
      type: 'voice:tokens',
      ...place.echo,
      ...place.tokens(place.id, user.id, found.mid, this.#clients.get(ws)?.host),
    };
  }

  // ------------------------------------------------------------- calls

  /** The conversation in `msg`, if this user is in it; and the other person. */
  #conversation(user, msg) {
    const conversation = this.#deps.dms?.forUser(user.id, Number(msg.conversationId));
    if (!conversation) return null;
    return { conversation, peerId: DirectMessages.peerOf(conversation, user.id) };
  }

  /**
   * Ring the other person in a conversation.
   *
   * `sealedKey` is the call's media key, sealed by the caller to the
   * conversation's keys exactly as a message is; the server passes it on and
   * cannot open it. Blocking either way round refuses the call as it refuses
   * a message.
   */
  #callStart(user, msg) {
    const found = this.#conversation(user, msg);
    if (!found) return { type: 'call:error', error: 'no_such_conversation' };
    const { conversation, peerId } = found;
    if (this.#deps.dms.isBlocked(user.id, peerId)) return { type: 'call:error', error: 'blocked' };
    if (typeof msg.sealedKey !== 'string' || !/^[A-Za-z0-9_-]{40,512}$/.test(msg.sealedKey)) {
      return { type: 'call:error', error: 'bad_message' };
    }
    const keys = this.#deps.dms.checkKeys(conversation, user.id, msg.senderKey, msg.recipientKey);
    if (!keys.ok) return { type: 'call:error', error: keys.error };

    const result = this.#deps.calls.start({
      conversationId: conversation.id,
      callerId: user.id,
      calleeId: peerId,
      sealedKey: msg.sealedKey,
      senderKey: msg.senderKey,
      recipientKey: msg.recipientKey,
      calleeOnline: this.#isOnline(peerId),
    });
    if (!result.ok) return { type: 'call:error', error: result.error };
    return { type: 'call:ok', call: publicCall(result.call) };
  }

  #callAnswer(user, msg) {
    const result = this.#deps.calls?.answer(Number(msg.conversationId), user.id);
    if (!result?.ok) return { type: 'call:error', error: 'no_such_call' };
    return { type: 'call:ok', call: publicCall(result.call) };
  }

  /** Cancel, decline or hang up: which one it is depends on who and when. See Calls.hangUp. */
  #callHangUp(user, msg) {
    const result = this.#deps.calls?.hangUp(Number(msg.conversationId), user.id);
    if (!result?.ok) return { type: 'call:error', error: 'no_such_call' };
    return { type: 'call:ok' };
  }

  /**
   * Take a media slot in a call: the caller while it rings, so they are ready
   * the moment it is answered, and the callee once they have answered. Being
   * in a call is being somewhere, so any voice channel is left first.
   */
  #callJoin(user, msg, ws) {
    const call = this.#deps.calls?.get(Number(msg.conversationId));
    if (!call || !Calls.isParty(call, user.id)) return { type: 'call:error', error: 'no_such_call' };
    if (call.state !== 'active' && call.callerId !== user.id) {
      return { type: 'call:error', error: 'not_answered' };
    }

    for (const { channelId } of this.#deps.voice.leaveAll(user.id)) this.broadcastRoster(channelId);
    const joined = this.#deps.calls.voice.join(call.conversationId, user);
    if (!joined.ok) return { type: 'call:error', error: joined.error };
    if (msg.muted !== undefined) {
      this.#deps.calls.voice.setMuted(call.conversationId, user.id, msg.muted, msg.deafened);
    }
    this.#deps.calls.broadcastRoster(call.conversationId);
    return {
      type: 'call:joined',
      conversationId: call.conversationId,
      mid: joined.mid,
      ...this.#deps.issueCallTokens(call.conversationId, user.id, joined.mid, this.#clients.get(ws)?.host),
      roster: this.#deps.calls.voice.roster(call.conversationId),
    };
  }

  // --------------------------------------------------------- soundpad

  /**
   * Fan out "play this clip" to everyone in the clicker's voice channel.
   *
   * Everyone plays their own locally cached copy. The server never touches
   * audio -- see the comment on Soundpad in chat.js for why mixing it into the
   * clicker's uplink is the wrong design.
   */
  #soundpadPlay(user, msg) {
    const channelId = Number(msg.channelId);
    const found = this.#deps.voice.find(channelId, user.id);
    if (!found) return { type: 'voice:error', error: 'not_in_channel' };

    const clip = this.#deps.soundpad?.get(Number(msg.clipId));
    if (!clip) return { type: 'voice:error', error: 'no_such_clip' };

    for (const member of this.#deps.voice.roster(channelId)) {
      this.toUser(member.userId, {
        type: 'soundpad:play',
        channelId,
        clipId: clip.id,
        hash: clip.file_hash,
        name: clip.name,
        by: user.nickname,
      });
    }
    return { type: 'voice:ok' };
  }

  // ------------------------------------------------------------- admin

  async #adminForceMute(user, msg) {
    if (user.role !== 'owner' && user.role !== 'admin') {
      return { type: 'voice:error', error: 'forbidden' };
    }
    const channelId = Number(msg.channelId);
    const mid = Number(msg.mid);
    if (!this.#deps.voice.setForceMuted(channelId, mid, msg.muted)) {
      return { type: 'voice:error', error: 'no_such_member' };
    }

    // Order matters. Send the roster BEFORE the kick: the kicked client will
    // not notice the dropped session for about nine seconds (measured), so this
    // event is the only thing that tells them -- and if it arrived after the
    // kick they would read the whole thing as a network error instead.
    this.broadcastRoster(channelId);

    if (msg.muted) await this.#deps.kickMember(channelId, mid);
    return { type: 'voice:ok' };
  }

  /** Move somebody out of a voice channel, or into one. */
  async #adminMove(user, msg) {
    if (user.role !== 'owner' && user.role !== 'admin') {
      return { type: 'voice:error', error: 'forbidden' };
    }
    const targetId = Number(msg.userId);
    const toId = msg.toChannelId == null ? null : Number(msg.toChannelId);

    const left = this.#deps.voice.leaveAll(targetId);
    for (const { channelId } of left) this.broadcastRoster(channelId);

    if (toId != null) {
      const channel = this.#deps.channels.get(toId);
      if (!channel || channel.kind !== 'voice') {
        return { type: 'voice:error', error: 'no_such_channel' };
      }
    }

    // The target does the actual join, because only their client can open the
    // peer connections. Telling them where to go keeps every WHIP/WHEP session
    // owned by the machine that has the microphone.
    this.toUser(targetId, { type: 'voice:moved', channelId: toId, by: user.nickname });
    return { type: 'voice:ok' };
  }

  // ------------------------------------------------------------- sending

  #send(ws, payload) {
    if (ws.readyState !== ws.OPEN) return;
    try { ws.send(JSON.stringify(payload)); } catch { /* closing */ }
  }

  /** Send to every socket a user has open. */
  toUser(userId, payload) {
    for (const [ws, client] of this.#clients) {
      if (client.user?.id === userId) this.#send(ws, payload);
    }
  }

  /**
   * Hang up on somebody, everywhere they are connected.
   *
   * A socket authenticates once, at hello, and is trusted for its lifetime
   * -- which is the right design for a push channel and exactly wrong for
   * an account that has just been deleted. Their sessions are gone, so the
   * next HTTP request fails, but the socket would go on receiving every
   * message on the server until they closed the app.
   */
  kickUser(userId, reason = 'account removed') {
    for (const [ws, client] of this.#clients) {
      if (client.user?.id !== userId) continue;
      this.#send(ws, { type: 'kicked', reason });
      try { ws.close(4403, reason); } catch { /* already going */ }
    }
  }

  /** Send to everyone authenticated. */
  broadcast(payload) {
    for (const [ws, client] of this.#clients) {
      if (client.user) this.#send(ws, payload);
    }
  }

  /**
   * Send to everyone authenticated who passes a test.
   *
   * The test is supplied by the caller rather than evaluated here, because
   * the only thing worth narrowing on so far is whether somebody may read a
   * channel, and that question belongs with the routes that already ask it.
   */
  broadcastWhere(payload, allow) {
    for (const [ws, client] of this.#clients) {
      if (client.user && allow(client.user)) this.#send(ws, payload);
    }
  }

  broadcastRoster(channelId) {
    this.broadcast({
      type: 'voice:roster',
      channelId,
      roster: this.#deps.voice.roster(channelId),
    });
  }

  /** The channel list changed: everyone needs the new one. */
  broadcastChannels() {
    this.broadcast({
      type: 'channels',
      channels: this.#deps.channels.list().map(publicChannel),
      groups: this.#deps.channels.groups().map(publicGroup),
    });
  }

  /**
   * Who is live on the flat username namespace, replacing the 3s poll.
   * `streamsFor(host)` builds the list for a socket's Host header, since the
   * watch URLs in it can depend on it.
   */
  broadcastStreams(streamsFor) {
    const byHost = new Map();
    for (const [ws, client] of this.#clients) {
      if (!client.user) continue;
      if (!byHost.has(client.host)) byHost.set(client.host, streamsFor(client.host));
      this.#send(ws, { type: 'streams', streams: byHost.get(client.host) });
    }
  }

  get clientCount() {
    return this.#clients.size;
  }

  close() {
    clearInterval(this.#timer);
    for (const ws of this.#clients.keys()) {
      try { ws.close(1001, 'server shutting down'); } catch { /* already gone */ }
    }
    this.#clients.clear();
    this.#wss.close();
  }
}
