const path = require('path');
const fs = require('fs');

module.exports = async function beforeBuild(context) {
  const exePath = path.resolve(
    context.appDir || path.join(__dirname, '..'),
    '..', 'backend', 'target', 'release', 'rook.exe'
  );

  if (!fs.existsSync(exePath)) {
    throw new Error(
      `rook.exe not found at ${exePath}. ` +
      `Run "npm run build:backend" or "npm run build".`
    );
  }

  const { size } = fs.statSync(exePath);
  console.log(`rook.exe present (${(size / 1024 / 1024).toFixed(1)} MB)`);
};
