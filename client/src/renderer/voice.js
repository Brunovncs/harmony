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

  /**
   * Incoming video, keyed `<mid>:<kind>` -- the channel mosaic.
   *
   * Separate from #subs rather than one map of everything, because the two are
   * driven by different parts of the roster and fail independently: losing a
   * camera must not take anybody's voice with it, and a member can publish a
   * camera and a screen at once.
   */
  /** @type {Map<string, {pc: RTCPeerConnection, stream: MediaStream, mid: number, kind: string}>} */
  #video = new Map();
  #openingVideo = new Set();

  /** @type {{pc: RTCPeerConnection, resourceUrl: string|null}|null} */
  #cam = null;
  #camStream = null;

  channelId = null;
  mid = null;
  token = '';
  whepBase = '';
  iceServers = [];
  /** The WHIP URLs the server minted for this membership: voice, cam, screen. */
  publishUrls = { voice: '', cam: '', screen: '' };
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

  configure({ channelId, mid, token, whepBase, iceServers, publish }) {
    this.channelId = channelId;
    this.mid = mid;
    this.token = token;
    this.whepBase = whepBase;
    if (publish) this.publishUrls = publish;
    if (iceServers) this.iceServers = iceServers;
  }

  /**
   * Re-key an existing membership after the server re-issues its tokens.
   *
   * The slot does not change, so nothing has to be torn down: only the URLs
   * handed to the NEXT subscription or publish need to be current. Sessions
   * already open keep running, because MediaMTX consults the auth hook at
   * setup and never again.
   */
  retoken({ token, publish }) {
    if (token) this.token = token;
    if (publish) this.publishUrls = publish;
  }

  /**
   * The WHEP URL for one of another member's paths in this channel.
   *
   * `kind` is the single letter MediaMTX sees in the path: v voice, c camera,
   * s screen. One read token covers all three -- the auth hook only compares
   * the channel for a read, so any member may watch any other member.
   */
  peerUrl(mid, kind = 'v') {
    const path = `vc-${this.channelId.toString(36)}-${mid.toString(36)}-${kind}`;
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

  // ---------------------------------------------------------- channel video

  /**
   * Publish a camera into this channel.
   *
   * A path of its own (`vc-<cid>-<mid>-c`), never a second track on the voice
   * path. MediaMTX's WHIP cannot renegotiate an added track -- measured in the
   * Phase 0 spike, where PATCH accepted only ICE trickle fragments -- so
   * adding video to the live audio session would mean tearing it down and
   * cutting everyone's audio to turn a camera on.
   */
  async startCam(stream, { bitrate = 400_000, framerate = 24 } = {}) {
    if (this.#cam) return;
    this.#camStream = stream;
    this.#cam = await publish({
      url: this.publishUrls.cam,
      stream,
      iceServers: this.iceServers,
      codec: 'H264',
      maxBitrate: bitrate,
      maxFramerate: framerate,
      contentHint: 'motion',
    });
  }

  async stopCam() {
    const cam = this.#cam;
    this.#cam = null;
    this.#camStream?.getTracks().forEach((t) => t.stop());
    this.#camStream = null;
    if (!cam) return;
    cam.pc.close();
    if (cam.resourceUrl) await harmony.api.hangup(cam.resourceUrl).catch(() => {});
  }

  get camLive() {
    return Boolean(this.#cam);
  }

  /** Tiles to draw, in a stable order so the grid does not reshuffle itself. */
  get videoTiles() {
    return [...this.#video.values()]
      .map(({ mid, kind, stream }) => ({ mid, kind, stream }))
      .sort((a, b) => a.mid - b.mid || a.kind.localeCompare(b.kind));
  }

  /**
   * Bring the video subscriptions in line with the roster.
   *
   * The same shape as syncPeers, and for the same reason: a publisher's path
   * is not readable until the first RTP packet arrives, so an early subscribe
   * 404s and has to be retried on a timer rather than on the next push, which
   * in a settled channel never comes.
   */
  async syncVideo(roster) {
    const current = roster ?? this.lastRoster ?? [];

    const wanted = new Map();
    for (const member of current) {
      if (member.mid === this.mid) continue;
      for (const kind of ['c', 's']) {
        if (member.publishing?.includes(kind)) {
          wanted.set(`${member.mid}:${kind}`, { mid: member.mid, kind });
        }
      }
    }

    for (const key of [...this.#video.keys()]) {
      if (!wanted.has(key)) this.unsubscribeVideo(key);
    }

    let changed = false;
    for (const [key, { mid, kind }] of wanted) {
      if (this.#video.has(key) || this.#openingVideo.has(key)) continue;
      this.#openingVideo.add(key);
      try {
        const { pc, stream } = await watch({
          url: this.peerUrl(mid, kind),
          iceServers: this.iceServers,
        });
        // A screen share carries the sharer's audio. It goes through gain.js
        // like every other incoming stream rather than through the <video>
        // element, so the tile stays muted and deafen covers it -- otherwise
        // deafening would silence everybody except the person sharing.
        const sink = createSink(stream);
        sink.set(this.deafened ? 0 : 1);
        this.#video.set(key, { pc, stream, sink, mid, kind });
        changed = true;
      } catch {
        // 404 while the path warms up. The caller's timer comes back.
      } finally {
        this.#openingVideo.delete(key);
      }
      await new Promise((r) => setTimeout(r, SUBSCRIBE_STAGGER_MS));
    }
    return { tiles: this.#video.size, changed };
  }

  /** True while somebody is publishing video we have not managed to open. */
  get hasMissingVideo() {
    return (this.lastRoster ?? []).some((m) => m.mid !== this.mid
      && ['c', 's'].some((k) => m.publishing?.includes(k) && !this.#video.has(`${m.mid}:${k}`)));
  }

  unsubscribeVideo(key) {
    const sub = this.#video.get(key);
    if (!sub) return;
    this.#video.delete(key);
    sub.sink?.close?.();
    sub.pc.close();
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
    // Screen shares too: a deafen that leaves one person's game audio playing
    // is not a deafen.
    for (const sub of this.#video.values()) sub.sink?.set(this.deafened ? 0 : 1);
    return this.deafened;
  }

  setPeerGain(mid, gain) {
    this.#subs.get(mid)?.sink.set(gain);
  }

  async leave() {
    this.lastRoster = [];
    await this.stopMic();
    await this.stopCam();
    for (const mid of [...this.#subs.keys()]) await this.unsubscribe(mid);
    for (const key of [...this.#video.keys()]) this.unsubscribeVideo(key);
    this.channelId = null;
    this.mid = null;
    this.token = '';
    this.publishUrls = { voice: '', cam: '', screen: '' };
  }
}
