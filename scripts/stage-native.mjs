import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { chmod, copyFile, mkdir, readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";

const root = new URL("../", import.meta.url);
const source = new URL("target/x86_64-unknown-linux-gnu/release/computer-use-mcp", root);
const destination = new URL("vendor/computer-use-mcp", root);
const manifest = JSON.parse(await readFile(new URL("package.json", root), "utf8"));
assert.equal(process.platform, "linux", "npm binaries must be built on Linux");
assert.equal(process.arch, "x64", "npm binaries must be built for x64");
assert.equal(
  execFileSync(fileURLToPath(source), ["version"], { encoding: "utf8" }).trim(),
  manifest.version,
  "Cargo and npm versions must match",
);

await mkdir(new URL("vendor/", root), { recursive: true });
await copyFile(source, destination);
await chmod(destination, 0o755);
await mkdir(new URL("skills/computer-use-mcp/", root), { recursive: true });
await copyFile(
  new URL("crates/computer-use-mcp/guidance/skill.md", root),
  new URL("skills/computer-use-mcp/SKILL.md", root),
);
