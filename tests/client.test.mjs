import assert from "node:assert/strict";
import { test } from "node:test";
import { DesktopClient, DesktopToolError } from "../scripts/lib/desktop-client.mjs";

const target = { app_instance_id: "app-0000000000000001", window_instance_id: "win-0000000000000001" };
const program = `
import readline from 'node:readline';
let generation = 0, actions = 0, text = 'Old', complete = false;
function observation(target) { ++generation; return {target, observation_id: 'obs-' + generation,
  accessibility: {context: complete ? ['Task complete'] : [],traversal:{truncated:!!process.env.TEST_INCOMPLETE}},
  elements: [{element_id: 'e-' + generation, name:'Apply', role:'push button', states:['showing'], capabilities:{invoke:true}},
    {element_id:'edit-' + generation,name:'Text',role:'text',value:text,states:['showing'],capabilities:{set_value:'text'}},
    {element_id:'check-' + generation,name:'Enabled',role:'check box',states:['showing','checked'],capabilities:{invoke:true}},
    ...(process.env.TEST_DUPLICATE_LABEL ? [{element_id:'duplicate',name:'Apply',role:'push button',capabilities:{invoke:true}}] : [])],
  screenshot: {frame_id: 'frame-' + generation}}; }
for await (const line of readline.createInterface({input: process.stdin})) {
  const request = JSON.parse(line);
  if (request.id === undefined) continue;
  if (request.params?.arguments?.hang) continue;
  if (request.params?.arguments?.malformed) { console.log('{}'); continue; }
   let result = request.method === 'initialize' ? {protocolVersion: request.params.protocolVersion} : {};
   if (request.method === 'initialize' && process.env.TEST_LEGACY_MCP) result.protocolVersion = '2024-11-05';
  const args = request.params?.arguments;
  if (request.method === 'tools/call') {
     if (args.fail) result = {isError: true, structuredContent: {outcome: 'unknown',...(args.recovery ? {replacement_observation:observation(args.target)} : {})}};
    else if (request.params.name === 'observe') result = {structuredContent: observation(args.target)};
    else if (request.params.name === 'act') {
      ++actions;
      if (String(actions) === process.env.TEST_FAIL_AT_STEP) result = {isError:true,structuredContent:{outcome:'unknown'}};
      else {
        if (args.operation.action?.type === 'set_value') text = args.operation.action.value;
        if (args.operation.action?.type === 'invoke') complete = true;
        result = {structuredContent: {source: args.source_observation, replacement_observation: observation(process.env.TEST_REPLACEMENT_TARGET ? {...args.target, window_instance_id: 'win-0000000000000002'} : args.target)}};
      }
    }
  }
  console.log(JSON.stringify({jsonrpc: '2.0', id: request.id, result}));
}
`;
function client(options = {}) { return new DesktopClient({ command: process.execPath, args: ["--input-type=module", "-e", program], ...options }); }

test("bounded opt-in workflows consume fresh exact handles and included positive evidence", async t => {
  const desktop = client(); t.after(() => desktop.close()); await desktop.initialize();
  const initial = await desktop.observe(target);
  const result = await desktop.workflow(initial, [
    {element:{name:'Enabled',role:'check box'},ensureChecked:true,action:{type:'invoke'}},
    {element:{name:'Text',role:'text'},action:{type:'set_value',value:'café Δ'}},
    {element:{name:'Apply',role:'push button'},action:{type:'invoke'}},
    {expectVisibleText:'Task complete'},
  ]);
  assert.deepEqual(result.steps.map(step => step.skipped), [true,false,false,true]);
  assert.equal(result.observation.data.observation_id, 'obs-3');
  assert.equal(result.observation.data.elements[1].value, 'café Δ');
  await assert.rejects(initial.act({type:'semantic'}), /consumed/);
  await assert.rejects(desktop.workflow(result.observation,[{expectVisibleText:'Missing'}]), /absence is not established/);
  assert.throws(() => desktop.workflow(result.observation, Array(33).fill({expectVisibleText:'Task complete'})), /1–32/);
});

test("workflow failures preserve native unknown outcomes and never replay earlier mutations", async t => {
  const desktop = client({env:{...process.env,TEST_FAIL_AT_STEP:'2'}}); t.after(() => desktop.close());
  await desktop.initialize(); const source = await desktop.observe(target);
  await assert.rejects(desktop.workflow(source,[
    {element:{name:'Text',role:'text'},action:{type:'set_value',value:'Once'}},
    {element:{name:'Apply',role:'push button'},action:{type:'invoke'}},
    {element:{name:'Text',role:'text'},action:{type:'set_value',value:'Never'}},
  ]), error => error instanceof DesktopToolError && error.result.structuredContent.outcome === 'unknown' && error.workflowCompletedSteps === 1);
  const next = await desktop.observe(target);
  assert.equal(next.data.elements[1].value, 'Once');
  assert.equal(next.data.observation_id, 'obs-3');
});

test("workflows refuse ambiguous current matches without dispatch and follow modal target handles", async t => {
  const ambiguous = client({env:{...process.env,TEST_DUPLICATE_LABEL:'1'}}); t.after(() => ambiguous.close());
  await ambiguous.initialize(); const source = await ambiguous.observe(target);
  await assert.rejects(ambiguous.workflow(source,[{element:{name:'Apply',role:'push button'},action:{type:'invoke'}}]), /one included exact/);
  assert.equal((await ambiguous.observe(target)).data.observation_id, 'obs-2');
  const incomplete = client({env:{...process.env,TEST_INCOMPLETE:'1'}});t.after(() => incomplete.close());
  await incomplete.initialize();const partial = await incomplete.observe(target);
  await assert.rejects(incomplete.workflow(partial,[{element:{name:'Apply',role:'push button'},action:{type:'invoke'}}]),/selection evidence is incomplete/);
  assert.equal((await incomplete.observe(target)).data.observation_id,'obs-2');
  const modal = client({env:{...process.env,TEST_REPLACEMENT_TARGET:'1'}}); t.after(() => modal.close());
  await modal.initialize(); const original = await modal.observe(target);
  const result = await modal.workflow(original,[
    {element:{name:'Apply',role:'push button'},action:{type:'invoke'}},
    {element:{name:'Apply',role:'push button'},action:{type:'invoke'}},
  ]);
  assert.equal(result.observation.data.target.window_instance_id, 'win-0000000000000002');
});

test("stateful client consumes exact observations and adopts replacement IDs", async t => {
  const desktop = client();
  t.after(() => desktop.close());
  await desktop.initialize();
  const original = await desktop.observe(target);
  assert.throws(() => { original.data.target.app_instance_id = "changed"; }, TypeError);
  assert.throws(() => original.semantic("wrong", {type: "invoke"}), /not in this observation/);
  const { result, observation } = await original.semantic("e-1", {type: "invoke"});
  assert.equal(result.structuredContent.source.observation_id, "obs-1");
  assert.equal(result.structuredContent.source.frame_id, "frame-1");
  assert.equal(observation.data.observation_id, "obs-2");
  await assert.rejects(original.act({type: "semantic"}), /superseded or consumed/);
  const concurrent = [observation.act({type: "semantic"}), observation.act({type: "semantic"})];
  assert.equal((await concurrent[0]).observation.data.observation_id, "obs-3");
  await assert.rejects(concurrent[1], /superseded or consumed/);
});

test("a modal replacement exposes its distinct exact target without retargeting input", async t => {
  const desktop = client({env: {...process.env, TEST_REPLACEMENT_TARGET: "1"}});
  t.after(() => desktop.close());
  await desktop.initialize();
  const source = await desktop.observe(target);
  const {result, observation: next} = await source.semantic("e-1", {type: "invoke"});
  assert.equal(result.structuredContent.source.observation_id, source.data.observation_id);
  assert.equal(next.data.target.app_instance_id, target.app_instance_id);
  assert.equal(next.data.target.window_instance_id, "win-0000000000000002");
  await assert.rejects(source.act({type: "semantic"}), /superseded or consumed/);
  await next.semantic("e-2", {type: "invoke"});
});

test("new observations supersede handles; unknown errors are returned without replay", async t => {
  const desktop = client();
  t.after(() => desktop.close());
  await desktop.initialize();
  const old = await desktop.observe(target);
  await desktop.observe(target);
  await assert.rejects(old.act({}), /superseded/);
  await assert.rejects(desktop.call("act", {target, fail: true}), error => error instanceof DesktopToolError && error.result.structuredContent.outcome === "unknown");
  const current = await desktop.observe(target);
  assert.equal(current.data.observation_id, "obs-3");
});

test("unknown errors expose fresh recovery authority without retaining consumed handles", async t => {
  const desktop = client(); t.after(() => desktop.close()); await desktop.initialize();
  const original = await desktop.observe(target);
  let recovery;
  await assert.rejects(desktop.call("act", {target, fail:true, recovery:true}), error => {
    assert.ok(error instanceof DesktopToolError);
    assert.equal(error.result.structuredContent.outcome, "unknown");
    recovery = error.observation;
    assert.equal(recovery.data.observation_id, "obs-2");
    return true;
  });
  await assert.rejects(original.act({type:"semantic"}), /superseded or consumed/);
  assert.equal((await recovery.semantic("e-2", {type:"invoke"})).observation.data.observation_id, "obs-3");
});

test("malformed protocol and deadlines fail the connection, not silently retry", async t => {
  for (const args of [{malformed: true}, {hang: true}]) {
    const desktop = client({ timeoutMs: 5_000 });
    t.after(() => assert.rejects(desktop.close(), /Abnormal desktop shutdown/));
    await desktop.initialize();
    // Exercise a hung/malformed call, not child startup under parallel load.
    desktop.timeoutMs = 300;
    await assert.rejects(desktop.call("list_desktop", args), /Unexpected|deadline/);
    await assert.rejects(desktop.call("list_desktop"), /Unexpected|deadline/);
  }
});

test("initialization and close are idempotent, and closed clients cannot send work", async () => {
  const desktop = client();
  try {
    await assert.rejects(desktop.call("list_desktop"), /Initialize/);
    assert.equal(desktop.initialize(), desktop.initialize());
    await desktop.initialize();
    await desktop.close();
    await desktop.close();
    await assert.rejects(desktop.call("list_desktop"), /closed/);
  } finally { await desktop.close(); }
});

test("failed observations and cross-window mutations discard uncertain handles", async t => {
  const desktop = client();
  t.after(() => desktop.close());
  await desktop.initialize();
  const original = await desktop.observe(target);
  await assert.rejects(desktop.observe(target, {fail: true}), DesktopToolError);
  await assert.rejects(original.act({}), /superseded/);
  const other = await desktop.observe({...target, window_instance_id: "win-0000000000000002"});
  const current = await desktop.observe(target);
  await current.act({type: "semantic"});
  await assert.rejects(other.act({}), /superseded/);
});

test("peers without structured-result authority fail initialization explicitly", async t => {
  const desktop = client({env: {...process.env, TEST_LEGACY_MCP: "1"}});
  t.after(() => assert.rejects(desktop.close(), /Abnormal desktop shutdown/));
  await assert.rejects(desktop.initialize(), /requires MCP 2025-06-18/);
  await assert.rejects(desktop.initialize(), /requires MCP 2025-06-18/);
});

test("graceful shutdown has its own worker-cleanup budget and forced stops are explicit", async () => {
  const graceful = new DesktopClient({
    command: process.execPath,
    args: ["-e", "process.stdin.resume(); process.stdin.on('end', () => setTimeout(() => {}, 50));"],
    timeoutMs: 5,
    closeTimeoutMs: 1_000,
  });
  await graceful.close();
  await graceful.close();
  const stuck = new DesktopClient({
    command: process.execPath,
    args: ["-e", "setTimeout(() => {}, 10000); process.stdin.resume();"],
    closeTimeoutMs: 30,
  });
  await assert.rejects(stuck.close(), /forced stop, private-session cleanup is not verified/);
  assert.throws(() => new DesktopClient({closeTimeoutMs: 0}), /closeTimeoutMs/);
});

test("nonzero exits and signals cannot pass graceful shutdown verification", async () => {
  for (const finish of ["process.exit(7)", "process.kill(process.pid, 'SIGTERM')"]) {
    const desktop = new DesktopClient({
      command: process.execPath,
      args: ["-e", `process.stdin.resume(); process.stdin.on('end', () => { ${finish}; });`],
    });
    await assert.rejects(desktop.close(), /Abnormal desktop shutdown.*cleanup is not verified/);
    await assert.rejects(desktop.close(), /Abnormal desktop shutdown/);
  }
});
