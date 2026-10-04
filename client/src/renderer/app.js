import { harmony } from './bridge.js';
import { publish, watch, hangup, createStatsReader, applySenderSettings } from './webrtc.js';
import { AudioBridge } from './audio-bridge.js';
import { runConnectionTest } from './connection-test.js';
import { ClipBuffer, CLIP_SECONDS } from './clip-buffer.js';
import { VoiceSession } from './voice.js';
import { createSink, MAX_GAIN, asPercent, playSample, setOutputDevice } from './gain.js';

// ---------------------------------------------------------------------------
// Quality presets
//
// Bitrate ceilings, not targets -- the encoder spends less on a static desktop.
// `width: null` means "whatever the source is", which is what you want when
// sharing a single window.
// ---------------------------------------------------------------------------

const QUALITY = {
  low: { label: 'Low — 720p, 30 fps', width: 1280, height: 720, fps: 30, bitrate: 3_000_000 },
  balanced: { label: 'Balanced — 1080p, 30 fps', width: 1920, height: 1080, fps: 30, bitrate: 8_000_000 },
  // Doubling the frame rate does not double the bits needed -- consecutive
  // frames are more alike at 60 fps than at 30 -- so 12 rather than 16, which
  // also leaves room below the native-resolution presets above.
  full60: { label: 'Full HD — 1080p, 60 fps', width: 1920, height: 1080, fps: 60, bitrate: 12_000_000 },
  high: { label: 'High — native, 60 fps', width: null, height: null, fps: 60, bitrate: 15_000_000 },
  ultra: { label: 'Ultra — native, 60 fps', width: null, height: null, fps: 60, bitrate: 25_000_000 },
};

/**
 * What the encoder should give up when it runs short of bandwidth.
 *
 * Measured on a 1080p screen full of motion: at a 4 Mbit ceiling "sharp" gives
 * about 31 fps at full resolution, and raising the ceiling to 10 Mbit takes it
 * to ~58 fps still at 1080p. "smooth" holds ~57 fps on a squeezed link but
 * settles at 720p to do it. Bitrate is a ceiling, not a target -- WebRTC's
 * congestion control still backs off to whatever the connection really has.
 */
const PRIORITY = {
  sharp: {
    label: 'Sharp — keep resolution',
    contentHint: 'detail',
    degradationPreference: 'maintain-resolution',
  },
  smooth: {
    label: 'Smooth — keep framerate',
    contentHint: 'motion',
    degradationPreference: 'balanced',
  },
};

const WATCH_RETRY_MS = 2500;

/**
 * Account state for the connect screen.
 *
 * `supported` stays false against a pre-accounts server, which is what keeps
 * the old single-box flow working untouched.
 */
const AUTH_DEFAULTS = {
  supported: false,
  hasAccounts: false,
  needsOwner: false,
  mode: 'login',
  token: '',
  user: null,
};

// ---------------------------------------------------------------------------

/**
 * Fold what someone typed into the form that is actually stored.
 *
 * Mirrors normalizeName() on the server: "Pedro Lucas" is a perfectly
 * reasonable thing to type and becomes `pedrolucas`. Doing it here as well
 * means the box shows what will be used instead of quietly rewriting it on
 * submit -- the server would reach the same answer either way, but only one of
 * those is honest about it.
 */
const normalizeName = (raw) => String(raw ?? '').trim().toLowerCase().replace(/\s+/g, '');

const $ = (id) => document.getElementById(id);

const el = {
  views: document.querySelectorAll('.view'),
  serverUrl: $('server-url'),
  passwordField: $('password-field'),
  password: $('server-password'),
  username: $('username'),
  usernameHint: $('username-hint'),
  accountFields: $('account-fields'),
  accountPassword: $('account-password'),
  accountPasswordLabel: $('account-password-label'),
  accountConfirmField: $('account-confirm-field'),
  accountConfirm: $('account-confirm'),
  ownerKeyField: $('owner-key-field'),
  ownerKey: $('owner-key'),
  rememberAccount: $('remember-account'),
  authModeText: $('auth-mode-text'),
  authModeToggle: $('auth-mode-toggle'),
  continue: $('continue'),
  connectError: $('connect-error'),
  liveList: $('live-list'),
  liveItems: $('live-items'),

  channelsWho: $('channels-who'),
  channelsAvatar: $('channels-avatar'),
  avatarButton: $('avatar-button'),
  avatarFile: $('avatar-file'),
  channelsRole: $('channels-role'),
  channelsShare: $('channels-share'),
  channelsWatch: $('channels-watch'),
  channelsSignout: $('channels-signout'),
  channelAdd: $('channel-add'),
  channelItems: $('channel-items'),
  channelsLayout: document.querySelector('.channels-layout'),
  channelStage: $('channel-stage'),
  channelsError: $('channels-error'),
  voiceIdle: $('voice-idle'),
  voiceActive: $('voice-active'),
  voiceName: $('voice-name'),
  voiceCount: $('voice-count'),
  voiceRoster: $('voice-roster'),
  chatActive: $('chat-active'),
  chatName: $('chat-name'),
  chatSearch: $('chat-search'),
  chatSearchClear: $('chat-search-clear'),
  chatPinned: $('chat-pinned'),
  chatLog: $('chat-log'),
  chatForm: $('chat-form'),
  chatInput: $('chat-input'),
  chatFile: $('chat-file'),
  chatAttach: $('chat-attach'),
  chatNote: $('chat-note'),
  soundpad: $('soundpad'),
  soundpadAdd: $('soundpad-add'),
  soundpadFile: $('soundpad-file'),
  soundpadGrid: $('soundpad-grid'),
  soundpadVolume: $('soundpad-volume'),
  soundpadVolumeLabel: $('soundpad-volume-label'),
  soundpadMute: $('soundpad-mute'),
  ask: $('ask'),
  askForm: $('ask-form'),
  askTitle: $('ask-title'),
  askText: $('ask-text'),
  askFields: $('ask-fields'),
  askError: $('ask-error'),
  askCancel: $('ask-cancel'),
  askOk: $('ask-ok'),
  channelVideo: $('channel-video'),
  voiceInput: $('voice-input'),
  voiceOutput: $('voice-output'),
  voiceDeviceNote: $('voice-device-note'),
  voiceCamera: $('voice-camera'),
  voiceCam: $('voice-cam'),
  voiceScreen: $('voice-screen'),
  voiceMute: $('voice-mute'),
  voiceDeafen: $('voice-deafen'),
  voiceSoundboard: $('voice-soundboard'),
  voiceLeave: $('voice-leave'),
  voicePanel: $('voice-panel'),
  voiceState: $('voice-state'),
  voiceWhere: $('voice-where'),
  peerMenu: $('peer-menu'),
  peerMenuAvatar: $('peer-menu-avatar'),
  peerMenuName: $('peer-menu-name'),
  peerMenuBody: $('peer-menu-body'),

  pickerUsername: $('picker-username'),
  pickerBack: $('picker-back'),
  pickerRefresh: $('picker-refresh'),
  tabs: document.querySelectorAll('.tab'),
  sourceGrid: $('source-grid'),
  quality: $('quality'),
  priority: $('priority'),
  fallbackField: $('fallback-field'),
  fallback: $('window-audio-fallback'),
  excludeField: $('exclude-field'),
  excludeApp: $('exclude-app'),
  audioInputField: $('audio-input-field'),
  audioInput: $('audio-input'),
  liveQuality: $('live-quality'),
  livePriority: $('live-priority'),
  changeSource: $('change-source'),
  monitorToggle: $('monitor-toggle'),
  audioNote: $('audio-note'),
  startStream: $('start-stream'),

  broadcastTitle: $('broadcast-title'),
  viewerCount: $('viewer-count'),
  stopStream: $('stop-stream'),
  preview: $('preview'),
  broadcastStats: $('broadcast-stats'),
  broadcastLimit: $('broadcast-limit'),
  broadcastAudioNote: $('broadcast-audio-note'),
  broadcastWatch: $('broadcast-watch'),
  togglePreview: $('toggle-preview'),
  previewOff: $('preview-off'),
  previewOffTitle: $('preview-off-title'),
  previewOffText: $('preview-off-text'),

  watchAll: $('watch-all'),
  mosaicGrid: $('mosaic-grid'),
  mosaicCount: $('mosaic-count'),
  mosaicLeave: $('mosaic-leave'),
  mosaicMute: $('mosaic-mute'),
  mosaicVolume: $('mosaic-volume'),
  mosaicVolumeLabel: $('mosaic-volume-label'),
  mosaicAdd: $('mosaic-add'),

  addStream: $('add-stream'),
  addStreamItems: $('add-stream-items'),
  addStreamEmpty: $('add-stream-empty'),
  addStreamClose: $('add-stream-close'),

  clipsEnabled: $('clips-enabled'),
  hwEncoding: $('hw-encoding'),
  hwEncodingNote: $('hw-encoding-note'),
  gpuPreferenceField: $('gpu-preference-field'),
  gpuPreference: $('gpu-preference'),
  gpuHint: $('gpu-hint'),
  gpuHintText: $('gpu-hint-text'),
  broadcastClip: $('broadcast-clip'),
  watchClip: $('watch-clip'),

  testConnection: $('test-connection'),
  diag: $('diag'),
  diagSteps: $('diag-steps'),
  diagVerdict: $('diag-verdict'),
  diagClose: $('diag-close'),

  watchAdd: $('watch-add'),
  watchFullscreen: $('watch-fullscreen'),
  stage: document.querySelector('#view-watch .stage'),
  // Fullscreen goes on the whole view, not just the stage, so the real
  // controls come with it -- otherwise fullscreen would need a second copy of
  // every button, and the two would drift apart.
  watchView: $('view-watch'),

  watchTitle: $('watch-title'),
  watchDot: $('watch-dot'),
  leaveStream: $('leave-stream'),
  remote: $('remote'),
  watchWaiting: $('watch-waiting'),
  watchWaitingText: $('watch-waiting-text'),
  togglePlay: $('toggle-play'),
  toggleMute: $('toggle-mute'),
  volume: $('volume'),
  volumeLabel: $('volume-label'),
  watchStats: $('watch-stats'),
};

/**
 * A blank mosaic.
 *
 * `selection` null means "show whatever is live"; a Set means the user picked
 * specific streams and new ones should not barge in. `closed` holds names the
 * user dismissed by hand, which is what stops the next sync from cheerfully
 * reopening them three seconds later. `maximized` is one tile filling the grid
 * -- still inside the window, unlike fullscreen.
 */
function freshMosaic() {
  return {
    tiles: new Map(),
    selection: null,
    closed: new Set(),
    maximized: null,
    master: { volume: 1, muted: false },
    iceServers: null,
  };
}

/** Everything mutable about the current session. */
const state = {
  settings: null,
  /** Who is signed in, and what this server supports. See AUTH_DEFAULTS. */
  auth: { ...AUTH_DEFAULTS },

  /**
   * Channels, and the voice channel we are in (if any).
   *
   * `list` and `occupancy` are mirrors of server pushes -- never edited
   * locally, so there is nothing to reconcile when a push arrives.
   */
  channels: {
    list: [],
    occupancy: {},
    /** The roster of the channel WE are in. */
    roster: [],
    /**
     * Every channel's roster, keyed by channel id.
     *
     * The server already broadcasts voice:roster for every channel to every
     * client, not just to that channel's members -- it has to, or a sidebar
     * could never show occupancy. Keeping the whole roster rather than only
     * its length is what lets the sidebar name the people in a channel you
     * are not in, which is the entire point of a sidebar.
     */
    rosters: {},
    joining: false,
  },

  /**
   * Everyone with an account, by id.
   *
   * Kept because the voice roster and chat messages carry a user id and a
   * nickname but not a picture -- the picture can change mid-session, and
   * denormalising it into every roster push would mean a stale avatar on every
   * screen until the next one. One map, updated by `user:updated`.
   */
  users: new Map(),

  /**
   * Where a screen share goes: null for the flat `<nickname>` namespace, or a
   * channel's own `vc-<cid>-<mid>-s` path.
   */
  share: { target: null },
  chat: { channelId: null, messages: [], pinned: [], searching: false, pendingFile: null },
  soundpad: { clips: [] },
  /** The webcam publish, which is a SECOND stream under `<nickname>-cam`. */
  camera: { stream: null, publication: null, session: null },
  voice: new VoiceSession(),
  audioAvailable: false,
  audioUnavailableReason: null,

  sources: [],
  cameras: [],
  audioInputs: [],
  processes: [],
  activeKind: 'screen',
  selectedSource: null,
  /** True while the picker is being used to swap the source of a live stream. */
  changingSource: false,

  /** Live broadcast: what we are sending and the senders to swap it on. */
  live: { videoSender: null, videoTrack: null, rawStream: null, source: null, audioMode: null },

  /**
   * Whether the preview is being painted, and what to paint.
   *
   * `hiddenByUser` is a deliberate choice and survives minimise/restore;
   * `windowVisible` is the automatic half. `stream` holds the downscaled copy
   * from previewCopy() -- never the published stream, which is far too
   * expensive to paint.
   */
  preview: { hiddenByUser: false, windowVisible: true, stream: null },

  server: '', // control server base URL, valid outside a session too
  session: null, // server response for the current username

  /** Set once the server says it wants a password. */
  passwordRequired: false,

  /** GPU encode/decode capability, from the main process. */
  gpu: null,

  /**
   * Single-stream watching. Kept here rather than read off the element because
   * the element is permanently muted -- gain.js owns what you actually hear.
   */
  watch: { volume: 1, muted: false, sink: null },

  /** Mosaic mode: one WHEP connection per tile. See freshMosaic(). */
  mosaic: freshMosaic(),

  // Last publish token this client was issued, so a retry on the same name
  // reclaims it instead of being told the name is taken by itself.
  lastClaim: null,
  pc: null,
  resourceUrl: null,
  localStream: null,
  bridge: new AudioBridge(),
  statsReader: null,

  /**
   * Rolling clip buffers, off unless the user opts in.
   * `own` follows the outgoing broadcast; `watch` the single-stream viewer;
   * mosaic tiles keep their own on each entry.
   */
  clips: { enabled: false, own: null, watch: null },

  /** @type {Record<string, number[]>} keyed by activity: session, watch, mosaic */
  timers: {},
};

function showView(id) {
  el.views.forEach((v) => {
    if (v.id === id) v.setAttribute('data-active', '');
    else v.removeAttribute('data-active');
  });
}

/**
 * Timers are grouped so one activity can be stopped without touching another.
 *
 * This matters once a broadcaster can open the mosaic while still live: closing
 * the mosaic must not cancel the heartbeat holding their username, or the stats
 * poller behind their own broadcast.
 */
function addTimer(handle, group = 'session') {
  (state.timers[group] ??= []).push(handle);
  return handle;
}

function clearTimers(group) {
  const groups = group ? [group] : Object.keys(state.timers);
  for (const name of groups) {
    for (const handle of state.timers[name] ?? []) {
      clearInterval(handle);
      clearTimeout(handle);
    }
    state.timers[name] = [];
  }
}

function showError(message) {
  el.connectError.textContent = message;
  el.connectError.hidden = !message;
}

// ---------------------------------------------------------------------------
// Boot
// ---------------------------------------------------------------------------

async function boot() {
  state.settings = await harmony.settings.get();
  el.serverUrl.value = state.settings.serverUrl;
  el.username.value = state.settings.username;
  el.password.value = state.settings.password ?? '';
  // Main holds the password for every request it makes; hand back what was
  // saved before anything asks the server for anything.
  await harmony.api.setPassword(el.password.value);

  el.rememberAccount.checked = state.settings.rememberAccount !== false;
  if (state.settings.sessionToken) {
    // Restore before probeServer, so /api/health is already authenticated and
    // a still-valid token skips the login form entirely.
    state.auth.token = state.settings.sessionToken;
    await harmony.api.setSessionToken(state.auth.token);
  }
  el.fallback.value = state.settings.windowAudioFallback;
  state.clips.enabled = Boolean(state.settings.clipsEnabled);
  el.clipsEnabled.checked = state.clips.enabled;

  for (const [key, preset] of Object.entries(QUALITY)) {
    const option = document.createElement('option');
    option.value = key;
    option.textContent = preset.label;
    el.quality.append(option);
  }
  el.quality.value = state.settings.quality in QUALITY ? state.settings.quality : 'balanced';
  el.liveQuality.replaceChildren(...[...el.quality.options].map((o) => o.cloneNode(true)));
  el.liveQuality.value = el.quality.value;

  for (const [key, mode] of Object.entries(PRIORITY)) {
    const option = document.createElement('option');
    option.value = key;
    option.textContent = mode.label;
    el.priority.append(option);
  }
  el.priority.value = state.settings.priority in PRIORITY ? state.settings.priority : 'sharp';
  el.livePriority.replaceChildren(...[...el.priority.options].map((o) => o.cloneNode(true)));
  el.livePriority.value = el.priority.value;

  const availability = await harmony.audio.availability();
  state.audioAvailable = availability.available;
  state.audioUnavailableReason = availability.reason;

  await refreshGpuStatus();

  if (state.settings.serverUrl) {
    await probeServer();
    refreshLiveList();
  }
}

/**
 * What the GPU is doing, and what the user has asked for.
 *
 * Chromium will not tell us which encoder a given stream ended up on -- the
 * `encoderImplementation` stat is in the spec but absent from this Electron
 * build. So report the capability, which is the honest thing we can know, and
 * say plainly when the user has turned it off themselves.
 */
async function refreshGpuStatus() {
  try {
    state.gpu = await harmony.gpu.status();
  } catch {
    state.gpu = null;
    return;
  }
  const { encodeAccelerated, decodeAccelerated, preference, videoEncode, adapters } = state.gpu;
  el.hwEncoding.checked = preference !== 'off';

  // Only worth offering where there is a second GPU to move to.
  const multiGpu = (adapters?.length ?? 0) > 1;
  const active = adapters?.find((a) => a.active)?.vendor ?? null;
  el.gpuPreferenceField.hidden = !multiGpu;
  if (multiGpu) {
    el.gpuPreference.value = state.gpu.adapterPreference ?? 'auto';
    el.gpuPreferenceField.querySelector('small').textContent =
      `Currently on ${active ?? 'an unknown GPU'}, of ${adapters.map((a) => a.vendor).join(' + ')}. ` +
      'Leave this alone unless you are troubleshooting: the discrete GPU is normally the right ' +
      'choice, because it is where the game being captured already lives.';
  }

  /*
   * Say so unprompted, because nobody would think to look for this, and the
   * fix is outside Harmony entirely.
   *
   * Chromium cannot composite across GPUs on Windows. On a hybrid laptop the
   * display is usually wired to the integrated GPU while Harmony renders on the
   * discrete one, so every frame of Harmony's window is copied between
   * adapters before the desktop compositor can draw it. Measured on a hybrid
   * laptop: the compositor alone cost 25% of a GPU, and dropped to 1.5% once
   * the panel was wired straight to the discrete GPU. See DUAL_GPU_WEIRDNESS.md.
   *
   * There is no switch Harmony can flip for this -- it is a firmware or driver
   * setting (MUX switch, NVIDIA Advanced Optimus, "Display mode" in Armoury
   * Crate / Lenovo Vantage). So this is a warning, not an offer.
   */
  const showHint = multiGpu;
  el.gpuHint.hidden = !showHint;
  if (showHint) {
    el.gpuHintText.textContent =
      `This machine has two GPUs (${adapters.map((a) => a.vendor).join(' + ')}). If your screen is ` +
      'wired to the integrated one, every frame Harmony draws is copied between GPUs before it ' +
      'reaches the display, which costs roughly 10% of the machine while you stream. Look for a ' +
      'MUX switch, NVIDIA Advanced Optimus, or a "display mode" setting in your laptop vendor\'s ' +
      'utility, and point the panel at the discrete GPU. Worth doing once — it is the single ' +
      'largest thing you can change.';
  }

  if (preference === 'off') {
    el.hwEncodingNote.textContent =
      'Off — encoding on the CPU. Turn this back on unless viewers saw a corrupt picture.';
  } else if (encodeAccelerated) {
    el.hwEncodingNote.textContent = `On — your GPU is encoding${
      decodeAccelerated ? ' and decoding' : ''
    }. NVENC, AMF or Quick Sync, whichever your driver provides.`;
  } else {
    el.hwEncodingNote.textContent = `No GPU encoder available (${videoEncode}) — falling back to software H.264, which is slower but works everywhere.`;
  }
}

/**
 * A short label for the stats line, saying what is really encoding.
 *
 * Prefers what the connection reports over what the GPU process advertises.
 * `getGPUFeatureStatus()` describes ordinary media playback and is no guide at
 * all to WebRTC: it said `video_encode: enabled` for months while every call
 * ran on the CPU because of the negotiated H.264 profile. `encoderImplementation`
 * is only populated when a real hardware encoder is running, so it cannot lie
 * in the same direction.
 */
function encoderLabel(stats) {
  if (stats?.implementation) {
    const vendor = /NVIDIA/i.test(stats.implementation)
      ? 'NVENC'
      : /AMD|AMF/i.test(stats.implementation)
        ? 'AMF'
        : /Intel|Quick/i.test(stats.implementation)
          ? 'Quick Sync'
          : 'GPU';
    return `${vendor} encode`;
  }
  if (!state.gpu) return null;
  if (state.gpu.preference === 'off') return 'CPU encode';
  // No implementation named while a stream is running means software, whatever
  // the GPU process claims it is capable of.
  return stats ? 'CPU encode' : null;
}

/**
 * Ask the server whether it wants a password, and show the field if it does.
 *
 * /api/health is the one endpoint outside the password gate, precisely so this
 * question can be asked before the user is prompted. A server that is simply
 * unreachable leaves the field as it is rather than hiding a password the user
 * already typed.
 */
async function probeServer() {
  const server = el.serverUrl.value.trim();
  if (!server) return;
  try {
    const health = await harmony.api.health(server);
    el.passwordField.hidden = !health.passwordRequired;
    state.passwordRequired = Boolean(health.passwordRequired);

    // `hasAccounts` is absent on a pre-accounts server, which is exactly how we
    // tell the two apart -- undefined means "this server has no account system",
    // so the whole block stays hidden and the single-box flow is unchanged.
    // It is also absent while unauthenticated, since /api/health withholds its
    // details until the shared password is right.
    state.auth.supported = health.hasAccounts !== undefined;
    state.auth.hasAccounts = Boolean(health.hasAccounts);
    state.auth.needsOwner = Boolean(health.needsOwner);

    // An empty server has nobody to log in as, so offer registration first.
    if (state.auth.supported && !state.auth.hasAccounts) state.auth.mode = 'register';
    await restoreSession();
    applyAuthMode();
  } catch {
    // Unreachable, or an older server with no passwordRequired field. Either
    // way, do not change what the user can see.
  }
}

/**
 * Show the account fields in the mode we are actually in.
 *
 * Registering and logging in are deliberately NOT inferred from whether the
 * nickname exists: the server answers "wrong nickname or password" to both, on
 * purpose, so that this screen cannot be used to enumerate who has an account.
 * That means the user has to say which they meant, and auto-creating an account
 * on a mistyped password would be the worst possible guess.
 */
/**
 * Turn a saved session token back into a signed-in user.
 *
 * "Stay signed in" has always SAVED the token -- boot() restores it and hands
 * it to main for every request. What it never did was tell the client who
 * that token belongs to, and the connect screen gates on `state.auth.user`,
 * which only authenticate() ever set. So a saved session still demanded the
 * password, and the setting looked like it was not saving anything.
 *
 * One call to /api/accounts/me closes that. A token the server no longer
 * accepts is dropped here rather than left to fail later with something
 * confusing.
 */
async function restoreSession() {
  if (!state.auth.token || !state.auth.supported || state.auth.user) return;
  try {
    const { user } = await harmony.api.me(el.serverUrl.value.trim());
    state.auth.user = user;
    el.username.value = user.nickname;
  } catch (err) {
    // 401 means expired or revoked -- an ordinary thing, not an error worth
    // showing. Anything else (server down) leaves the token alone so a
    // reachable server can still honour it later.
    if (err.status === 401 || err.code === 'login_required') await adoptSession('');
  }
}

function applyAuthMode() {
  const on = state.auth.supported;
  // Nothing to type when we are already signed in: the fields would only
  // invite somebody to re-enter a password they do not need.
  const signedIn = Boolean(state.auth.user);
  el.accountFields.hidden = !on || signedIn;
  document.body.dataset.authMode = on ? state.auth.mode : 'none';

  if (signedIn) {
    el.usernameHint.textContent = `Signed in as ${state.auth.user.nickname}.`;
    el.authModeText.textContent = 'Not you?';
    el.authModeToggle.textContent = 'Sign out';
    el.continue.textContent = 'Continue';
    return;
  }

  if (!on) {
    el.usernameHint.textContent =
      'Free name \u2192 you start streaming. Name already live \u2192 you join and watch.';
    // Deliberately no early return: the mode-dependent labels below are kept up
    // to date even while the block is hidden, so the fields are already correct
    // the moment a server reveals them.
  }

  const registering = state.auth.mode === 'register';
  el.accountPasswordLabel.textContent = registering ? 'Choose a password' : 'Your password';
  el.accountPassword.placeholder = registering ? 'At least 6 characters' : '';
  el.accountConfirmField.hidden = !registering;
  el.ownerKeyField.hidden = !(registering && state.auth.needsOwner);
  // Guarded, unlike everything else here: this hint sits OUTSIDE
  // #account-fields and means something different on a server with no
  // accounts, where the block above has already set it.
  if (on) {
    el.usernameHint.textContent = registering
      ? 'This becomes your permanent name. 2-20 characters, and it is what your stream is called.'
      : 'The nickname you registered with.';
  }
  el.authModeText.textContent = registering ? 'Already registered?' : 'No account yet?';
  el.authModeToggle.textContent = registering ? 'Sign in' : 'Create one';
  el.continue.textContent = registering ? 'Create account' : 'Continue';
}

/** Hand the token to main, and persist it only if they asked us to. */
async function adoptSession(token) {
  state.auth.token = token ?? '';
  await harmony.api.setSessionToken(state.auth.token);
  await harmony.settings.set({
    sessionToken: el.rememberAccount.checked ? state.auth.token : '',
    rememberAccount: el.rememberAccount.checked,
  });
}

/**
 * Log in or register, depending on the mode. Returns the account, or throws
 * with a message already fit to show.
 */
async function authenticate(server, nickname) {
  const registering = state.auth.mode === 'register';
  const password = el.accountPassword.value;
  const ownerKey = el.ownerKey.value.trim();

  if (registering && password !== el.accountConfirm.value) {
    throw new Error('The two passwords do not match.');
  }

  const result = registering
    ? await harmony.api.register(server, nickname, password, ownerKey)
    : await harmony.api.login(server, nickname, password, ownerKey);

  await adoptSession(result.token);
  state.auth.user = result.user;

  // Say so rather than silently making them a member -- someone pasting a key
  // that does not work needs to know before they wonder why they cannot
  // create channels.
  if (result.ownerKeyRejected) {
    showError('That owner key was not accepted, so this account is an ordinary member.');
  } else if (result.ownerClaimed) {
    showError('');
  }
  return result.user;
}

// ---------------------------------------------------------------------------
// Connect view
// ---------------------------------------------------------------------------

async function refreshLiveList() {
  const server = el.serverUrl.value.trim();
  if (!server) return;
  // Nothing to show until there is a password to show it with, and asking
  // anyway would only produce a 401 on every poll.
  if (state.passwordRequired && !el.password.value) {
    el.liveList.hidden = true;
    return;
  }

  try {
    const { streams } = await harmony.api.streams(server);
    el.liveItems.replaceChildren();

    if (!streams.length) {
      el.liveList.hidden = true;
      return;
    }
    el.passwordField.classList.remove('bad');

    for (const stream of streams) {
      const li = document.createElement('li');

      const dot = document.createElement('span');
      dot.className = 'dot live';

      const who = document.createElement('span');
      who.className = 'who';
      who.textContent = stream.username;

      const count = document.createElement('span');
      count.className = 'pill';
      count.textContent = `${stream.viewers} watching`;

      li.append(dot, who, count);
      li.addEventListener('click', () => {
        el.username.value = stream.username;
        startSession();
      });
      el.liveItems.append(li);
    }
    el.liveList.hidden = false;
  } catch (err) {
    el.liveList.hidden = true;
    // A password problem is the one failure here worth surfacing: without it
    // the list just silently stays empty and looks like "nobody is streaming".
    if (err.code === 'bad_password' || err.code === 'locked_out') {
      el.passwordField.hidden = false;
      el.passwordField.classList.add('bad');
      showError(err.message);
    }
  }
}

async function startSession() {
  const server = el.serverUrl.value.trim();
  const username = normalizeName(el.username.value);

  if (!server) return showError('Enter the address of your Harmony server.');
  if (!username) return showError('Pick a username.');

  showError('');
  el.continue.disabled = true;
  el.continue.textContent = 'Connecting…';

  try {
    // Before anything else reaches the server, so the very first request of the
    // session already carries it.
    await harmony.api.setPassword(el.password.value);

    // A server we have not reached yet has not told us whether it has accounts.
    if (!state.auth.supported) await probeServer();

    /**
     * Mirror the server's own rule exactly.
     *
     * A server with an account system but no accounts yet still answers
     * anonymous claims -- that is what keeps a fresh install usable the moment
     * it starts, before anyone has registered. So signing in is REQUIRED only
     * once an account exists, and OPTIONAL (but honoured) before that, which is
     * how the first person registers at all.
     *
     * Getting this wrong in either direction is visible: too strict and a brand
     * new server cannot be used without registering first; too lax and the
     * impersonation hole /api/session was fixed for stays open on the client
     * side.
     */
    const mustSignIn = state.auth.supported && state.auth.hasAccounts;
    const wantsSignIn = state.auth.supported && el.accountPassword.value.length > 0;

    // `state.auth.user` is set either by a sign-in just now or by a saved
    // session restored in probeServer. Checking the password box instead of
    // this is what made "stay signed in" useless.
    if (mustSignIn && !state.auth.user && !el.accountPassword.value) {
      el.accountPassword.focus();
      throw new Error('This server has accounts. Enter your password, or create an account.');
    }

    if ((mustSignIn || wantsSignIn) && !state.auth.user) {
      await authenticate(server, username);
    }

    const held = state.lastClaim?.username === username ? state.lastClaim.token : undefined;
    const session = await harmony.api.session(server, username, held);
    state.session = { ...session, server };
    if (session.token) state.lastClaim = { username, token: session.token };
    el.passwordField.classList.remove('bad');

    await harmony.settings.set({ serverUrl: server, username, password: el.password.value });
    state.settings = await harmony.settings.get();

    // A signed-in user gets the lobby; everyone else keeps the original
    // straight-to-your-stream flow, which is what an account-less server and
    // every 1.0.0 client still do.
    if (state.auth.user) {
      state.server = server;
      await enterChannels();
    } else if (session.role === 'broadcaster') {
      await enterPicker();
    } else {
      await enterWatch();
    }
  } catch (err) {
    showError(err.message);
    if (err.code === 'bad_password' || err.code === 'locked_out') {
      el.passwordField.hidden = false;
      el.passwordField.classList.add('bad');
      el.password.focus();
      el.password.select();
    }
    if (err.code === 'bad_credentials' || err.code === 'weak_password') {
      el.accountPassword.focus();
      el.accountPassword.select();
    }
    if (err.code === 'nickname_taken') {
      // They meant to sign in. Put them there rather than making them find the
      // link themselves.
      state.auth.mode = 'login';
      applyAuthMode();
      el.accountPassword.focus();
    }
    // A failed login must not leave a half-adopted session behind.
    if (state.auth.supported && !state.auth.user) await adoptSession('');
  } finally {
    el.continue.disabled = false;
    el.continue.textContent =
      state.auth.supported && state.auth.mode === 'register' ? 'Create account' : 'Continue';
  }
}

// ---------------------------------------------------------------------------
// Channels and voice
// ---------------------------------------------------------------------------

function showChannelsError(message) {
  el.channelsError.textContent = message;
  el.channelsError.hidden = !message;
}

const isAdmin = () => state.auth.user?.role === 'owner' || state.auth.user?.role === 'admin';

/* Asking the person something ------------------------------------------
 *
 * Electron does not implement window.prompt(). Not discouraged -- absent,
 * and it throws. So every prompt() in this file was code that could never
 * run, and the features behind them (naming a channel, naming a soundpad
 * clip, entering a channel password, renaming) were all unreachable the
 * moment anybody pressed the button.
 *
 * They survived every test because the UI suite STUBBED window.prompt
 * before clicking. Replacing a platform function in a test is how you end
 * up proving your code works against a platform you do not have. Nothing is
 * stubbed now; the test drives this dialog.
 *
 * One dialog, reused: a title, some fields, OK and Cancel. It resolves with
 * the values, or null if they backed out.
 */

/** @type {null | ((value: object | null) => void)} */
let askResolve = null;

function closeAsk(value) {
  const resolve = askResolve;
  askResolve = null;
  try { el.ask.close(); } catch { /* already closed */ }
  el.askFields.replaceChildren();
  el.askError.hidden = true;
  resolve?.(value);
}

/**
 * @param {{title: string, text?: string, okLabel?: string,
 *          fields?: Array<{name: string, label: string, type?: string,
 *                          value?: string, placeholder?: string,
 *                          options?: Array<{value: string, label: string}>,
 *                          required?: boolean}>}} spec
 * @returns {Promise<Record<string, string> | null>}
 */
function ask(spec) {
  // A second question while one is open would orphan the first promise.
  if (askResolve) closeAsk(null);

  el.askTitle.textContent = spec.title;
  el.askText.textContent = spec.text ?? '';
  el.askText.hidden = !spec.text;
  el.askOk.textContent = spec.okLabel ?? 'OK';

  el.askFields.replaceChildren(...(spec.fields ?? []).map((field) => {
    const label = document.createElement('label');
    label.className = 'field';

    const caption = document.createElement('span');
    caption.textContent = field.label;
    label.append(caption);

    let input;
    if (field.options) {
      input = document.createElement('select');
      input.append(...field.options.map((o) => new Option(o.label, o.value)));
    } else {
      input = document.createElement('input');
      input.type = field.type ?? 'text';
      input.placeholder = field.placeholder ?? '';
      input.autocomplete = 'off';
    }
    input.name = field.name;
    input.value = field.value ?? '';
    if (field.required) input.required = true;
    label.append(input);
    return label;
  }));

  el.ask.showModal();
  // Focus the first field: answering without reaching for the mouse is most
  // of why a prompt was reached for in the first place.
  el.askFields.querySelector('input, select')?.focus();

  return new Promise((resolve) => {
    askResolve = resolve;
  });
}

/** Yes or no, in the same dialog, so nothing depends on window.confirm. */
async function askConfirm(title, { text, okLabel = 'Delete' } = {}) {
  return (await ask({ title, text, okLabel, fields: [] })) !== null;
}

/** Profile pictures ------------------------------------------------------ */

/** Square side we store an avatar at, and the ceiling the server enforces. */
const AVATAR_PX = 256;
const AVATAR_MAX_BYTES = 256 * 1024;

const knownUser = (id) => state.users.get(id) ?? null;

/**
 * One avatar: the picture if there is one, initials if there is not.
 *
 * `harmony://app/media/<hash>` is same-origin, so the CSP's `img-src 'self'`
 * covers it with no change, and the main process downloads and verifies the
 * file the first time the element asks for it.
 */
function avatarEl(user, extraClass = '') {
  const span = document.createElement('span');
  span.className = `avatar ${extraClass}`.trim();
  if (user?.avatarHash) {
    const img = document.createElement('img');
    img.src = harmony.mediaUrl(user.avatarHash);
    img.alt = '';
    span.append(img);
  } else {
    span.textContent = String(user?.nickname ?? '?').slice(0, 2).toUpperCase();
  }
  return span;
}

/**
 * Repaint our own picture in the bar.
 *
 * Moves the children out of a throwaway avatarEl rather than building the
 * markup a second way, so the bar can never drift from the rows.
 */
function renderOwnAvatar() {
  el.channelsAvatar.replaceChildren(...avatarEl(state.auth.user).childNodes);
}

/**
 * Everything the media cache must not evict, recomputed from scratch.
 *
 * media.keep REPLACES the set rather than adding to it, so each caller
 * working out its own half is a bug waiting to happen: whoever ran last wins
 * and the other half starts being evicted. Avatars made that concrete --
 * they are drawn on every screen constantly, so losing one means downloading
 * it again immediately. Hence one function, called from all three places that
 * change any part of it.
 */
function refreshKeepSet() {
  return harmony.media.keep([
    ...[...state.users.values()].map((u) => u.avatarHash).filter(Boolean),
    ...state.soundpad.clips.map((c) => c.hash),
    ...state.chat.pinned.map((m) => m.attachmentHash).filter(Boolean),
  ]).catch(() => { /* the cache is a cache */ });
}

/** Pull the roster so every id in a message or a roster row has a picture. */
async function refreshUsers() {
  try {
    const { users } = await harmony.api.roster(state.server);
    state.users = new Map(users.map((u) => [u.id, u]));
    await refreshKeepSet();
  } catch {
    // Not fatal: without it everyone simply shows initials.
  }
}

/**
 * Downscale a picked image and upload it.
 *
 * Resized here rather than on the server, which is what keeps the server free
 * of an image library. A 256-pixel square JPEG is a few kilobytes, so the
 * server's cap is a backstop against somebody posting a raw photo through the
 * API rather than something this path ever hits.
 */
async function setOwnAvatar(file) {
  try {
    showChannelsError('');
    const bitmap = await createImageBitmap(file);
    const canvas = document.createElement('canvas');
    canvas.width = AVATAR_PX;
    canvas.height = AVATAR_PX;
    const ctx = canvas.getContext('2d');
    // Cover rather than fit: a letterboxed avatar in a circle looks broken.
    const scale = Math.max(AVATAR_PX / bitmap.width, AVATAR_PX / bitmap.height);
    const w = bitmap.width * scale;
    const h = bitmap.height * scale;
    ctx.drawImage(bitmap, (AVATAR_PX - w) / 2, (AVATAR_PX - h) / 2, w, h);
    bitmap.close();

    const encode = (quality) =>
      new Promise((done) => canvas.toBlob(done, 'image/jpeg', quality));
    let blob = await encode(0.85);
    if (blob && blob.size > AVATAR_MAX_BYTES) blob = await encode(0.6);
    if (!blob) throw new Error('Could not read that image.');
    if (blob.size > AVATAR_MAX_BYTES) throw new Error('That picture is too detailed to shrink.');

    const bytes = new Uint8Array(await blob.arrayBuffer());
    const upload = await harmony.media.upload(state.server, bytes, 'image/jpeg');
    const { user } = await harmony.api.setAvatar(state.server, upload.hash);
    state.auth.user = user;
    state.users.set(user.id, user);
    renderOwnAvatar();
    refreshKeepSet();
  } catch (err) {
    showChannelsError(err.message);
  }
}

/**
 * Open the realtime socket and show the lobby.
 *
 * Only ever reached by a signed-in user: an anonymous client on a server with
 * no accounts keeps the original single-box flow, which is what lets a 1.0.0
 * deployment and its tests carry on unchanged.
 */
async function enterChannels() {
  el.channelsWho.textContent = state.auth.user.nickname;
  el.channelsRole.textContent = state.auth.user.role === 'member' ? '' : state.auth.user.role;
  el.channelAdd.hidden = !isAdmin();
  // The media route serves `harmony://app/media/<hash>` -- a hash and nothing
  // else, which is what lets it be used straight in an <img src>. Main needs
  // to be told separately where to download from.
  await harmony.media.setServer(state.server);
  showChannelsError('');
  renderOwnAvatar();
  await refreshUsers();
  loadSoundpad();
  showView('view-channels');

  try {
    const hello = await harmony.realtime.connect(state.server, state.auth.token);
    state.channels.list = hello.channels ?? [];
    state.channels.occupancy = hello.occupancy ?? {};
    // A client that just connected has missed every roster broadcast, so the
    // whole picture arrives once with the hello.
    state.channels.rosters = hello.rosters ?? {};
  } catch (err) {
    showChannelsError(`Live updates unavailable: ${err.message}`);
    // Fall back to the REST list so the lobby is still usable read-only.
    try {
      const { channels, occupancy, rosters } = await harmony.api.channels(state.server);
      state.channels.list = channels;
      state.channels.occupancy = occupancy ?? {};
      state.channels.rosters = rosters ?? {};
    } catch { /* nothing more to try */ }
  }
  renderChannels();
}

/**
 * Move one channel one place and send the whole resulting order.
 *
 * The server takes a complete permutation rather than "move X to N", so two
 * admins reordering at once cannot interleave into an order neither of them
 * chose. That makes this the client's job: work out the list we want, send it.
 */
async function nudgeChannel(id, delta) {
  const ids = state.channels.list.map((c) => c.id);
  const from = ids.indexOf(id);
  const to = from + delta;
  if (from < 0 || to < 0 || to >= ids.length) return;
  ids.splice(to, 0, ...ids.splice(from, 1));
  try {
    await harmony.api.reorderChannels(state.server, ids);
    // The server broadcasts the new list; nothing is applied locally.
  } catch (err) {
    showChannelsError(err.message);
  }
}

async function editChannel(channel) {
  const answer = await ask({
    title: `Edit #${channel.name}`,
    text: 'Leave the password empty to remove it.',
    okLabel: 'Save',
    fields: [
      { name: 'name', label: 'Name', value: channel.name, required: true },
      { name: 'password', label: 'Password', type: 'password', placeholder: 'No password' },
    ],
  });
  if (!answer) return;
  try {
    await harmony.api.updateChannel(state.server, channel.id, {
      name: answer.name,
      password: answer.password,
    });
  } catch (err) {
    showChannelsError(err.message);
  }
}

/** A small admin button that must not also trigger the row's own click. */
function rowButton(label, title, onClick) {
  const button = document.createElement('button');
  button.className = 'ghost tiny';
  button.textContent = label;
  button.title = title;
  button.addEventListener('click', (event) => {
    event.stopPropagation();
    onClick();
  });
  return button;
}

function renderChannels() {
  const items = state.channels.list.map((channel, index) => {
    const li = document.createElement('li');
    // Named, because the member list under a channel is an <li> too and
    // "every li in the sidebar" stopped meaning "every channel".
    li.className = 'channel-row';
    li.dataset.id = String(channel.id);
    if (channel.id === state.voice.channelId) li.setAttribute('data-active', '');

    const kind = document.createElement('span');
    kind.className = 'kind';
    kind.textContent = channel.kind === 'voice' ? '\u{1F50A}' : '#';

    const name = document.createElement('span');
    name.textContent = channel.name;

    li.append(kind, name);

    if (channel.locked) {
      const lock = document.createElement('span');
      lock.className = 'tag';
      lock.textContent = channel.unlocked ? 'unlocked' : 'locked';
      li.append(lock);
    }

    const occupants = state.channels.occupancy[channel.id] ?? 0;
    if (channel.kind === 'voice' && occupants) {
      const count = document.createElement('span');
      count.className = 'count';
      count.textContent = String(occupants);
      li.append(count);
    }

    if (isAdmin()) {
      const tools = document.createElement('span');
      tools.className = 'row-tools';
      const up = rowButton('\u25B2', 'Move up', () => nudgeChannel(channel.id, -1));
      const down = rowButton('\u25BC', 'Move down', () => nudgeChannel(channel.id, 1));
      up.disabled = index === 0;
      down.disabled = index === state.channels.list.length - 1;
      tools.append(
        up,
        down,
        rowButton('\u270E', 'Rename or set a password', () => editChannel(channel)),
        rowButton('\u2715', 'Delete this channel', async () => {
          if (!await askConfirm(`Delete #${channel.name}?`, {
            text: 'Everything written in it goes too. This cannot be undone.',
          })) return;
          try {
            await harmony.api.deleteChannel(state.server, channel.id);
          } catch (err) {
            showChannelsError(err.message);
          }
        }),
      );
      li.append(tools);
    }

    li.addEventListener('click', () => onChannelClick(channel));

    // Who is in this voice channel, under it, the way a sidebar shows it.
    //
    // Returned as a SECOND top-level node rather than nested inside the row:
    // the row has a click handler that joins the channel, and a nested list
    // would make every click on a member's name join it too.
    const members = state.channels.rosters[channel.id] ?? [];
    if (channel.kind !== 'voice' || members.length === 0) return [li];

    const list = document.createElement('li');
    list.className = 'channel-members';
    list.dataset.for = String(channel.id);
    list.append(...members.map((member) => {
      const row = document.createElement('span');
      row.className = 'channel-member';
      row.append(avatarEl(knownUser(member.userId) ?? { nickname: member.nickname }, 'tiny'));

      const name = document.createElement('span');
      name.className = 'member-name';
      name.textContent = member.nickname;
      row.append(name);

      // Exactly the indicators the voice pane shows, from the same helper --
      // the two lists are looked at side by side, and nothing is more
      // confusing than the same person reading differently in each.
      if (member.forceMuted) row.setAttribute('data-forced', '');
      else if (member.muted) row.setAttribute('data-muted', '');

      const status = document.createElement('span');
      status.className = 'status';
      renderStatus(status, member);
      row.append(status);

      // The same menu as the roster. Right-clicking somebody in the sidebar
      // is the natural thing to try, and an admin muting someone in a
      // channel they are not in is the main reason to want it.
      row.title = 'Right-click for volume and controls';
      row.addEventListener('contextmenu', (event) => {
        event.preventDefault();
        event.stopPropagation();
        openPeerMenu(channel.id, member.mid, event);
      });
      return row;
    }));

    return [li, list];
  });

  el.channelItems.replaceChildren(...items.flat());
}

async function onChannelClick(channel) {
  if (channel.kind !== 'voice') return openTextChannel(channel);
  // Clicking the channel you are already in is how you get back to the
  // streams after reading a text channel. It used to do nothing at all,
  // which left the chat covering the thing you were trying to watch with no
  // obvious way to put it away.
  if (channel.id === state.voice.channelId) {
    closeChat();
    return undefined;
  }
  return joinVoice(channel);
}

/**
 * Decide what the right-hand column is showing, and whether it exists.
 *
 * ONE function, called from everywhere that could change the answer, rather
 * than each caller toggling the two or three `hidden` flags it happens to
 * know about. The previous arrangement had joinVoice, closeChat,
 * openTextChannel and renderChannelVideo each setting a subset, which is why
 * opening a text channel during a screen share showed both.
 *
 * The stage holds the chat OR the mosaic, never both: they are two things
 * you look at, and the window is not big enough to look at two things.
 */
function applyStage() {
  const chatting = Boolean(state.chat.channelId);
  const tiles = el.channelVideo.childElementCount > 0;

  el.chatActive.hidden = !chatting;
  el.channelVideo.hidden = chatting || !tiles;

  const stage = chatting || tiles;
  el.channelStage.hidden = !stage;

  // The grid's column count comes off these, because an empty track still
  // reserves its width -- see the comment on .channels-layout.
  el.channelsLayout.toggleAttribute('data-stage', stage);
  el.channelsLayout.toggleAttribute('data-voice', !el.voiceActive.hidden);
}

async function joinVoice(channel, password) {
  if (state.channels.joining) return;
  state.channels.joining = true;
  showChannelsError('');

  try {
    if (state.voice.channelId) await leaveVoice({ silent: true });

    const reply = await harmony.realtime.request('voice:join', {
      channelId: channel.id,
      ...(password ? { password } : {}),
    });

    if (reply.type !== 'voice:joined') {
      if (reply.error === 'password_required' || reply.error === 'bad_password') {
        state.channels.joining = false;
        const answer = await ask({
          title: `${channel.name} needs a password`,
          text: reply.error === 'bad_password' ? 'That one was not right.' : '',
          okLabel: 'Join',
          fields: [{ name: 'password', label: 'Password', type: 'password', required: true }],
        });
        if (answer?.password) return joinVoice(channel, answer.password);
        return;
      }
      if (reply.error === 'channel_full') {
        return showChannelsError(`${channel.name} is full (${reply.cap} people).`);
      }
      return showChannelsError(`Could not join: ${reply.error}`);
    }

    state.voice.configure({
      channelId: channel.id,
      mid: reply.mid,
      token: reply.token,
      whepBase: reply.whepBase,
      // The WHIP URLs for this membership's own camera and screen paths. The
      // client never builds them: they are minted with the slot baked in, and
      // the auth hook refuses a publish whose slot does not match.
      publish: reply.publish,
      iceServers: state.session?.iceServers ?? state.mosaic?.iceServers ?? [],
    });

    // Joining a voice channel puts the streams back on the stage. Clicking a
    // voice channel is the only way back from a text channel, so leaving the
    // chat up here would make the stage a one-way door again.
    closeChat();

    el.voiceName.textContent = channel.name;
    el.voiceIdle.hidden = true;
    el.voiceActive.hidden = false;
    applyVoiceConnection(true);
    applyStage();

    await state.voice.startMic(
      reply.publish.voice,
      // The voice preference, not the capture-card one. See settings.js.
      deviceForVoiceInput(),
    );
    // Tell the server the path is live, so other members know to subscribe.
    await harmony.realtime.request('voice:publishing', {
      channelId: channel.id, kind: 'v', on: true,
    });

    applyVoiceButtons();
    renderVoiceRoster(reply.roster ?? []);
    renderChannelVideo();
    renderChannels();
    // Labels are only populated once a getUserMedia has been granted, which
    // startMic above has just done -- so this is the first moment the pickers
    // can show device names rather than blanks.
    await refreshVoiceDevices({ apply: false });
    await applyVoiceOutput(el.voiceOutput.value);

    // The speaking ring. 100 ms is the usual figure for this: slower and a
    // short word never lights it, faster and it costs more than it is worth
    // for something nobody can perceive.
    addTimer(setInterval(renderSpeaking, SPEAKING_POLL_MS), 'voice');

    // Renew the media tokens at half their life, so a missed tick still
    // leaves a wide margin and neither side has to trust the other's clock.
    const life = Number(reply.expiresInMs) || 10 * 60 * 1000;
    addTimer(
      setInterval(refreshVoiceTokens, Math.max(5_000, Math.floor(life / 2))),
      'voice',
    );

    // Reconcile subscriptions on a slow timer as well as on roster pushes.
    //
    // A publisher's path is not readable for a moment after its WHIP returns
    // 201 -- MediaMTX only marks it online once RTP arrives -- so an early
    // subscribe gets a 404. In a settled channel no further roster push ever
    // comes, so without this that peer is inaudible until somebody happens to
    // join or mute. Measured on a real 16-member channel.
    addTimer(setInterval(() => {
      if (!state.voice.channelId) return;

      // First drop anything that has quietly stopped delivering, so the
      // subscribe pass below sees it as missing and rebuilds it. Without
      // this, a peer whose path was rebuilt stays silent for ever.
      state.voice.reapStalled()
        .then((dead) => {
          if (dead.length) return state.voice.syncPeers();
          return undefined;
        })
        .catch(() => { /* the next tick tries again */ });

      if (state.voice.hasMissingPeers) {
        state.voice.syncPeers()
          .then((result) => {
            // A refused subscription is not the warm-up window: retrying it
            // with the same token would fail forever. Get a new one first.
            const refused = result?.missed?.some(
              (m) => m.reason === 'unauthorized' || m.reason === 'media_error',
            );
            if (refused) return refreshVoiceTokens();
            return undefined;
          })
          .catch(() => { /* retried on the next tick */ });
      }
      // Video has exactly the same warm-up window, and a camera turned on in
      // a settled channel produces one roster push -- which arrives before
      // the path is readable.
      if (state.voice.hasMissingVideo) {
        state.voice.syncVideo()
          .then((r) => { if (r.changed) renderChannelVideo(); })
          .catch(() => { /* retried on the next tick */ });
      }
    }, VOICE_RECONCILE_MS), 'voice');
  } catch (err) {
    showChannelsError(err.message);
    await leaveVoice({ silent: true }).catch(() => {});
  } finally {
    state.channels.joining = false;
  }
}

/**
 * Re-issue the channel's media tokens before they expire.
 *
 * A channel token lasts ten minutes by default and MediaMTX only consults
 * the auth hook when a session is SET UP. So an expired token never
 * interrupts anything already running -- it stops anything NEW. Ten minutes
 * into a call that means: you cannot hear anybody who joins or unmutes from
 * then on, you cannot start your camera, and a screen share fails with a
 * 401 that the client used to report as "the username reservation may have
 * expired".
 *
 * All three were the same missing timer. The server has always had the
 * handler and the client has always had retoken(); nothing called either.
 *
 * @returns {Promise<boolean>} whether the tokens are now fresh
 */
async function refreshVoiceTokens() {
  const channelId = state.voice.channelId;
  if (!channelId) return false;
  try {
    const reply = await harmony.realtime.request('voice:refresh', { channelId });
    if (reply.type !== 'voice:tokens') return false;
    state.voice.retoken(reply);
    return true;
  } catch {
    // The socket is down, which the reconnect path already handles by
    // rejoining -- and a rejoin issues fresh tokens anyway.
    return false;
  }
}

async function leaveVoice({ silent = false } = {}) {
  clearTimers('voice');
  const channelId = state.voice.channelId;
  // A screen share published into this channel has nowhere to go once we are
  // out of it, and its path stops being authorised the moment the slot is
  // released.
  if (state.share.target?.channelId === channelId) await teardown();
  await state.voice.leave();
  if (channelId && !silent) {
    await harmony.realtime.request('voice:leave', { channelId }).catch(() => {});
  }
  state.channels.roster = [];
  closePeerMenu();
  el.voiceActive.hidden = true;
  el.voiceIdle.hidden = state.chat.channelId !== null;
  renderChannelVideo();
  applyVoiceButtons();
  renderChannels();
}

function applyVoiceButtons() {
  el.voiceMute.textContent = state.voice.muted ? 'Unmute mic' : 'Mute mic';
  el.voiceMute.toggleAttribute('data-on', state.voice.muted);
  el.voiceDeafen.textContent = state.voice.deafened ? 'Undeafen' : 'Deafen';
  el.voiceDeafen.toggleAttribute('data-on', state.voice.deafened);
  el.voiceCam.textContent = state.voice.camLive ? 'Stop camera' : 'Start camera';
  el.voiceCam.toggleAttribute('data-on', state.voice.camLive);

  // Mute and deafen light up red rather than blue: they are the two that
  // mean something is NOT happening, and a lit button that means "off" is
  // how you end up talking to a room that cannot hear you.
  el.voiceMute.toggleAttribute('data-off-state', true);
  el.voiceDeafen.toggleAttribute('data-off-state', true);

  const sharing = state.share.target?.channelId === state.voice.channelId
    && Boolean(state.voice.channelId);
  el.voiceScreen.textContent = sharing ? 'Stop sharing' : 'Share screen here';
  el.voiceScreen.toggleAttribute('data-on', sharing);

  el.voiceSoundboard.toggleAttribute('data-on', !el.soundpad.hidden);

  const channel = state.channels.list.find((c) => c.id === state.voice.channelId);
  el.voiceWhere.textContent = channel ? channel.name : '';

  /*
   * The top-right share button goes away while you are in a call.
   *
   * It publishes to the FLAT namespace -- the one an old client watches
   * through the mosaic -- and the panel's button publishes into the channel.
   * Having both reachable at once meant picking the wrong one shared your
   * screen to a place the people you were talking to could not see, with no
   * feedback that anything was wrong except that nobody said anything.
   */
  el.channelsShare.hidden = Boolean(state.voice.channelId);
}

/** Connected, or trying to be. Driven by the socket, never by guesswork. */
function applyVoiceConnection(up) {
  el.voicePanel.dataset.state = up ? 'connected' : 'connecting';
  el.voiceState.textContent = up ? 'Voice connected' : 'Reconnecting\u2026';
}

/**
 * What somebody's state is, as small glyphs.
 *
 * One helper for the sidebar and the voice pane, so the two can never
 * disagree about what a person is doing -- which they would within a week if
 * each built its own.
 *
 * Muted and admin-muted are deliberately the SAME glyph in different colours
 * rather than two different symbols: they mean the same thing to a listener
 * (this person is not audible), and the difference is who decided, which is
 * what the colour and the tooltip carry.
 */
const STATUS = [
  { key: 'forced', glyph: '\u{1F507}', title: 'muted by an admin' },
  { key: 'muted', glyph: '\u{1F507}', title: 'muted themselves' },
  { key: 'cam', glyph: '\u{1F4F7}', title: 'camera on' },
  { key: 'screen', glyph: '\u{1F5A5}', title: 'sharing a screen' },
];

function statusKeys(member) {
  const keys = [];
  if (member.forceMuted) keys.push('forced');
  else if (member.muted) keys.push('muted');
  if (member.publishing?.includes('c')) keys.push('cam');
  if (member.publishing?.includes('s')) keys.push('screen');
  return keys;
}

/** Fill a container with the glyphs for this member, reusing it in place. */
function renderStatus(container, member) {
  const keys = statusKeys(member);
  container.replaceChildren(...keys.map((key) => {
    const { glyph, title } = STATUS.find((x) => x.key === key);
    const span = document.createElement('span');
    span.className = 'status-dot';
    span.dataset.kind = key;
    span.textContent = glyph;
    span.title = title;
    return span;
  }));
  container.hidden = keys.length === 0;
}

/**
 * Build one roster row.
 *
 * Split from the update below because rows are REUSED: a volume slider
 * rebuilt underneath a finger stops moving, and a roster push arrives every
 * time anybody mutes. Only the parts that change are rewritten.
 */
function voiceRow(member) {
  const li = document.createElement('li');
  li.dataset.mid = String(member.mid);

  li.append(avatarEl(knownUser(member.userId) ?? { nickname: member.nickname }));

  const name = document.createElement('span');
  name.className = 'member-name';
  li.append(name);

  const status = document.createElement('span');
  status.className = 'status';
  li.append(status);

  /*
   * Everything you can do TO somebody is behind a right-click.
   *
   * It used to be on the row: a local mute, a volume slider, a percentage, a
   * force-mute button and a move dropdown, per person. Five controls and a
   * name in a column that is now a third of the window is not a layout, and
   * the slider in particular ended up about two centimetres wide.
   *
   * Nothing here captures the member object. Rows are reused across roster
   * pushes and slots are reused across joins, so the menu reads whoever the
   * row currently belongs to at the moment it is opened -- see the comment
   * on openPeerMenu.
   */
  li.title = 'Right-click for volume and controls';
  li.addEventListener('contextmenu', (event) => {
    event.preventDefault();
    openPeerMenu(state.voice.channelId, Number(li.dataset.mid), event);
  });

  return li;
}

/**
 * Show a volume, and show when it is doing something unusual.
 *
 * Past 100% the signal is being amplified rather than attenuated, which can
 * distort and is the first thing to suspect when somebody sounds bad. It is
 * worth making impossible to miss rather than leaving it to be read off a
 * slider position.
 */
function applyPeerVolumeLook(volume, label, percent, muted) {
  volume.value = String(percent);
  const boosted = percent > 100;
  volume.toggleAttribute('data-boosted', boosted && !muted);
  label.toggleAttribute('data-boosted', boosted && !muted);
  label.textContent = muted ? 'muted' : `${percent}%`;
  label.title = boosted && !muted
    ? 'Louder than the original. Amplified audio can distort.'
    : '';
}

/**
 * The menu you get by right-clicking somebody.
 *
 * Takes a channel and a SLOT rather than a member object, and looks the
 * person up when it opens. The roster arrives again on every mute, join and
 * leave, and a slot is handed to the next person to join once its previous
 * holder leaves, so a captured object can end up describing somebody else
 * entirely -- which is exactly how a volume set on one person used to land
 * on another.
 */
let peerMenuOpen = null;

function closePeerMenu() {
  el.peerMenu.hidden = true;
  peerMenuOpen = null;
}

function openPeerMenu(channelId, mid, event) {
  const roster = channelId === state.voice.channelId
    ? state.channels.roster
    : (state.channels.rosters[channelId] ?? []);
  const member = roster.find((m) => m.mid === mid);
  if (!member) return;
  // Both halves, because slots are per channel: mid 1 in the channel you are
  // in is you, and mid 1 in the one next door is somebody else entirely.
  if (channelId === state.voice.channelId && mid === state.voice.mid) return;

  peerMenuOpen = { channelId, mid };
  const userId = member.userId;

  el.peerMenuAvatar.replaceChildren(
    ...avatarEl(knownUser(userId) ?? { nickname: member.nickname }).childNodes,
  );
  el.peerMenuName.textContent = member.nickname;

  const rows = [];

  // Local audio, and only where it can do anything: turning somebody in
  // another channel down is a control that visibly does nothing.
  if (channelId === state.voice.channelId) {
    const mute = document.createElement('button');
    mute.type = 'button';
    mute.className = 'ghost menu-item peer-mute';

    const row = document.createElement('div');
    row.className = 'peer-menu-row peer-audio';
    const volume = document.createElement('input');
    volume.type = 'range';
    volume.className = 'peer-volume';
    volume.min = '0';
    volume.max = String(asPercent(MAX_GAIN));
    volume.step = '5';
    volume.title = 'How loudly you hear this person';
    const label = document.createElement('span');
    label.className = 'peer-volume-label';
    row.append(volume, label);

    const paint = () => {
      const muted = state.voice.peerMuted(userId);
      applyPeerVolumeLook(volume, label, asPercent(state.voice.peerGain(userId)), muted);
      mute.textContent = muted ? 'Unmute for me' : 'Mute for me';
      mute.toggleAttribute('data-on', muted);
    };

    mute.addEventListener('click', () => {
      const now = state.voice.setPeerMuted(userId, !state.voice.peerMuted(userId));
      // Un-muting somebody who is still at zero would be a no-op that looks
      // like a broken button.
      if (!now && state.voice.peerGain(userId) === 0) state.voice.setPeerGain(userId, 1);
      paint();
      renderVoiceRoster(state.channels.roster);
    });
    volume.addEventListener('input', () => {
      const percent = Number(volume.value);
      state.voice.setPeerGain(userId, percent / 100);
      applyPeerVolumeLook(volume, label, percent, false);
    });

    paint();
    rows.push(mute, row);
  }

  if (isAdmin()) {
    if (rows.length) rows.push(document.createElement('hr'));

    const force = document.createElement('button');
    force.type = 'button';
    force.className = 'ghost menu-item force-mute';
    force.textContent = member.forceMuted ? 'Unmute' : 'Force mute';
    force.addEventListener('click', () => {
      // Read the CURRENT roster rather than the member captured above: the
      // menu can sit open while somebody else mutes them.
      const live = (channelId === state.voice.channelId
        ? state.channels.roster
        : (state.channels.rosters[channelId] ?? [])).find((m) => m.mid === mid);
      harmony.realtime.request('admin:force-mute', {
        channelId, mid, muted: !live?.forceMuted,
      }).catch((err) => showChannelsError(err.message));
      closePeerMenu();
    });

    /**
     * Move somebody into another channel.
     *
     * A select rather than a button per channel: the server already accepts
     * any voice channel as a destination, and the list is as long as the
     * server's. Disconnecting is the same request with a null destination,
     * which is why it sits in the same control.
     */
    const move = document.createElement('select');
    move.className = 'move-select';
    move.title = 'Move this person';
    move.append(new Option('Move\u2026', ''));
    for (const channel of state.channels.list) {
      if (channel.kind !== 'voice' || channel.id === channelId) continue;
      move.append(new Option(channel.name, String(channel.id)));
    }
    move.append(new Option('Disconnect', 'none'));
    move.addEventListener('change', () => {
      const choice = move.value;
      move.value = '';
      if (!choice) return;
      harmony.realtime.request('admin:move', {
        userId,
        toChannelId: choice === 'none' ? null : Number(choice),
      }).catch((err) => showChannelsError(err.message));
      closePeerMenu();
    });

    rows.push(force, move);
  }

  // Nothing on offer -- an ordinary member right-clicking somebody in a
  // channel they are not in. A menu with no entries is worse than none.
  if (rows.length === 0) {
    peerMenuOpen = null;
    return;
  }
  el.peerMenuBody.replaceChildren(...rows);

  // Shown before measuring, off-screen, because a hidden element has no size
  // and the whole point of the clamp below is to keep it on screen.
  el.peerMenu.style.left = '-9999px';
  el.peerMenu.style.top = '0px';
  el.peerMenu.hidden = false;
  const box = el.peerMenu.getBoundingClientRect();
  const x = Math.min(event.clientX, window.innerWidth - box.width - 8);
  const y = Math.min(event.clientY, window.innerHeight - box.height - 8);
  el.peerMenu.style.left = `${Math.max(8, x)}px`;
  el.peerMenu.style.top = `${Math.max(8, y)}px`;
}

function renderVoiceRoster(roster) {
  state.channels.roster = roster;
  el.voiceCount.textContent = `${roster.length} ${roster.length === 1 ? 'person' : 'people'}`;

  // Rows are reused rather than rebuilt. A roster push arrives on every mute,
  // join and leave; rebuilding would drop a half-dragged volume slider and
  // close an open move menu every time somebody else did anything.
  const existing = new Map(
    [...el.voiceRoster.children].map((li) => [li.dataset.mid, li]),
  );

  el.voiceRoster.replaceChildren(...roster.map((member) => {
    const li = existing.get(String(member.mid)) ?? voiceRow(member);
    li.dataset.userId = String(member.userId);

    const picture = avatarEl(knownUser(member.userId) ?? { nickname: member.nickname });
    li.querySelector('.avatar').replaceChildren(...picture.childNodes);

    li.querySelector('.member-name').textContent =
      member.nickname + (member.mid === state.voice.mid ? ' (you)' : '');
    renderStatus(li.querySelector('.status'), member);

    // Greys their picture: you can still see them talking, which is the
    // point, but it should not look like audio that is failing.
    li.toggleAttribute('data-local-muted', state.voice.peerMuted(member.userId));

    return li;
  }));

  // An open menu follows the person it was opened on. Leaving it showing a
  // force-mute for somebody who has left is how an admin mutes a stranger.
  if (peerMenuOpen && !roster.some((m) => m.mid === peerMenuOpen.mid)) closePeerMenu();
}

/**
 * Paint the speaking ring.
 *
 * Polled rather than pushed: whether somebody is talking changes several
 * times a second, and an event per change would be a storm for something
 * that is only ever a CSS class. Reading an AnalyserNode is cheap, and this
 * touches one attribute per person.
 */
function renderSpeaking() {
  if (!state.voice.channelId) return;
  const speaking = state.voice.speakingMids();

  for (const li of el.voiceRoster.children) {
    li.toggleAttribute('data-speaking', speaking.has(Number(li.dataset.mid)));
  }

  // The sidebar too: it is the list you look at when you are in another
  // channel, and "who is talking in there" is most of why you would look.
  const members = state.channels.rosters[state.voice.channelId] ?? [];
  const list = el.channelItems.querySelector(
    `.channel-members[data-for="${state.voice.channelId}"]`,
  );
  if (!list) return;
  [...list.children].forEach((row, i) => {
    const member = members[i];
    row.toggleAttribute('data-speaking', Boolean(member) && speaking.has(member.mid));
  });
}

// ---------------------------------------------------------------------------
// Voice devices
//
// Two choices, remembered, and both of them hot-pluggable.
//
// The rule throughout is that a saved device id is a PREFERENCE rather than a
// requirement. Unplugging a headset mid-call falls back to the system default
// and keeps the preference, so plugging it back in picks it up again without
// anybody opening a menu -- which is what people mean by "it should just
// work", and the opposite of what storing "whatever is selected right now"
// would do.
// ---------------------------------------------------------------------------

/** Devices seen at the last enumeration, so a change can be compared. */
let lastDevices = { inputs: [], outputs: [], cameras: [] };

const deviceNote = (text) => {
  el.voiceDeviceNote.textContent = text;
};

/**
 * Fill one picker, keeping the saved preference selected where it still
 * exists and falling back to the default where it does not.
 *
 * @returns {string} the device actually selected
 */
function fillDevicePicker(select, devices, preferred, defaultLabel) {
  const options = [
    { id: '', name: defaultLabel },
    ...devices.map((d, i) => ({ id: d.deviceId, name: d.label || `Device ${i + 1}` })),
  ];
  select.replaceChildren(...options.map(({ id, name }) => {
    const option = document.createElement('option');
    option.value = id;
    option.textContent = name;
    return option;
  }));

  const available = devices.some((d) => d.deviceId === preferred);
  select.value = available ? preferred : '';
  // The preference is NOT rewritten here. It stays pointing at the device
  // that is missing, which is what lets it come back on its own.
  return select.value;
}

/**
 * Enumerate, repopulate both pickers, and apply anything that changed.
 *
 * Called on joining a channel and again on every devicechange. Idempotent:
 * applying a device that is already in use is a no-op, so the common case of
 * "a device appeared that we do not care about" costs one enumeration.
 */
async function refreshVoiceDevices({ apply = true } = {}) {
  // Re-read rather than trusting the copy in memory. A devicechange is the
  // one moment the preference genuinely matters, and settings can have been
  // written by another window or by a previous run of this one.
  state.settings = await harmony.settings.get();

  let devices;
  try {
    devices = await navigator.mediaDevices.enumerateDevices();
  } catch (err) {
    deviceNote(`Could not list audio devices: ${err.message}`);
    return;
  }

  // 'communications' is a Windows alias for the default, and listing it beside
  // the real device makes it look as though there are two of everything.
  const inputs = devices.filter((d) => d.kind === 'audioinput' && d.deviceId !== 'communications');
  const outputs = devices.filter((d) => d.kind === 'audiooutput' && d.deviceId !== 'communications');
  lastDevices = { inputs, outputs };

  // Cameras are listed here too, so that the device row is one place rather
  // than a microphone here and a webcam buried in the source picker. Their
  // labels stay blank until camera permission has been granted once, which
  // is why the fallback name is "Camera 2" rather than nothing.
  const cameras = devices.filter((d) => d.kind === 'videoinput');
  lastDevices = { inputs, outputs, cameras };

  const wantedIn = state.settings.voiceInputId ?? '';
  const wantedOut = state.settings.voiceOutputId ?? '';
  const wantedCam = state.settings.voiceCameraId ?? '';
  const chosenIn = fillDevicePicker(el.voiceInput, inputs, wantedIn, 'System default');
  const chosenOut = fillDevicePicker(el.voiceOutput, outputs, wantedOut, 'System default');
  const chosenCam = fillDevicePicker(el.voiceCamera, cameras, wantedCam, 'Default camera');

  // Chromium only exposes audiooutput once microphone permission has been
  // granted, and never on some Linux setups. An empty list is not a fault.
  el.voiceOutput.disabled = outputs.length === 0;
  el.voiceCamera.disabled = cameras.length === 0;

  const missing = [];
  if (wantedIn && chosenIn !== wantedIn) missing.push('microphone');
  if (wantedOut && chosenOut !== wantedOut) missing.push('output');
  if (wantedCam && chosenCam !== wantedCam) missing.push('camera');
  deviceNote(missing.length
    ? `Your chosen ${missing.join(' and ')} ${missing.length > 1 ? 'are' : 'is'} unplugged. `
      + 'Using the system default until it is back.'
    : '');

  if (!apply) return;
  await applyVoiceInput(chosenIn);
  await applyVoiceOutput(chosenOut);
  await applyVoiceCamera(chosenCam);
}

/** Point the microphone at a device, if we are publishing one. */
async function applyVoiceInput(deviceId) {
  if (!state.voice.micLive) return;
  // Already there: switching would cost a getUserMedia and a track swap for
  // nothing, and devicechange fires several times for one physical plug.
  if (state.voice.micDeviceId === deviceId) return;
  if (!deviceId && !state.settings.voiceInputId) return;

  const ok = await state.voice.switchMic(deviceId);
  if (!ok) deviceNote('That microphone could not be opened. Still using the previous one.');
}

async function applyVoiceOutput(deviceId) {
  const ok = await setOutputDevice(deviceId);
  if (!ok && deviceId) {
    deviceNote('This build cannot choose an output device; using the system default.');
  }
}

/**
 * Point the camera at a device, if one is running.
 *
 * Same shape as the microphone, and for the same reason: replaceTrack on the
 * live sender rather than a fresh publish, so nobody watching has to tear
 * their subscription down and rebuild it to see you switch webcam.
 */
async function applyVoiceCamera(deviceId) {
  if (!state.voice.camLive) return;
  if (state.voice.camDeviceId === deviceId) return;
  if (!deviceId && !state.settings.voiceCameraId) return;

  const stream = await openCamera(deviceId).catch(() => null);
  if (!stream) {
    deviceNote('That camera could not be opened. Still using the previous one.');
    return;
  }
  if (!await state.voice.switchCam(stream)) {
    stream.getTracks().forEach((t) => t.stop());
    deviceNote('That camera could not be opened. Still using the previous one.');
    return;
  }
  renderChannelVideo();
}

/**
 * React to hardware being plugged in or pulled out.
 *
 * Registered once at startup rather than per channel, because the event is
 * about the machine and not about the call -- and because removing and
 * re-adding a listener on every join is how one ends up with six of them.
 */
navigator.mediaDevices?.addEventListener?.('devicechange', () => {
  if (!state.voice.channelId) return;
  refreshVoiceDevices().catch((err) => console.warn('[devices]', err.message));
});

// ---------------------------------------------------------------------------
// The channel mosaic
//
// Every camera and screen share inside the voice channel you are in, opened
// automatically from the roster. It is the same WHEP subscription the flat
// mosaic uses, with two differences: the paths are channel-scoped
// (`vc-<cid>-<mid>-c` / `-s`) and the read token is the per-channel one, so a
// stream shared into a locked channel is not watchable by someone who never
// got in.
// ---------------------------------------------------------------------------

const KIND_LABEL = { c: 'camera', s: 'screen' };

const nameOfMid = (mid) =>
  state.channels.roster.find((m) => m.mid === mid)?.nickname ?? `slot ${mid}`;

/**
 * Draw the tiles.
 *
 * Nodes for tiles that are still wanted are REUSED rather than rebuilt:
 * reassigning a <video>'s srcObject restarts playback, so rebuilding the grid
 * on every roster push would make every tile stutter whenever anybody muted.
 */
/**
 * Your own camera and screen, as tiles alongside everybody else's.
 *
 * From the LOCAL stream, never by subscribing to our own path: that would
 * pay for a whole extra relay round trip to show something already in
 * memory, and add a second of delay to it.
 *
 * Worth having rather than clever to omit. Sharing a screen and seeing
 * nothing appear reads as "it did not work" -- there is no other feedback
 * that it did, because the one person who cannot see your tile is you.
 */
function ownChannelTiles() {
  const mine = [];
  if (state.voice.camStream) {
    mine.push({ mid: state.voice.mid, kind: 'c', stream: state.voice.camStream, own: true });
  }
  if (state.share.target?.channelId === state.voice.channelId && state.preview.stream) {
    mine.push({ mid: state.voice.mid, kind: 's', stream: state.preview.stream, own: true });
  }
  return mine;
}

function renderChannelVideo() {
  const tiles = state.voice.channelId
    ? [...ownChannelTiles(), ...state.voice.videoTiles]
    : [];
  const existing = new Map(
    [...el.channelVideo.children].map((node) => [node.dataset.key, node]),
  );

  el.channelVideo.replaceChildren(...tiles.map((tile) => {
    const { mid, kind, stream, own } = tile;
    const key = own ? `me:${kind}` : tile.key;
    const caption = `${own ? 'you' : nameOfMid(mid)} \u00B7 ${KIND_LABEL[kind] ?? kind}`;

    const kept = existing.get(key);
    if (kept) {
      kept.querySelector('figcaption .tile-name').textContent = caption;
      return kept;
    }

    const figure = document.createElement('figure');
    figure.className = 'channel-tile';
    figure.dataset.key = key;
    if (own) figure.setAttribute('data-own', '');

    const video = document.createElement('video');
    video.autoplay = true;
    video.playsInline = true;
    // Muted on purpose: a screen share's audio goes through gain.js like
    // every other incoming stream, so that deafen reaches it. Letting the
    // element play it too would double it.
    video.muted = true;
    video.srcObject = stream;

    const label = document.createElement('figcaption');
    const name = document.createElement('span');
    name.className = 'tile-name';
    name.textContent = caption;
    label.append(name);

    /*
     * The same controls the flat mosaic has, because this IS a mosaic --
     * it just happens to be the channel's rather than the server's, and
     * somebody sharing a screen into a voice channel wants to make it big
     * and turn the game audio down exactly as they would anywhere else.
     */
    const controls = document.createElement('span');
    controls.className = 'tile-controls';

    // Volume only where there is something to turn down: a camera has no
    // audio, and your own tiles are local, never played back.
    if (!own && kind === 's') {
      const muteBtn = document.createElement('button');
      muteBtn.className = 'tile-btn';
      muteBtn.type = 'button';
      muteBtn.dataset.role = 'mute';
      muteBtn.innerHTML = '&#128266;';
      muteBtn.title = 'Mute this share';

      const volume = document.createElement('input');
      volume.type = 'range';
      volume.className = 'tile-volume';
      volume.min = '0';
      volume.max = String(asPercent(MAX_GAIN));
      volume.value = String(asPercent(tile.gain ?? 1));
      volume.title = 'Volume for this share';

      const apply = (percent) => {
        state.voice.setTileGain(key, percent / 100);
        volume.toggleAttribute('data-boosted', percent > 100);
        muteBtn.innerHTML = percent === 0 ? '&#128263;' : '&#128266;';
      };
      volume.addEventListener('input', (event) => {
        event.stopPropagation();
        apply(Number(volume.value));
      });
      muteBtn.addEventListener('click', (event) => {
        event.stopPropagation();
        const next = Number(volume.value) === 0 ? 100 : 0;
        volume.value = String(next);
        apply(next);
      });

      controls.append(muteBtn, volume);
    }

    const bigBtn = document.createElement('button');
    bigBtn.className = 'tile-btn';
    bigBtn.type = 'button';
    bigBtn.dataset.role = 'maximize';
    bigBtn.innerHTML = '&#10530;';
    bigBtn.title = 'Maximize, without leaving the channel';

    const fsBtn = document.createElement('button');
    fsBtn.className = 'tile-btn';
    fsBtn.type = 'button';
    fsBtn.dataset.role = 'fullscreen';
    fsBtn.innerHTML = '&#9974;';
    fsBtn.title = 'Fullscreen';

    const hideBtn = document.createElement('button');
    hideBtn.className = 'tile-btn';
    hideBtn.type = 'button';
    hideBtn.dataset.role = 'minimize';
    hideBtn.innerHTML = '&#8211;';
    hideBtn.title = 'Minimize to a strip';

    const maximize = () => {
      const wasBig = figure.hasAttribute('data-big');
      for (const node of el.channelVideo.children) node.removeAttribute('data-big');
      if (!wasBig) figure.setAttribute('data-big', '');
      bigBtn.title = wasBig ? 'Maximize, without leaving the channel' : 'Back to the grid';
    };

    bigBtn.addEventListener('click', (event) => { event.stopPropagation(); maximize(); });
    fsBtn.addEventListener('click', (event) => {
      event.stopPropagation();
      toggleFullscreen(figure);
    });
    hideBtn.addEventListener('click', (event) => {
      event.stopPropagation();
      figure.toggleAttribute('data-small');
      figure.removeAttribute('data-big');
    });

    controls.append(bigBtn, fsBtn, hideBtn);
    label.append(controls);

    // The whole tile is still a maximize target: it is what people reach
    // for before they find the button.
    figure.addEventListener('click', maximize);

    figure.append(video, label);
    return figure;
  }));

  // After the children exist, not before: applyStage counts them to decide
  // whether there is a stage at all.
  applyStage();
}

// ---------------------------------------------------------------------------
// Text channels
// ---------------------------------------------------------------------------

async function openTextChannel(channel) {
  state.chat.channelId = channel.id;
  state.chat.searching = false;
  state.chat.pendingFile = null;
  el.chatName.textContent = `#${channel.name}`;
  el.chatSearch.value = '';
  el.chatSearchClear.hidden = true;
  el.chatNote.textContent = '';
  el.voiceIdle.hidden = true;
  applyStage();
  renderChannels();

  try {
    const { messages, pinned } = await harmony.api.messages(state.server, channel.id);
    state.chat.messages = messages;
    state.chat.pinned = pinned;
    renderChat({ scrollToBottom: true });
    // Pinned attachments are part of the keep-set: they are the things
    // somebody decided were worth coming back to, so they survive eviction.
    await refreshKeepSet();
  } catch (err) {
    showChannelsError(err.message);
  }
}

function closeChat() {
  state.chat.channelId = null;
  if (el.voiceActive.hidden) el.voiceIdle.hidden = false;
  applyStage();
}

/** One message row. Attachments are rendered from the local cache. */
function messageRow(message) {
  const row = document.createElement('div');
  row.className = 'chat-msg';
  row.dataset.id = String(message.id);
  if (message.pinned) row.setAttribute('data-pinned', '');

  const who = document.createElement('span');
  who.className = 'who';
  who.append(
    avatarEl(knownUser(message.userId) ?? { nickname: message.nickname }, 'tiny'),
    document.createTextNode(message.nickname),
  );

  const text = document.createElement('span');
  text.className = 'text';
  // textContent, never innerHTML: a chat message is the most obvious place in
  // the app for someone to try injecting markup.
  text.textContent = message.body;

  if (message.attachmentHash) {
    // harmony://app/media/<hash> -- same-origin, so the CSP allows it, and the
    // main process downloads and verifies it on first use. See media-cache.js.
    const url = harmony.mediaUrl(message.attachmentHash);
    if (message.mediaType === 'image') {
      const img = document.createElement('img');
      img.src = url;
      img.alt = 'attachment';
      img.loading = 'lazy';
      text.append(img);
    } else if (message.mediaType === 'video') {
      const video = document.createElement('video');
      video.src = url;
      video.controls = true;
      text.append(video);
    } else if (message.mediaType === 'audio') {
      const audio = document.createElement('audio');
      audio.src = url;
      audio.controls = true;
      text.append(audio);
    } else {
      const link = document.createElement('a');
      link.href = url;
      link.textContent = 'attachment';
      text.append(document.createElement('br'), link);
    }
  }

  const when = document.createElement('span');
  when.className = 'when';
  when.textContent = new Date(message.createdAt).toLocaleTimeString([], {
    hour: '2-digit', minute: '2-digit',
  });

  const pin = document.createElement('button');
  pin.className = 'ghost small';
  pin.textContent = message.pinned ? 'Unpin' : 'Pin';
  pin.addEventListener('click', async () => {
    try {
      await harmony.api.pinMessage(state.server, message.id, !message.pinned);
    } catch (err) {
      showChannelsError(err.message);
    }
  });

  row.append(who, text, when, pin);

  // Your own, or anybody's if you are an admin -- the same rule the server
  // enforces, so a button that appears always works.
  if (message.userId === state.auth.user?.id || isAdmin()) {
    const remove = document.createElement('button');
    remove.className = 'ghost small danger';
    remove.textContent = 'Delete';
    remove.addEventListener('click', async () => {
      if (!await askConfirm('Delete this message?')) return;
      try {
        await harmony.api.deleteMessage(state.server, message.id);
        // The server pushes message:deleted to everyone, including us.
      } catch (err) {
        showChannelsError(err.message);
      }
    });
    row.append(remove);
  }

  return row;
}

/**
 * Render the log.
 *
 * The only subtle part is four lines: capture whether the pane was already at
 * the bottom BEFORE appending, and only auto-scroll if it was. Without that,
 * reading back through history gets yanked to the end by every new message
 * that arrives.
 */
function renderChat({ scrollToBottom = false } = {}) {
  const log = el.chatLog;
  const wasAtBottom = log.scrollHeight - log.scrollTop - log.clientHeight < 40;

  log.replaceChildren(...state.chat.messages.map(messageRow));

  if (state.chat.pinned.length) {
    el.chatPinned.hidden = false;
    el.chatPinned.replaceChildren(
      ...state.chat.pinned.map((m) => {
        const line = document.createElement('div');
        line.textContent = `\u{1F4CC} ${m.nickname}: ${m.body || '(attachment)'}`;
        return line;
      }),
    );
  } else {
    el.chatPinned.hidden = true;
  }

  if (scrollToBottom || wasAtBottom) log.scrollTop = log.scrollHeight;
}

async function sendMessage() {
  const body = el.chatInput.value.trim();
  const file = state.chat.pendingFile;
  if (!body && !file) return;

  el.chatInput.value = '';
  state.chat.pendingFile = null;
  el.chatNote.textContent = '';

  try {
    let attachmentHash = null;
    if (file) {
      el.chatNote.textContent = `Uploading ${file.name}\u2026`;
      const bytes = new Uint8Array(await file.arrayBuffer());
      const upload = await harmony.media.upload(state.server, bytes, file.type);
      attachmentHash = upload.hash;
      el.chatNote.textContent = '';
    }
    await harmony.api.postMessage(state.server, state.chat.channelId, { body, attachmentHash });
    // The server echoes it back over the socket, so nothing is appended here.
  } catch (err) {
    el.chatNote.textContent = err.message;
    el.chatInput.value = body; // give them their text back
  }
}

async function runSearch() {
  const query = el.chatSearch.value.trim();
  if (!query) {
    state.chat.searching = false;
    el.chatSearchClear.hidden = true;
    return openTextChannel(state.channels.list.find((c) => c.id === state.chat.channelId));
  }

  try {
    const { mode, results } = await harmony.api.search(state.server, state.chat.channelId, query);
    state.chat.searching = true;
    state.chat.messages = results.slice().reverse();
    el.chatSearchClear.hidden = false;
    renderChat({ scrollToBottom: true });
    el.chatNote.textContent = results.length
      // Worth saying: a 1-2 character query silently cannot use the trigram
      // index, so it falls back to a plain substring scan. Showing which ran
      // makes "why did that find nothing" answerable.
      ? `${results.length} result${results.length === 1 ? '' : 's'} (${mode})`
      : `No matches (${mode}).`;
  } catch (err) {
    el.chatNote.textContent = err.message;
  }
  return undefined;
}

// ---------------------------------------------------------------------------
// Soundpad
// ---------------------------------------------------------------------------

async function loadSoundpad() {
  try {
    const { clips } = await harmony.api.soundpad(state.server);
    state.soundpad.clips = clips;
    renderSoundpad();
    // Clips must survive cache eviction: the first press of a button should
    // never be a 300 ms download, and they are small.
    await refreshKeepSet();
    // Warm the cache now rather than on the first click. The protocol handler
    // downloads on demand, so simply asking for each URL is enough.
    for (const clip of clips) {
      fetch(harmony.mediaUrl(clip.hash)).catch(() => { /* will retry on click */ });
    }
  } catch (err) {
    showChannelsError(err.message);
  }
}

/**
 * How loudly clips play here, as a gain.
 *
 * Read from settings every time rather than cached, for the same reason the
 * device pickers re-read them: a second window, or a previous run, can have
 * changed it, and the only thing worse than a volume that does not persist
 * is one that persists differently in two places.
 */
function soundpadGain() {
  const percent = state.settings.soundpadVolume;
  return Math.max(0, Math.min(MAX_GAIN, (typeof percent === 'number' ? percent : 100) / 100));
}

/** Paint the soundpad's own volume control from the saved setting. */
function applySoundpadVolume() {
  const percent = typeof state.settings.soundpadVolume === 'number'
    ? state.settings.soundpadVolume
    : 100;
  // Reuses the per-person treatment, including the amber warning past 100%:
  // a clip amplified three and a half times is exactly as likely to distort
  // as a person is, and it is the same slider doing the same thing.
  applyPeerVolumeLook(el.soundpadVolume, el.soundpadVolumeLabel, percent, false);
  el.soundpadMute.innerHTML = percent === 0 ? '&#128263;' : '&#128266;';
  el.soundpadMute.title = percent === 0 ? 'Unmute the soundpad' : 'Mute the soundpad';
  el.soundpadMute.toggleAttribute('data-on', percent === 0);
}

function renderSoundpad() {
  el.soundpadAdd.hidden = !isAdmin();
  applySoundpadVolume();
  // Shown unless the panel's soundboard button has been used to put it
  // away. Default-on rather than default-off: a clip you cannot find is a
  // clip nobody plays, and the button is there to reclaim the space in a
  // narrow window rather than to reveal a hidden feature.
  el.soundpad.hidden = el.soundpad.dataset.shown === '0'
    || (!state.soundpad.clips.length && !isAdmin());

  el.soundpadGrid.replaceChildren(...state.soundpad.clips.map((clip, index) => {
    const button = document.createElement('button');
    button.className = 'ghost small';
    button.textContent = clip.name;
    button.addEventListener('click', () => {
      if (!state.voice.channelId) return showChannelsError('Join a voice channel first.');
      // Only the event is sent. Every client plays its own cached copy -- see
      // the Soundpad comment in the server's chat.js for why.
      return harmony.realtime
        .request('soundpad:play', { channelId: state.voice.channelId, clipId: clip.id })
        .catch((err) => showChannelsError(err.message));
    });

    if (!isAdmin()) return button;

    button.addEventListener('contextmenu', async (event) => {
      event.preventDefault();
      if (!await askConfirm(`Delete the clip "${clip.name}"?`)) return;
      try {
        await harmony.api.deleteClip(state.server, clip.id);
      } catch (err) {
        showChannelsError(err.message);
      }
    });

    // Wrapped only for admins, so everyone else gets a plain grid of buttons
    // and the arrows do not take up room they do not earn.
    const cell = document.createElement('span');
    cell.className = 'clip-cell';
    cell.append(
      rowButton('\u25C0', 'Move left', () => nudgeClip(clip.id, -1)),
      button,
      rowButton('\u25B6', 'Move right', () => nudgeClip(clip.id, 1)),
    );
    cell.firstChild.disabled = index === 0;
    cell.lastChild.disabled = index === state.soundpad.clips.length - 1;
    return cell;
  }));
}

/** Same whole-list contract as the channels. See nudgeChannel. */
async function nudgeClip(id, delta) {
  const ids = state.soundpad.clips.map((c) => c.id);
  const from = ids.indexOf(id);
  const to = from + delta;
  if (from < 0 || to < 0 || to >= ids.length) return;
  ids.splice(to, 0, ...ids.splice(from, 1));
  try {
    await harmony.api.reorderClips(state.server, ids);
  } catch (err) {
    showChannelsError(err.message);
  }
}

/**
 * The server's ceiling, mirrored here.
 *
 * Checked before the upload rather than after, because the upload is what
 * costs: a 20 MB file would be sent in full, stored, and only then refused by
 * the soundpad -- leaving an orphan behind for the next eviction to find.
 */
const MAX_CLIP_BYTES = 2 * 1024 * 1024;

async function addSoundpadClip(file) {
  try {
    el.channelsError.hidden = true;
    if (file.size > MAX_CLIP_BYTES) {
      throw new Error(
        `"${file.name}" is ${(file.size / 1024 / 1024).toFixed(1)} MB. Clips are limited to `
        + `${MAX_CLIP_BYTES / 1024 / 1024} MB -- every client downloads every clip.`,
      );
    }
    const bytes = new Uint8Array(await file.arrayBuffer());
    const upload = await harmony.media.upload(state.server, bytes, file.type);
    const answer = await ask({
      title: 'Name this clip',
      okLabel: 'Add',
      fields: [{
        name: 'name',
        label: 'Name',
        value: file.name.replace(/\.[^.]+$/, '').slice(0, 32),
        required: true,
      }],
    });
    if (!answer?.name) return;
    await harmony.api.addClip(state.server, { name: answer.name, hash: upload.hash });
  } catch (err) {
    showChannelsError(err.message);
  }
}

// ---------------------------------------------------------------------------
// Webcam
//
// A SECOND publish under `<nickname>-cam`, not a second track on the existing
// one: MediaMTX's WHIP cannot renegotiate an added track (measured in the
// Phase 0 spike -- PATCH accepts only ICE trickle fragments), so adding a
// camera to a live path would mean tearing it down and cutting the audio
// everyone is listening to.
//
// This reuses publish() untouched, which is also how the H.264 High-profile
// ordering is preserved here by construction rather than by copying it.
// ---------------------------------------------------------------------------

const CAMERA = { width: 640, height: 360, frameRate: 24, bitrate: 400_000 };

/**
 * Open a camera, preferring the chosen one.
 *
 * `exact` rather than `ideal` so that a device which is gone FAILS instead of
 * silently handing back a different webcam -- the caller falls back to the
 * default itself, and says so. The preference is never rewritten, which is
 * what lets a camera that is plugged back in be picked up again.
 */
const openCamera = (deviceId) => navigator.mediaDevices.getUserMedia({
  video: {
    width: { ideal: CAMERA.width },
    height: { ideal: CAMERA.height },
    frameRate: { ideal: CAMERA.frameRate },
    ...(deviceId ? { deviceId: { exact: deviceId } } : {}),
  },
});

async function startCamera() {
  if (state.camera.publication || state.voice.camLive) return;
  const nickname = state.auth.user?.nickname;
  if (!nickname) return showChannelsError('Sign in first.');

  try {
    const wanted = state.settings.voiceCameraId ?? '';
    let stream = wanted ? await openCamera(wanted).catch(() => null) : null;
    if (!stream) {
      if (wanted) deviceNote('Your chosen camera is unplugged. Using the default one.');
      stream = await openCamera('');
    }

    /*
     * Inside a voice channel the camera goes to the CHANNEL path.
     *
     * That is what makes it part of the channel mosaic: the other members
     * already hold a read token for this channel, and they learn there is
     * something to watch from the roster rather than from the flat stream
     * list. It also means a camera shared into a locked channel is not
     * visible to someone who never got in, which the flat `<nickname>-cam`
     * namespace cannot express.
     *
     * Outside one it falls back to the flat path, which is how a camera is
     * watchable from the ordinary mosaic by a client that knows nothing about
     * channels.
     */
    if (state.voice.channelId) {
      // A camera is usually started well into a call, which is exactly when
      // the join-time token has gone stale.
      await refreshVoiceTokens();
      await state.voice.startCam(stream, {
        bitrate: CAMERA.bitrate,
        framerate: CAMERA.frameRate,
      });
      await harmony.realtime.request('voice:publishing', {
        channelId: state.voice.channelId, kind: 'c', on: true,
      });
      applyVoiceButtons();
      renderChannelVideo();
      return undefined;
    }

    state.camera.stream = stream;

    // The server appends `-cam` to our authenticated nickname itself; we
    // cannot and must not name the path. `-cam` is refused as a registerable
    // nickname, so nobody else can ever hold this one.
    const session = await harmony.api.session(state.server, null, undefined, 'camera');
    if (session.role !== 'broadcaster') {
      throw new Error('Your camera path is already in use.');
    }

    state.camera.publication = await publish({
      url: session.whipUrl,
      stream: state.camera.stream,
      iceServers: session.iceServers,
      maxBitrate: CAMERA.bitrate,
      maxFramerate: CAMERA.frameRate,
      contentHint: 'motion',
    });

    // Its own heartbeat group, so stopping the camera does not disturb the
    // screen share's claim and vice versa.
    addTimer(
      setInterval(
        () => harmony.api.heartbeat(state.server, session.username, session.token).catch(() => {}),
        Math.max(5000, session.heartbeatMs ?? 10_000),
      ),
      'camera',
    );
    state.camera.session = session;
    el.voiceCam.textContent = 'Stop camera';
  } catch (err) {
    showChannelsError(err.message);
    await stopCamera();
  }
  return undefined;
}

async function stopCamera() {
  if (state.voice.camLive) {
    const channelId = state.voice.channelId;
    await state.voice.stopCam();
    if (channelId) {
      await harmony.realtime.request('voice:publishing', {
        channelId, kind: 'c', on: false,
      }).catch(() => { /* leaving the channel says the same thing */ });
    }
    applyVoiceButtons();
    renderChannelVideo();
    return;
  }

  clearTimers('camera');
  const publication = state.camera.publication;
  state.camera.publication = null;
  state.camera.stream?.getTracks().forEach((t) => t.stop());
  state.camera.stream = null;
  el.voiceCam.textContent = 'Start camera';

  if (publication) {
    publication.pc.close();
    if (publication.resourceUrl) await harmony.api.hangup(publication.resourceUrl).catch(() => {});
  }
  const session = state.camera.session;
  state.camera.session = null;
  if (session) {
    await harmony.api.release(state.server, session.username, session.token).catch(() => {});
  }
}

/**
 * Everything the server pushes.
 *
 * The roster is the single source of truth for who to subscribe to, which is
 * why syncPeers() is driven from here rather than from the join: a member who
 * unmutes ten minutes later is just another roster push.
 */
function onRealtimeEvent(msg) {
  switch (msg.type) {
    case 'channels':
      state.channels.list = msg.channels;
      renderChannels();
      break;

    case 'voice:roster':
      // Kept for EVERY channel, not just ours: this is what the sidebar
      // draws, and it is the only way to see who is in a channel before
      // deciding whether to join it.
      state.channels.rosters[msg.channelId] = msg.roster;
      state.channels.occupancy[msg.channelId] = msg.roster.length;
      if (msg.channelId !== state.voice.channelId) {
        renderChannels();
        break;
      }
      renderVoiceRoster(msg.roster);
      renderChannels();
      state.voice.syncPeers(msg.roster).catch(() => { /* retried next push */ });
      state.voice.syncVideo(msg.roster)
        .then(() => renderChannelVideo())
        .catch(() => { /* the reconcile timer comes back */ });
      // Captions carry nicknames from the roster we just replaced.
      renderChannelVideo();

      // A force-mute arrives here and nowhere else. The Phase 0 spike measured
      // the victim's peer connection still reporting `connected` for about
      // nine seconds after the server kills their session, so connection state
      // cannot be what drives this -- the push has to.
      if (msg.roster.some((m) => m.mid === state.voice.mid && m.forceMuted)) {
        showChannelsError('An admin muted your microphone.');
      }
      break;

    case 'message':
      if (msg.message.channelId === state.chat.channelId && !state.chat.searching) {
        state.chat.messages.push(msg.message);
        renderChat();
      }
      break;

    case 'message:updated':
      if (msg.message.channelId === state.chat.channelId) {
        const index = state.chat.messages.findIndex((m) => m.id === msg.message.id);
        if (index >= 0) state.chat.messages[index] = msg.message;
        state.chat.pinned = state.chat.pinned.filter((m) => m.id !== msg.message.id);
        if (msg.message.pinned) state.chat.pinned.unshift(msg.message);
        renderChat();
      }
      break;

    case 'message:deleted':
      if (msg.channelId === state.chat.channelId) {
        state.chat.messages = state.chat.messages.filter((m) => m.id !== msg.id);
        state.chat.pinned = state.chat.pinned.filter((m) => m.id !== msg.id);
        renderChat();
      }
      break;

    case 'soundpad':
      state.soundpad.clips = msg.clips;
      renderSoundpad();
      break;

    case 'user:updated':
      state.users.set(msg.user.id, msg.user);
      refreshKeepSet();
      if (msg.user.id === state.auth.user?.id) {
        state.auth.user = msg.user;
        renderOwnAvatar();
      }
      // Pictures are drawn from this map in three places, and the cheapest
      // way to be sure none of them is stale is to draw them all again.
      renderVoiceRoster(state.channels.roster);
      renderChat();
      break;

    case 'soundpad:play':
      // Deafened means deafened. The soundpad goes to the context's
      // destination directly rather than through a peer sink, so nothing
      // else would have silenced it -- which would make "Deafen" a button
      // that silences people but not airhorns.
      if (state.voice.deafened) break;
      // Into the PLAYBACK context, never the outgoing mix. See playSample().
      playSample(harmony.mediaUrl(msg.hash), { gain: soundpadGain() }).catch((err) =>
        showChannelsError(`Could not play "${msg.name}": ${err.message}`));
      break;

    case 'voice:moved':
      if (msg.channelId == null) {
        leaveVoice();
        showChannelsError(`${msg.by} disconnected you.`);
      } else {
        const target = state.channels.list.find((c) => c.id === msg.channelId);
        if (target) {
          showChannelsError(`${msg.by} moved you to ${target.name}.`);
          joinVoice(target);
        }
      }
      break;

    case 'streams':
      // Replaces the 3-second /api/streams poll. The slow reconciliation tick
      // in syncMosaic stays as a safety net for a dropped socket.
      state.lastStreams = msg.streams;
      if (document.getElementById('view-mosaic').hasAttribute('data-active')) {
        syncMosaic({ streams: msg.streams }).catch(() => { /* next tick retries */ });
      }
      break;

    case 'realtime:down':
      showChannelsError('Reconnecting\u2026');
      applyVoiceConnection(false);
      break;

    case 'realtime:up':
      showChannelsError('');
      applyVoiceConnection(true);
      // Everything the hello carries, not just the channel list: a client
      // that was away has missed every roster broadcast in between, and the
      // hello is the one message that brings the whole picture back.
      if (msg.channels) state.channels.list = msg.channels;
      if (msg.rosters) state.channels.rosters = msg.rosters;
      if (msg.occupancy) state.channels.occupancy = msg.occupancy;
      renderChannels();

      // A reconnect means the server has forgotten our presence, because
      // presence IS the socket. Rejoin rather than appearing to be in a channel
      // nobody else can see us in.
      //
      // This had never once run: the event was emitted with its type
      // overwritten by the hello's own, so it arrived as 'hello-ok' and fell
      // through to default. A client that dropped came back connected but
      // silently out of its voice channel, still showing "Reconnecting...".
      if (state.voice.channelId) {
        const channel = state.channels.list.find((c) => c.id === state.voice.channelId);
        if (channel) joinVoice(channel);
      }
      break;

    case 'realtime:rejected':
      showChannelsError('This session expired. Sign in again.');
      break;

    default:
      break;
  }
}

// ---------------------------------------------------------------------------
// Source picker
// ---------------------------------------------------------------------------

async function enterPicker() {
  const target = state.share.target;
  el.pickerUsername.textContent = target ? `#${target.name}` : state.session.username;
  state.selectedSource = null;
  state.changingSource = false;
  el.startStream.disabled = true;
  el.startStream.textContent = 'Start streaming';
  el.pickerBack.textContent = 'Cancel';
  showView('view-picker');
  await loadSources();
  updateAudioNote();

  // A channel share holds a SLOT, not a username: the slot is held by the
  // WebSocket being open, so there is nothing here to keep alive. Starting
  // the flat heartbeat anyway would renew a claim on the flat namespace that
  // this share is never going to use.
  if (target) return;

  // Hold the username while the user browses windows and picks a quality.
  addTimer(
    setInterval(() => {
      harmony.api
        .heartbeat(state.session.server, state.session.username, state.session.token)
        .catch(() => {});
    }, state.session.heartbeatMs ?? 10_000),
  );
}

/**
 * Share a screen into the voice channel you are in.
 *
 * The whole picker, encoder and stats path is reused unchanged -- only the
 * WHIP URL differs, which is the point of keeping the publish target in
 * state.share rather than reading it off state.session.
 */
async function shareScreenHere() {
  if (state.share.target) return stopBroadcast();
  if (!state.voice.channelId) {
    showChannelsError('Join a voice channel first.');
    return undefined;
  }
  if (!state.voice.publishUrls.screen) {
    showChannelsError('This channel did not give out a screen path. Rejoin it.');
    return undefined;
  }
  // Before reading publishUrls, not after: the picker is where somebody
  // spends thirty seconds choosing a window, and the URL captured here is
  // the one that gets published with.
  await refreshVoiceTokens();

  const channel = state.channels.list.find((c) => c.id === state.voice.channelId);
  state.share.target = {
    channelId: state.voice.channelId,
    url: state.voice.publishUrls.screen,
    name: channel?.name ?? 'this channel',
  };
  await enterPicker();
  return undefined;
}

async function loadSources() {
  el.sourceGrid.replaceChildren(message('Loading…'));
  try {
    state.sources = await harmony.sources.list();
  } catch (err) {
    el.sourceGrid.replaceChildren(message(err.message));
    return;
  }
  await loadDevices();
  loadProcesses();
  renderSources();
}

/**
 * Cameras, capture cards and audio inputs.
 *
 * Device labels are blank until the user has granted access once, so ask for a
 * throwaway stream first -- otherwise the picker is a list of "Camera 1",
 * "Camera 2" with no way to tell a webcam from a capture card.
 */
async function loadDevices() {
  try {
    let devices = await navigator.mediaDevices.enumerateDevices();
    if (devices.some((d) => d.kind === 'videoinput' && !d.label)) {
      const probe = await navigator.mediaDevices
        .getUserMedia({ video: true, audio: true })
        .catch(() => null);
      probe?.getTracks().forEach((t) => t.stop());
      devices = await navigator.mediaDevices.enumerateDevices();
    }

    state.cameras = devices
      .filter((d) => d.kind === 'videoinput')
      .map((d, i) => ({
        id: d.deviceId,
        name: d.label || `Camera ${i + 1}`,
        kind: 'camera',
        thumbnail: null,
        icon: null,
        resolution: null,
      }));

    state.audioInputs = devices
      .filter((d) => d.kind === 'audioinput' && d.deviceId !== 'communications')
      .map((d, i) => ({ id: d.deviceId, name: d.label || `Audio input ${i + 1}` }));

    el.audioInput.replaceChildren();
    for (const input of state.audioInputs) {
      const option = document.createElement('option');
      option.value = input.id;
      option.textContent = input.name;
      el.audioInput.append(option);
    }
    const none = document.createElement('option');
    none.value = '';
    none.textContent = 'No audio';
    el.audioInput.append(none);
    if (state.settings.audioInputId) el.audioInput.value = state.settings.audioInputId;
  } catch (err) {
    console.warn('[devices]', err.message);
    state.cameras = [];
  }
}

/** Processes whose audio could be kept out of a screen share. */
async function loadProcesses() {
  try {
    state.processes = await harmony.sources.processes();
  } catch {
    state.processes = [];
  }

  el.excludeApp.replaceChildren();
  const none = document.createElement('option');
  none.value = '';
  none.textContent = 'Nothing — share all system audio';
  el.excludeApp.append(none);

  for (const proc of state.processes) {
    const option = document.createElement('option');
    option.value = String(proc.pid);
    option.textContent = `${proc.name} — ${proc.title.slice(0, 40)}`;
    el.excludeApp.append(option);
  }
}

function message(text) {
  const p = document.createElement('p');
  p.className = 'empty';
  p.textContent = text;
  return p;
}

function renderSources() {
  const items =
    state.activeKind === 'camera'
      ? state.cameras
      : state.sources.filter((s) => s.kind === state.activeKind);
  el.sourceGrid.replaceChildren();

  if (!items.length) {
    el.sourceGrid.append(
      message(
        state.activeKind === 'camera'
          ? 'No cameras or capture cards found.'
          : `No ${state.activeKind}s found.`,
      ),
    );
    return;
  }

  for (const source of items) {
    const button = document.createElement('button');
    button.className = 'source';
    button.type = 'button';

    if (source.thumbnail) {
      const img = document.createElement('img');
      img.className = 'thumb';
      img.src = source.thumbnail;
      img.alt = '';
      button.append(img);
    }

    const meta = document.createElement('div');
    meta.className = 'meta';
    if (source.icon) {
      const icon = document.createElement('img');
      icon.src = source.icon;
      icon.alt = '';
      meta.append(icon);
    }
    const label = document.createElement('span');
    label.className = 'label';
    label.textContent = source.resolution ? `${source.name} · ${source.resolution}` : source.name;
    label.title = source.name;
    meta.append(label);
    button.append(meta);

    if (state.selectedSource?.id === source.id) button.setAttribute('data-selected', '');

    button.addEventListener('click', () => {
      state.selectedSource = source;
      el.startStream.disabled = false;
      renderSources();
      updateAudioNote();
    });

    el.sourceGrid.append(button);
  }
}

/**
 * Decide where the audio for this share comes from, and say so in the UI.
 *
 * The rule: whole screen -> all system audio; single window -> only that
 * application's audio. The second half needs the native Windows capture; when
 * that is missing the user chooses between silence and system audio rather than
 * having other apps leak into the stream without being told.
 */
function audioPlan(forSource) {
  const source = forSource ?? state.selectedSource;
  const kind = source?.kind ?? state.activeKind;

  // A camera or capture card has no loopback audio of its own; its sound comes
  // from whichever input the user picked.
  if (kind === 'camera') {
    if (!el.audioInput.value) {
      return { via: 'none', note: 'Sharing without audio.', warn: false };
    }
    const label = state.audioInputs.find((d) => d.id === el.audioInput.value)?.name ?? 'the selected input';
    return { via: 'device', note: `Viewers will hear ${label}.`, warn: false };
  }

  if (state.audioAvailable) {
    if (kind === 'window') {
      return { via: 'native', note: 'Viewers will hear only this application.', warn: false };
    }
    const excluded = state.processes.find((p) => String(p.pid) === el.excludeApp.value);
    return {
      via: 'native',
      note: excluded
        ? `Viewers will hear system audio, except ${excluded.name}.`
        : 'Viewers will hear all system audio.',
      warn: false,
    };
  }

  if (kind === 'screen') {
    return { via: 'chromium', note: 'Viewers will hear all system audio.', warn: false };
  }

  if (el.fallback.value === 'system') {
    return {
      via: 'chromium',
      note: `Per-app audio unavailable — ALL system audio will be shared. ${state.audioUnavailableReason ?? ''}`,
      warn: true,
    };
  }

  return {
    via: 'none',
    note: `Per-app audio unavailable — sharing without sound. ${state.audioUnavailableReason ?? ''}`,
    warn: true,
  };
}

function updateAudioNote() {
  const kind = state.selectedSource?.kind ?? state.activeKind;

  el.fallbackField.hidden = state.audioAvailable || kind !== 'window';
  // Excluding an app needs the native per-process capture.
  el.excludeField.hidden = kind !== 'screen' || !state.audioAvailable;
  el.audioInputField.hidden = kind !== 'camera';

  const plan = audioPlan();
  el.audioNote.textContent = plan.note;
  el.audioNote.toggleAttribute('data-warn', plan.warn);
}

// ---------------------------------------------------------------------------
// Broadcasting
// ---------------------------------------------------------------------------

/**
 * Wait until the control server reports our username as live.
 *
 * Doubles as a catch-all: if ICE came up but no media is flowing, MediaMTX
 * never marks the path ready and the user is told, instead of staring at a
 * "Live" badge nobody can see.
 */
async function confirmLive({ timeoutMs = 15_000 } = {}) {
  const deadline = Date.now() + timeoutMs;
  while (Date.now() < deadline) {
    try {
      const { streams } = await harmony.api.streams(state.session.server);
      if (streams.some((s) => s.username === state.session.username)) return;
    } catch {
      // Transient -- keep trying until the deadline.
    }
    await new Promise((resolve) => setTimeout(resolve, 600));
  }
  throw new Error(
    'The server never saw this stream go live. Someone else may already be publishing under this name.',
  );
}

/**
 * Open a capture source and return its video track.
 *
 * Screens and windows come from getDisplayMedia; cameras and capture cards are
 * ordinary getUserMedia devices. Any audio that arrives attached to the capture
 * is routed into the mixer rather than published directly, so the track being
 * sent never changes.
 */
async function acquireVideo(source, preset, plan) {
  const { track, rawStream } = await openCapture(source, preset, plan);
  return { track, rawStream, previewTrack: await previewCopy(track) };
}

/** What the preview is downscaled to. See previewCopy(). */
const PREVIEW = { width: 960, height: 540, fps: 10 };

/**
 * A deliberately cheap copy of the capture, for the preview element only.
 *
 * Chromium gives every track taken off a source its own downscale and
 * frame-rate decimation, so a clone can run at 960x540x10 while the track
 * being encoded stays at native resolution and full rate. Measured on this
 * build: 4.2 Mpx/s against 221 Mpx/s for a 1440p60 capture -- a fiftieth of
 * the work, for a picture whose whole job is to tell you that you are sharing
 * the right window.
 *
 * Worth the trouble because of where that work lands. Painting the preview is
 * GPU work on the same adapter the game is using, and it scales with the
 * source resolution rather than with the bitrate -- so a native-resolution
 * preview of a native-resolution game asks that GPU to push roughly twice the
 * pixels it was already pushing, which is why the preview costs several times
 * what the encoder does.
 *
 * Falls back to the unconstrained clone if the constraints are refused, which
 * is no worse than not having tried.
 */
async function previewCopy(track) {
  const clone = track.clone();
  try {
    await clone.applyConstraints({
      width: { max: PREVIEW.width },
      height: { max: PREVIEW.height },
      frameRate: { max: PREVIEW.fps },
    });
  } catch {
    /* Source refuses to rescale. The clone is still a working preview. */
  }
  return clone;
}

async function openCapture(source, preset, plan) {
  if (source.kind === 'camera') {
    const stream = await navigator.mediaDevices.getUserMedia({
      video: {
        deviceId: { exact: source.id },
        width: { ideal: preset.width ?? 1920 },
        height: { ideal: preset.height ?? 1080 },
        frameRate: { ideal: preset.fps },
      },
      audio: false,
    });
    return { track: stream.getVideoTracks()[0], rawStream: stream };
  }

  await harmony.sources.select(source.id, { loopbackAudio: plan.via === 'chromium' });

  const video = { frameRate: { ideal: preset.fps, max: preset.fps } };
  if (preset.width) {
    video.width = { max: preset.width };
    video.height = { max: preset.height };
  }

  const stream = await navigator.mediaDevices.getDisplayMedia({
    video,
    audio: plan.via === 'chromium',
  });
  return { track: stream.getVideoTracks()[0], rawStream: stream };
}

/**
 * Point the mixer at whatever this source's audio should be. The published
 * audio track is untouched -- only what feeds it changes.
 */
async function routeAudio(source, plan) {
  state.bridge.detachPcm();
  state.bridge.detachDevice();

  if (plan.via === 'device') {
    try {
      const label = await state.bridge.attachDevice(el.audioInput.value || undefined);
      return { mode: 'device', note: `Viewers will hear ${label}.` };
    } catch (err) {
      return { mode: 'none', note: `Could not open that audio input: ${err.message}` };
    }
  }

  if (plan.via === 'chromium') {
    state.bridge.attachStream(state.live.rawStream);
    return { mode: 'chromium-loopback', note: plan.note };
  }

  if (plan.via === 'native') {
    const result = await harmony.audio.start({
      kind: source.kind,
      sourceId: source.id,
      sourceName: source.name,
      fallback: el.fallback.value,
      excludePid: source.kind === 'screen' ? Number(el.excludeApp.value) || null : null,
    });
    if (result.mode !== 'none' && result.mode !== 'chromium-loopback') state.bridge.attachPcm();
    return { mode: result.mode, note: result.note ?? plan.note };
  }

  await harmony.audio.stop().catch(() => {});
  return { mode: 'none', note: plan.note };
}

async function startBroadcast() {
  const source = state.selectedSource;
  if (!source) return;

  const preset = QUALITY[el.quality.value];
  const priority = PRIORITY[el.priority.value] ?? PRIORITY.sharp;
  const plan = audioPlan();

  el.startStream.disabled = true;
  el.startStream.textContent = 'Going live…';

  await harmony.settings.set({
    quality: el.quality.value,
    priority: el.priority.value,
    windowAudioFallback: el.fallback.value,
  });

  try {
    // Video first: picking a capture source is what grants the user activation
    // an AudioContext needs to leave the suspended state.
    const { track: videoTrack, rawStream, previewTrack } = await acquireVideo(source, preset, plan);
    state.live.rawStream = rawStream;
    state.live.source = source;
    state.preview.stream = new MediaStream([previewTrack]);

    // One audio track for the whole broadcast, created before anything is
    // published so that switching sources later needs no renegotiation.
    const audioTrack = await state.bridge.start();

    const audio = await routeAudio(source, plan);
    state.live.audioMode = audio.mode;
    const audioNote = audio.note;

    const stream = new MediaStream([videoTrack, audioTrack]);
    state.localStream = stream;
    state.live.videoTrack = videoTrack;

    // Stopping the share from the OS overlay ends the track, not the session.
    videoTrack.addEventListener('ended', () => stopBroadcast());

    // Choosing a source can take a while, and a channel token minted before
    // the picker opened may have expired while a window was being chosen.
    if (state.share.target) {
      await refreshVoiceTokens();
      state.share.target.url = state.voice.publishUrls.screen || state.share.target.url;
    }

    const { pc, resourceUrl } = await publish({
      url: state.share.target ? state.share.target.url : state.session.whipUrl,
      stream,
      iceServers: state.session.iceServers,
      codec: 'H264',
      maxBitrate: preset.bitrate,
      maxFramerate: preset.fps,
      contentHint: priority.contentHint,
      degradationPreference: priority.degradationPreference,
      insertableStreams: state.clips.enabled,
    });

    state.live.videoSender = pc.getSenders().find((s) => s.track?.kind === 'video') ?? null;

    // Our own frames are already encoded on the way out, so buffering them
    // is pure copying.
    state.clips.own = attachClips(pc, state.session.username, 'sender');
    el.broadcastClip.hidden = !state.clips.own;

    state.pc = pc;
    state.resourceUrl = resourceUrl;
    state.statsReader = createStatsReader(pc, 'outbound');

    /*
     * A successful WHIP handshake is not proof of being on air. MediaMTX
     * answers the offer before it decides whether this publisher may have the
     * path, so if the name is already taken at the media-server level the
     * connection comes up and the stream then goes nowhere. Confirm the server
     * actually sees us live rather than trusting the 201.
     *
     * Not for a channel share: /api/streams deliberately filters `vc-*` out,
     * so the check could only ever fail. The equivalent there is telling the
     * channel we are publishing, which is what makes everyone else subscribe.
     */
    if (state.share.target) {
      await harmony.realtime.request('voice:publishing', {
        channelId: state.share.target.channelId, kind: 's', on: true,
      });
    } else {
      await confirmLive();
    }

    // Re-read rather than trusting what boot() saw: the GPU process reports
    // roughly 300ms after the window loads, and boot() runs before that. Main
    // caches the answer, so this is free once it has settled.
    refreshGpuStatus();

    applyPreviewVisibility();
    el.broadcastTitle.textContent = state.share.target
      ? `Sharing in ${state.share.target.name}`
      : `Live as ${state.session.username}`;
    if (state.share.target) el.viewerCount.textContent = '';
    applyVoiceButtons();
    renderChannelVideo();
    el.broadcastAudioNote.textContent = audioNote;
    el.broadcastStats.textContent = 'Connecting…';
    el.liveQuality.value = el.quality.value;
    el.livePriority.value = el.priority.value;
    updateMonitorButton();
    /*
     * A channel share goes BACK TO THE CHANNEL rather than to the broadcast
     * screen.
     *
     * The broadcast screen is the right place for a flat share, where there
     * is nothing else going on. Here there is: the roster, the chat and
     * everyone else's video. Parking the sharer on a preview of their own
     * screen would take all of that away from them the moment they started
     * sharing, which is the opposite of what a voice channel is for. The
     * preview is also the expensive thing to paint, and Chromium stops
     * compositing it as soon as the view is inactive.
     */
    showView(state.share.target ? 'view-channels' : 'view-broadcast');

    pc.addEventListener('connectionstatechange', () => {
      if (['failed', 'closed'].includes(pc.connectionState)) {
        stopBroadcast(`Connection ${pc.connectionState}.`);
      }
    });

    addTimer(setInterval(updateBroadcastStats, 1000));
  } catch (err) {
    const wasChannel = Boolean(state.share.target);
    await teardown();
    const text = err.name === 'NotAllowedError' ? 'Screen capture was blocked.' : err.message;
    if (wasChannel) {
      showChannelsError(text);
      showView('view-channels');
    } else {
      showError(text);
      showView('view-connect');
    }
  } finally {
    el.startStream.disabled = false;
    el.startStream.textContent = 'Start streaming';
  }
}

/**
 * Paint the preview, or stop painting it.
 *
 * Detaching `srcObject` is what actually saves the work: the video element
 * stops being composited, while the MediaStreamTrack behind it carries on being
 * captured and encoded, because the RTCRtpSender holds that track independently
 * of any element displaying it. Hiding with CSS would not do this -- the frames
 * would still arrive and still be painted.
 *
 * Why it matters here more than in an ordinary app: Harmony disables Chromium's
 * occlusion and background throttling so the encoder keeps running while the
 * broadcaster looks at what they are sharing. That same setting means a preview
 * sitting on a second monitor, or behind a fullscreen game, is composited
 * forever at full rate -- on the very GPU the game is using.
 *
 * Two things stop it: the user asking, and the window being minimised. Losing
 * focus deliberately does not, even though it is free performance -- a preview
 * that blanks itself every time you click elsewhere reads as a bug, and with
 * the preview downscaled (previewCopy) and the display no longer crossing
 * adapters, what it saves is no longer worth what it costs in confusion.
 */
function applyPreviewVisibility() {
  const reason = previewPauseReason();

  if (!reason) {
    if (state.preview.stream && el.preview.srcObject !== state.preview.stream) {
      el.preview.srcObject = state.preview.stream;
    }
  } else if (el.preview.srcObject) {
    el.preview.srcObject = null;
  }

  el.previewOff.hidden = !reason;
  if (reason === 'minimised') {
    el.previewOffTitle.textContent = 'Preview paused';
    el.previewOffText.textContent =
      'You are still live. Harmony is not drawing the preview while the window is minimised, ' +
      'because nothing would see it — that work would be pure waste.';
  } else {
    el.previewOffTitle.textContent = 'Preview hidden';
    el.previewOffText.textContent =
      'You are still live. Drawing the preview costs GPU work on top of the game you are sharing, ' +
      'so hiding it is free performance.';
  }

  el.togglePreview.textContent = state.preview.hiddenByUser ? 'Show preview' : 'Hide preview';
  el.togglePreview.classList.toggle('active', state.preview.hiddenByUser);
}

/** @returns {'user'|'minimised'|null} null meaning "paint it". */
function previewPauseReason() {
  if (state.preview.hiddenByUser) return 'user';
  if (!state.preview.windowVisible) return 'minimised';
  return null;
}

/** Plain-language version of WebRTC's qualityLimitationReason. */
const LIMIT_TEXT = {
  bandwidth: 'limited by your upload speed',
  cpu: 'limited by this computer',
  other: 'limited',
};

async function updateBroadcastStats() {
  if (!state.statsReader) return;
  const s = await state.statsReader();
  const bits = [
    s.width ? `${s.width}×${s.height}` : null,
    s.fps ? `${s.fps} fps` : null,
    s.kbps ? `${(s.kbps / 1000).toFixed(1)} Mbps` : null,
    s.codec,
    s.rtt != null ? `${s.rtt} ms` : null,
    s.availableKbps ? `link ${(s.availableKbps / 1000).toFixed(1)} Mbps` : null,
    s.encodeMs != null ? `encode ${s.encodeMs.toFixed(1)} ms` : null,
    encoderLabel(s),
  ].filter(Boolean);
  el.broadcastStats.textContent = bits.join('  ·  ') || 'Connecting…';

  // Say why the picture is worse than requested, rather than leaving the
  // broadcaster to guess whether it is them, their connection, or the server.
  const limit = s.limitedBy && s.limitedBy !== 'none' ? LIMIT_TEXT[s.limitedBy] ?? s.limitedBy : null;
  el.broadcastLimit.textContent = limit ? `⚠ ${limit}` : '';
  el.broadcastLimit.hidden = !limit;

  // A channel share is not in /api/streams at all -- `vc-*` is filtered out
  // so a 1.0.0 client does not list thirty paths it cannot name. The channel
  // roster is the count that means anything there.
  if (state.share.target) {
    const n = state.channels.roster.length;
    el.viewerCount.textContent = `${Math.max(0, n - 1)} in the channel`;
    return;
  }

  try {
    const { streams } = await harmony.api.streams(state.session.server);
    const mine = streams.find((x) => x.username === state.session.username);
    el.viewerCount.textContent = `${mine?.viewers ?? 0} watching`;
  } catch {
    /* transient */
  }
}

// ---------------------------------------------------------------------------
// Clips
// ---------------------------------------------------------------------------

/**
 * Start buffering a connection, if the user has clips switched on.
 *
 * Taps the encoded frames rather than the decoded ones, so nothing is
 * re-encoded; see clip-buffer.js. Returns null when there is nothing to tap,
 * which for a receiver means media has not started flowing yet.
 *
 * @param {'sender'|'receiver'} side
 */
function attachClips(pc, label, side) {
  if (!state.clips.enabled || !pc) return null;

  const buffer = new ClipBuffer(label);
  const parts = side === 'sender' ? pc.getSenders() : pc.getReceivers();
  let hasVideo = false;

  for (const part of parts) {
    if (part.track?.kind === 'video') hasVideo = buffer.attach(part, 'video') || hasVideo;
    else if (part.track?.kind === 'audio') buffer.attach(part, 'audio');
  }
  return hasVideo ? buffer : null;
}

async function saveClip(buffer, label, button) {
  if (!buffer) return;
  const original = button?.textContent;
  if (button) {
    button.disabled = true;
    button.textContent = 'Saving…';
  }
  try {
    const { data, seconds } = buffer.build();
    const { path } = await harmony.clips.save(data, label);
    toast(`Clip saved — ${seconds.toFixed(0)}s, ${(data.byteLength / 1e6).toFixed(1)} MB`, path);
  } catch (err) {
    toast(`Could not save the clip: ${err.message}`);
  } finally {
    if (button) {
      button.disabled = false;
      button.textContent = original;
    }
  }
}

/** Small transient message; clicking it reveals the file. */
function toast(message, filePath) {
  let node = document.getElementById('toast');
  if (!node) {
    node = document.createElement('div');
    node.id = 'toast';
    node.className = 'toast';
    document.body.append(node);
  }
  node.textContent = message + (filePath ? '  (click to show)' : '');
  node.onclick = filePath ? () => harmony.clips.reveal(filePath).catch(() => {}) : null;
  node.classList.toggle('clickable', Boolean(filePath));
  node.hidden = false;
  clearTimeout(node.dataset.timer);
  node.dataset.timer = setTimeout(() => {
    node.hidden = true;
  }, 6000);
}

/** Commit a source chosen from the picker while the broadcast is running. */
async function applySourceChange() {
  const source = state.selectedSource;
  if (!source) return;

  el.startStream.disabled = true;
  el.startStream.textContent = 'Switching…';
  try {
    await changeLiveSource(source);
    state.changingSource = false;
    showView('view-broadcast');
  } catch (err) {
    el.audioNote.textContent =
      err.name === 'NotAllowedError' ? 'That source was not allowed.' : err.message;
    el.audioNote.toggleAttribute('data-warn', true);
  } finally {
    el.startStream.disabled = false;
    el.startStream.textContent = state.changingSource ? 'Use this source' : 'Start streaming';
  }
}

/** Apply the quality and priority selectors to a stream that is already live. */
async function applyLiveQuality() {
  const preset = QUALITY[el.liveQuality.value];
  const priority = PRIORITY[el.livePriority.value] ?? PRIORITY.sharp;
  const { videoSender, videoTrack } = state.live;
  if (!videoSender || !videoTrack) return;

  videoTrack.contentHint = priority.contentHint;

  // Resolution and frame rate live on the capture; bitrate and the degradation
  // policy live on the sender. Both have to move together.
  try {
    await videoTrack.applyConstraints({
      frameRate: { ideal: preset.fps, max: preset.fps },
      ...(preset.width ? { width: { max: preset.width }, height: { max: preset.height } } : {}),
    });
  } catch {
    // Some capture sources refuse constraint changes; the sender limits below
    // still apply, so this is not worth failing over.
  }

  await applySenderSettings(videoSender, {
    maxBitrate: preset.bitrate,
    maxFramerate: preset.fps,
    degradationPreference: priority.degradationPreference,
  });

  await harmony.settings.set({ quality: el.liveQuality.value, priority: el.livePriority.value });
}

/**
 * Swap what is being streamed without interrupting the broadcast.
 *
 * replaceTrack() changes the sender's source in place, so there is no
 * renegotiation and viewers keep the same connection -- the picture simply
 * becomes something else.
 */
async function changeLiveSource(source) {
  const preset = QUALITY[el.liveQuality.value];
  const priority = PRIORITY[el.livePriority.value] ?? PRIORITY.sharp;
  const plan = audioPlan(source);

  const previousTrack = state.live.videoTrack;
  const previousStream = state.live.rawStream;
  const previousPreview = state.preview.stream;

  const { track, rawStream, previewTrack } = await acquireVideo(source, preset, plan);
  state.preview.stream = new MediaStream([previewTrack]);
  state.live.rawStream = rawStream;
  state.live.source = source;
  state.selectedSource = source;

  track.contentHint = priority.contentHint;
  await state.live.videoSender.replaceTrack(track);
  state.live.videoTrack = track;
  track.addEventListener('ended', () => stopBroadcast());

  // Only now retire the old capture, so there is no gap in between.
  previousTrack?.stop();
  previousStream?.getTracks().forEach((t) => t.stop());
  previousPreview?.getTracks().forEach((t) => t.stop());

  const audio = await routeAudio(source, plan);
  state.live.audioMode = audio.mode;
  el.broadcastAudioNote.textContent = audio.note ?? '';

  state.localStream = new MediaStream([track, ...state.localStream.getAudioTracks()]);
  // Respects a hidden preview: switching source must not silently turn the
  // painting back on.
  applyPreviewVisibility();

  await applyLiveQuality();
  updateMonitorButton();
}

/**
 * Monitoring is only offered for input devices.
 *
 * With a screen or window share the machine is already playing that sound out
 * loud, and feeding it back into the speakers would land straight back in a
 * system-audio capture. A capture card is the opposite case: nothing plays its
 * audio unless we do.
 */
function canMonitor() {
  return state.live.audioMode === 'device';
}

function updateMonitorButton() {
  const allowed = canMonitor();
  el.monitorToggle.disabled = !allowed;
  if (!allowed) {
    state.bridge.setMonitor(false);
    el.monitorToggle.classList.remove('active');
    el.monitorToggle.title =
      state.live.audioMode === 'none'
        ? 'Nothing to monitor: this source has no audio'
        : 'You already hear this audio through your speakers';
    return;
  }
  const on = state.bridge.monitoring;
  el.monitorToggle.classList.toggle('active', on);
  el.monitorToggle.title = on ? 'Stop hearing my own audio' : 'Hear my own audio';
}

async function stopBroadcast(reason) {
  const wasChannel = Boolean(state.share.target);
  await teardown();
  if (wasChannel) {
    if (reason) showChannelsError(reason);
    showView('view-channels');
    applyVoiceButtons();
    renderChannelVideo();
    return;
  }
  if (reason) showError(reason);
  showView('view-connect');
  refreshLiveList();
}

// ---------------------------------------------------------------------------
// Watching
// ---------------------------------------------------------------------------

async function enterWatch() {
  el.watchTitle.textContent = `Watching ${state.session.username}`;
  el.watchWaiting.hidden = false;
  el.watchWaitingText.textContent = state.session.pending
    ? `Waiting for ${state.session.username} to start sharing…`
    : 'Connecting…';
  el.watchDot.classList.remove('live');
  el.watchStats.textContent = '';
  showView('view-watch');

  connectWatch();
}

async function connectWatch() {
  try {
    const { pc, stream, resourceUrl } = await watch({
      url: state.session.whepUrl,
      iceServers: state.session.iceServers,
      insertableStreams: state.clips.enabled,
    });

    // Attach at once rather than waiting for 'connected': with insertable
    // streams on, incoming frames are held until something reads them.
    state.clips.watch = attachClips(pc, state.session.username, 'receiver');
    el.watchClip.hidden = !state.clips.watch;

    state.pc = pc;
    state.resourceUrl = resourceUrl;
    state.statsReader = createStatsReader(pc, 'inbound');

    el.remote.srcObject = stream;
    // Muted element, gain node does the playing -- same arrangement as the
    // mosaic tiles, and for the same reason: 100% is not always loud enough.
    el.remote.muted = true;
    state.watch.sink?.close();
    state.watch.sink = createSink(stream);
    applyWatchAudio();

    pc.addEventListener('connectionstatechange', () => {
      if (pc.connectionState === 'connected') {
        el.watchWaiting.hidden = true;
        el.watchDot.classList.add('live');
      } else if (['failed', 'disconnected', 'closed'].includes(pc.connectionState)) {
        // The broadcaster stopped or the network dropped. Go back to polling
        // instead of erroring out -- they may well come straight back.
        el.watchWaiting.hidden = false;
        el.watchWaitingText.textContent = `Stream ended. Waiting for ${state.session.username}…`;
        el.watchDot.classList.remove('live');
        retryWatch();
      }
    });

    addTimer(setInterval(updateWatchStats, 1000), 'watch');
  } catch (err) {
    if (err.code === 'not_live' || err.code === 'media_error') {
      el.watchWaitingText.textContent = `Waiting for ${state.session.username} to start sharing…`;
      retryWatch();
    } else {
      await teardown();
      showError(err.message);
      showView('view-connect');
    }
  }
}

function retryWatch() {
  clearTimers('watch');
  state.pc?.close();
  state.pc = null;
  state.statsReader = null;
  addTimer(setTimeout(connectWatch, WATCH_RETRY_MS), 'watch');
}

async function updateWatchStats() {
  if (!state.statsReader) return;
  const s = await state.statsReader();
  const bits = [
    s.width ? `${s.width}×${s.height}` : null,
    s.fps ? `${s.fps} fps` : null,
    s.kbps ? `${(s.kbps / 1000).toFixed(1)} Mbps` : null,
    s.codec,
    s.rtt != null ? `${s.rtt} ms` : null,
  ].filter(Boolean);
  el.watchStats.textContent = bits.join('  ·  ');
}

// ---------------------------------------------------------------------------
// Mosaic: every live stream at once
// ---------------------------------------------------------------------------

/**
 * Narrower than this and a tile is not worth showing.
 *
 * Generous on purpose: these tiles carry screen shares, and a 1080p desktop
 * squeezed into 260px is unreadable. One column of usable tiles that you scroll
 * beats two columns of thumbnails.
 */
const MIN_TILE_WIDTH = 320;

/** Below this a tile has no room for a volume slider beside its buttons. */
const COMPACT_TILE_WIDTH = 260;

const GRID_GAP = 12;

/**
 * Lay the tiles out to fit the box in both directions.
 *
 * Choosing columns from the width alone is not enough: four tiles in two
 * columns of a wide window produce two rows taller than the window, and the
 * bottom row's controls end up below the fold where nobody can reach them.
 *
 * So try every column count and keep the one that makes each 16:9 tile largest
 * while still fitting the available height. Tiles are then given an explicit
 * width, and the grid is centred, so they stay exactly 16:9 rather than being
 * stretched into letterboxes.
 */
function layoutMosaic() {
  const { maximized, tiles } = state.mosaic;

  // One tile filling the grid is just the one-column case with everything else
  // hidden, so the same fitting code covers it.
  for (const [username, entry] of tiles) {
    entry.el.hidden = Boolean(maximized) && username !== maximized;
  }

  const count = maximized && tiles.has(maximized) ? 1 : tiles.size;
  if (!count) return;

  // clientWidth includes the grid's own padding, but the tiles are laid out in
  // the content box inside it. Measuring the wrong box made every layout up to
  // 2 x 12px too wide, which is exactly enough to raise a horizontal scrollbar
  // on a grid that was otherwise a perfect fit.
  const style = getComputedStyle(el.mosaicGrid);
  const padX = parseFloat(style.paddingLeft) + parseFloat(style.paddingRight);
  const padY = parseFloat(style.paddingTop) + parseFloat(style.paddingBottom);

  const width = (el.mosaicGrid.clientWidth || window.innerWidth) - padX;
  const height = (el.mosaicGrid.clientHeight || window.innerHeight) - padY;
  if (width < 2 || height < 2) return; // not laid out yet

  let best = null;
  for (let cols = 1; cols <= count; cols++) {
    const rows = Math.ceil(count / cols);
    const cellW = (width - GRID_GAP * (cols - 1)) / cols;
    const cellH = (height - GRID_GAP * (rows - 1)) / rows;
    if (cellW <= 0 || cellH <= 0) continue;

    // Largest 16:9 box that fits the cell.
    const tileW = Math.min(cellW, (cellH * 16) / 9);
    if (tileW < MIN_TILE_WIDTH) continue;
    if (!best || tileW > best.tileW) best = { cols, tileW };
  }

  // Nothing fits the height at a usable size: fall back to filling the width
  // and letting the grid scroll, which is better than unreadable thumbnails.
  if (!best) {
    const cols = Math.max(1, Math.floor((width + GRID_GAP) / (MIN_TILE_WIDTH + GRID_GAP)));
    best = { cols, tileW: (width - GRID_GAP * (cols - 1)) / cols };
  }

  el.mosaicGrid.style.setProperty('--cols', best.cols);
  el.mosaicGrid.style.setProperty('--tile-w', `${Math.floor(best.tileW)}px`);

  for (const entry of state.mosaic.tiles.values()) {
    entry.el.classList.toggle('compact', best.tileW < COMPACT_TILE_WIDTH);
  }
}

/**
 * @param {string[]|null} usernames  specific streams to show, or null for all
 */
/** True when we are publishing right now, so the mosaic must not disturb it. */
const isBroadcasting = () => Boolean(state.live.videoSender && state.pc);

async function enterMosaic(usernames = null) {
  const server = el.serverUrl.value.trim() || state.server;
  if (!server) return showError('Enter the address of your Harmony server.');

  state.server = server;
  state.mosaic = freshMosaic();
  state.mosaic.selection = usernames ? new Set(usernames) : null;
  el.mosaicLeave.textContent = (() => {
    if (isBroadcasting() && !state.share.target) return 'Back to my stream';
    return state.auth.user ? 'Back to channels' : 'Leave';
  })();
  el.mosaicGrid.replaceChildren();
  el.mosaicVolume.value = '100';
  el.mosaicVolumeLabel.textContent = '100%';
  el.mosaicMute.innerHTML = '&#128266;';
  showView('view-mosaic');

  await syncMosaic();
  // Streams come and go while you watch; the grid follows. This used to be the
  // only mechanism and ran every 3 seconds. The server now pushes the list on
  // change, so this is demoted to a reconciliation net for the case the socket
  // is down -- which is also why it is not removed outright.
  addTimer(setInterval(syncMosaic, MOSAIC_RECONCILE_MS), 'mosaic');
  addTimer(setInterval(updateMosaicMeta, 1000), 'mosaic');
}

/**
 * How often to reconcile the mosaic against the server by polling.
 *
 * Fifteen seconds rather than three, because the authoritative path is now a
 * push. This only has to cover a socket that has quietly died, and the
 * realtime watchdog already notices that within 35 s.
 */
const MOSAIC_RECONCILE_MS = 15_000;

/**
 * How often to retry voice subscriptions that did not come up.
 *
 * Short, because the gap it covers is a peer being silently inaudible, and
 * cheap, because it does nothing at all unless something is actually missing.
 */
/**
 * The saved microphone, or undefined for the system default.
 *
 * Returns undefined rather than '' because getUserMedia treats an explicit
 * empty deviceId as a constraint that nothing satisfies.
 */
function deviceForVoiceInput() {
  const wanted = state.settings.voiceInputId;
  if (!wanted) return undefined;
  // Only if it is actually there; otherwise the exact-device constraint
  // throws and the join fails rather than falling back.
  return lastDevices.inputs.some((d) => d.deviceId === wanted) ? wanted : undefined;
}

const VOICE_RECONCILE_MS = 4000;
const SPEAKING_POLL_MS = 100;

async function syncMosaic({ streams: pushed } = {}) {
  let streams = pushed;
  let iceServers;

  if (!streams) {
    try {
      ({ streams, iceServers } = await harmony.api.streams(state.server));
    } catch {
      return; // transient; the next tick retries
    }
  }
  if (iceServers) state.mosaic.iceServers = iceServers;

  const { selection, closed } = state.mosaic;
  const wanted = streams.filter((s) => {
    // Closing a tile has to stick. Without this the next sync -- three seconds
    // later -- sees the stream still live and opens it straight back up.
    if (closed.has(s.username)) return false;
    if (selection) return selection.has(s.username);
    // Watching everything while broadcasting should not include a second copy
    // of your own stream -- the preview already shows it, and pulling it back
    // down from the server would just spend bandwidth twice.
    return !(isBroadcasting() && s.username === state.session?.username);
  });
  const wantedNames = new Set(wanted.map((s) => s.username));

  for (const username of [...state.mosaic.tiles.keys()]) {
    if (!wantedNames.has(username)) removeTile(username);
  }
  for (const stream of wanted) {
    if (!state.mosaic.tiles.has(stream.username)) openTile(stream);
  }

  const count = state.mosaic.tiles.size;
  el.mosaicCount.textContent = `${count} ${count === 1 ? 'stream' : 'streams'}`;
  layoutMosaic();

  const empty = el.mosaicGrid.querySelector('.empty');
  if (!count && !empty) {
    el.mosaicGrid.replaceChildren(
      message(
        closed.size
          ? 'No streams open. Use + Add stream to bring one back.'
          : selection
            ? 'None of the chosen streams are live.'
            : 'Nobody is streaming right now.',
      ),
    );
  } else if (count && empty) {
    empty.remove();
  }
}

/** Add a stream to the mosaic, switching into it from wherever we are. */
async function addStreamToMosaic(username) {
  closeAddStream();

  if (state.mosaic.tiles.size || document.getElementById('view-mosaic').hasAttribute('data-active')) {
    // Already in the mosaic: widen the selection and let the next sync open it.
    // Asking for it explicitly also overrides an earlier dismissal.
    state.mosaic.closed.delete(username);
    if (state.mosaic.selection) state.mosaic.selection.add(username);
    await syncMosaic();
    return;
  }

  // Coming from the single-stream view: keep what we were watching and add to it.
  const current = state.session?.username;
  await teardown();
  await enterMosaic(current ? [current, username] : [username]);
}

/**
 * Note this never calls /api/session: watching is public and claiming a name
 * here could steal it from a broadcaster who stopped a moment ago.
 */
function openTile({ username, whepUrl }) {
  const tile = document.createElement('div');
  tile.className = 'tile';
  tile.dataset.user = username;

  const video = document.createElement('video');
  video.autoplay = true;
  video.playsInline = true;
  // Permanently muted: the element only ever shows the picture, and gain.js
  // plays the sound. Leaving it unmuted would play everything twice.
  video.muted = true;

  const status = document.createElement('div');
  status.className = 'tile-status';
  const spinner = document.createElement('div');
  spinner.className = 'spinner';
  const statusText = document.createElement('span');
  statusText.textContent = `Connecting to ${username}…`;
  status.append(spinner, statusText);

  const bar = document.createElement('div');
  bar.className = 'tile-bar';
  const dot = document.createElement('span');
  dot.className = 'dot live';
  const name = document.createElement('span');
  name.className = 'tile-name';
  name.textContent = username;
  const meta = document.createElement('span');
  meta.className = 'tile-meta';

  const controls = document.createElement('div');
  controls.className = 'tile-controls';

  const muteBtn = document.createElement('button');
  muteBtn.className = 'tile-btn';
  muteBtn.type = 'button';
  muteBtn.dataset.role = 'mute';
  muteBtn.innerHTML = '&#128266;';
  muteBtn.title = `Mute ${username}`;

  const volume = document.createElement('input');
  volume.type = 'range';
  volume.className = 'tile-volume';
  volume.min = '0';
  volume.max = String(asPercent(MAX_GAIN));
  volume.value = '100';
  volume.title = `Volume for ${username}`;

  // Maximize fills the mosaic with this one stream but stays inside the window,
  // so the rest of the app -- and everything else on the desktop -- is still
  // visible. Fullscreen, next to it, covers the screen.
  const maxBtn = document.createElement('button');
  maxBtn.className = 'tile-btn';
  maxBtn.type = 'button';
  maxBtn.dataset.role = 'maximize';
  maxBtn.innerHTML = '&#10530;';
  maxBtn.title = `Maximize ${username}`;

  const fsBtn = document.createElement('button');
  fsBtn.className = 'tile-btn';
  fsBtn.type = 'button';
  fsBtn.dataset.role = 'fullscreen';
  fsBtn.innerHTML = '&#9974;';
  fsBtn.title = 'Fullscreen';

  const closeBtn = document.createElement('button');
  closeBtn.className = 'tile-btn';
  closeBtn.type = 'button';
  closeBtn.dataset.role = 'close';
  closeBtn.innerHTML = '&#10005;';
  closeBtn.title = `Close ${username}`;

  const clipBtn = document.createElement('button');
  clipBtn.className = 'tile-btn';
  clipBtn.type = 'button';
  clipBtn.dataset.role = 'clip';
  clipBtn.innerHTML = '&#9986;';
  clipBtn.title = `Save the last ${CLIP_SECONDS} seconds`;
  clipBtn.hidden = true;

  controls.append(muteBtn, volume, clipBtn, maxBtn, fsBtn, closeBtn);
  bar.append(dot, name, meta, controls);

  tile.append(video, status, bar);
  el.mosaicGrid.append(tile);

  const entry = {
    el: tile,
    video,
    status,
    statusText,
    meta,
    muteBtn,
    fsBtn,
    maxBtn,
    closeBtn,
    clipBtn,
    clips: null,
    pc: null,
    resourceUrl: null,
    stats: null,
    // Per-tile audio. Every stream can be heard at once; these are how you
    // balance them rather than being forced to pick just one. `sink` is the
    // gain node that actually plays it, created once the stream arrives.
    volume: 1,
    muted: false,
    sink: null,
  };
  state.mosaic.tiles.set(username, entry);

  // Controls sit inside the tile, so stop their clicks reaching it.
  muteBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    entry.muted = !entry.muted;
    muteBtn.innerHTML = entry.muted ? '&#128263;' : '&#128266;';
    muteBtn.title = `${entry.muted ? 'Unmute' : 'Mute'} ${username}`;
    applyTileAudio();
  });

  volume.addEventListener('click', (e) => e.stopPropagation());
  volume.addEventListener('input', (e) => {
    e.stopPropagation();
    entry.volume = Number(volume.value) / 100;
    if (entry.volume > 0 && entry.muted) {
      entry.muted = false;
      muteBtn.innerHTML = '&#128266;';
    }
    applyTileAudio();
  });

  clipBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    saveClip(entry.clips, username, clipBtn);
  });

  maxBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    toggleMaximized(username);
  });

  closeBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    closeTile(username);
  });

  fsBtn.addEventListener('click', (e) => {
    e.stopPropagation();
    toggleFullscreen(tile);
  });
  tile.addEventListener('dblclick', () => toggleFullscreen(tile));

  watch({ url: whepUrl, iceServers: state.mosaic.iceServers, insertableStreams: state.clips.enabled })
    .then(({ pc, stream, resourceUrl }) => {
      // The tile may have been removed while we were connecting.
      if (state.mosaic.tiles.get(username) !== entry) {
        hangup(pc, resourceUrl);
        return;
      }
      entry.pc = pc;
      entry.resourceUrl = resourceUrl;
      entry.stats = createStatsReader(pc, 'inbound');
      entry.clips = attachClips(pc, username, 'receiver');
      if (entry.clipBtn) entry.clipBtn.hidden = !entry.clips;
      video.srcObject = stream;
      entry.sink = createSink(stream);
      status.hidden = true;
      // applyTileAudio owns the level; setting it here as well once cost every
      // tile its sound, by throwing before it could run.
      applyTileAudio();
    })
    .catch((err) => {
      if (state.mosaic.tiles.get(username) !== entry) return;
      spinner.remove();
      entry.statusText.textContent = err.code === 'not_live' ? 'Stream ended' : err.message;
    });
}

/**
 * Dismiss one stream from the mosaic.
 *
 * The connection is torn down, not just hidden -- a tile you cannot see should
 * not still be costing you a decoder and the bandwidth of a 1080p stream. The
 * name is remembered so the periodic sync does not reopen it; + Add stream is
 * the way back.
 */
function closeTile(username) {
  const { mosaic } = state;
  mosaic.closed.add(username);
  mosaic.selection?.delete(username);
  if (mosaic.maximized === username) mosaic.maximized = null;
  removeTile(username);

  const count = mosaic.tiles.size;
  el.mosaicCount.textContent = `${count} ${count === 1 ? 'stream' : 'streams'}`;
  layoutMosaic();

  if (!count) {
    el.mosaicGrid.replaceChildren(
      message('No streams open. Use + Add stream to bring one back.'),
    );
  }
}

/** Fill the grid with one stream, without leaving the window. */
function toggleMaximized(username) {
  const { mosaic } = state;
  mosaic.maximized = mosaic.maximized === username ? null : username;

  for (const [name, entry] of mosaic.tiles) {
    const on = mosaic.maximized === name;
    entry.maxBtn.innerHTML = on ? '&#10529;' : '&#10530;';
    entry.maxBtn.title = on ? 'Back to the grid' : `Maximize ${name}`;
  }
  layoutMosaic();
}

function removeTile(username) {
  const entry = state.mosaic.tiles.get(username);
  if (!entry) return;
  state.mosaic.tiles.delete(username);
  // A maximized stream that ends must not leave the grid stuck showing nothing.
  if (state.mosaic.maximized === username) state.mosaic.maximized = null;
  entry.clips?.detach();
  // If this tile was filling the screen, do not leave the user stranded there.
  if (document.fullscreenElement === entry.el) document.exitFullscreen().catch(() => {});
  entry.video.srcObject = null;
  entry.sink?.close();
  entry.sink = null;
  if (entry.pc) hangup(entry.pc, entry.resourceUrl);
  entry.el.remove();
}

/** The single-stream viewer's level, mirrored onto the element for inspection. */
function applyWatchAudio() {
  const { volume, muted, sink } = state.watch;
  const gain = muted ? 0 : Math.min(MAX_GAIN, volume);
  sink?.set(gain);
  el.remote.dataset.gain = String(gain);
  el.remote.dataset.muted = String(muted);

  el.volume.value = String(asPercent(volume));
  el.volumeLabel.textContent = `${asPercent(volume)}%`;
  el.volumeLabel.classList.toggle('boosted', volume > 1);
  el.toggleMute.innerHTML = muted ? '&#128263;' : '&#128266;';
  el.toggleMute.title = muted ? 'Unmute' : 'Mute';
}

/**
 * Every tile can be heard at once; the master control scales them all.
 * Effective volume is the tile's own level times the master level.
 *
 * The element stays muted and a GainNode does the playing, which is what allows
 * a level above 100% -- see gain.js. `dataset.gain` mirrors the result so the
 * effective level is visible to anything inspecting the DOM, including tests,
 * now that `video.volume` no longer means anything.
 */
function applyTileAudio() {
  const { master } = state.mosaic;
  for (const entry of state.mosaic.tiles.values()) {
    const silent = entry.muted || master.muted;
    const gain = silent ? 0 : Math.min(MAX_GAIN, entry.volume * master.volume);
    entry.sink?.set(gain);
    entry.video.dataset.gain = String(gain);
    entry.video.dataset.muted = String(silent);
  }
}

async function updateMosaicMeta() {
  for (const entry of state.mosaic.tiles.values()) {
    if (!entry.stats) continue;
    const s = await entry.stats();
    entry.meta.textContent = s.width ? `${s.height}p · ${s.fps} fps` : '';
  }
}

async function leaveMosaic() {
  // Only the mosaic's own timers: a broadcast may still be running behind it.
  clearTimers('mosaic');
  if (document.fullscreenElement) await document.exitFullscreen().catch(() => {});
  for (const username of [...state.mosaic.tiles.keys()]) removeTile(username);
  state.mosaic = freshMosaic();

  if (isBroadcasting() && !state.share.target) {
    showView('view-broadcast');
    return;
  }
  // Somebody signed in came from the channels, and that is where Leave has
  // to put them back. Sending them to the connect screen -- which is what
  // this did -- drops a signed-in person at a login form with no obvious way
  // back into the channel they were in a moment ago.
  if (state.auth.user) {
    showView('view-channels');
    renderChannels();
    return;
  }
  showView('view-connect');
  refreshLiveList();
}

// ---------------------------------------------------------------------------
// Fullscreen
//
// The real Fullscreen API rather than a CSS overlay, so a stream covers the
// taskbar like any other video. Chromium already exits on Escape; the explicit
// key handler is for the case where focus sits somewhere that swallows it.
// ---------------------------------------------------------------------------

function toggleFullscreen(element) {
  if (document.fullscreenElement === element) {
    document.exitFullscreen().catch(() => {});
  } else {
    element.requestFullscreen().catch((err) => console.warn('[fullscreen]', err.message));
  }
}

// Escape backs out of exactly one thing at a time, outermost first.
document.addEventListener('keydown', (event) => {
  if (event.key !== 'Escape') return;
  if (document.fullscreenElement) {
    event.preventDefault();
    document.exitFullscreen().catch(() => {});
  } else if (!el.addStream.hidden) {
    closeAddStream();
  } else if (state.mosaic.maximized) {
    event.preventDefault();
    toggleMaximized(state.mosaic.maximized);
  }
});

document.addEventListener('fullscreenchange', () => {
  const active = document.fullscreenElement;
  for (const entry of state.mosaic.tiles.values()) {
    const on = entry.el === active;
    entry.fsBtn.innerHTML = on ? '&#10005;' : '&#9974;';
    entry.fsBtn.title = on ? 'Exit fullscreen (Esc)' : 'Fullscreen';
  }
  const watching = el.watchView === active;
  el.watchFullscreen.innerHTML = watching ? '&#10005;' : '&#9974;';
  el.watchFullscreen.title = watching ? 'Exit fullscreen (Esc)' : 'Fullscreen';

  // Start every fullscreen session with the chrome out of the way; the pointer
  // reaching for the bottom of the screen is what brings it back.
  document.querySelectorAll('.hud-visible').forEach((n) => n.classList.remove('hud-visible'));
});

/**
 * Reveal the controls when the pointer goes looking for them.
 *
 * Fullscreen is for watching, so the bars are hidden by default -- but they
 * have to be reachable, and the two things people reach for are volume and the
 * way out. Bottom-edge proximity is the convention every video player uses, so
 * it needs no explaining.
 */
const HUD_ZONE_PX = 120;

document.addEventListener('mousemove', (event) => {
  const fs = document.fullscreenElement;
  if (!fs) return;
  const rect = fs.getBoundingClientRect();
  // A share of the height as well as a fixed band, so the target is not
  // uncomfortably thin on a 4K screen.
  const zone = Math.max(HUD_ZONE_PX, rect.height * 0.15);
  fs.classList.toggle('hud-visible', event.clientY >= rect.bottom - zone);
});

// ---------------------------------------------------------------------------
// Add-stream picker
// ---------------------------------------------------------------------------

async function openAddStream() {
  el.addStreamItems.replaceChildren();
  el.addStreamEmpty.hidden = true;
  el.addStream.hidden = false;

  const already = new Set([
    ...state.mosaic.tiles.keys(),
    ...(state.session ? [state.session.username] : []),
  ]);

  let streams = [];
  try {
    ({ streams } = await harmony.api.streams(state.server || el.serverUrl.value.trim()));
  } catch {
    /* fall through to the empty message */
  }

  const options = streams.filter((s) => !already.has(s.username));
  if (!options.length) {
    el.addStreamEmpty.hidden = false;
    return;
  }

  for (const stream of options) {
    const li = document.createElement('li');
    const dot = document.createElement('span');
    dot.className = 'dot live';
    const who = document.createElement('span');
    who.className = 'who';
    who.textContent = stream.username;
    const count = document.createElement('span');
    count.className = 'pill';
    count.textContent = `${stream.viewers} watching`;
    li.append(dot, who, count);
    li.addEventListener('click', () => addStreamToMosaic(stream.username));
    el.addStreamItems.append(li);
  }
}

function closeAddStream() {
  el.addStream.hidden = true;
}

// ---------------------------------------------------------------------------
// Teardown
// ---------------------------------------------------------------------------

async function teardown() {
  clearTimers();

  /*
   * A channel share is torn down differently from a flat one, in two ways
   * that both matter.
   *
   * It has to tell the channel it stopped, or every other member keeps a
   * subscription open to a path with nothing coming out of it. And it must
   * NOT release the flat username claim below: that claim belongs to this
   * sign-in, not to this share, and giving it back here would quietly drop
   * the name while the person is still signed in under it.
   */
  const channelShare = state.share.target;
  state.share.target = null;
  if (channelShare) {
    await harmony.realtime
      .request('voice:publishing', { channelId: channelShare.channelId, kind: 's', on: false })
      .catch(() => { /* leaving the channel says the same thing */ });
  }

  if (state.pc) await hangup(state.pc, state.resourceUrl);
  state.pc = null;
  state.resourceUrl = null;
  state.statsReader = null;

  await state.bridge.stop();
  await harmony.audio.stop().catch(() => {});
  await harmony.sources.clear().catch(() => {});

  state.localStream?.getTracks().forEach((t) => t.stop());
  state.localStream = null;
  state.clips.own?.detach();
  state.clips.watch?.detach();
  state.clips.own = null;
  state.clips.watch = null;
  el.broadcastClip.hidden = true;
  el.watchClip.hidden = true;

  state.live.rawStream?.getTracks().forEach((t) => t.stop());
  state.preview.stream?.getTracks().forEach((t) => t.stop());
  state.preview.stream = null;
  state.live = { videoSender: null, videoTrack: null, rawStream: null, source: null, audioMode: null };
  state.changingSource = false;
  el.preview.srcObject = null;
  el.remote.srcObject = null;
  state.watch.sink?.close();
  state.watch.sink = null;

  // Give the username back at once rather than waiting for the claim to lapse.
  if (state.session?.token && !channelShare) {
    await harmony.api
      .release(state.session.server, state.session.username, state.session.token)
      .catch(() => {});
    state.lastClaim = null;
  }
  if (!channelShare) state.session = null;
  state.selectedSource = null;
}

/**
 * A snapshot of this client's view of the call.
 *
 * Deliberately a permanent part of the app rather than a test-only hook.
 * Every voice bug reported so far -- one-way audio, a stuck reconnect, a
 * channel nobody could hear -- looked identical from the UI, and answering
 * "did you subscribe, and is the audio routed" needed a guess each time.
 * It exposes nothing the person cannot already see on their own screen.
 */
window.__harmony = () => ({
  signedInAs: state.auth.user?.nickname ?? null,
  view: document.querySelector('.view[data-active]')?.id ?? null,
  voice: state.voice.diagnostics(),
  sharing: state.share.target?.channelId ?? null,
  channels: state.channels.list.map((c) => `${c.kind}:${c.id}:${c.name}`),
});

/** What our microphone is putting on the wire. Async; see publishStats. */
window.__harmonyPublish = () => state.voice.publishStats();

/** What each voice subscription is receiving. Async; see subscribeStats. */
window.__harmonySubs = () => state.voice.subscribeStats();

// ---------------------------------------------------------------------------
// Wiring
// ---------------------------------------------------------------------------

el.continue.addEventListener('click', startSession);

// --- channels and voice ---------------------------------------------------

harmony.realtime.onEvent(onRealtimeEvent);

el.channelsShare.addEventListener('click', () => {
  // Clear the channel target first. enterPicker() reads it to decide which
  // WHIP URL the share publishes to, and it is left set after a channel
  // share stops -- so the top-right button could re-enter the picker still
  // pointed at a channel, which is most of why it behaved oddly.
  state.share.target = null;
  return enterPicker();
});
el.channelsWatch.addEventListener('click', () => enterMosaic());

el.channelsSignout.addEventListener('click', async () => {
  await leaveVoice({ silent: true }).catch(() => {});
  await harmony.realtime.disconnect();
  await harmony.api.logout(state.server).catch(() => {});
  state.auth.user = null;
  await adoptSession('');
  showView('view-connect');
});

el.voiceMute.addEventListener('click', async () => {
  const muted = state.voice.setMuted(!state.voice.muted);
  applyVoiceButtons();
  await harmony.realtime
    .request('voice:mute', { channelId: state.voice.channelId, muted })
    .catch(() => { /* local mute still applies */ });
});

el.voiceDeafen.addEventListener('click', () => {
  // Deafening implies muting: being able to hear nobody while still talking is
  // never what anyone means by it, and it is how people end up broadcasting a
  // conversation they think is private.
  state.voice.setDeafened(!state.voice.deafened);
  if (state.voice.deafened && !state.voice.muted) {
    state.voice.setMuted(true);
    harmony.realtime
      .request('voice:mute', { channelId: state.voice.channelId, muted: true })
      .catch(() => {});
  }
  applyVoiceButtons();
});

el.voiceLeave.addEventListener('click', () => leaveVoice());

el.askForm.addEventListener('submit', (event) => {
  event.preventDefault();
  const missing = [...el.askFields.querySelectorAll('[required]')]
    .find((input) => !input.value.trim());
  if (missing) {
    el.askError.textContent = 'That one cannot be empty.';
    el.askError.hidden = false;
    missing.focus();
    return;
  }
  const values = {};
  for (const input of el.askFields.querySelectorAll('input, select')) {
    values[input.name] = input.value;
  }
  closeAsk(values);
});
el.askCancel.addEventListener('click', () => closeAsk(null));
// Esc closes a <dialog> without submitting, and the promise must still settle.
el.ask.addEventListener('cancel', (event) => {
  event.preventDefault();
  closeAsk(null);
});

el.avatarButton.addEventListener('click', () => el.avatarFile.click());
el.avatarFile.addEventListener('change', () => {
  const file = el.avatarFile.files?.[0];
  // Reset first, so picking the same file twice in a row still fires 'change'.
  el.avatarFile.value = '';
  if (file) setOwnAvatar(file);
});

el.voiceInput.addEventListener('change', async () => {
  await harmony.settings.set({ voiceInputId: el.voiceInput.value });
  state.settings = await harmony.settings.get();
  deviceNote('');
  await applyVoiceInput(el.voiceInput.value);
});

el.voiceOutput.addEventListener('change', async () => {
  await harmony.settings.set({ voiceOutputId: el.voiceOutput.value });
  state.settings = await harmony.settings.get();
  deviceNote('');
  await applyVoiceOutput(el.voiceOutput.value);
});

el.voiceScreen.addEventListener('click', () => shareScreenHere());

/*
 * The soundpad's volume.
 *
 * Written on 'change' rather than 'input' -- dragging a slider fires input
 * for every pixel, and each one is a settings file write. The label follows
 * 'input' so it still moves under the finger.
 */
el.soundpadVolume.addEventListener('input', () => {
  applyPeerVolumeLook(
    el.soundpadVolume, el.soundpadVolumeLabel, Number(el.soundpadVolume.value), false,
  );
});
el.soundpadVolume.addEventListener('change', async () => {
  await harmony.settings.set({ soundpadVolume: Number(el.soundpadVolume.value) });
  state.settings = await harmony.settings.get();
  applySoundpadVolume();
});
el.soundpadMute.addEventListener('click', async () => {
  const now = Number(el.soundpadVolume.value);
  // Unmuting from zero goes back to 100, not to zero-but-not-muted, which
  // is a button that does nothing.
  await harmony.settings.set({ soundpadVolume: now === 0 ? 100 : 0 });
  state.settings = await harmony.settings.get();
  applySoundpadVolume();
});

el.voiceSoundboard.addEventListener('click', () => {
  el.soundpad.dataset.shown = el.soundpad.dataset.shown === '0' ? '1' : '0';
  renderSoundpad();
  applyVoiceButtons();
});

el.voiceCamera.addEventListener('change', async () => {
  await harmony.settings.set({ voiceCameraId: el.voiceCamera.value });
  state.settings = await harmony.settings.get();
  deviceNote('');
  await applyVoiceCamera(el.voiceCamera.value);
});

/*
 * Dismissing the right-click menu.
 *
 * pointerdown rather than click, so it closes on the way down like every
 * other menu; capture, so it still closes when the press lands on something
 * that stops propagation. The menu itself is excluded, or dragging its
 * volume slider would close it on the first pixel.
 */
document.addEventListener('pointerdown', (event) => {
  if (el.peerMenu.hidden) return;
  if (el.peerMenu.contains(event.target)) return;
  closePeerMenu();
}, true);
document.addEventListener('keydown', (event) => {
  if (event.key === 'Escape') closePeerMenu();
});
// Scrolling the roster out from under it would leave it pointing at nothing.
el.voiceRoster.addEventListener('scroll', () => closePeerMenu());
window.addEventListener('blur', () => closePeerMenu());

el.voiceCam.addEventListener('click', () =>
  (state.camera.publication || state.voice.camLive ? stopCamera() : startCamera()));

el.soundpadAdd.addEventListener('click', () => el.soundpadFile.click());
el.soundpadFile.addEventListener('change', () => {
  const file = el.soundpadFile.files?.[0];
  el.soundpadFile.value = '';
  if (file) addSoundpadClip(file);
});

// --- chat -----------------------------------------------------------------

el.chatForm.addEventListener('submit', (event) => {
  event.preventDefault();
  sendMessage();
});

el.chatAttach.addEventListener('click', () => el.chatFile.click());

el.chatFile.addEventListener('change', () => {
  const file = el.chatFile.files?.[0] ?? null;
  state.chat.pendingFile = file;
  el.chatNote.textContent = file ? `Attached ${file.name}. Press Send.` : '';
  // Reset, so picking the same file twice in a row still fires 'change'.
  el.chatFile.value = '';
});

let searchTimer = null;
el.chatSearch.addEventListener('input', () => {
  // Debounced: every keystroke is a round trip and a full-text query
  // otherwise, and the answer for a half-typed word is never useful.
  clearTimeout(searchTimer);
  searchTimer = setTimeout(() => runSearch(), 250);
});

el.chatSearchClear.addEventListener('click', () => {
  el.chatSearch.value = '';
  runSearch();
});

el.channelAdd.addEventListener('click', async () => {
  // One dialog with three fields, rather than three questions in a row and
  // a confirm box asking somebody to remember that "OK means voice".
  const answer = await ask({
    title: 'New channel',
    okLabel: 'Create',
    fields: [
      { name: 'name', label: 'Name', placeholder: 'Game Night', required: true },
      {
        name: 'kind',
        label: 'Kind',
        options: [{ value: 'voice', label: 'Voice' }, { value: 'text', label: 'Text' }],
      },
      { name: 'password', label: 'Password', type: 'password', placeholder: 'Open to everyone' },
    ],
  });
  if (!answer?.name) return;
  const { name, kind } = answer;
  const password = answer.password || undefined;
  try {
    await harmony.api.createChannel(state.server, { kind, name, password });
    // The server broadcasts the new list to everyone, including us.
  } catch (err) {
    showChannelsError(err.message);
  }
});
el.username.addEventListener('keydown', (e) => e.key === 'Enter' && startSession());
el.serverUrl.addEventListener('keydown', (e) => e.key === 'Enter' && startSession());
el.password.addEventListener('keydown', (e) => e.key === 'Enter' && startSession());
el.accountPassword.addEventListener('keydown', (e) => e.key === 'Enter' && startSession());
el.accountConfirm.addEventListener('keydown', (e) => e.key === 'Enter' && startSession());

/**
 * Normalise the username box on the way out of it, not on every keystroke.
 *
 * On 'blur' and not 'input' deliberately: rewriting the value mid-word moves
 * the caret and makes typing a name with a space in it feel broken, even though
 * the result is the same. Waiting until they leave the field shows the stored
 * form without fighting them for the cursor.
 */
el.username.addEventListener('blur', () => {
  const folded = normalizeName(el.username.value);
  if (folded !== el.username.value) el.username.value = folded;
});

el.authModeToggle.addEventListener('click', async () => {
  // While a saved session is in force this button is the only way out of it,
  // because the account fields are hidden. Signing out puts the form back.
  if (state.auth.user) {
    await harmony.api.logout(state.server || el.serverUrl.value.trim()).catch(() => {});
    state.auth.user = null;
    await adoptSession('');
    state.auth.mode = 'login';
    showError('');
    applyAuthMode();
    el.accountPassword.focus();
    return;
  }
  state.auth.mode = state.auth.mode === 'register' ? 'login' : 'register';
  showError('');
  applyAuthMode();
  el.accountPassword.focus();
});

// Reflect the initial state once at startup, so the labels and
// data-auth-mode are never stale before the first server probe.
applyAuthMode();

el.rememberAccount.addEventListener('change', async () => {
  await harmony.settings.set({
    rememberAccount: el.rememberAccount.checked,
    // Unticking it has to forget what is already stored, or "remember me" is a
    // setting that only ever points one way.
    sessionToken: el.rememberAccount.checked ? state.auth.token : '',
  });
});
el.serverUrl.addEventListener('change', async () => {
  // A different server may have a different answer about passwords.
  await probeServer();
  refreshLiveList();
});
// Typing a new password is a reason to retry the list that just failed.
el.password.addEventListener('change', async () => {
  await harmony.api.setPassword(el.password.value);
  refreshLiveList();
});

el.tabs.forEach((tab) => {
  tab.addEventListener('click', () => {
    el.tabs.forEach((t) => t.removeAttribute('data-active'));
    tab.setAttribute('data-active', '');
    state.activeKind = tab.dataset.kind;
    renderSources();
    updateAudioNote();
  });
});

el.pickerRefresh.addEventListener('click', loadSources);
el.fallback.addEventListener('change', updateAudioNote);
el.excludeApp.addEventListener('change', updateAudioNote);
el.audioInput.addEventListener('change', () => {
  harmony.settings.set({ audioInputId: el.audioInput.value }).catch(() => {});
  updateAudioNote();
});

el.startStream.addEventListener('click', () => {
  // The picker doubles as "change source" once a broadcast is running.
  if (state.changingSource) return applySourceChange();
  return startBroadcast();
});

el.liveQuality.addEventListener('change', applyLiveQuality);
el.livePriority.addEventListener('change', applyLiveQuality);

el.changeSource.addEventListener('click', async () => {
  state.changingSource = true;
  state.selectedSource = null;
  el.startStream.disabled = true;
  el.startStream.textContent = 'Use this source';
  el.pickerBack.textContent = 'Back to stream';
  el.pickerUsername.textContent = state.session.username;
  showView('view-picker');
  await loadSources();
  updateAudioNote();
});

/** Chromium reads the adapter switch once, at startup, so this needs a restart. */
async function setGpuPreference(value) {
  await harmony.settings.set({ gpuPreference: value });
  state.settings = await harmony.settings.get();
  el.gpuPreference.value = value;
  // The dual-GPU warning stays up: it is about how the display is wired, which
  // this setting cannot change.
  toast(
    isBroadcasting()
      ? 'Saved. It applies next time Harmony starts — restarting now would end your stream.'
      : 'Saved. Restart Harmony to apply it — click here to restart now.',
  );
  const node = document.getElementById('toast');
  if (node && !isBroadcasting()) {
    node.classList.add('clickable');
    node.onclick = () => harmony.relaunch().catch(() => {});
  }
}

el.gpuPreference.addEventListener('change', () => setGpuPreference(el.gpuPreference.value));

el.togglePreview.addEventListener('click', () => {
  state.preview.hiddenByUser = !state.preview.hiddenByUser;
  applyPreviewVisibility();
  if (state.preview.hiddenByUser) {
    toast('Preview hidden. You are still live — this only stops Harmony drawing it.');
  }
});

// The hidden panel sits over the stage, so it is the obvious thing to click to
// get the picture back.
el.previewOff.addEventListener('click', () => {
  if (state.preview.hiddenByUser) {
    state.preview.hiddenByUser = false;
    applyPreviewVisibility();
  }
});

// Minimising is the automatic half: the window is gone, so painting it is pure
// waste. Automatic, and it does not overwrite a deliberate choice.
harmony.onWindowVisibility((visible) => {
  state.preview.windowVisible = visible;
  applyPreviewVisibility();

  // Mosaic tiles too. Each one is a decoder feeding a composited element, and a
  // minimised window shows none of it. Pausing leaves the connection up, so
  // restoring resumes at live rather than reconnecting. The single-stream view
  // is left alone on purpose -- it has a pause button the user owns.
  for (const entry of state.mosaic.tiles.values()) {
    if (visible) entry.video.play().catch(() => {});
    else entry.video.pause();
  }
});

el.monitorToggle.addEventListener('click', () => {
  if (!canMonitor()) return;
  state.bridge.setMonitor(!state.bridge.monitoring);
  updateMonitorButton();
});

el.pickerBack.addEventListener('click', async () => {
  // While live, the picker is a detour rather than a way out.
  if (state.changingSource) {
    state.changingSource = false;
    showView(state.share.target ? 'view-channels' : 'view-broadcast');
    return;
  }
  // Backing out of a channel share: drop the target and go back to the
  // channel, which is where they came from. Nothing was published, so there
  // is nothing to tear down but the intent.
  if (state.share.target) {
    state.share.target = null;
    applyVoiceButtons();
    showView('view-channels');
    return;
  }
  await teardown();
  showView('view-connect');
  refreshLiveList();
});

el.stopStream.addEventListener('click', () => stopBroadcast());

// Wrapped: addEventListener would otherwise pass the click Event as `usernames`.
// Watching others without interrupting your own broadcast.
el.broadcastWatch.addEventListener('click', () => enterMosaic());

el.clipsEnabled.addEventListener('change', () => {
  state.clips.enabled = el.clipsEnabled.checked;
  harmony.settings.set({ clipsEnabled: state.clips.enabled }).catch(() => {});
  if (!state.clips.enabled) {
    // Stop buffering immediately and give the memory back; the taps themselves
    // survive until the connection ends, but they stop retaining anything.
    state.clips.own?.clear();
    state.clips.watch?.clear();
    for (const entry of state.mosaic.tiles.values()) entry.clips?.clear();
  }
  toast(
    state.clips.enabled
      ? `Clips on — the last ${CLIP_SECONDS}s of each stream will be kept in memory.`
      : 'Clips off.',
  );
});

/**
 * The encoder choice is a Chromium command-line switch, and those are read once
 * at startup -- so unlike every other setting here, this one cannot apply to
 * the running process. Say so, and offer the restart rather than leaving the
 * checkbox looking like it did something.
 */
el.hwEncoding.addEventListener('change', async () => {
  const preference = el.hwEncoding.checked ? 'auto' : 'off';
  await harmony.settings.set({ hardwareEncoding: preference });
  state.settings = await harmony.settings.get();

  el.hwEncodingNote.textContent =
    preference === 'off'
      ? 'Will encode on the CPU after a restart.'
      : 'Will use the GPU again after a restart.';

  if (isBroadcasting()) {
    toast('Saved. It applies next time Harmony starts — restarting now would end your stream.');
    return;
  }
  toast('Saved. Restart Harmony to apply it — click here to restart now.');
  const node = document.getElementById('toast');
  if (node) {
    node.classList.add('clickable');
    node.onclick = () => harmony.relaunch().catch(() => {});
  }
});

el.broadcastClip.addEventListener('click', () =>
  saveClip(state.clips.own, state.session?.username ?? 'me', el.broadcastClip),
);
el.watchClip.addEventListener('click', () =>
  saveClip(state.clips.watch, state.session?.username ?? 'stream', el.watchClip),
);

el.testConnection.addEventListener('click', async () => {
  const server = el.serverUrl.value.trim();
  if (!server) return showError('Enter the address of your Harmony server first.');

  el.diagSteps.replaceChildren();
  el.diagVerdict.textContent = 'Testing…';
  el.diag.hidden = false;
  el.testConnection.disabled = true;

  const addStep = ({ name, ok, detail }) => {
    const li = document.createElement('li');
    const mark = document.createElement('span');
    mark.className = `diag-mark ${ok ? 'good' : 'bad'}`;
    mark.textContent = ok ? '✓' : '✕';
    const text = document.createElement('span');
    text.textContent = detail ? `${name} — ${detail}` : name;
    li.append(mark, text);
    el.diagSteps.append(li);
  };

  try {
    const { ok, verdict } = await runConnectionTest(server, addStep);
    el.diagVerdict.textContent = verdict;
    el.diagVerdict.classList.toggle('bad', !ok);
  } catch (err) {
    el.diagVerdict.textContent = err.message;
    el.diagVerdict.classList.add('bad');
  } finally {
    el.testConnection.disabled = false;
  }
});

el.diagClose.addEventListener('click', () => {
  el.diag.hidden = true;
});
el.diag.addEventListener('click', (e) => {
  if (e.target === el.diag) el.diag.hidden = true;
});

el.watchAll.addEventListener('click', () => enterMosaic());
el.mosaicLeave.addEventListener('click', leaveMosaic);

el.mosaicMute.addEventListener('click', () => {
  const { master } = state.mosaic;
  master.muted = !master.muted;
  el.mosaicMute.innerHTML = master.muted ? '&#128263;' : '&#128266;';
  el.mosaicMute.title = master.muted ? 'Unmute everything' : 'Mute everything';
  applyTileAudio();
});

el.mosaicVolume.addEventListener('input', () => {
  const value = Number(el.mosaicVolume.value);
  state.mosaic.master.volume = value / 100;
  el.mosaicVolumeLabel.textContent = `${value}%`;
  el.mosaicVolumeLabel.classList.toggle('boosted', value > 100);
  if (value > 0 && state.mosaic.master.muted) {
    state.mosaic.master.muted = false;
    el.mosaicMute.innerHTML = '&#128266;';
  }
  applyTileAudio();
});

el.mosaicAdd.addEventListener('click', openAddStream);
el.watchAdd.addEventListener('click', openAddStream);
el.addStreamClose.addEventListener('click', closeAddStream);
el.addStream.addEventListener('click', (e) => {
  if (e.target === el.addStream) closeAddStream(); // click the backdrop to dismiss
});

el.watchFullscreen.addEventListener('click', () => toggleFullscreen(el.watchView));
el.remote.addEventListener('dblclick', () => toggleFullscreen(el.watchView));

el.leaveStream.addEventListener('click', async () => {
  await teardown();
  showView('view-connect');
  refreshLiveList();
});

el.togglePlay.addEventListener('click', () => {
  if (el.remote.paused) {
    el.remote.play();
    el.togglePlay.innerHTML = '&#10074;&#10074;';
    el.togglePlay.title = 'Pause';
  } else {
    el.remote.pause();
    el.togglePlay.innerHTML = '&#9654;';
    el.togglePlay.title = 'Resume';
  }
});

el.toggleMute.addEventListener('click', () => {
  state.watch.muted = !state.watch.muted;
  applyWatchAudio();
});

el.volume.addEventListener('input', () => {
  state.watch.volume = Number(el.volume.value) / 100;
  if (state.watch.volume > 0) state.watch.muted = false;
  applyWatchAudio();
});

// Re-flow the mosaic as the window changes size.
let resizeTimer = null;
window.addEventListener('resize', () => {
  clearTimeout(resizeTimer);
  resizeTimer = setTimeout(layoutMosaic, 120);
});

// Release the username even if the user closes the window mid-stream.
window.addEventListener('beforeunload', () => {
  if (state.session?.token) {
    harmony.api.release(state.session.server, state.session.username, state.session.token);
  }
});

boot();
