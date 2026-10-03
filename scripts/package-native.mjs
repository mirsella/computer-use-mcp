import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { copyFile, mkdir, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const root = new URL("../", import.meta.url);
const manifest = JSON.parse(await readFile(new URL("package.json", root), "utf8"));
assert.equal(process.platform, "linux", "npm binaries must be built on Linux");
assert.equal(process.arch, "x64", "npm binaries must be built for x64");
execFileSync("cargo", [
  "install", "--path", "crates/computer-use-mcp", "--bin", "computer-use-mcp",
  "--locked", "--target", "x86_64-unknown-linux-gnu", "--root", "vendor",
  "--no-track", "--force",
], { cwd: root, stdio: "inherit" });
assert.equal(
  execFileSync(fileURLToPath(new URL(manifest.bin["computer-use-mcp"], root)), ["version"], {
    encoding: "utf8",
  }).trim(),
  manifest.version,
  "Cargo and npm versions must match",
);

await mkdir(new URL("skills/computer-use-mcp/", root), { recursive: true });
await copyFile(
  new URL("crates/computer-use-mcp/guidance/skill.md", root),
  new URL("skills/computer-use-mcp/SKILL.md", root),
);
