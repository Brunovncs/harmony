// Voice channels: one microphone out, N-1 subscriptions in.
//
// The relay cannot mix. Discord sends each listener one pre-mixed stream;
// mixing is decode + sum + re-encode, which is exactly the transcoding Harmony
// refuses to do and a Pi 5 could not do anyway. So every member subscribes to
// every other member individually and the browser mixes locally. That is why
// the member cap is a real constraint rather than a conservative guess: the
// relay carries N*(N-1) streams.
//
// Phase 0 measured the client side of that at 7.3% of one core for sixteen
// concurrent subscriptions with zero loss, so the cap is about the relay's
// bandwidth, not this file.

import { publish, watch } from './webrtc.js';
import { createSink } from './gain.js';

/**
 * Microphone constraints.
 *
 * All three processors ON, which is the opposite of what AudioBridge does for
 * a capture card -- and correct for the opposite reason. A capture card is
 * already-mixed game audio where echo cancellation would duck the music and
 * noise suppression would eat it; a microphone in a room with speakers is the
 * textbook case for all three.
 */
const MIC_CONSTRAINTS = {
  echoCancellation: true,
  noiseSuppression: true,
  autoGainControl: true,
  channelCount: 1,
};

/** Opus for speech. 32 kbps is ~52 kbps on the wire once RTP is counted. */
const VOICE_BITRATE = 32_000;

/**
 * Stagger between opening subscriptions.
 *
 * The N-th person to join opens N-1 WHEP sessions at once, each with its own
 * ICE gather and DTLS handshake. Firing them simultaneously is the join storm
 * the costing called out as the third thing to break.
 */
const SUBSCRIBE_STAGGER_MS = 75;

export class VoiceSession {
  /** @type {{pc: RTCPeerConnection, resourceUrl: string|null}|null} */
  #mic = null;
  #micStream = null;
  /** @type {Map<number, {pc: RTCPeerConnection, sink: object, stream: MediaStream}>} */
  #subs = new Map();
  #opening = new Set();

  channelId = null;
  mid = null;
  token = '';
  whepBase = '';
  iceServers = [];
  /**
   * The last roster the server sent.
   *
   * Kept so subscriptions can be retried without waiting for another push --
   * see the note on syncPeers about why that matters.
   */
  lastRoster = [];

  /** Local mute: the track stays published, the audio stops. */
  muted = false;
  /** Output mute, applied to every incoming sink. */
  deafened = false;

  get micLive() {
    return Boolean(this.#mic);
  }

  get subscriberCount() {
    return this.#subs.size;
  }

  get subscribedMids() {
    return [...this.#subs.keys()];
  }

  configure({ channelId, mid, token, whepBase, iceServers }) {
    this.channelId = channelId;
    this.mid = mid;
    this.token = token;
    this.whepBase = whepBase;
    if (iceServers) this.iceServers = iceServers;
  }

  /** The WHEP URL for another member's voice path in this channel. */
  peerUrl(mid) {
    const path = `vc-${this.channelId.toString(36)}-${mid.toString(36)}-v`;
    return `${this.whepBase}/${path}/whep?token=${encodeURIComponent(this.token)}`;
  }

  // -------------------------------------------------------------- outgoing

  async startMic(publishUrl, deviceId) {
    if (this.#mic) return;
    this.#micStream = await navigator.mediaDevices.getUserMedia({
      audio: { ...MIC_CONSTRAINTS, ...(deviceId ? { deviceId: { exact: deviceId } } : {}) },
    });
    // Apply the current mute state immediately: joining already muted must not
    // broadcast a second of room noise before the toggle is read.
    for (const track of this.#micStream.getAudioTracks()) track.enabled = !this.muted;

    this.#mic = await publish({
      url: publishUrl,
      stream: this.#micStream,
      iceServers: this.iceServers,
    });

    const sender = this.#mic.pc.getSenders().find((s) => s.track?.kind === 'audio');
    if (sender) {
      const params = sender.getParameters();
      params.encodings = params.encodings?.length ? params.encodings : [{}];
      params.encodings[0].maxBitrate = VOICE_BITRATE;
      await sender.setParameters(params).catch(() => { /* not fatal */ });
    }
  }

  async stopMic() {
    const mic = this.#mic;
    this.#mic = null;
    this.#micStream?.getTracks().forEach((t) => t.stop());
    this.#micStream = null;
    if (!mic) return;
    mic.pc.close();
    if (mic.resourceUrl) await harmony.api.hangup(mic.resourceUrl).catch(() => {});
  }

  /**
   * Mute by disabling the track, never by stopping the publish.
   *
   * Stopping it would destroy the MediaMTX path and force every other member
   * to tear down and rebuild their subscription -- 2(N-1) sessions churned on
   * every push-to-talk. A disabled track collapses to comfort noise instead,
   * and unmuting is instant.
   */
  setMuted(muted) {
    this.muted = Boolean(muted);
    for (const track of this.#micStream?.getAudioTracks() ?? []) {
      track.enabled = !this.muted;
    }
    return this.muted;
  }

  // -------------------------------------------------------------- incoming

  /**
   * Bring subscriptions in line with the roster.
   *
   * Idempotent and safe to call repeatedly, which is exactly how it is used:
   * on every roster push AND on a slow timer. The timer is not belt-and-braces,
   * it is load-bearing, and a relay load test is what proved it.
   *
   * MediaMTX answers a WHIP publish with 201 as soon as SIGNALLING completes,
   * but the path only becomes readable once the first RTP packet actually
   * arrives. A peer who subscribes inside that window gets
   * `404 no stream is available` -- measured on a real 16-member channel,
   * where every other member failed to subscribe to one slot for exactly that
   * reason.
   *
   * Retrying on the next roster push is not enough, because in a settled
   * channel there is no next push: nobody joins, leaves or mutes, so the
   * failure is permanent and that one person is silently inaudible to
   * everybody. Hence `retrySoon`, and hence the caller's timer.
   */
  async syncPeers(roster) {
    if (roster) this.lastRoster = roster;
    const current = this.lastRoster ?? [];

    const wanted = new Set(
      current
        .filter((m) => m.mid !== this.mid && m.publishing?.includes('v'))
        .map((m) => m.mid),
    );

    for (const mid of [...this.#subs.keys()]) {
      if (!wanted.has(mid)) await this.unsubscribe(mid);
    }

    const missed = [];
    for (const mid of wanted) {
      if (this.#subs.has(mid) || this.#opening.has(mid)) continue;
      this.#opening.add(mid);
      try {
        await this.subscribe(mid);
      } catch (err) {
        missed.push({ mid, reason: err?.code ?? err?.message ?? 'failed' });
      } finally {
        this.#opening.delete(mid);
      }
      await new Promise((r) => setTimeout(r, SUBSCRIBE_STAGGER_MS));
    }
    return { subscribed: this.#subs.size, missed };
  }

  /** True while somebody on the roster is publishing but not yet subscribed. */
  get hasMissingPeers() {
    return (this.lastRoster ?? []).some(
      (m) => m.mid !== this.mid && m.publishing?.includes('v') && !this.#subs.has(m.mid),
    );
  }

  async subscribe(mid) {
    const { pc, stream } = await watch({
      url: this.peerUrl(mid),
      iceServers: this.iceServers,
      media: 'audio',
    });
    const sink = createSink(stream);
    sink.set(this.deafened ? 0 : 1);
    this.#subs.set(mid, { pc, sink, stream });
    return sink;
  }

  async unsubscribe(mid) {
    const sub = this.#subs.get(mid);
    if (!sub) return;
    this.#subs.delete(mid);
    sub.sink.close?.();
    sub.pc.close();
  }

  /**
   * Deafen: silence everyone, without tearing the subscriptions down.
   *
   * Gain rather than hang-up because this is a toggle people flip constantly.
   * Hanging up would make un-deafening cost a full round of ICE per member.
   */
  setDeafened(deafened) {
    this.deafened = Boolean(deafened);
    for (const sub of this.#subs.values()) sub.sink.set(this.deafened ? 0 : 1);
    return this.deafened;
  }

  setPeerGain(mid, gain) {
    this.#subs.get(mid)?.sink.set(gain);
  }

  async leave() {
    this.lastRoster = [];
    await this.stopMic();
    for (const mid of [...this.#subs.keys()]) await this.unsubscribe(mid);
    this.channelId = null;
    this.mid = null;
    this.token = '';
  }
}
