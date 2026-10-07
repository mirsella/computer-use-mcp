import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { access, mkdtemp, readFile, rm } from "node:fs/promises";
import { constants } from "node:fs";
import { join, resolve } from "node:path";
import { tmpdir } from "node:os";
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
  const hooks = await plugin.server(undefined, { enabled: true });
  const config = { skills: { paths: ["/existing/skills"] } };
  await hooks.config(config);
  await hooks.config(config);
  assert.equal(plugin.id, manifest.name);
  assert.deepEqual(config.mcp.computer_use, {
    type: "local",
    command: [binary, "mcp", "--compact-tools"],
    enabled: true,
    timeout: 150_000,
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

test("plugin is disabled by default and skips the skill", async () => {
  const hooks = await plugin.server();
  const config = { skills: { paths: ["/existing/skills"] } };
  await hooks.config(config);
  assert.deepEqual(config.mcp.computer_use, {
    type: "local",
    command: [binary, "mcp", "--compact-tools"],
    enabled: false,
    timeout: 150_000,
  });
  assert.deepEqual(config.skills.paths, ["/existing/skills"]);
  await assert.rejects(plugin.server(undefined, { enabled: "true" }), /boolean/);
});

test("explicit MCP settings, disabled state, and permissions take precedence", async () => {
  const hooks = await plugin.server();
  const skillsPath = fileURLToPath(new URL("skills", root));
  for (const [existing, expectSkills] of [
    [{ enabled: false }, false],
    [{ type: "local", command: ["custom-server", "mcp"], timeout: 1234 }, true],
  ]) {
    const config = { mcp: { computer_use: existing }, permission: { "computer_use_*": "ask" } };
    await hooks.config(config);
    assert.strictEqual(config.mcp.computer_use, existing);
    assert.deepEqual(config.permission, { "computer_use_*": "ask" });
    assert.deepEqual(config.skills, expectSkills ? { paths: [skillsPath] } : undefined);
  }
});

test("bundled executable has the package version", async () => {
  await access(binary, constants.X_OK);
  assert.equal(execFileSync(binary, ["version"], { encoding: "utf8" }).trim(), manifest.version);
});

test("compact mode respects global and agent-specific permissions and tool switches", async (t) => {
  const warn = t.mock.method(console, "warn", () => {});
  const cases = [
    [{ permission: { "*": "allow", "computer_use_*": "ask" } }, true],
    [{ permission: "ask" }, true],
    [{ tools: { "computer_use_*": false } }, true],
    [{ permission: { computer_use_dispatch: "deny" } }, true],
    [{ permission: { computer_use_act: "ask" } }, false],
    [{ permission: { "computer_use_*": "allow", computer_use_observe: "deny" } }, false],
    [{ permission: { "computer_use_a*": "deny" } }, false],
    [{ permission: { "computer_use_ac?": "ask" } }, false],
    [{ permission: { "computer_use_act.extra": "deny" } }, true],
    [{ tools: { computer_use_launch_application: false } }, false],
    [{ agent: { reviewer: { permission: { computer_use_act: "deny" } } } }, false],
    [{ agent: { reviewer: { tools: { computer_use_wait_for: false } } } }, false],
    [{ agent: { reviewer: { permission: { "computer_use_*": "ask" } } } }, true],
  ];
  for (const [policy, compact] of cases) {
    const config = structuredClone(policy);
    const hooks = await plugin.server(undefined, { enabled: true });
    await hooks.config(config);
    assert.deepEqual(config.mcp.computer_use.command, compact
      ? [binary, "mcp", "--compact-tools"] : [binary, "mcp"], JSON.stringify(policy));
    for (const key of Object.keys(policy)) assert.deepEqual(config[key], policy[key]);
  }
  assert.equal(warn.mock.callCount(), cases.filter(([, compact]) => !compact).length);
  const config = {};
  await (await plugin.server(undefined, { compactTools: false })).config(config);
  assert.deepEqual(config.mcp.computer_use.command, [binary, "mcp"]);
  await assert.rejects(plugin.server(undefined, { compactTools: "false" }), /boolean/);
});

async function connect(t, compact, protocolVersion = "2025-11-25") {
  const env = { ...process.env };
  const state = await mkdtemp(join(tmpdir(), "computer-use-mcp-history-"));
  env.XDG_STATE_HOME = state;
  t.after(() => rm(state, { recursive: true, force: true }));
  for (const key of ["WAYLAND_DISPLAY", "DISPLAY", "DBUS_SESSION_BUS_ADDRESS", "XDG_SESSION_TYPE"]) {
    delete env[key];
  }
  const child = spawn(binary, compact ? ["mcp", "--compact-tools"] : ["mcp"], { env, stdio: ["pipe", "pipe", "pipe"] });
  await once(child, "spawn");
  const closed = once(child, "close");
  t.after(async () => {
    if (child.exitCode === null && child.signalCode === null) child.kill("SIGKILL");
    await closed;
  });
  let stderr = "";
  child.stderr.setEncoding("utf8").on("data", (chunk) => { stderr += chunk; });
  const lines = createInterface({ input: child.stdout })[Symbol.asyncIterator]();
  const send = (message) => child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`);
  let nextId = 0;
  async function request(method, params = {}) {
    const id = ++nextId;
    send({ id, method, params });
    while (true) {
      const { value, done } = await lines.next();
      assert.equal(done, false, `MCP exited before response: ${stderr}`);
      const message = JSON.parse(value);
      if (message.id === undefined) continue;
      assert.equal(message.id, id, "unexpected MCP response ID");
      return message;
    }
  }
  const { result: initialized } = await request("initialize", {
    protocolVersion,
    capabilities: {},
    clientInfo: { name: "npm-package-test", version: "1.0.0" },
  });
  assert.equal(initialized.serverInfo.version, manifest.version);
  send({ method: "notifications/initialized" });
  return {
    request,
    state,
    async close() {
      child.stdin.end();
      assert.deepEqual(await closed, [0, null], stderr);
      assert.doesNotMatch(stderr, /starting KDE desktop session/);
    },
  };
}

test("compact MCP discovers exact schemas and preserves direct validation without a desktop", { timeout: 15_000 }, async (t) => {
  const direct = await connect(t, false);
  const compact = await connect(t, true);
  const { result: { tools } } = await direct.request("tools/list");
  assert.deepEqual(tools.map((tool) => tool.name), [
    "list_desktop", "launch_application", "activate_window", "observe", "act", "wait_for",
  ]);
  const wait = tools.find((tool) => tool.name === "wait_for");
  assert.ok(wait.inputSchema.properties.condition.oneOf.some(
    (condition) => condition.properties.type.const === "human_idle",
  ));
  assert.equal(wait.inputSchema.properties.timeout_ms.maximum, 120_000);
  const config = {};
  await (await plugin.server()).config(config);
  assert.ok(config.mcp.computer_use.timeout > wait.inputSchema.properties.timeout_ms.maximum,
    "the host timeout must cover the server's longest bounded wait");
  const { result: compactList } = await compact.request("tools/list");
  assert.deepEqual(compactList.tools.map((tool) => tool.name), ["help", "dispatch"]);
  assert.equal(compactList.tools[0].annotations.readOnlyHint, true);
  assert.equal(compactList.tools[1].annotations.readOnlyHint, false);
  assert.equal(compactList.tools[1].annotations.destructiveHint, true);
  const call = (client, name, args) => client.request("tools/call", { name, arguments: args });
  const { result: catalog } = await call(compact, "help", {});
  assert.deepEqual(JSON.parse(catalog.content[0].text), { actions: tools.map((tool) => tool.name) });
  for (const tool of tools) {
    const { result: help } = await call(compact, "help", { action: tool.name });
    assert.equal(help.content.length, 1);
    assert.equal(help.structuredContent, undefined, "help must not duplicate schemas");
    assert.deepEqual(JSON.parse(help.content[0].text), tool);
    const original = await call(direct, tool.name, {});
    const wrapped = await call(compact, "dispatch", { action: tool.name, arguments: {} });
    assert.equal(wrapped.result.isError, true);
    assert.deepEqual(wrapped.result, original.result);
  }
  for (const args of [
    {}, { action: "dispatch", arguments: {} }, { action: "unknown", arguments: {} },
    { action: "list_desktop", arguments: [] },
    { action: "list_desktop", arguments: { scope: "windows" }, desktop: "background" },
    { action: "list_desktop", arguments: { scope: "windows", desktop: "invalid" } },
  ]) {
    const { result } = await call(compact, "dispatch", args);
    assert.equal(result.isError, true);
    assert.equal(result.structuredContent.outcome, "not_started");
  }
  for (const args of [{ action: null }, { action: "unknown" }, { extra: true }]) {
    assert.equal((await call(compact, "help", args)).result.isError, true);
  }
  assert.equal((await call(compact, "act", {})).error.code, -32602);
  assert.equal((await call(direct, "dispatch", {})).error.code, -32602);
  await direct.close();
  await compact.close();
});

test("compact responses honor the negotiated protocol", { timeout: 15_000 }, async (t) => {
  const client = await connect(t, true, "2025-03-26");
  for (const [name, args] of [
    ["help", { action: "unknown" }],
    ["dispatch", { action: "act", arguments: {} }],
  ]) {
    const { result } = await client.request("tools/call", { name, arguments: args });
    assert.equal(result.isError, true);
    assert.equal(result.structuredContent, undefined);
    assert.ok(result.content[0].text);
  }
  await client.close();
});

test("public call history records compact validation and protocol errors without contents", { timeout: 15_000 }, async (t) => {
  const client = await connect(t, true, "2025-03-26");
  const secret = "private request contents";
  await client.request("tools/call", { name: "help", arguments: {} });
  await client.request("tools/call", { name: "dispatch", arguments: { action: "act", arguments: { text: secret } } });
  await client.request("tools/call", { name: secret, arguments: {} });
  await client.close();
  const history = execFileSync(binary, ["history"], {
    env: { ...process.env, XDG_STATE_HOME: client.state }, encoding: "utf8",
  });
  const records = history.trim().split("\n").map(JSON.parse);
  assert.equal(records.length, 6);
  const finished = records.filter((record) => record.event === "finished");
  assert.equal(finished[0].status, "succeeded");
  assert.equal(finished[1].tool, "dispatch");
  assert.equal(finished[1].action, "act");
  assert.equal(finished[1].result.code, "invalid_arguments");
  assert.equal(finished[1].result.outcome, "not_started");
  assert.equal(finished[2].tool, "unknown");
  assert.equal(finished[2].result.protocol_error, true);
  assert.ok(finished.every((record) => Number.isFinite(record.duration_ms)));
  assert.ok(!history.includes(secret));
  const errors = execFileSync(binary, ["history", "--errors"], {
    env: { ...process.env, XDG_STATE_HOME: client.state }, encoding: "utf8",
  }).trim().split("\n").map(JSON.parse);
  assert.equal(errors.length, 2);
  assert.ok(errors.every((record) => record.status === "error"));
  const query = (...arguments_) => {
    const output = execFileSync(binary, ["history", ...arguments_], {
      env: { ...process.env, XDG_STATE_HOME: client.state }, encoding: "utf8",
    });
    return output.trim() ? output.trim().split("\n").map(JSON.parse) : [];
  };
  const last = records.filter((record) => record.call_id === finished[2].call_id);
  assert.deepEqual(query("--last", "1"), last);
  assert.deepEqual(query("--call-id", finished[2].call_id), last);
  assert.deepEqual(query("--since", "15m"), records);
  assert.deepEqual(query("--since", "0"), records);
  assert.deepEqual(query("--errors", "--last", "1", "--since", "1h"), [finished[2]]);
  assert.deepEqual(query("--last", "0"), []);
  assert.throws(() => query("--last"));
  assert.throws(() => query("--since", "yesterday"));
  assert.throws(() => query("--call-id"));
});
