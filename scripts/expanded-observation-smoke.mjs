#!/usr/bin/env node
// Native toolkit regressions in the owned Wayland runner, not model benchmarks.
import assert from "node:assert/strict";
import { spawn, execFileSync } from "node:child_process";
import { once } from "node:events";
import { randomUUID } from "node:crypto";
import { mkdir, readFile, writeFile, access } from "node:fs/promises";
import { createInterface } from "node:readline";
import { fileURLToPath } from "node:url";
import { DesktopClient, DesktopToolError } from "./lib/desktop-client.mjs";

assert.equal(process.env.COMPUTER_USE_MCP_ISOLATED, "1");
assert.equal(process.env.GDK_BACKEND, "wayland");
await access(process.env.COMPUTER_USE_MCP_ISOLATION_MARKER);
const root = fileURLToPath(new URL("../", import.meta.url));
const task = process.argv[2] ?? "qt_invoice";
assert.ok(["qt_invoice", "gtk4_form", "settings", "unicode-preflight", "canvas", "kwrite"].includes(task));
const seed=Number(process.argv[3] ?? 101);
const fixtureTask = task === "unicode-preflight" ? "gtk4_form" : task;
const spec = JSON.parse(execFileSync("python3", ["-B", "-c", "import sys,json;sys.path.insert(0,'scripts');from desktop_bench_workloads import workload;print(json.dumps(workload(sys.argv[1],int(sys.argv[2]))))", fixtureTask,String(seed)], {cwd:root, encoding:"utf8"}));
const directory = `${root}.scratch/expanded-observation-smoke/${randomUUID()}`;
await mkdir(directory, {recursive:true,mode:0o700});
const state = `${directory}/receiver.json`;
const fixture = spawn("python3", ["-B", `${root}scripts/fixtures/deep-desktop-bench.py`, "--task", fixtureTask, "--seed", String(seed), "--state", state], {stdio:["ignore","pipe","inherit"]});
const exited = once(fixture, "close");
const lines = createInterface({input:fixture.stdout});
const readyDeadline = setTimeout(() => fixture.kill("SIGTERM"), 15_000);
let desktop, current;
try {
  const ready = await Promise.race([once(lines,"line").then(([line]) => line),exited.then(() => {throw new Error("Fixture exited before readiness");})]);
  assert.equal(ready, "READY"); clearTimeout(readyDeadline);
  desktop = new DesktopClient({command:process.env.COMPUTER_USE_MCP_TEST_BINARY ?? `${root}target/release/computer-use-mcp`,timeoutMs:30_000,onStderr:chunk=>process.stderr.write(chunk)});
  await desktop.initialize();
  if (task === "kwrite") {
    const ready=await desktop.call("wait_for",{condition:{type:"window_opened",desktop_id:"org.kde.kwrite.desktop"},timeout_ms:5000});
    assert.equal(ready.structuredContent.satisfied,true,"KWrite must publish its actual window before discovery");
  }
  const catalog = await desktop.call("list_desktop", {scope:"windows"});
  const targets = catalog.structuredContent.windows.filter(window => window.source.authority === "atspi" && (task === "kwrite" ? window.title.includes("KWrite") : window.title === "Desktop Benchmark"));
  assert.equal(targets.length, 1, "Require exact native top-level accessibility scope, not a compositor join");
  current = await desktop.observe(targets[0].target, {accessibility:{scope:"interactive",limits:{max_nodes:80}}});
  const original = current.data.target;
  const element = (name, capability) => {
    const matches = current.data.elements.filter(node => node.name === name && (!capability || node.capabilities?.[capability]));
    assert.equal(matches.length, 1, `Require one advertised ${name}/${capability}`); return matches[0];
  };
  const step = async (name, action = {type:"invoke"}) => {
    const node = element(name, action.type === "set_value" ? "set_value" : action.type);
    const next = await current.semantic(node.element_id, action); assert.ok(next.observation); current = next.observation;
  };
  if (task === "kwrite") {
    // Establish visual input before opening a menu: portal setup can dismiss it.
    await desktop.observe(current.data.target, { view: "screenshot" });
    current = await desktop.observe(current.data.target, { accessibility: { limits: { max_nodes: 80 } } });
     // Open the actual Qt modal with a click grounded only in its returned PNG.
     // DoAction for this menu item blocks inside Qt's nested modal event loop.
     const editorName=current.data.elements.find(node=>node.name && node.capabilities?.set_value==="text")?.name;
     assert.ok(editorName,"Retain only the advertised editor label, never its consumed ID");
    await writeFile(`${directory}/initial.json`,JSON.stringify(current.data,null,2));
    const menu = current.data.elements.filter(node=>node.name === "Settings" && node.capabilities?.named_actions?.some(action=>action.name === "Show Menu"));
    assert.equal(menu.length,1);
    current = (await current.semantic(menu[0].element_id,{type:"named",name:"Show Menu"})).observation;
    await writeFile(`${directory}/menu.json`,JSON.stringify(current.data,null,2));
    current = await desktop.observe(current.data.target,{accessibility:{search:{name:"Configure KWrite…",exact:true,limit:5,max_nodes:5000},limits:{max_nodes:80}}});
    await writeFile(`${directory}/menu-search.json`,JSON.stringify(current.data,null,2));
    const settings = element("Configure KWrite…","invoke");
    assert.equal(settings.keybinding,undefined,"Do not invent a shortcut for Configure");
    current=await desktop.observe(current.data.target,{view:"screenshot"});
    const image=current.result.content.find(item=>item.type === "image");assert.ok(image);
    await writeFile(`${directory}/configure.png`,Buffer.from(image.data,"base64"));
    const tsv=execFileSync("tesseract",[`${directory}/configure.png`,"stdout","--psm","11","tsv"],{encoding:"utf8",timeout:10000});
    const words=tsv.trim().split("\n").slice(1).map(row=>row.split("\t")).filter(row=>row.length>=12 && Number(row[10])>=40);
    const matches=words.filter(row=>row[11].toLowerCase().startsWith("configure") && words.some(other=>other.slice(1,5).join()===row.slice(1,5).join() && /kwrite/i.test(other[11])));
    assert.equal(matches.length,1,"Require unique Configure KWrite text in the actual PNG");
     const hit=matches[0];
     current=(await current.act({type:"pointer",action:{type:"click",x:Number(hit[6])+Number(hit[8])/2,y:Number(hit[7])+Number(hit[9])/2}})).observation;
     assert.ok(current);
     await writeFile(`${directory}/click.json`,JSON.stringify(current.data,null,2));
     if(current.data.target.window_instance_id===original.window_instance_id){
       // Dispatch completion need not include the newly published native modal.
       // Discover read-only; never repeat the already dispatched click.
       const windows=await desktop.call("list_desktop",{scope:"windows"});
       const modal=windows.structuredContent.windows.filter(window=>window.source.authority==="atspi" && window.target.app_instance_id===original.app_instance_id && window.target.window_instance_id!==original.window_instance_id);
       assert.equal(modal.length,1,"Require one fresh native modal, not a title/PID join");
       current=await desktop.observe(modal[0].target,{accessibility:{limits:{max_nodes:24}}});
     }
     assert.notEqual(current.data.target.window_instance_id,original.window_instance_id);
    await writeFile(`${directory}/modal.json`,JSON.stringify(current.data,null,2));
    const search=async name=>{current=await desktop.observe(current.data.target,{accessibility:{search:{name,exact:true,limit:5,max_nodes:5000},limits:{max_nodes:80}}});};
    await search("Open/Save");
    await step("Open/Save");
    await search("Append newline at end of file on save");
    const option = element("Append newline at end of file on save","invoke");
     if (option.states.includes("checked")) await step(option.name);
     await search("OK");
     await step("OK");
     assert.deepEqual(current.data.target,original);
     await search(editorName);
    const editors=current.data.elements.filter(node=>node.capabilities?.set_value === "text");assert.equal(editors.length,1);
    current=(await current.semantic(editors[0].element_id,{type:"set_value",value:spec.params.text+"\n"+spec.params.recipient})).observation;
    const focused=current.data.elements.filter(node=>node.capabilities?.set_value === "text");assert.equal(focused.length,1);
    current=(await current.act({type:"keyboard",focus:{type:"semantic",element_id:focused[0].element_id},events:[{type:"press",key:"Ctrl+S"}]})).observation;
    const saved=JSON.parse(await readFile(state,"utf8"));
    assert.equal(await readFile(saved.document_path,"utf8"),spec.params.text+"\n"+spec.params.recipient);
    assert.equal(saved.success,true);
  } else if (task === "canvas") {
    current=await desktop.observe(original,{view:"screenshot"});
    const png=current.result.content.find(item=>item.type === "image");assert.ok(png);
    await writeFile(`${directory}/canvas.png`,Buffer.from(png.data,"base64"));
    // Coordinates come only from this returned PNG's colored pixels, never
    // fixture coordinates or accessibility extents.
    const point=JSON.parse(execFileSync("python3",["-B","-c","from PIL import Image;import json,sys;im=Image.open(sys.argv[1]).convert('RGB');p=[(x,y) for y in range(im.height) for x in range(im.width) if (lambda c:c[1]>20 and c[1]>2*c[0] and c[1]>2*c[2])(im.getpixel((x,y)))];assert len(p)>1000;print(json.dumps({'x':sum(x for x,y in p)/len(p),'y':sum(y for x,y in p)/len(p)}))",`${directory}/canvas.png`],{encoding:"utf8"}));
    current=(await current.act({type:"keyboard",focus:{type:"point",...point},events:[{type:"type",text:spec.params.text},{type:"press",key:"Return"}]})).observation;
    const received=JSON.parse(await readFile(state,"utf8"));
    assert.equal(received.typed,spec.params.text);
    assert.equal(received.success,true,"Actual key-press receiver must see the complete Unicode string and submit");
  } else if (task === "unicode-preflight") {
    const reference = element("Reference", "set_value");
    current=(await current.act({type:"keyboard",focus:{type:"semantic",element_id:reference.element_id},events:[{type:"press",key:"Ctrl+A"},{type:"type",text:spec.params.text}]})).observation;
    assert.equal(element("Reference","set_value").text,spec.params.text);
  } else if (task === "gtk4_form") {
    await step("Reference", {type:"set_value",value:spec.params.text});
    assert.equal(element("Reference","set_value").text, spec.params.text);
    const notifications=current.data.elements.find(node=>node.name === "Notifications" && node.role === "check box");assert.ok(notifications);
    assert.equal(notifications.capabilities.focus,false,"Verified GTK version must not advertise its unimplemented focus method");
    await assert.rejects(current.semantic(notifications.element_id,{type:"focus"}),error=>{
      assert.ok(error instanceof DesktopToolError);assert.equal(error.result.structuredContent.outcome,"not_started");
      assert.equal(error.result.structuredContent.code,"capability_unavailable");return true;
    });
  } else if (task === "settings") {
    const tab = element("Delivery"); assert.equal(tab.capabilities.invoke, false);
    const lists = current.data.elements.filter(node => node.role === "page tab list" && node.capabilities?.focus);
    assert.equal(lists.length, 1);
    current = (await current.act({type:"keyboard",focus:{type:"semantic",element_id:lists[0].element_id},events:[{type:"press",key:"Right"}]})).observation;
    assert.ok(current);
    const reference = element("Reference","set_value"), alerts = element("Delivery alerts","invoke"), save = element("Save","invoke");
    current = (await desktop.workflow(current,[
      {element:{name:reference.name,role:reference.role},action:{type:"set_value",value:spec.params.text}},
      {element:{name:alerts.name,role:alerts.role},action:{type:"invoke"},ensureChecked:true},
      {element:{name:save.name,role:save.role},action:{type:"invoke"}},
      {expectVisibleText:"Task complete"},
    ])).observation;
  } else {
    await step("Customer", {type:"set_value",value:spec.params.recipient});
    await step("Units", {type:"set_value",value:String(spec.params.units)});
    assert.equal(element("Units","set_value").capabilities.set_value,"number");
    assert.equal(Number(element("Units","set_value").value),spec.params.units,"Native spin-box value is committed before focus leaves it");
    const combos = current.data.elements.filter(node => node.role === "combo box" && node.capabilities?.invoke);
    assert.equal(combos.length,1);
    await assert.rejects(current.semantic(combos[0].element_id,{type:"focus"}),error=>{
      assert.ok(error instanceof DesktopToolError);assert.equal(error.result.structuredContent.outcome,"unknown");
      assert.ok(error.observation);current=error.observation;return true;
    });
    // A false GrabFocus reply is not permanent negative capability evidence.
    assert.equal(current.data.elements.find(node=>node.role === "combo box").capabilities.focus,true);
    const focusedCombo=current.data.elements.filter(node=>node.role === "combo box" && node.capabilities?.invoke);assert.equal(focusedCombo.length,1);
    current = (await current.semantic(focusedCombo[0].element_id,{type:"invoke"})).observation;
    const choices = current.data.elements.filter(node => node.role === "list item" && node.name === spec.params.choice && node.capabilities?.invoke);
    assert.equal(choices.length,1);
    current = (await current.semantic(choices[0].element_id,{type:"invoke"})).observation;
    const insured = element("Insured","invoke");
    current = (await desktop.workflow(current,[{element:{name:insured.name,role:insured.role},action:{type:"invoke"},ensureChecked:true}])).observation;
    const review = element("Review invoice","invoke");
    const done = await desktop.workflow(current,[
      {element:{name:review.name,role:review.role},action:{type:"invoke"}},
      {element:{name:"Confirm invoice",role:"button"},action:{type:"invoke"}},
      {expectVisibleText:"Task complete"},
    ]);
    assert.notEqual(done.steps[0].target.window_instance_id,original.window_instance_id);
    current = done.observation;
    assert.deepEqual(current.data.target,original);
  }
  const receiver = JSON.parse(await readFile(state,"utf8"));
  if (["qt_invoice","settings"].includes(task)) assert.equal(receiver.success,true);
  assert.equal(receiver.wrong_submissions,0); assert.equal(receiver.unsafe_action,false);
  console.log(JSON.stringify({task,receiver,evidence:directory},null,2));
} finally {
  clearTimeout(readyDeadline);
  if (current) await writeFile(`${directory}/observation.json`, JSON.stringify(current.data,null,2), {mode:0o600});
  try { await desktop?.close(); } finally {
    lines.close(); fixture.kill("SIGTERM"); const deadline = setTimeout(() => fixture.kill("SIGKILL"),10_000);
    try { await exited; } finally {clearTimeout(deadline);}
  }
}
