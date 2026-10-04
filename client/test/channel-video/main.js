// Test shell for the channel mosaic. Deliberately NOT Harmony's main process:
// webSecurity is off so the renderer can talk to MediaMTX directly, which the
// real app's CSP forbids by design (the app routes every request through its
// own main process instead). This exists only to drive WHIP and WHEP against
// a real MediaMTX.
const { app, BrowserWindow, session } = require('electron');
const path = require('node:path');

app.commandLine.appendSwitch('use-fake-device-for-media-stream');
app.commandLine.appendSwitch('use-fake-ui-for-media-stream');
app.commandLine.appendSwitch('disable-background-timer-throttling');
app.commandLine.appendSwitch('disable-renderer-backgrounding');
app.commandLine.appendSwitch('disable-backgrounding-occluded-windows');

app.whenReady().then(() => {
  session.defaultSession.setPermissionRequestHandler((_wc, _p, cb) => cb(true));
  const win = new BrowserWindow({
    width: 900, height: 600, show: false,
    webPreferences: {
      contextIsolation: false,
      nodeIntegration: false,
      sandbox: false,
      webSecurity: false,
      backgroundThrottling: false,
    },
  });
  win.loadFile(path.join(__dirname, 'index.html'));
});
app.on('window-all-closed', () => app.quit());
