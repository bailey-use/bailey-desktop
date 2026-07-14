import {
  chmodSync,
  copyFileSync,
  cpSync,
  existsSync,
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  statSync,
  writeFileSync,
} from "node:fs";
import { tmpdir } from "node:os";
import { basename, dirname, join, relative, resolve, sep } from "node:path";
import { fileURLToPath } from "node:url";
import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";

const scriptDir = dirname(fileURLToPath(import.meta.url));
const desktopDir = resolve(scriptDir, "..");
const release = process.argv.includes("--release");
const sourceDir = resolve(
  process.env.BAILEY_USE_SOURCE_DIR ?? resolve(desktopDir, "..", "..", "bailey-use"),
);
const destination = join(desktopDir, "src-tauri", "resources", "bailey-runtime");
const desktopPackage = JSON.parse(readFileSync(join(desktopDir, "package.json"), "utf8"));
const localToolsPackage = readJson(join(sourceDir, "package.json"));
const extensionManifest = readJson(join(sourceDir, "extension", "manifest.json"));
const target = process.env.TAURI_ENV_TARGET_TRIPLE ?? rustHost();

assertFile(join(sourceDir, "src", "mcp", "server.js"), "Bailey Local Tools source");
rmSync(destination, { recursive: true, force: true });
mkdirSync(destination, { recursive: true });

for (const entry of ["src", "extension", "native-host"]) {
  cpSync(join(sourceDir, entry), join(destination, entry), { recursive: true });
}
mkdirSync(join(destination, "scripts"), { recursive: true });
for (const script of ["browser-install.mjs", "browser-launch.mjs", "browser-smoke.mjs"]) {
  copyFileSync(join(sourceDir, "scripts", script), join(destination, "scripts", script));
}
copyFileSync(join(sourceDir, "package.json"), join(destination, "package.json"));

const nodeName = target.includes("windows") ? "node.exe" : "node";
const bundledNode = join(destination, "node", nodeName);
mkdirSync(dirname(bundledNode), { recursive: true });
copyFileSync(process.execPath, bundledNode);
if (!target.includes("windows")) chmodSync(bundledNode, 0o755);
const nodeLicense = findNodeLicense(process.execPath);
if (nodeLicense) {
  copyFileSync(nodeLicense, join(destination, "node", "LICENSE"));
} else if (release) {
  throw new Error("Cannot package Node.js without its LICENSE file.");
}

const archive = target.includes("windows")
  ? join(sourceDir, "dist", "computer-use", "bailey-computer-use-windows.zip")
  : target.includes("apple-darwin")
    ? join(sourceDir, "dist", "computer-use", "bailey-computer-use-macos.zip")
    : null;
let cuaBundled = false;
if (archive && existsSync(archive)) {
  verifyArchiveChecksum(archive);
  const temporary = mkdtempSync(join(tmpdir(), "bailey-runtime-"));
  try {
    execFileSync("tar", ["-xf", archive, "-C", temporary], { stdio: "pipe" });
    const driver = findNamedFile(temporary, target.includes("windows") ? "cua-driver.exe" : "cua-driver");
    if (!driver) throw new Error(`Cua Driver executable is missing from ${archive}`);
    cpSync(dirname(driver), join(destination, "computer-use", "driver"), { recursive: true });
    const cuaLicense = findNamedFile(temporary, "cua-MIT.txt");
    if (!cuaLicense) throw new Error(`Cua Driver MIT license is missing from ${archive}`);
    mkdirSync(join(destination, "licenses"), { recursive: true });
    copyFileSync(cuaLicense, join(destination, "licenses", "cua-MIT.txt"));
    if (!target.includes("windows")) chmodSync(join(destination, "computer-use", "driver", basename(driver)), 0o755);
    cuaBundled = true;
  } finally {
    rmSync(temporary, { recursive: true, force: true });
  }
} else if (release) {
  throw new Error(`Missing platform Cua Driver package: ${archive ?? target}`);
}

const files = inventoryFiles(destination);
writeFileSync(join(destination, "manifest.json"), `${JSON.stringify({
  schemaVersion: 1,
  desktopVersion: desktopPackage.version,
  localToolsVersion: localToolsPackage.version,
  target,
  compatibility: {
    desktopMajor: Number(desktopPackage.version.split(".")[0]),
    localToolsMajor: Number(localToolsPackage.version.split(".")[0]),
    mcpProtocol: "2025-03-26",
  },
  components: {
    localTools: true,
    nativeHost: true,
    extension: extensionManifest.version,
    bundledNode: true,
    nodeVersion: process.version,
    cuaDriver: cuaBundled,
    cuaDriverVersion: process.env.BAILEY_CUA_DRIVER_VERSION ?? "0.6.8",
    cuaDriverLicense: cuaBundled,
  },
  files,
}, null, 2)}\n`);

console.log(`Prepared integrated Bailey runtime for ${target}: ${destination}`);

function readJson(path) {
  assertFile(path, path);
  return JSON.parse(readFileSync(path, "utf8"));
}

function assertFile(path, label) {
  if (!existsSync(path) || !statSync(path).isFile()) throw new Error(`Missing ${label}: ${path}`);
}

function rustHost() {
  const output = execFileSync("rustc", ["-vV"], { encoding: "utf8" });
  const host = output.match(/^host: (.+)$/m)?.[1];
  if (!host) throw new Error("Could not determine Rust host target");
  return host;
}

function findNodeLicense(executable) {
  let current = dirname(executable);
  for (let depth = 0; depth < 5; depth += 1) {
    for (const name of ["LICENSE", "LICENSE.txt"]) {
      const candidate = join(current, name);
      if (existsSync(candidate)) return candidate;
    }
    current = dirname(current);
  }
  return null;
}

function findNamedFile(root, name) {
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const absolute = join(root, entry.name);
    if (entry.isFile() && entry.name === name) return absolute;
    if (entry.isDirectory()) {
      const nested = findNamedFile(absolute, name);
      if (nested) return nested;
    }
  }
  return null;
}

function inventoryFiles(root) {
  return walkFiles(root)
    .map((absolute) => {
      const contents = readFileSync(absolute);
      return {
        path: relative(root, absolute).split(sep).join("/"),
        size: contents.length,
        sha256: createHash("sha256").update(contents).digest("hex"),
      };
    })
    .sort((left, right) => left.path.localeCompare(right.path));
}

function walkFiles(root) {
  const files = [];
  for (const entry of readdirSync(root, { withFileTypes: true })) {
    const absolute = join(root, entry.name);
    if (entry.isFile()) files.push(absolute);
    if (entry.isDirectory()) files.push(...walkFiles(absolute));
  }
  return files;
}

function verifyArchiveChecksum(archive) {
  const sidecar = `${archive}.sha256`;
  assertFile(sidecar, "Cua Driver checksum");
  const expected = readFileSync(sidecar, "utf8").trim().split(/\s+/)[0]?.toLowerCase();
  const actual = createHash("sha256").update(readFileSync(archive)).digest("hex");
  if (!expected || expected !== actual) {
    throw new Error(`Cua Driver checksum mismatch: ${archive}`);
  }
}
