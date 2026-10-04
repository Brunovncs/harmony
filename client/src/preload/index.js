// The only bridge between the renderer and Node. Every capability is an
// explicit, narrow method -- the renderer never sees ipcRenderer itself.
//
// Each method resolves with main's { ok, data } | { ok, error } envelope,
// untouched. Nothing is thrown here on purpose: an Error crossing
// contextBridge arrives in the renderer stripped of its custom properties, so
// the renderer rebuilds it on its own side instead (src/renderer/bridge.js).

const { contextBridge, ipcRenderer } = require('electron');

const invoke = (channel) => (...args) => ipcRenderer.invoke(channel, ...args);

contextBridge.exposeInMainWorld('harmony', {
  platform: process.platform,

  settings: {
    get: invoke('settings:get'),
    set: invoke('settings:set'),
  },

  gpu: {
    status: invoke('gpu:status'),
  },

  relaunch: invoke('app:relaunch'),

  /**
   * Fires when the window is minimised or restored.
   * @param {(visible: boolean) => void} handler
   */
  onWindowVisibility(handler) {
    const listener = (_event, visible) => handler(visible);
    ipcRenderer.on('window:visibility', listener);
    return () => ipcRenderer.removeListener('window:visibility', listener);
  },

  sources: {
    list: invoke('sources:list'),
    processes: invoke('sources:processes'),
    select: invoke('sources:select'),
    clear: invoke('sources:clear'),
  },

  audio: {
    availability: invoke('audio:availability'),
    start: invoke('audio:start'),
    stop: invoke('audio:stop'),

    /**
     * Raw interleaved S16LE stereo 48 kHz PCM from the native capture.
     * @param {(chunk: Uint8Array) => void} handler
     * @returns {() => void} unsubscribe
     */
    onPcm(handler) {
      const listener = (_event, chunk) => handler(chunk);
      ipcRenderer.on('audio:pcm', listener);
      return () => ipcRenderer.removeListener('audio:pcm', listener);
    },
  },

  clips: {
    save: invoke('clips:save'),
    reveal: invoke('clips:reveal'),
  },

  media: {
    setServer: invoke('media:server'),
    keep: invoke('media:keep'),
    stats: invoke('media:stats'),
    upload: invoke('media:upload'),
  },

  realtime: {
    connect: invoke('realtime:connect'),
    request: invoke('realtime:request'),
    disconnect: invoke('realtime:disconnect'),

    /**
     * Server-pushed frames: roster changes, channel edits, stream lists.
     * @param {(msg: object) => void} handler
     * @returns {() => void} unsubscribe
     */
    onEvent(handler) {
      const listener = (_event, msg) => handler(msg);
      ipcRenderer.on('realtime:event', listener);
      return () => ipcRenderer.removeListener('realtime:event', listener);
    },
  },

  // Every network request goes through main -- see src/main/api.js.
  api: {
    setPassword: invoke('api:password'),
    setSessionToken: invoke('api:session-token'),
    register: invoke('api:register'),
    login: invoke('api:login'),
    logout: invoke('api:logout'),
    me: invoke('api:me'),
    setAvatar: invoke('api:set-avatar'),
    roster: invoke('api:roster'),
    setRole: invoke('api:set-role'),
    channels: invoke('api:channels'),
    createChannel: invoke('api:create-channel'),
    updateChannel: invoke('api:update-channel'),
    deleteChannel: invoke('api:delete-channel'),
    reorderChannels: invoke('api:reorder-channels'),
    messages: invoke('api:messages'),
    postMessage: invoke('api:post-message'),
    pinMessage: invoke('api:pin-message'),
    deleteMessage: invoke('api:delete-message'),
    search: invoke('api:search'),
    soundpad: invoke('api:soundpad'),
    addClip: invoke('api:add-clip'),
    deleteClip: invoke('api:delete-clip'),
    reorderClips: invoke('api:reorder-clips'),
    health: invoke('api:health'),
    streams: invoke('api:streams'),
    session: invoke('api:session'),
    heartbeat: invoke('api:heartbeat'),
    release: invoke('api:release'),
    sdp: invoke('api:sdp'),
    hangup: invoke('api:hangup'),
  },
});
