import { chmodSync, copyFileSync, mkdirSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const desktopDir = resolve(scriptDir, "..");
const rootDir = resolve(desktopDir, "..");
const release = process.argv.includes("--release");
const rustc = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
const host = rustc.match(/^host: (.+)$/m)?.[1];
const target = process.env.TAURI_ENV_TARGET_TRIPLE || host;

if (!target) {
  throw new Error("Could not determine the Rust target triple");
}

const args = [
  "build",
  "--manifest-path",
  join(rootDir, "Cargo.toml"),
  "--bin",
  "aivo",
  "--target",
  target,
];
if (release) args.push("--release");

execFileSync("cargo", args, { cwd: rootDir, stdio: "inherit" });

const extension = target.includes("windows") ? ".exe" : "";
const profile = release ? "release" : "debug";
const source = join(rootDir, "target", target, profile, `aivo${extension}`);
const destination = join(
  desktopDir,
  "src-tauri",
  "binaries",
  `aivo-app-server-${target}${extension}`,
);
mkdirSync(dirname(destination), { recursive: true });
copyFileSync(source, destination);
if (!extension) chmodSync(destination, 0o755);

console.log(`Prepared ${destination}`);
