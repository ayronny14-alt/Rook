const { spawnSync } = require('child_process');
const path = require('path');
const fs = require('fs');

const backendDir = path.resolve(__dirname, '..', '..', 'backend');
const exePath = path.join(backendDir, 'target', 'release', 'rook.exe');

if (!fs.existsSync(path.join(backendDir, 'Cargo.toml'))) {
  console.error(`Cargo.toml not found at ${backendDir}`);
  process.exit(1);
}

const result = spawnSync('cargo', ['build', '--release'], {
  cwd: backendDir,
  stdio: 'inherit',
  shell: process.platform === 'win32',
});

if (result.status !== 0) process.exit(result.status || 1);

if (!fs.existsSync(exePath)) {
  console.error(`cargo reported success but ${exePath} is missing`);
  process.exit(1);
}

const { size } = fs.statSync(exePath);
console.log(`rook.exe built (${(size / 1024 / 1024).toFixed(1)} MB)`);
