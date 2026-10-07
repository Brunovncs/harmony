const { app, BrowserWindow, desktopCapturer, ipcMain, net, protocol, session, shell } = require('electron');
const path = require('node:path');
const { pathToFileURL } = require('node:url');

const settings = require('./settings');
const sources = require('./sources');
const appAudio = require('./app-audio');
const api = require('./api');
const clips = require('./clips');
const gpu = require('./gpu');
const { RealtimeClient } = require('./realtime');
const { MediaCache, HASH_RE } = require('./media-cache');

const RENDERER_DIR = path.join(__dirname, '..', 'renderer');

/** Set once the renderer tells us which server it is talking to. */
let mediaServer = '';
/** @type {MediaCache|null} */
let mediaCache = null;
const MODULES_DIR = path.join(__dirname, '..', '..', 'node_modules');

// Chromium throttles renderers whose window is hidden, minimised or covered by
// another window: timers drop to roughly once a second and the WebRTC encoder
// slows to match. For an ordinary app that saves battery. For a screen sharer it
// means the stream collapses the moment the broadcaster switches to the thing
// they are sharing -- which is always. All three of these must be off.
app.commandLine.appendSwitch('disable-background-timer-throttling');
app.commandLine.appendSwitch('disable-renderer-backgrounding');
app.commandLine.appendSwitch('disable-backgrounding-occluded-windows');

// GPU H.264 encoding (NVENC / AMF / Quick Sync) is already the default -- see
// gpu.js. This only ever turns it OFF, for a machine whose driver produces a
// broken stream. Switches are read once at startup, hence the restart.
if (gpu.applyEncodingPreference(settings.read().hardwareEncoding)) {
  console.warn('[gpu] hardware video encoding disabled by user setting');
}

// On a two-GPU laptop, which one Harmony runs on decides whether it competes
// with the game for the same adapter. See gpu.js.
const adapter = gpu.applyAdapterPreference(settings.read().gpuPreference);
if (adapter) console.log(`[gpu] preferring the ${adapter} GPU`);

// The UI is served over a custom scheme rather than file://, because a file://
// document is an opaque origin: ES modules and AudioWorklet.addModule() both
// fail CORS there. Marking the scheme secure also makes it a secure context,
// which getDisplayMedia() requires.
//
// Must run before app 'ready'.
protocol.registerSchemesAsPrivileged([
  {
    scheme: 'harmony',
    privileges: { standard: true, secure: true, supportFetchAPI: true, corsEnabled: true },
  },
]);

/** @type {BrowserWindow|null} */
let win = null;

/**
 * The source the user picked in our own UI, parked here for
 * setDisplayMediaRequestHandler -- that handler cannot prompt, so the choice
 * has to be made before getDisplayMedia() is called.
 * @type {Electron.DesktopCapturerSource|null}
 */
let pendingSource = null;

/**
 * Whether to attach Chromium's own system-wide loopback audio. Only used when
 * the native per-application capture is unavailable -- see src/main/app-audio.js.
 */
let pendingLoopbackAudio = false;

function registerProtocol() {
  protocol.handle('harmony', async (request) => {
    const { pathname } = new URL(request.url);
    const rel = decodeURIComponent(pathname).replace(/^\/+/, '') || 'index.html';

    // `media/<hash>` is fetched from the server on first use and served from
    // disk forever after. Because the handler awaits the cache, an
    // <img src="harmony://app/media/..."> in the renderer just works: no IPC,
    // no loading state, and nothing in the renderer that knows or cares
    // whether it was a hit or a miss.
    //
    // Note this lives under `app/` deliberately. The scheme is registered
    // {standard: true}, so the host is part of the origin; `harmony://media`
    // would be a different origin and the CSP's `'self'` would block every
    // image it was added to allow.
    if (rel.startsWith('media/')) {
      const hash = rel.slice('media/'.length);
      if (!HASH_RE.test(hash)) return new Response('Bad hash', { status: 400 });
      if (!mediaCache || !mediaServer) return new Response('No server', { status: 503 });
      try {
        const { path: file, contentType } = await mediaCache.get(hash, (h) =>
          api.fetchUpload(mediaServer, h));
        const response = await net.fetch(pathToFileURL(file).toString());
        return new Response(response.body, {
          status: 200,
          headers: {
            'Content-Type': contentType,
            'Content-Security-Policy': "default-src 'none'; sandbox",
            'X-Content-Type-Options': 'nosniff',
            // Content-addressed: the bytes behind a hash never change.
            'Cache-Control': 'public, max-age=31536000, immutable',
          },
        });
      } catch (err) {
        return new Response(`Media unavailable: ${err.message}`, { status: 404 });
      }
    }

    // `vendor/...` serves ES modules straight from node_modules, so third-party
    // libraries stay managed by npm instead of being copied into the repo.
    const underVendor = rel.startsWith('vendor/');
    const root = underVendor ? MODULES_DIR : RENDERER_DIR;
    const target = path.join(root, underVendor ? rel.slice('vendor/'.length) : rel);

    // Never serve anything outside the directory we chose.
    if (target !== root && !target.startsWith(root + path.sep)) {
      return new Response('Forbidden', { status: 403 });
    }
    return net.fetch(pathToFileURL(target).toString());
  });
}

function createWindow() {
  win = new BrowserWindow({
    width: 1180,
    height: 800,
    // Small enough to sit in a corner of a second monitor. The layout has
    // breakpoints down to this size; below it things genuinely stop fitting.
    minWidth: 560,
    minHeight: 420,
    backgroundColor: '#0f1116',
    title: 'Harmony',
    autoHideMenuBar: true,
    show: false,
    webPreferences: {
      preload: path.join(__dirname, '..', 'preload', 'index.js'),
      contextIsolation: true,
      nodeIntegration: false,
      // Belt and braces with the command-line switches above: this is the
      // per-window form of the same thing.
      backgroundThrottling: false,
      sandbox: true,
    },
  });

  win.once('ready-to-show', () => win.show());
  win.loadURL('harmony://app/index.html');

  /**
   * Tell the renderer when the window is out of sight.
   *
   * Chromium would normally stop painting a minimised window by itself, but the
   * three anti-throttling switches at the top of this file deliberately stop it
   * doing that -- they are what keeps the encoder running while the broadcaster
   * is looking at the thing they are sharing. The cost is that the preview keeps
   * being composited forever, at full rate, on a machine that is usually also
   * running a game. So take responsibility for it explicitly: the renderer
   * detaches the preview here and reattaches on restore, while the capture and
   * the encoder carry on untouched.
   */
  const sendVisibility = (visible) => {
    if (!win?.isDestroyed()) win.webContents.send('window:visibility', visible);
  };
  win.on('minimize', () => sendVisibility(false));
  win.on('restore', () => sendVisibility(true));
  win.on('show', () => sendVisibility(true));
  win.on('hide', () => sendVisibility(false));

  // In a dev run, renderer errors would otherwise vanish into DevTools nobody
  // has open. Warnings and errors only -- this is not a console mirror.
  if (!app.isPackaged) {
    win.webContents.on('console-message', (event) => {
      if (event.level === 'warning' || event.level === 'error') {
        console.log(`[renderer:${event.level}] ${event.message} (${event.sourceId}:${event.lineNumber})`);
      }
    });
    win.webContents.on('did-fail-load', (_e, code, desc, url) => {
      console.error(`[renderer] failed to load ${url}: ${desc} (${code})`);
    });
  }

  win.webContents.setWindowOpenHandler(({ url }) => {
    shell.openExternal(url);
    return { action: 'deny' };
  });

  win.on('closed', () => {
    appAudio.stop();
    win = null;
  });
}

app.whenReady().then(() => {
  mediaCache = new MediaCache({
    dir: path.join(app.getPath('userData'), 'media'),
    budgetBytes: (settings.read().mediaCacheMb ?? 512) * 1024 * 1024,
  });
  registerProtocol();
  gpu.watch();

  // getDisplayMedia() resolves to whatever the user already chose in our picker.
  // Passing {} denies the request.
  session.defaultSession.setDisplayMediaRequestHandler(
    (_request, callback) => {
      if (!pendingSource) return callback({});
      // Normally video only: audio comes from the native capture pipeline,
      // which can scope itself to a single application. Chromium's loopback is
      // system-wide, so it is used solely as a fallback the user opted into.
      callback(
        pendingLoopbackAudio
          ? { video: pendingSource, audio: 'loopback' }
          : { video: pendingSource },
      );
    },
    { useSystemPicker: false },
  );

  // 'fullscreen' belongs here too: Electron gates HTML fullscreen behind a
  // permission, and denying it makes element.requestFullscreen() hang forever
  // rather than reject -- a silent failure that looks like a broken button.
  session.defaultSession.setPermissionRequestHandler((_wc, permission, callback) => {
    callback(['media', 'display-capture', 'audioCapture', 'fullscreen'].includes(permission));
  });

  createWindow();

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
  });
});

app.on('window-all-closed', () => {
  appAudio.stop();
  realtime.disconnect();
  if (process.platform !== 'darwin') app.quit();
});

// ---------------------------------------------------------------------------
// IPC
// ---------------------------------------------------------------------------

/**
 * Every handler answers with the same envelope: { ok, data } or { ok, error }.
 *
 * Errors are not thrown across the boundary because an Error crossing
 * contextBridge loses its custom properties -- `err.code` arrives undefined in
 * the renderer, and the UI switches on exactly that. A plain object survives
 * structured cloning intact; the renderer turns it back into a real Error on
 * its own side (see src/renderer/bridge.js).
 */
const handle = (channel, fn) => {
  ipcMain.handle(channel, async (...args) => {
    try {
      return { ok: true, data: await fn(...args) };
    } catch (err) {
      return {
        ok: false,
        error: { message: err.message, code: err.code ?? 'error', status: err.status ?? 0 },
      };
    }
  });
};

handle('settings:get', () => settings.read());
handle('settings:set', (_e, patch) => settings.write(patch ?? {}));

handle('sources:list', () => sources.list());
handle('sources:processes', () => sources.listProcesses());

handle('sources:select', async (_e, sourceId, options = {}) => {
  const all = await desktopCapturer.getSources({
    types: ['screen', 'window'],
    thumbnailSize: { width: 1, height: 1 },
  });
  pendingSource = all.find((s) => s.id === sourceId) ?? null;
  pendingLoopbackAudio = Boolean(options.loopbackAudio);
  return Boolean(pendingSource);
});

handle('sources:clear', () => {
  pendingSource = null;
  pendingLoopbackAudio = false;
  return true;
});

handle('audio:availability', () => appAudio.availability());

handle('audio:start', (_e, opts) => {
  const target = win;
  return appAudio.start(opts, (chunk) => {
    if (target && !target.isDestroyed()) target.webContents.send('audio:pcm', chunk);
  });
});

handle('clips:save', (_e, data, label) => clips.save(data, label));
handle('clips:reveal', (_e, file) => clips.reveal(file));

handle('audio:stop', () => {
  appAudio.stop();
  return true;
});

handle('gpu:status', async () => ({
  ...(await gpu.status()),
  preference: settings.read().hardwareEncoding,
  adapterPreference: settings.read().gpuPreference,
}));

// Changing the encoder preference means changing a Chromium switch, which is
// only read at startup.
handle('app:relaunch', () => {
  app.relaunch();
  app.quit();
  return true;
});

handle('api:password', (_e, value) => api.setPassword(value));
handle('api:session-token', (_e, value) => api.setSessionToken(value));
handle('api:register', (_e, s, n, p, k) => api.register(s, n, p, k));
handle('api:login', (_e, s, n, p, k) => api.login(s, n, p, k));
handle('api:logout', (_e, s) => api.logout(s));
handle('api:me', (_e, s) => api.me(s));
handle('api:set-avatar', (_e, s, hash) => api.setAvatar(s, hash));
handle('api:roster', (_e, s) => api.roster(s));
handle('api:set-role', (_e, s, id, role) => api.setRole(s, id, role));
handle('api:set-display-name', (_e, s, name) => api.setDisplayName(s, name));
handle('api:rename-clip', (_e, s, id, body) => api.renameClip(s, id, body));
handle('api:create-group', (_e, s, name) => api.createGroup(s, name));
handle('api:rename-group', (_e, s, id, name) => api.renameGroup(s, id, name));
handle('api:delete-group', (_e, s, id) => api.deleteGroup(s, id));
handle('api:arrange', (_e, s, body) => api.arrange(s, body));
handle('api:channels', (_e, s) => api.channels(s));
handle('api:create-channel', (_e, s, body) => api.createChannel(s, body));
handle('api:update-channel', (_e, s, id, body) => api.updateChannel(s, id, body));
handle('api:delete-channel', (_e, s, id) => api.deleteChannel(s, id));
handle('api:reorder-channels', (_e, s, ids) => api.reorderChannels(s, ids));

// ---------------------------------------------------------------------------
// Realtime
//
// One socket for the whole app, owned here. Server-pushed frames are forwarded
// to the renderer on 'realtime:event' using the same push pattern as
// 'window:visibility' and 'audio:pcm'.
// ---------------------------------------------------------------------------

const realtime = new RealtimeClient();

realtime.on('event', (payload) => {
  if (win && !win.isDestroyed()) win.webContents.send('realtime:event', payload);
});

handle('realtime:connect', (_e, server, token) => realtime.connect(server, token));
handle('realtime:request', (_e, type, payload) => realtime.request(type, payload));
/**
 * Which server the media route should download from.
 *
 * Set by the renderer at connect time rather than baked into every URL,
 * because the URL is `harmony://app/media/<hash>` -- a hash and nothing else,
 * which is what lets it be used directly in an <img> tag.
 */
handle('media:server', (_e, server) => {
  mediaServer = String(server ?? '');
  return true;
});
handle('media:keep', (_e, hashes) => {
  mediaCache?.setKeepSet(Array.isArray(hashes) ? hashes : []);
  return true;
});
handle('media:stats', () => mediaCache?.stats() ?? null);
handle('media:upload', (_e, server, bytes, contentType) =>
  api.uploadFile(server, bytes, contentType));

handle('api:messages', (_e, s, id, before) => api.messages(s, id, before));
handle('api:post-message', (_e, s, id, body) => api.postMessage(s, id, body));
handle('api:pin-message', (_e, s, id, pinned) => api.pinMessage(s, id, pinned));
handle('api:delete-message', (_e, s, id) => api.deleteMessage(s, id));
handle('api:search', (_e, s, id, q) => api.search(s, id, q));
handle('api:react', (_e, s, id, emoji, on) => api.react(s, id, emoji, on));
handle('api:emojis', (_e, s) => api.emojis(s));
handle('api:add-emoji', (_e, s, body) => api.addEmoji(s, body));
handle('api:delete-emoji', (_e, s, id) => api.deleteEmoji(s, id));
handle('api:soundpad', (_e, s) => api.soundpad(s));
handle('api:add-clip', (_e, s, body) => api.addClip(s, body));
handle('api:delete-clip', (_e, s, id) => api.deleteClip(s, id));
handle('api:reorder-clips', (_e, s, ids) => api.reorderClips(s, ids));

handle('realtime:disconnect', () => {
  realtime.disconnect();
  return true;
});
handle('api:health', (_e, server) => api.health(server));
handle('api:streams', (_e, server) => api.streams(server));
handle('api:session', (_e, server, username, token, kind) =>
  api.session(server, username, token, kind));
handle('api:heartbeat', (_e, server, u, t) => api.heartbeat(server, u, t));
handle('api:release', (_e, server, u, t) => api.release(server, u, t));
handle('api:sdp', (_e, url, offer) => api.sdpExchange(url, offer));
handle('api:hangup', (_e, resourceUrl) => api.deleteResource(resourceUrl).then(() => true));
