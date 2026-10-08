// Global hotkeys: mute, deafen, soundpad clips.
//
// GLOBAL, through Electron's globalShortcut, because the moment somebody
// wants a mute key is the moment Harmony does not have focus -- they are in
// the game. A key handled in the renderer would only work while they were
// looking at Harmony, which is when they need it least.
//
// The cost of global is that a registered combination is taken from every
// other application while Harmony runs: Windows delivers it to us and to
// nobody else. That is why the renderer refuses a plain letter without a
// modifier (it would stop that letter being typed anywhere), and why a
// combination another program already owns is reported back rather than
// silently doing nothing -- RegisterHotKey fails for it, and the person
// needs to know to pick another.
//
// The renderer owns WHAT is bound; this module only holds the registrations.
// set() replaces the whole set every time, so there is no add/remove
// bookkeeping to drift out of step with the settings file.

const { globalShortcut } = require('electron');

/** @type {(id: string) => void} */
let fire = () => {};

/** What is registered right now, for unregistering exactly that. */
let active = [];

function onFire(handler) {
  fire = handler;
}

/**
 * Replace every registration.
 *
 * @param {{id: string, accelerator: string}[]} bindings
 * @returns {{id: string, accelerator: string, ok: boolean, error?: string}[]}
 */
function set(bindings) {
  clear();
  const seen = new Set();
  return (Array.isArray(bindings) ? bindings : []).map(({ id, accelerator }) => {
    const accel = String(accelerator ?? '').trim();
    if (!id || !accel) return { id, accelerator: accel, ok: false, error: 'empty' };
    // The same combination twice would register once and quietly drop the
    // second; say so instead.
    if (seen.has(accel.toLowerCase())) return { id, accelerator: accel, ok: false, error: 'duplicate' };
    seen.add(accel.toLowerCase());
    try {
      const ok = globalShortcut.register(accel, () => fire(id));
      if (!ok) return { id, accelerator: accel, ok: false, error: 'in_use' };
      active.push(accel);
      return { id, accelerator: accel, ok: true };
    } catch {
      // An accelerator Electron cannot parse throws rather than returning
      // false. The renderer builds them, so this is a bug if it happens.
      return { id, accelerator: accel, ok: false, error: 'invalid' };
    }
  });
}

function clear() {
  for (const accel of active) {
    try {
      globalShortcut.unregister(accel);
    } catch { /* already gone */ }
  }
  active = [];
}

module.exports = { set, clear, onFire };
