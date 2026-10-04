// Volume past 100%.
//
// A <video> element's `volume` is clamped to 1.0, so a stream that was quiet at
// the source can never be made loud enough. Routing the audio through a
// GainNode lifts that ceiling: the element stays muted and Web Audio does the
// playing, which is the only way to amplify a MediaStream in a renderer.
//
// The cost is that `video.volume` and `video.muted` stop being the source of
// truth, so everything that used to read them reads `sink.gain` instead.

/** Loudest we will go. Past this, most sources are more distortion than signal. */
export const MAX_GAIN = 3.5;

/**
 * Speaking detection.
 *
 * Three mechanisms, and all three turned out to be necessary once this met
 * a real voice rather than a test tone.
 *
 * 1. TWO THRESHOLDS. A single one makes the ring strobe, because the gaps
 *    between syllables genuinely are silence.
 * 2. AN ENVELOPE with a fast attack and a slow release. A 43 ms window lands
 *    inside one of those gaps often enough that instantaneous RMS flickers
 *    even well above the threshold.
 * 3. A HOLD, to carry across the longer gaps between words.
 *
 * The thresholds are deliberately low. The first version used 0.02, which is
 * about -34 dBFS: fine for Chromium's fake microphone, which is a full-scale
 * tone, and far too high for somebody speaking normally into a headset after
 * Opus at 32 kbps has been through it. The reported symptom was a ring that
 * "blinked twice" during a sentence, which is exactly what a threshold
 * sitting near the peaks of speech rather than its body looks like.
 */
const SPEAK_ON = 0.0075;
const SPEAK_OFF = 0.0035;
const SPEAK_HOLD_MS = 400;

/** How fast the envelope falls when the sound stops. Per read, at ~100 ms. */
const ENVELOPE_RELEASE = 0.65;

/** How often to look for an audio track that has not arrived yet. */
const RETRY_MS = 200;
/** And how long to keep looking before accepting the stream has no audio. */
const RETRY_GIVE_UP_MS = 30_000;

/**
 * Root-mean-square of what an analyser is hearing right now, 0..1.
 *
 * RMS rather than the peak: a single loud sample is a click, and peak makes
 * a keyboard sound like talking.
 */
function rms(analyser, buffer) {
  analyser.getFloatTimeDomainData(buffer);
  let sum = 0;
  for (let i = 0; i < buffer.length; i += 1) sum += buffer[i] * buffer[i];
  return Math.sqrt(sum / buffer.length);
}

/** Wrap an analyser in the envelope and hysteresis above. */
function speechGate(analyser) {
  const buffer = new Float32Array(analyser.fftSize);
  let speaking = false;
  let until = 0;
  let envelope = 0;

  return () => {
    const level = rms(analyser, buffer);
    // Instant attack, gradual release: the envelope follows the loudest
    // thing heard recently rather than whatever this 43 ms happens to hold.
    envelope = level > envelope ? level : envelope * ENVELOPE_RELEASE;

    const now = performance.now();
    if (envelope > SPEAK_ON) {
      speaking = true;
      until = now + SPEAK_HOLD_MS;
    } else if (speaking && envelope < SPEAK_OFF && now > until) {
      speaking = false;
    }
    return { speaking, level: envelope };
  };
}

/**
 * Meter a stream WITHOUT playing it.
 *
 * For your own microphone: it is already going out over WebRTC, and routing
 * it to the destination as well is how you end up listening to yourself with
 * a few hundred milliseconds of delay. An AnalyserNode with nothing connected
 * downstream still runs, because it is a pull-free node that taps whatever
 * reaches it.
 */
export function createMeter(stream) {
  const analyser = context().createAnalyser();
  analyser.fftSize = 2048;
  analyser.smoothingTimeConstant = 0.2;

  let source = null;
  try {
    source = context().createMediaStreamSource(stream);
    source.connect(analyser);
  } catch (err) {
    console.warn('[gain] could not meter stream:', err.message);
  }

  const read = speechGate(analyser);
  return {
    read,
    get speaking() {
      return read().speaking;
    },
    close() {
      try { source?.disconnect(); } catch { /* already gone */ }
      try { analyser.disconnect(); } catch { /* already gone */ }
      source = null;
    },
  };
}

let ctx = null;

/**
 * One AudioContext for every stream being played.
 *
 * Created on first use rather than at load: a context made before any user
 * gesture starts suspended, and Chromium counts the click that joined a stream
 * as activation, so by the time anything needs playing there is a gesture to
 * ride on.
 */
function context() {
  if (!ctx) ctx = new AudioContext();
  // Autoplay policy can still park it; resuming is free when already running.
  if (ctx.state === 'suspended') ctx.resume().catch(() => {});
  return ctx;
}

/**
 * Route a received stream's audio through a gain node.
 *
 * @param {MediaStream} stream
 * @returns {{set: (v: number) => void, value: number, speaking: boolean, close: () => void}}
 */
export function createSink(stream) {
  const node = context().createGain();
  node.gain.value = 1;
  node.connect(context().destination);

  /*
   * The analyser sits BEFORE the gain node, so what it reports is what the
   * speaker is sending rather than how loudly we have chosen to play them.
   * After the gain node, muting somebody locally would also stop their
   * speaking ring -- and "is this person talking" is exactly what you want to
   * know about somebody you have muted.
   */
  const analyser = context().createAnalyser();
  analyser.fftSize = 2048;
  analyser.smoothingTimeConstant = 0.2;
  analyser.connect(node);
  const read = speechGate(analyser);

  /*
   * Hook the audio up when it exists, which is not necessarily now.
   *
   * createMediaStreamSource captures whatever audio track the stream holds
   * at the moment it is called, and WHEP delivers audio and video as
   * separate `track` events -- so the audio often is not there yet.
   *
   * The first version waited for the stream's `addtrack` event. That event
   * NEVER FIRES HERE: `MediaStream.addTrack()` called from script does not
   * raise it, by specification, and the streams being played are built by
   * webrtc.js doing exactly that. So whenever the audio track happened to
   * arrive after this ran, the source was never connected -- no analyser,
   * and more to the point no sound, permanently and silently.
   *
   * That is what "I still cannot hear a thing" was. It was intermittent and
   * asymmetric because it is a race: whichever side's track lost it went
   * quiet while every roster, indicator and connection state looked perfect.
   *
   * So: keep the event (harmless, and correct for streams the user agent
   * builds), and poll as well, because the event cannot be relied on.
   */
  let source = null;
  let retry = null;

  const stopRetrying = () => {
    if (retry) clearInterval(retry);
    retry = null;
  };

  const connect = () => {
    if (source) return stopRetrying();
    if (stream.getAudioTracks().length === 0) return undefined;
    try {
      source = context().createMediaStreamSource(stream);
      source.connect(analyser);
      stopRetrying();
    } catch (err) {
      console.warn('[gain] could not route stream audio:', err.message);
    }
    return undefined;
  };

  connect();
  stream.addEventListener('addtrack', connect);
  if (!source) {
    retry = setInterval(connect, RETRY_MS);
    retry.unref?.();
    // A stream that never carries audio is ordinary -- a camera, a silent
    // screen share -- so stop looking rather than polling for the life of
    // the call.
    setTimeout(stopRetrying, RETRY_GIVE_UP_MS);
  }

  const sink = {
    value: 1,
    /** True while this stream is carrying speech. See speechGate. */
    get speaking() {
      return source ? read().speaking : false;
    },
    /**
     * Whether the stream's audio is actually routed yet.
     *
     * Worth exposing rather than inferring: `connected === false` is the
     * difference between "nobody is talking" and "this person cannot be
     * heard at all", and from the outside those look identical.
     */
    get connected() {
      return source !== null;
    },
    /** The current envelope, for diagnosing a silent-but-connected stream. */
    get level() {
      return source ? read().level : 0;
    },
    set(value) {
      const clamped = Math.max(0, Math.min(MAX_GAIN, value));
      sink.value = clamped;
      // setTargetAtTime rather than a bare assignment: stepping gain straight
      // from 0 to 3.5 is an audible click.
      node.gain.setTargetAtTime(clamped, context().currentTime, 0.015);
    },
    close() {
      stopRetrying();
      stream.removeEventListener('addtrack', connect);
      try {
        source?.disconnect();
      } catch {
        /* already gone */
      }
      try {
        analyser.disconnect();
      } catch {
        /* already gone */
      }
      try {
        node.disconnect();
      } catch {
        /* already gone */
      }
      source = null;
    },
  };
  return sink;
}

/** Decoded clips, so the second press of a soundpad button is instant. */
const samples = new Map();

/**
 * Play a one-shot sound into the PLAYBACK context.
 *
 * This is the single easiest thing to get wrong in the soundpad, so it is
 * worth being explicit: the clip goes here, into the context that plays what
 * you HEAR. It must never be routed into AudioBridge's gain node, which is the
 * OUTGOING mix being published. Doing that re-broadcasts the clip to everyone
 * who is already playing it locally, and they hear it twice, slightly out of
 * phase.
 *
 * The URL is `harmony://app/media/<hash>`, which is same-origin -- so `fetch`
 * is allowed by the CSP's `'self'` and no network permission is needed.
 *
 * @param {string} url
 * @param {{gain?: number}} options
 */
export async function playSample(url, { gain = 1 } = {}) {
  let buffer = samples.get(url);
  if (!buffer) {
    const response = await fetch(url);
    if (!response.ok) throw new Error(`could not load the clip (${response.status})`);
    buffer = await context().decodeAudioData(await response.arrayBuffer());
    samples.set(url, buffer);
  }

  const source = context().createBufferSource();
  source.buffer = buffer;
  const node = context().createGain();
  node.gain.value = Math.max(0, Math.min(MAX_GAIN, gain));
  source.connect(node);
  node.connect(context().destination);
  source.start();
  // Overlapping presses are fine -- each gets its own source node -- but the
  // nodes have to be released or they accumulate for the life of the context.
  source.addEventListener('ended', () => {
    try {
      source.disconnect();
      node.disconnect();
    } catch { /* already torn down */ }
  });
  return source;
}

/**
 * Send everything we play to a particular output device.
 *
 * One call covers every voice subscription, every screen share's audio and
 * the soundpad, because they all share the one AudioContext -- which is the
 * reason that context exists rather than a node graph per stream.
 *
 * Returns false rather than throwing when the device is gone: unplugging
 * headphones mid-call is an ordinary event, not an error, and the caller's
 * job is then to fall back rather than to report a failure.
 */
export async function setOutputDevice(deviceId) {
  const ctx = context();
  if (typeof ctx.setSinkId !== 'function') return false;
  try {
    // '' is the system default. setSinkId takes the empty string for that,
    // the same as an <audio> element does.
    await ctx.setSinkId(deviceId || '');
    return true;
  } catch (err) {
    console.warn('[gain] could not switch output:', err.message);
    return false;
  }
}

/** Percent for the UI, from the 0..MAX_GAIN scale. */
export const asPercent = (gain) => Math.round(gain * 100);
