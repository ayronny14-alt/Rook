// babysits the rust child. brings it back if it dies, gives up if it keeps dying.
const { spawn } = require('child_process');
const path = require('path');
const fs = require('fs');

let backendProc = null;
let restartCount = 0;
let restartTimestamps = [];
const MAX_RESTARTS = 3; // three strikes. we out.
const RESTART_WINDOW_MS = 10_000;
let onLogCb = null;
let onExitCb = null;
let supervisorStopped = false;

function findBackendExe() {
  // bundled first, release next, debug last, cargo if you're really stuck.
  const bundled = path.join(process.resourcesPath, 'backend', 'rook.exe');
  if (fs.existsSync(bundled)) return { cmd: bundled, args: [], cwd: path.dirname(bundled), mode: 'bundled' };

  const base = path.join(__dirname, '..', '..', 'backend');
  const release = path.join(base, 'target', 'release', 'rook.exe');
  const debug   = path.join(base, 'target', 'debug',   'rook.exe');
  if (fs.existsSync(release)) return { cmd: release, args: [], cwd: base, mode: 'release' };
  if (fs.existsSync(debug))   return { cmd: debug,   args: [], cwd: base, mode: 'debug' };
  return { cmd: 'cargo', args: ['run', '--release'], cwd: base, shell: true, mode: 'cargo' };
}

function spawnBackend() {
  if (supervisorStopped) return;

  const { cmd, args, cwd, shell, mode } = findBackendExe();
  console.log(`[backend] starting via ${mode}: ${cmd} (restart #${restartCount})`);

  const backendEnv = { ...process.env };
  if (mode !== 'bundled') backendEnv.ROOK_DEV = '1';

  backendProc = spawn(cmd, args, {
    cwd,
    shell: shell || false,
    windowsHide: true,
    env: backendEnv,
  });

  backendProc.stdout?.on('data', d => onLogCb?.(d.toString().trimEnd()));
  backendProc.stderr?.on('data', d => onLogCb?.(d.toString().trimEnd()));

  backendProc.on('exit', (code, signal) => {
    console.log(`[backend] exited code=${code} signal=${signal}`);
    backendProc = null;
    onExitCb?.({ code, restartCount });

    if (supervisorStopped) return;
    // Intentional exit (code 0) - don't respawn
    if (code === 0) return;

    // Prune timestamps outside the crash window
    const now = Date.now();
    restartTimestamps = restartTimestamps.filter(t => now - t < RESTART_WINDOW_MS);

    if (restartTimestamps.length >= MAX_RESTARTS) {
      // three strikes. we out.
      const msg = `[backend] crashed ${MAX_RESTARTS} times in ${RESTART_WINDOW_MS / 1000}s - giving up`;
      console.error(msg);
      onLogCb?.(msg);
      onExitCb?.({ code, restartCount, gaveUp: true });
      return;
    }

    restartTimestamps.push(now);
    restartCount++;
    const delay = 1000;
    console.log(`[backend] respawning in ${delay}ms (attempt ${restartCount}/${MAX_RESTARTS})`);
    onLogCb?.(`[backend] restarting… (attempt ${restartCount}/${MAX_RESTARTS})`);
    setTimeout(spawnBackend, delay);
  });

  backendProc.on('error', err => {
    console.error('[backend] spawn error:', err.message);
    onLogCb?.(`ERROR: ${err.message}`);
  });

  return backendProc;
}

function start(onLog, onExit) {
  if (backendProc) return;
  onLogCb = onLog;
  onExitCb = onExit;
  supervisorStopped = false;
  restartCount = 0;
  restartTimestamps = [];
  spawnBackend();
  return backendProc;
}

function stop() {
  supervisorStopped = true;
  if (backendProc) {
    backendProc.kill();
    backendProc = null;
  }
}

function getRestartCount() { return restartCount; }
function isRunning() { return !!backendProc; }

module.exports = { start, stop, getRestartCount, isRunning };
