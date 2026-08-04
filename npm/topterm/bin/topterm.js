#!/usr/bin/env node
"use strict";

const { spawnSync } = require("child_process");

const PACKAGES = {
  "darwin-arm64": "topterm-darwin-arm64",
  "darwin-x64": "topterm-darwin-x64",
};

const key = `${process.platform}-${process.arch}`;
const pkg = PACKAGES[key];

if (!pkg) {
  console.error(
    `topterm: no prebuilt binary for ${key}. topterm only supports macOS (arm64/x64).`
  );
  process.exit(1);
}

let binPath;
try {
  binPath = require.resolve(`${pkg}/bin/topterm`);
} catch (err) {
  console.error(
    `topterm: could not find the ${pkg} package.\n` +
      `Try reinstalling: npm install topterm --force`
  );
  process.exit(1);
}

const result = spawnSync(binPath, process.argv.slice(2), { stdio: "inherit" });

if (result.error) {
  console.error(`topterm: failed to launch binary: ${result.error.message}`);
  process.exit(1);
}

process.exit(result.status === null ? 1 : result.status);
