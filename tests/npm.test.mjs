import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { access, readFile } from "node:fs/promises";
import { constants } from "node:fs";
import { resolve } from "node:path";
import { createInterface } from "node:readline";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = process.env.COMPUTER_USE_MCP_PACKAGE_DIR
  ? pathToFileURL(`${resolve(process.env.COMPUTER_USE_MCP_PACKAGE_DIR)}/`)
  : new URL("../", import.meta.url);
const manifest = JSON.parse(await readFile(new URL("package.json", root), "utf8"));
const plugin = (await import(new URL(manifest.exports["./server"], root))).default;
const binary = fileURLToPath(new URL(manifest.bin["computer-use-mcp"], root));

test("plugin registers the bundled server and synchronized skill idempotently", async () => {
  const hooks = await plugin.server();
  const config = { skills: { paths: ["/existing/skills"] } };
  await hooks.config(config);
  await hooks.config(config);
  assert.equal(plugin.id, manifest.name);
  assert.deepEqual(config.mcp.computer_use, {
    type: "local",
    command: [binary, "mcp"],
    enabled: true,
    timeout: 90_000,
  });
  assert.deepEqual(config.skills.paths, [
    "/existing/skills",
    fileURLToPath(new URL("skills", root)),
  ]);
  assert.equal(
    await readFile(new URL("skills/computer-use-mcp/SKILL.md", root), "utf8"),
    await readFile(new URL("../crates/computer-use-mcp/guidance/skill.md", import.meta.url), "utf8"),
  );
});

test("explicit MCP settings, disabled state, and permissions take precedence", async () => {
  const hooks = await plugin.server();
  for (const existing of [
    { enabled: false },
    { type: "local", command: ["custom-server", "mcp"], timeout: 1234 },
  ]) {
    const config = { mcp: { computer_use: existing }, permission: { "computer_use_*": "ask" } };
    await hooks.config(config);
    assert.strictEqual(config.mcp.computer_use, existing);
    assert.deepEqual(config.permission, { "computer_use_*": "ask" });
  }
});

test("bundled executable has the package version", async () => {
  await access(binary, constants.X_OK);
  assert.equal(execFileSync(binary, ["version"], { encoding: "utf8" }).trim(), manifest.version);
});

test("bundled MCP initializes and lists all six tools without a desktop", { timeout: 15_000 }, async (t) => {
  const env = { ...process.env };
  for (const key of ["WAYLAND_DISPLAY", "DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "XDG_SESSION_TYPE"]) {
    delete env[key];
  }
  const child = spawn(binary, ["mcp"], { env, stdio: ["pipe", "pipe", "pipe"] });
  t.after(() => { if (child.exitCode === null) child.kill("SIGKILL"); });
  const exited = once(child, "exit");
  let stderr = "";
  child.stderr.setEncoding("utf8").on("data", (chunk) => { stderr += chunk; });
  const lines = createInterface({ input: child.stdout })[Symbol.asyncIterator]();
  const send = (message) => child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
  async function receive(id) {
    while (true) {
      const { value, done } = await lines.next();
      assert.equal(done, false, `MCP exited before response: ${stderr}`);
      const message = JSON.parse(value);
      if (message.id !== id) continue;
      assert.equal(message.error, undefined);
      return message.result;
    }
  }
  send({ id: 1, method: "initialize", params: {
    protocolVersion: "2025-11-25",
    capabilities: {},
    clientInfo: { name: "npm-package-test", version: "1.0.0" },
  } });
  const initialized = await receive(1);
  assert.equal(initialized.serverInfo.version, manifest.version);
  send({ method: "notifications/initialized" });
  send({ id: 2, method: "tools/list", params: {} });
  const { tools } = await receive(2);
  assert.deepEqual(tools.map((tool) => tool.name), [
    "list_desktop", "launch_application", "activate_window", "observe", "act", "wait_for",
  ]);
  child.stdin.end();
  assert.deepEqual(await exited, [0, null], stderr);
});
