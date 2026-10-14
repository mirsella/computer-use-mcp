import assert from "node:assert/strict";
import { execFileSync, spawn } from "node:child_process";
import { once } from "node:events";
import { access, mkdtemp, readFile, realpath, rm } from "node:fs/promises";
import { constants } from "node:fs";
import { join } from "node:path";
import { tmpdir } from "node:os";
import { createInterface } from "node:readline";
import { test } from "node:test";
import { fileURLToPath, pathToFileURL } from "node:url";

const root = process.env.COMPUTER_USE_MCP_PACKAGE_DIR
  ? pathToFileURL(`${await realpath(process.env.COMPUTER_USE_MCP_PACKAGE_DIR)}/`)
  : new URL("../", import.meta.url);
const manifest = JSON.parse(await readFile(new URL("package.json", root), "utf8"));
const plugin = (await import(new URL(manifest.exports["./server"], root))).default;
const binary = fileURLToPath(new URL(manifest.bin["computer-use-mcp"], root));

test("Codex packages the same standalone server and canonical skill", async () => {
  const load = async (path) => JSON.parse(await readFile(new URL(path, root), "utf8"));
  const codex = await load(".codex-plugin/plugin.json");
  const marketplace = await load(".agents/plugins/marketplace.json");
  const { mcpServers: { computer_use: server } } = await load(".mcp.json");
  assert.equal(codex.name, "computer-use");
  assert.equal(codex.version, manifest.version);
  assert.equal(codex.skills, "./skills/");
  assert.equal(marketplace.name, "computer-use-mcp");
  assert.equal(marketplace.plugins[0].name, codex.name);
  assert.equal(fileURLToPath(new URL(server.command, root)), binary);
  assert.deepEqual(server.args, ["mcp", "--compact-tools"]);
  assert.equal(server.tool_timeout_sec, 150);
  for (const file of [".codex-plugin/plugin.json", ".agents/plugins/marketplace.json", ".mcp.json"])
    assert.ok(manifest.files.includes(file), `${file} must ship in the tarball`);
});

async function configure(options = {}, permissions = [], existing, listAgents = async () => ({data:[{permissions}]})) {
  let server = existing;
  const skills = [];
  let transform;
  let prompt;
  const editor = {get: () => existing, set: (_name, value) => {server = value;}};
  await plugin.setup({ options,
    agent: {list: listAgents},
    mcp: {transform: async fn => {transform=fn; fn(editor);}, reload: async () => transform(editor)},
    session: {hook: async (_name, fn) => {prompt=fn;}},
    skill: {transform: async fn => fn({add: skill => skills.push(skill)})},
  });
  await prompt?.();
  return {server, skills};
}
test("native adapter registers its server and skill", async () => {
  const {server, skills} = await configure();
  assert.deepEqual(server.command, [binary,"mcp","--compact-tools"]);
  assert.equal(server.timeout.execution,150000);
  assert.equal(server.codemode,false);
  assert.equal(skills.length,1);
  assert.equal(await readFile(new URL("skills/computer-use-mcp/SKILL.md",root),"utf8"),await readFile(new URL("../crates/computer-use-mcp/guidance/skill.md",import.meta.url),"utf8"));
});
test("disablement, custom MCPs and invalid options", async () => {
  assert.deepEqual((await configure({enabled:false})).skills,[]);
  await assert.rejects(configure({enabled:"yes"}),/boolean/);
  const existing={type:"local",command:["custom","mcp"],disabled:false};
  assert.strictEqual((await configure({},[],existing)).server,existing);
});
test("direct tools and custom registrations do not wait on agent catalogs", async () => {
  const catalog = () => { throw new Error("agent catalog must not be read"); };
  const direct = await configure({compactTools:false}, [], undefined, catalog);
  assert.deepEqual(direct.server.command, [binary, "mcp"]);
  const custom = {type:"local", command:["custom","mcp"], disabled:false};
  const existing = await configure({}, [], custom, catalog);
  assert.strictEqual(existing.server, custom);
  assert.equal(existing.skills.length, 1);
});

test("bundled executable has the package version", async () => {
  await access(binary, constants.X_OK);
  assert.equal(execFileSync(binary, ["version"], { encoding: "utf8" }).trim(), manifest.version);
});

test("native per-tool rules select direct tools", async () => {
  for (const action of ["computer_use_act","computer_use_observe","computer_use_a*"]) {
    const {server}=await configure({},[{action,resource:"*",effect:"ask"}]);
    assert.deepEqual(server.command,[binary,"mcp"]);
  }
  const {server}=await configure({},[{action:"computer_use_*",resource:"*",effect:"ask"}]);
  assert.equal(server.command.at(-1),"--compact-tools");
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
  const { server } = await configure();
  assert.ok(server.timeout.execution > wait.inputSchema.properties.timeout_ms.maximum,
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
