#!/usr/bin/env node
'use strict';

const { spawn } = require('node:child_process');
const { binaryPath } = require('..');

let bin;
try {
  bin = binaryPath();
} catch (e) {
  console.error(e.message);
  process.exit(1);
}

const child = spawn(bin, process.argv.slice(2), { stdio: 'inherit' });

// The child shares our terminal, so it already receives Ctrl-C itself; keep the
// wrapper alive until it decides how to exit, and relay signals sent only to us.
for (const sig of ['SIGINT', 'SIGTERM', 'SIGHUP']) {
  process.on(sig, () => child.kill(sig));
}

child.on('error', (e) => {
  console.error(`devsandbox: failed to start ${bin}: ${e.message}`);
  process.exit(1);
});

child.on('exit', (code, signal) => {
  if (signal) {
    // Re-raise so our parent sees the same termination cause.
    process.removeAllListeners(signal);
    process.kill(process.pid, signal);
  } else {
    process.exit(code ?? 1);
  }
});
