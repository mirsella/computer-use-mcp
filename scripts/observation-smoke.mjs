#!/usr/bin/env node
// Real GTK callbacks and the public client, only inside an owned private runner.
import assert from "node:assert/strict";
import { spawn } from "node:child_process";
import { randomUUID } from "node:crypto";
import { once } from "node:events";
import { access, mkdir, readFile, writeFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { createInterface } from "node:readline";
import { DesktopClient } from "./lib/desktop-client.mjs";

assert.equal(process.env.COMPUTER_USE_MCP_ISOLATED, "1", "Use scripts/run-isolated-session.sh");
assert.equal(process.env.GDK_BACKEND, "wayland");
assert.ok(process.env.COMPUTER_USE_MCP_ISOLATION_MARKER);
await access(process.env.COMPUTER_USE_MCP_ISOLATION_MARKER);
const root = fileURLToPath(new URL("../", import.meta.url));
const directory = `${root}target/observation-smoke`;
await mkdir(directory, { recursive: true, mode: 0o700 });
const state = `${directory}/${randomUUID()}.json`;
const task = process.argv[2] ?? "medium";
assert.ok(["medium", "hard"].includes(task));
const fixture = spawn("python3", ["-B", `${root}scripts/fixtures/desktop-bench.py`, "--task", task, "--state", state], {
  stdio: ["ignore", "pipe", "inherit"],
});
const exited = once(fixture, "close");
const lines = createInterface({ input: fixture.stdout });
const timeout = setTimeout(() => fixture.kill("SIGTERM"), 15_000);
let desktop;
try {
  const ready = await Promise.race([once(lines, "line").then(([line]) => line), exited.then(() => { throw new Error("Fixture exited before readiness"); })]);
  assert.equal(ready, "READY");
  clearTimeout(timeout);
  desktop = new DesktopClient({ command: `${root}target/debug/computer-use-mcp`, timeoutMs: 30_000 });
  await desktop.initialize();
  const catalog = await desktop.call("list_desktop", { scope: "windows" });
  const windows = catalog.structuredContent.windows.filter(window => window.title === "Desktop Benchmark" && window.source.authority === "atspi");
  assert.equal(windows.length, 1, "Require one exact AT-SPI catalog entry, without joining other backends");
  const target = windows[0].target;
  const observed = await desktop.observe(target, { accessibility: { scope: task === "medium" ? "full" : "interactive", limits: { max_nodes: 40 } } });
  await writeFile(`${state}.observation.json`, JSON.stringify(observed.data, null, 2), {mode: 0o600});
  assert.ok(observed.data.elements.length <= 40);
  if (task === "hard") {
    let current = observed;
    const step = async (name, action = {type: "invoke"}) => {
      const matches = current.data.elements.filter(element => element.name === name);
      assert.equal(matches.length, 1, `Require one advertised ${name}`);
      const done = await current.semantic(matches[0].element_id, action);
      assert.ok(done.observation, "Every step must return fresh authority without another discovery/observe");
      current = done.observation;
      return done;
    };
    await step("Start shipment");
    await step("Recipient", {type: "set_value", value: "Ada Lovelace"});
    await step("Dispatch note", {type: "set_value", value: "café Δ 42"});
    const checkbox = current.data.elements.find(element => element.name === "Express delivery");
    assert.ok(checkbox && !checkbox.states.includes("checked"));
    const toggled = await step("Express delivery");
    assert.ok(current.data.elements.find(element => element.name === "Express delivery").states.includes("checked"));
    assert.ok(toggled.result.content.some(item => item.type === "text" && item.text.includes("checked=true")));
    await step("Review shipment");
    assert.equal(current.data.target.app_instance_id, target.app_instance_id);
    assert.notEqual(current.data.target.window_instance_id, target.window_instance_id);
    const modal = current;
    await step("Confirm shipment");
    assert.deepEqual(current.data.target, target, "Closing the exact modal returns the exact surviving main target");
    assert.ok(current.data.accessibility.context.includes("Task complete"), "Interactive state must retain visible status context");
    await assert.rejects(modal.act({type: "semantic"}), /superseded or consumed/);
    const receiver = JSON.parse(await readFile(state, "utf8"));
    assert.equal(receiver.success, true);
    assert.equal(receiver.wrong_submissions, 0);
    console.log(JSON.stringify({task, success: true, receiver, state}, null, 2));
  } else {
  const footer = observed.data.elements.find(element => element.name === "Confirm inventory batch");
  assert.ok(footer, `A real footer after 2,000 rows must survive a 40-node observation; evidence: ${state}.observation.json`);
  const found = await desktop.observe(target, { accessibility: {
    scope: "full", search: { name: footer.name, role: footer.role, exact: true, max_nodes: 160 }, limits: { max_nodes: 40 },
  } });
  assert.equal(found.data.elements.length, 1);
  assert.equal(found.data.elements[0].name, footer.name);
  assert.ok(found.data.accessibility.traversal.detailed_nodes < found.data.accessibility.traversal.visited_nodes);
  assert.equal(found.data.accessibility.traversal.truncated, true, "The unscanned collection cannot establish absence");
  const completed = await found.semantic(found.data.elements[0].element_id, { type: "invoke" });
  assert.ok(completed.observation, "Native action must provide a replacement observation handle");
  await assert.rejects(found.act({ type: "semantic" }), /superseded or consumed/);
  const receiver = JSON.parse(await readFile(state, "utf8"));
  assert.equal(receiver.success, true);
  assert.ok(receiver.events.includes("inventory-confirmed"));
  console.log(JSON.stringify({ success: true, output_nodes: observed.data.elements.length, traversal: found.data.accessibility.traversal, receiver, state }, null, 2));
  }
} finally {
  clearTimeout(timeout);
  try { await desktop?.close(); } finally {
    lines.close();
    fixture.kill("SIGTERM");
    const kill = setTimeout(() => fixture.kill("SIGKILL"), 10_000);
    try { await exited; } finally { clearTimeout(kill); }
  }
}
