// Tiny JSON-file settings store. Keeps the server address and the user's last
// username/quality choice between runs, which matters for the portable build --
// there is no installer to write them for us.

const { app } = require('electron');
const fs = require('node:fs');
const path = require('node:path');

const FILE = path.join(app.getPath('userData'), 'settings.json');

const DEFAULTS = {
  serverUrl: '',
  username: '',
  // Only used when the server is configured to want one. Stored in the clear in
  // this file, like every other setting -- it is a shared room password, not a
  // credential that protects anything else, and the alternative (retyping it on
  // every launch) is what makes people pick a worse password.
  password: '',
  // The logged-in account's bearer token, kept when "remember me" is ticked.
  //
  // Deliberately the TOKEN and not the account password: it expires on its own,
  // the server can revoke it, and it is useless against any other service the
  // person may have reused that password on. The shared `password` above is a
  // different thing -- a room key, not a personal credential.
  sessionToken: '',
  rememberAccount: true,
  // Disk budget for cached avatars, attachments and soundpad clips. Evicted
  // least-recently-used first, never touching pinned or soundpad files.
  mediaCacheMb: 512,
  quality: 'balanced',
  // 'sharp' keeps resolution and drops frames; 'smooth' does the opposite.
  priority: 'sharp',
  // Last audio input used with a camera or capture card.
  audioInputId: '',
  /*
   * Voice-channel devices, kept apart from audioInputId on purpose.
   *
   * That one is the input paired with a capture card -- console audio, line
   * level, no echo cancellation. A microphone is the opposite of it in every
   * respect, and sharing one setting means picking a sensible microphone
   * silently breaks the capture card you set up last week.
   *
   * Empty means "whatever the system calls default". A specific id is a
   * PREFERENCE, not a requirement: if that device is unplugged the voice
   * falls back to the default and the preference is kept, so plugging the
   * headset back in restores it without anyone touching a menu.
   */
  voiceInputId: '',
  voiceOutputId: '',
  // Rolling clip buffer. Off by default: it is memory the user did not ask for.
  clipsEnabled: false,
  // GPU video encoding (NVENC / AMF / Quick Sync). 'auto' leaves Chromium to
  // use it, which it does by default and which is measurably faster; 'off'
  // forces software H.264, for a machine whose driver misbehaves.
  hardwareEncoding: 'auto',
  // Which GPU Harmony runs on where there are two. 'auto' lets Chromium pick
  // (the dedicated one); 'integrated' keeps Harmony off the GPU a game is
  // using. See gpu.js for the measurement behind this.
  gpuPreference: 'auto',
  // What to do when a window is shared but per-application audio is not
  // available (non-Windows, or the native module is missing).
  //   'silent' -- send no audio, never leak other apps' sound
  //   'system' -- fall back to whole-system audio
  windowAudioFallback: 'silent',
};

let cache = null;

function read() {
  if (cache) return cache;
  try {
    cache = { ...DEFAULTS, ...JSON.parse(fs.readFileSync(FILE, 'utf8')) };
  } catch {
    cache = { ...DEFAULTS };
  }
  return cache;
}

function write(patch) {
  cache = { ...read(), ...patch };
  try {
    fs.mkdirSync(path.dirname(FILE), { recursive: true });
    fs.writeFileSync(FILE, JSON.stringify(cache, null, 2));
  } catch (err) {
    console.warn('[settings] could not persist:', err.message);
  }
  return cache;
}

module.exports = { read, write, DEFAULTS };
