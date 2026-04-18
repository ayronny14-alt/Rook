const { app, BrowserWindow, ipcMain, shell, Tray, Menu, globalShortcut, Notification, nativeImage, dialog } = require('electron');
const path = require('path');
const net  = require('net');
const https = require('https');
const fs   = require('fs');
const zlib = require('zlib');
const { spawn: spawnDetached } = require('child_process');
const backendManager = require('./backend-manager');

// hand-rolled PNG so we don't need an icon asset in the repo.
// yes this is weird, no i'm not adding a build step for one tray icon.
function createAmberIcon() {
  const W = 32, H = 32;
  const raw = Buffer.alloc(H * (1 + W * 4));
  for (let y = 0; y < H; y++) {
    const off = y * (1 + W * 4);
    raw[off] = 0; // filter byte
    for (let x = 0; x < W; x++) {
      const pi = off + 1 + x * 4;
      // #C86E3D = 200, 110, 61, 255  (amber-rust accent)
      raw[pi] = 200; raw[pi+1] = 110; raw[pi+2] = 61; raw[pi+3] = 255;
    }
  }
  const compressed = zlib.deflateSync(raw);
  function crc32(buf) {
    let c = 0xFFFFFFFF;
    for (const b of buf) { c ^= b; for (let i = 0; i < 8; i++) c = (c >>> 1) ^ (c & 1 ? 0xEDB88320 : 0); }
    return (c ^ 0xFFFFFFFF) >>> 0;
  }
  function chunk(type, data) {
    const lenB = Buffer.allocUnsafe(4); lenB.writeUInt32BE(data.length, 0);
    const tb   = Buffer.from(type, 'ascii');
    const crcB = Buffer.allocUnsafe(4); crcB.writeUInt32BE(crc32(Buffer.concat([tb, data])), 0);
    return Buffer.concat([lenB, tb, data, crcB]);
  }
  const ihdr = Buffer.allocUnsafe(13);
  ihdr.writeUInt32BE(W, 0); ihdr.writeUInt32BE(H, 4);
  ihdr[8] = 8; ihdr[9] = 6; ihdr[10] = ihdr[11] = ihdr[12] = 0; // bit-depth=8, RGBA
  return nativeImage.createFromBuffer(Buffer.concat([
    Buffer.from([0x89,0x50,0x4E,0x47,0x0D,0x0A,0x1A,0x0A]),
    chunk('IHDR', ihdr), chunk('IDAT', compressed), chunk('IEND', Buffer.alloc(0)),
  ]));
}

const IS_DEV = process.argv.includes('--dev') || process.env.ELECTRON_IS_DEV === '1';
if (IS_DEV) console.log('[Rook] Running in DEV mode');

// Prevent multiple Electron processes from launching simultaneously.
// A second launch focuses the existing window instead of spawning a new backend.
const gotSingleInstanceLock = app.requestSingleInstanceLock();
if (!gotSingleInstanceLock) {
  app.quit();
} else {
  app.on('second-instance', () => {
    if (mainWindow) {
      if (mainWindow.isMinimized()) mainWindow.restore();
      mainWindow.show();
      mainWindow.focus();
    }
  });
}

const { autoUpdater } = require('electron-updater');

const UPDATE_CHECK_INTERVAL_MS = 4 * 60 * 60 * 1000;
const UPDATE_FOCUS_MIN_MS = 2 * 60 * 1000;

let updatePollTimer = null;
let updateState = {
  currentVersion: app.getVersion(),
  status: 'idle',
  updateAvailable: false,
  latestVersion: null,
  downloadUrl: null,
  notes: '',
  progress: null,
  error: null,
  checkedAt: null,
};

function publicUpdateState() {
  return { ...updateState };
}

function emitUpdateState() {
  mainWindow?.webContents.send('update-state', publicUpdateState());
}

function setUpdateState(patch) {
  updateState = { ...updateState, ...patch, currentVersion: app.getVersion() };
  emitUpdateState();
}

// ask nicely before chewing through someone's data cap.
autoUpdater.autoDownload = false;
autoUpdater.autoInstallOnAppQuit = true;

autoUpdater.on('checking-for-update', () => {
  setUpdateState({ status: 'checking', error: null, progress: null });
});
autoUpdater.on('update-available', info => {
  setUpdateState({
    status: 'available',
    updateAvailable: true,
    latestVersion: info?.version || null,
    notes: typeof info?.releaseNotes === 'string' ? info.releaseNotes : '',
    progress: null,
    error: null,
    checkedAt: Date.now(),
  });
});
autoUpdater.on('update-not-available', info => {
  setUpdateState({
    status: 'idle',
    updateAvailable: false,
    latestVersion: info?.version || app.getVersion(),
    progress: null,
    error: null,
    checkedAt: Date.now(),
  });
});
autoUpdater.on('download-progress', p => {
  setUpdateState({
    status: 'downloading',
    progress: {
      percent: Math.round(p.percent || 0),
      downloadedBytes: p.transferred || 0,
      totalBytes: p.total || 0,
    },
  });
});
autoUpdater.on('update-downloaded', info => {
  setUpdateState({
    status: 'ready',
    updateAvailable: true,
    latestVersion: info?.version || null,
    progress: { percent: 100, downloadedBytes: 0, totalBytes: 0 },
    error: null,
  });
});
autoUpdater.on('error', err => {
  setUpdateState({
    status: 'error',
    error: err?.message || String(err),
    progress: null,
    checkedAt: Date.now(),
  });
});

async function checkForUpdates() {
  if (!app.isPackaged) {
    setUpdateState({
      status: 'idle',
      updateAvailable: false,
      latestVersion: null,
      progress: null,
      error: null,
      checkedAt: Date.now(),
    });
    return publicUpdateState();
  }
  try {
    await autoUpdater.checkForUpdates();
  } catch (err) {
    // error event handler already records state — this just stops the
    // unhandled-rejection noise in the log.
    console.error('[update] check failed:', err.message || err);
  }
  return publicUpdateState();
}

async function startUpdateInstall() {
  if (!app.isPackaged) {
    throw new Error('Updater only runs in packaged builds');
  }
  if (updateState.status === 'ready') {
    // Download already finished — jump straight to install.
    isShuttingDown = true;
    backendManager.stop();
    pipeReady = false;
    pendingQueue = [];
    try { pipeClient?.destroy(); } catch (_) {}
    // isSilent=true hides the NSIS installer UI on update; isForceRunAfter=true relaunches.
    autoUpdater.quitAndInstall(true, true);
    return publicUpdateState();
  }
  if (!updateState.updateAvailable) {
    throw new Error('No update is available');
  }
  setUpdateState({
    status: 'downloading',
    error: null,
    progress: { percent: 0, downloadedBytes: 0, totalBytes: 0 },
  });
  try {
    await autoUpdater.downloadUpdate();
  } catch (err) {
    setUpdateState({ status: 'error', error: err?.message || String(err) });
    throw err;
  }
  return publicUpdateState();
}

function startUpdatePolling() {
  clearInterval(updatePollTimer);
  checkForUpdates().catch(() => {});
  updatePollTimer = setInterval(() => {
    checkForUpdates().catch(() => {});
  }, UPDATE_CHECK_INTERVAL_MS);
}

function getEnvPath() {
  if (app.isPackaged) return path.join(app.getPath('userData'), '.env');
  return path.join(__dirname, '..', '..', '.env');
}

let isFirstRun = false;

function runSetupIfNeeded() {
  if (!app.isPackaged) return;
  const envPath  = getEnvPath();
  const userData = app.getPath('userData');
  if (fs.existsSync(envPath)) return; // already set up

  isFirstRun = true;
  fs.mkdirSync(userData, { recursive: true });
  fs.writeFileSync(envPath, '', 'utf8');
}

(function loadEnv() {
  try {
    const lines = fs.readFileSync(getEnvPath(), 'utf8').split('\n');
    for (const line of lines) {
      const m = line.match(/^\s*([A-Z_]+)\s*=\s*(.+)\s*$/);
      if (m && !process.env[m[1]]) process.env[m[1]] = m[2].trim();
    }
  } catch (_) {}
})();

const PIPE_NAME = '\\\\.\\pipe\\rook';

let pipeClient  = null;
let pipeBuffer  = '';
let mainWindow  = null;
let tray        = null;
let pipeReady   = false;
let pendingQueue = [];
let retryDelay  = 200;
let retryTimer  = null;
let lastErrorCode = null;
let reconnectAttempt = 0;
let isShuttingDown = false;

function scheduleReconnect() {
  if (retryTimer || isShuttingDown) return;
  reconnectAttempt++;
  const delay = Math.min(retryDelay, 10_000);
  retryTimer = setTimeout(() => { retryTimer = null; connectPipe(); }, delay);
  // Exponential backoff: 200 → 400 → 800 → … capped at 10s
  retryDelay = Math.min(retryDelay * 2, 10_000);

  mainWindow?.webContents.send('pipe-status', {
    connected: false,
    reconnecting: true,
    attempt: reconnectAttempt,
    retryDelayMs: delay,
    backendRestarts: backendManager.getRestartCount(),
  });
}

function connectPipe() {
  if (isShuttingDown) return;
  pipeClient = net.createConnection(PIPE_NAME);
  pipeClient.setNoDelay(true);

  pipeClient.on('connect', () => {
    console.log('[pipe] connected');
    pipeReady = true;
    retryDelay = 200;
    reconnectAttempt = 0;
    lastErrorCode = null;
    for (const m of pendingQueue) sendRaw(m);
    pendingQueue = [];
    mainWindow?.webContents.send('pipe-status', {
      connected: true,
      reconnecting: false,
      attempt: 0,
      backendRestarts: backendManager.getRestartCount(),
    });
  });

  pipeClient.on('data', chunk => {
    pipeBuffer += chunk.toString('utf8');
    let nl;
    while ((nl = pipeBuffer.indexOf('\n')) !== -1) {
      const line = pipeBuffer.slice(0, nl).trim();
      pipeBuffer = pipeBuffer.slice(nl + 1);
      if (!line) continue;
      try {
        const msg = JSON.parse(line);
        mainWindow?.webContents.send('backend-message', msg);
      } catch (_) {}
    }
  });

  pipeClient.on('error', err => {
    if (err.code !== lastErrorCode) {
      console.log(`[pipe] waiting for backend (${err.code})`);
      lastErrorCode = err.code;
    }
    pipeReady = false;
    scheduleReconnect();
  });

  pipeClient.on('close', () => {
    pipeReady = false;
    if (!isShuttingDown) scheduleReconnect();
    else mainWindow?.webContents.send('pipe-status', { connected: false, reconnecting: false, attempt: 0 });
  });
}

function sendRaw(json) {
  if (pipeReady && pipeClient) pipeClient.write(json + '\n', 'utf8');
  else pendingQueue.push(json);
}

function createWindow() {
  mainWindow = new BrowserWindow({
    width: 1380,
    height: 860,
    minWidth: 860,
    minHeight: 600,
    backgroundColor: '#0d0d0d',
    frame: false,
    webPreferences: {
      preload: path.join(__dirname, 'preload.js'),
      contextIsolation: true,
      nodeIntegration: false,
      webviewTag: false,
    },
  });

  mainWindow.loadFile(path.join(__dirname, 'renderer', 'index.html'));

  // Hide to tray instead of closing — lets global hotkey bring it back
  mainWindow.on('close', (e) => {
    if (!isShuttingDown) {
      e.preventDefault();
      mainWindow.hide();
    }
  });

  mainWindow.on('closed', () => { mainWindow = null; });
}

function githubGet(url, attempt = 0) {
  return new Promise((resolve, reject) => {
    const req = https.get(url, {
      headers: {
        'User-Agent': 'Rook/1.0',
        'Accept': 'application/vnd.github.v3+json',
        ...(process.env.GITHUB_TOKEN ? { Authorization: `token ${process.env.GITHUB_TOKEN}` } : {}),
      },
      timeout: 10000,
    }, res => {
      let data = '';
      res.on('data', c => data += c);
      res.on('end', () => {
        try { resolve(JSON.parse(data)); }
        catch (e) { reject(e); }
      });
    });
    req.on('timeout', () => { req.destroy(); reject(new Error('GitHub request timed out')); });
    req.on('error', err => {
      if (attempt < 2) {
        setTimeout(() => githubGet(url, attempt + 1).then(resolve, reject), 1000 * (attempt + 1));
      } else { reject(err); }
    });
  });
}

function mapRepo(item, pluginType) {
  return {
    id:           item.full_name,
    name:         item.name,
    full_name:    item.full_name,
    description:  item.description || '',
    repo_url:     item.html_url,
    stars:        item.stargazers_count  || 0,
    forks:        item.forks_count       || 0,
    language:     item.language          || '',
    topics:       item.topics            || [],
    updated_at:   item.updated_at        || '',
    owner_avatar: item.owner?.avatar_url || '',
    plugin_type:  pluginType,
    status:       'available',
  };
}

ipcMain.handle('browse-plugins', async (_e, opts = {}) => {
  const { type = 'all', sort = 'stars', page = 1 } = opts;
  const PER_PAGE = 30;

  const queries = [];
  if (type === 'all' || type === 'mcp') {
    queries.push({ q: 'topic:mcp-server',             pluginType: 'mcp'   });
    queries.push({ q: 'topic:model-context-protocol', pluginType: 'mcp'   });
  }
  if (type === 'all' || type === 'skill') {
    queries.push({ q: 'topic:rook-skill',          pluginType: 'skill' });
    queries.push({ q: 'topic:agent-skill',            pluginType: 'skill' });
    queries.push({ q: 'topic:claude-skill',           pluginType: 'skill' }); // ecosystem-compatible
  }

  const seen = new Set();
  const results = [];
  let maxTotal = 0;

  await Promise.all(queries.map(async ({ q, pluginType }) => {
    try {
      const url = `https://api.github.com/search/repositories?q=${encodeURIComponent(q)}&sort=${sort}&order=desc&per_page=${PER_PAGE}&page=${page}`;
      const data = await githubGet(url);
      if (data.total_count) maxTotal = Math.max(maxTotal, data.total_count);
      for (const item of (data.items || [])) {
        if (seen.has(item.full_name)) continue;
        seen.add(item.full_name);
        results.push(mapRepo(item, pluginType));
      }
    } catch (_) {}
  }));

  if (sort === 'stars')   results.sort((a, b) => b.stars - a.stars);
  else if (sort === 'updated') results.sort((a, b) => (b.updated || 0) - (a.updated || 0));

  const hasMore = page * PER_PAGE < Math.min(maxTotal, 900);
  return { items: results, page, hasMore };
});

ipcMain.handle('github-search', async (_e, query) => {
  const queries = [
    { q: `${query} topic:mcp-server`,             pluginType: 'mcp'   },
    { q: `${query} topic:model-context-protocol`, pluginType: 'mcp'   },
    { q: `${query} topic:rook-skill`,          pluginType: 'skill' },
    { q: `${query} topic:agent-skill`,            pluginType: 'skill' },
    { q: `${query} topic:claude-skill`,           pluginType: 'skill' }, // ecosystem-compatible
  ];
  const seen = new Set();
  const results = [];

  await Promise.all(queries.map(async ({ q, pluginType }) => {
    try {
      const url = `https://api.github.com/search/repositories?q=${encodeURIComponent(q)}&sort=stars&order=desc&per_page=30`;
      const data = await githubGet(url);
      for (const item of (data.items || [])) {
        if (seen.has(item.full_name)) continue;
        seen.add(item.full_name);
        results.push(mapRepo(item, pluginType));
      }
    } catch (_) {}
  }));

  results.sort((a, b) => b.stars - a.stars);
  return results;
});

ipcMain.on('set-github-token', (_e, token) => {
  process.env.GITHUB_TOKEN = token;
});

ipcMain.on('save-env-settings', (_e, { apiUrl, apiKey, model }) => {
  const envPath = getEnvPath();
  const lines = [];
  const MANAGED = [
    'ROOK_LLM_BASE_URL', 'ROOK_LLM_API_KEY', 'ROOK_LLM_MODEL',
  ];
  try {
    const existing = fs.readFileSync(envPath, 'utf8').split('\n');
    for (const line of existing) {
      const key = line.split('=')[0].trim();
      if (!MANAGED.includes(key)) lines.push(line);
    }
  } catch (_) {}
  const resolvedApiKey = apiKey || process.env.ROOK_LLM_API_KEY || '';
  const resolvedApiUrl = apiUrl || process.env.ROOK_LLM_BASE_URL || '';
  const resolvedModel  = model  || process.env.ROOK_LLM_MODEL   || '';
  if (resolvedApiKey)  lines.push(`ROOK_LLM_API_KEY=${resolvedApiKey}`);
  if (resolvedApiUrl)  lines.push(`ROOK_LLM_BASE_URL=${resolvedApiUrl}`);
  if (resolvedModel)   lines.push(`ROOK_LLM_MODEL=${resolvedModel}`);
  try { fs.writeFileSync(envPath, lines.join('\n')); } catch (_) {}
  if (resolvedApiKey)  process.env.ROOK_LLM_API_KEY         = resolvedApiKey;
  if (resolvedApiUrl)  process.env.ROOK_LLM_BASE_URL        = resolvedApiUrl;
  if (resolvedModel)   process.env.ROOK_LLM_MODEL           = resolvedModel;
});

ipcMain.on('send-to-backend', (_e, msg) => sendRaw(JSON.stringify(msg)));
ipcMain.handle('open-external', (_e, url) => shell.openExternal(url));
ipcMain.handle('is-dev-mode',     () => IS_DEV);
ipcMain.handle('get-app-version', () => app.getVersion());
ipcMain.handle('get-update-state', () => publicUpdateState());
ipcMain.handle('check-for-updates', async () => checkForUpdates());
ipcMain.handle('start-update', async () => startUpdateInstall());
ipcMain.on('win-minimize', () => mainWindow?.minimize());
ipcMain.on('win-maximize', () => mainWindow?.isMaximized() ? mainWindow.unmaximize() : mainWindow?.maximize());
ipcMain.on('win-close',    () => mainWindow?.close());

// Manual reconnect trigger from renderer
ipcMain.on('reconnect-now', () => {
  if (retryTimer) { clearTimeout(retryTimer); retryTimer = null; }
  retryDelay = 200;
  connectPipe();
});

ipcMain.on('show-notification', (_e, { title, body }) => {
  if (Notification.isSupported()) {
    const n = new Notification({ title: title || 'Rook', body: body || '', icon: createAmberIcon() });
    n.on('click', () => { mainWindow?.show(); mainWindow?.focus(); });
    n.show();
  }
});

ipcMain.handle('get-login-item', () => app.getLoginItemSettings().openAtLogin);
ipcMain.on('toggle-login-item', (_e, enable) => {
  app.setLoginItemSettings({ openAtLogin: !!enable, path: app.getPath('exe') });
});

ipcMain.handle('pick-folder', async () => {
  const result = await dialog.showOpenDialog(mainWindow, {
    properties: ['openDirectory'],
    title: 'Select folder to index',
  });
  return result.canceled ? null : result.filePaths[0];
});

ipcMain.handle('read-file-drop', (_e, filePath) => {
  try {
    const stat = fs.statSync(filePath);
    if (stat.size > 2 * 1024 * 1024) return { error: 'File too large (>2 MB)' };
    const content = fs.readFileSync(filePath, 'utf8');
    return { content, name: path.basename(filePath) };
  } catch (err) {
    return { error: err.message };
  }
});

function createTray() {
  const icon = createAmberIcon().resize({ width: 16, height: 16 });
  tray = new Tray(icon);
  tray.setToolTip('Rook');

  const menu = Menu.buildFromTemplate([
    {
      label: 'Show Rook',
      click: () => { mainWindow?.show(); mainWindow?.focus(); },
    },
    {
      label: 'New Chat',
      click: () => {
        mainWindow?.show();
        mainWindow?.focus();
        mainWindow?.webContents.send('tray-new-chat');
      },
    },
    { type: 'separator' },
    {
      label: 'Quit Rook',
      click: () => { isShuttingDown = true; app.quit(); },
    },
  ]);

  tray.setContextMenu(menu);
  tray.on('click', () => {
    if (mainWindow) { mainWindow.show(); mainWindow.focus(); }
    else createWindow();
  });
  tray.on('double-click', () => {
    if (mainWindow) { mainWindow.show(); mainWindow.focus(); }
    else createWindow();
  });
}

function relayLog(line) {
  mainWindow?.webContents.send('backend-log', line);
}

app.on('before-quit', (e) => {
  if (isShuttingDown) return;
  isShuttingDown = true;
  e.preventDefault();

  // Send graceful shutdown signal to backend, then wait up to 3s before force-quit
  if (pipeReady && pipeClient) {
    try {
      pipeClient.write(JSON.stringify({ type: 'graceful_shutdown', id: 'shutdown-0' }) + '\n', 'utf8');
    } catch (_) {}
  }

  setTimeout(() => {
    backendManager.stop();
    app.quit();
  }, 1500);
});

app.whenReady().then(() => {
  runSetupIfNeeded();
  createWindow();
  createTray();

  // Alt+Space — show/hide Rook from anywhere on the desktop
  globalShortcut.register('Alt+Space', () => {
    if (!mainWindow) { createWindow(); return; }
    if (mainWindow.isVisible() && mainWindow.isFocused()) {
      mainWindow.hide();
    } else {
      mainWindow.show();
      mainWindow.focus();
    }
  });

  mainWindow.webContents.on('did-finish-load', () => {
    mainWindow.webContents.send('backend-starting');
    mainWindow.webContents.send('app-init', { firstRun: isFirstRun });
    // Push saved login-item state to renderer
    mainWindow.webContents.send('login-item-state', app.getLoginItemSettings().openAtLogin);
    // Start polling now that the renderer is live and can receive 'update-state' events.
    // The first check fires immediately inside startUpdatePolling() so the banner
    // appears as soon as the UI is ready, without waiting for the interval.
    startUpdatePolling();
    backendManager.start(
      line => relayLog(line),
      ({ code, restartCount: rc, gaveUp }) => {
        mainWindow?.webContents.send('backend-exited', { code, restartCount: rc, gaveUp: !!gaveUp });
      }
    );
    setTimeout(connectPipe, 1200);
  });

  // Re-check for updates when the window gains focus (throttled)
  mainWindow.on('focus', () => {
    const lastCheck = updateState.checkedAt || 0;
    if (Date.now() - lastCheck > UPDATE_FOCUS_MIN_MS) {
      checkForUpdates().catch(() => {});
    }
  });

  app.on('activate', () => {
    if (BrowserWindow.getAllWindows().length === 0) createWindow();
    else { mainWindow?.show(); mainWindow?.focus(); }
  });
});

// Unregister shortcuts on quit
app.on('will-quit', () => globalShortcut.unregisterAll());

// Keep app alive in tray when all windows close
app.on('window-all-closed', () => {
  // On Windows, don't auto-quit — user can bring it back via tray or Alt+Space
  // If they want to quit, they use the tray menu
  if (process.platform === 'darwin') app.quit();
});
