#!/usr/bin/env node

const { spawn, spawnSync } = require("node:child_process");
const os = require("node:os");
const { getInstalledBinaryPath, getPackageRoot } = require("../lib/paths");
const { formatLaunchError } = require("../lib/launcher");
const {
  WINDOWS_NPM_UPDATE_COMMAND,
  shouldDelegateWindowsNpmUpdate,
  spawnWindowsNpmUpdate
} = require("../lib/update");

const args = process.argv.slice(2);

function forwardExit(child) {
  child.on("exit", (code, signal) => {
    if (signal) {
      process.exitCode = 128 + (os.constants.signals[signal] || 1);
      return;
    }

    process.exit(code ?? 1);
  });
}

const useColor =
  Boolean(process.stdout.isTTY) &&
  !process.env.NO_COLOR &&
  !args.includes("--no-color");
const CYAN = useColor ? "\x1b[36m" : "";
const GREEN = useColor ? "\x1b[32m" : "";
const DIM = useColor ? "\x1b[2m" : "";
const RESET = useColor ? "\x1b[0m" : "";

function readInstalledVersion() {
  try {
    const result = spawnSync(getInstalledBinaryPath(), ["--version"], {
      encoding: "utf8",
      timeout: 5000
    });
    if (result.status === 0 && typeof result.stdout === "string") {
      const trimmed = result.stdout.trim();
      if (trimmed) {
        const parts = trimmed.split(/\s+/);
        return parts[parts.length - 1];
      }
    }
  } catch {
    // best-effort: fall through to null
  }
  return null;
}

function runWindowsNpmUpdate() {
  console.log(`${CYAN}→${RESET} Updating via npm...`);
  console.log(`  ${DIM}Running:${RESET} ${GREEN}${WINDOWS_NPM_UPDATE_COMMAND}${RESET}`);

  const child = spawnWindowsNpmUpdate(spawn);
  child.on("error", (error) => {
    console.error(`Failed to launch npm.cmd for update: ${error.message}`);
    process.exit(1);
  });
  child.on("exit", (code, signal) => {
    if (signal) {
      process.exitCode = 128 + (os.constants.signals[signal] || 1);
      return;
    }
    if (code === 0) {
      const version = readInstalledVersion();
      const suffix = version ? ` to version ${version}` : "";
      console.log(`${GREEN}✓${RESET} Updated${suffix}`);
    }
    process.exit(code ?? 1);
  });
}

if (shouldDelegateWindowsNpmUpdate(args, { packageRoot: getPackageRoot() })) {
  runWindowsNpmUpdate();
} else {
  const binaryPath = getInstalledBinaryPath();
  const child = spawn(binaryPath, args, {
    stdio: "inherit"
  });

  forwardExit(child);
  child.on("error", (error) => {
    console.error(formatLaunchError(error, binaryPath));
    process.exit(1);
  });
}
