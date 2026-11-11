import { spawn } from "node:child_process";
import { fileURLToPath } from "node:url";

export class DesktopToolError extends Error {
  constructor(result) {
    super("Desktop operation failed; inspect result outcome before recovery");
    this.name = "DesktopToolError";
    this.result = result;
  }
}

const targetKey = target => JSON.stringify([target.app_instance_id, target.window_instance_id]);

/** A stdio client, not another executor. No retries or selector-based relocation. */
export class DesktopClient {
  #child; #pending = new Map(); #next = 0; #buffer = "";
  #failure; #tail = Promise.resolve(); #observations = new Map(); #exit; #killTimer;
  #initialization; #initialized = false; #ended = false; #closing;

  constructor({ command = fileURLToPath(new URL("../../vendor/bin/computer-use-mcp", import.meta.url)),
    args = ["mcp"], env = process.env, timeoutMs = 150_000, closeTimeoutMs = 35_000, onStderr } = {}) {
    if (!Number.isSafeInteger(timeoutMs) || timeoutMs < 1) throw new TypeError("timeoutMs must be positive");
    if (!Number.isSafeInteger(closeTimeoutMs) || closeTimeoutMs < 1) throw new TypeError("closeTimeoutMs must be positive");
    this.timeoutMs = timeoutMs;
    this.closeTimeoutMs = closeTimeoutMs;
    this.#child = spawn(command, args, { env, stdio: ["pipe", "pipe", "pipe"] });
    this.#exit = new Promise(resolve => this.#child.once("close", (code, signal) => resolve({ code, signal })));
    this.#child.once("error", error => this.#fail(error));
    this.#child.once("close", () => {
      this.#ended = true;
      clearTimeout(this.#killTimer);
      this.#fail(new Error("Desktop transport closed; dispatched operations may have unknown outcomes"));
    });
    this.#child.stdin.on("error", error => this.#fail(error));
    this.#child.stderr.on("data", chunk => {
      try { onStderr?.(chunk); } catch (error) { this.#fail(error); }
    });
    this.#child.stdout.setEncoding("utf8").on("data", chunk => {
      if (this.#failure) return;
      this.#buffer += chunk;
      if (Buffer.byteLength(this.#buffer) > 16 * 1024 * 1024) return this.#fail(new Error("MCP response exceeds client buffer limit"));
      let end;
      while ((end = this.#buffer.indexOf("\n")) >= 0) {
        const line = this.#buffer.slice(0, end);
        this.#buffer = this.#buffer.slice(end + 1);
        try {
          const response = JSON.parse(line);
          if (response.method) {
            if (response.id !== undefined) throw new Error("Unexpected server request");
            continue;
          }
          const pending = this.#pending.get(response.id);
          if (!pending) throw new Error("Unexpected MCP response identity");
          if (!response.error && !("result" in response)) throw new Error("MCP response has no result");
          this.#pending.delete(response.id);
          clearTimeout(pending.timer);
          if (response.error) pending.reject(new Error(`MCP protocol error ${response.error.code}`));
          else pending.resolve(response.result);
        } catch (error) { this.#fail(error); return; }
      }
    });
  }

  #fail(error) {
    if (this.#failure) return;
    this.#failure = error;
    this.#observations.clear();
    for (const pending of this.#pending.values()) { clearTimeout(pending.timer); pending.reject(error); }
    this.#pending.clear();
    this.#child.stdin.destroy();
    if (!this.#ended) {
      this.#child.kill("SIGTERM");
      this.#killTimer = setTimeout(() => this.#child.kill("SIGKILL"), this.closeTimeoutMs);
      this.#killTimer.unref();
    }
  }

  #send(message) { this.#child.stdin.write(`${JSON.stringify({ jsonrpc: "2.0", ...message })}\n`); }

  #rpc(method, params) {
    if (this.#failure) return Promise.reject(this.#failure);
    const id = ++this.#next;
    return new Promise((resolve, reject) => {
      const timer = setTimeout(() => this.#fail(new Error("MCP deadline exceeded; mutation outcome may be unknown; no retry attempted")), this.timeoutMs);
      this.#pending.set(id, { resolve, reject, timer });
      this.#send({ id, method, params });
    });
  }

  #enqueue(run) {
    const result = this.#tail.then(run);
    this.#tail = result.catch(() => {});
    return result;
  }

  initialize() {
    return this.#initialization ??= this.#enqueue(async () => {
      const result = await this.#rpc("initialize", { protocolVersion: "2025-06-18", capabilities: {}, clientInfo: { name: "computer-use-client", version: "1" } });
      if (result.protocolVersion !== "2025-06-18") {
        const error = new Error("Desktop client requires MCP 2025-06-18 structured results");
        this.#fail(error);
        throw error;
      }
      this.#send({ method: "notifications/initialized" });
      this.#initialized = true;
      return result;
    });
  }

  async #invoke(name, args) {
    if (!this.#initialized) throw new Error("Initialize the desktop client before calling tools");
    // A mutation can change focus and other windows, not only its target.
    if (["act", "activate_window", "launch_application"].includes(name)) this.#observations.clear();
    if (name === "observe") this.#observations.delete(targetKey(args.target));
    const result = await this.#rpc("tools/call", { name, arguments: args });
    if (result.isError) {
      // Readback does not make an uncertain mutation successful. Expose it to
      // the caller for an explicit recovery decision; workflows still stop.
      const error = new DesktopToolError(result);
      error.observation = this.#retain(result.structuredContent?.replacement_observation, result);
      throw error;
    }
    if (name === "observe") this.#retain(result.structuredContent, result);
    if (name === "act") this.#retain(result.structuredContent?.replacement_observation, result);
    return result;
  }

  call(name, args = {}) {
    const copy = structuredClone(args);
    return this.#enqueue(() => this.#invoke(name, copy));
  }

  #retain(data, result) {
    if (!data?.observation_id || !data.target) return;
    const observation = new Observation(this, data, result);
    this.#observations.set(targetKey(data.target), observation);
    return observation;
  }

  observe(target, options = {}) {
    const args = structuredClone({ view: "accessibility", ...options, target });
    return this.#enqueue(async () => {
      const result = await this.#invoke("observe", args);
      const observation = this.#observations.get(targetKey(args.target));
      if (!observation || observation.result !== result) throw new Error("Observe returned no structured observation");
      return observation;
    });
  }

  actFrom(observation, operation) {
    const copy = structuredClone(operation);
    return this.#enqueue(() => this.#actFrom(observation, copy));
  }

  async #actFrom(observation, operation) {
      if (this.#observations.get(targetKey(observation.data.target)) !== observation) throw new Error("Observation was superseded or consumed; observe again");
      const result = await this.#invoke("act", {
        target: observation.data.target,
        source_observation: { observation_id: observation.data.observation_id, frame_id: observation.data.screenshot?.frame_id ?? null },
        operation,
      });
      const next = result.structuredContent?.replacement_observation?.target;
      return { result, observation: next ? this.#observations.get(targetKey(next)) ?? null : null };
  }

  /** Opt-in local scripting helper. No new agent executor, retries, or permissions. */
  workflow(observation, steps) {
    const plan = structuredClone(steps);
    if (!Array.isArray(plan) || plan.length < 1 || plan.length > 32) throw new TypeError("A workflow needs 1–32 steps");
    for (const step of plan) {
      if (!step || typeof step !== "object" || Array.isArray(step)) throw new TypeError("Invalid workflow step");
      if (typeof step.expectVisibleText === "string" && step.expectVisibleText.length > 0 && Object.keys(step).length === 1) continue;
      if (Object.keys(step).some(key => !["element", "action", "ensureChecked"].includes(key)) ||
          !step.element || Object.keys(step.element).some(key => !["name", "role"].includes(key)) ||
          typeof step.element.name !== "string" || typeof step.element.role !== "string" ||
          !["invoke", "focus", "set_value"].includes(step.action?.type) ||
          Object.keys(step.action).some(key => !(step.action.type === "set_value" ? ["type", "value"] : ["type"]).includes(key)) ||
          (step.action.type === "set_value" && typeof step.action.value !== "string") ||
          ("ensureChecked" in step && (typeof step.ensureChecked !== "boolean" || step.action.type !== "invoke"))) {
        throw new TypeError("Workflow actions need exact native name/role and invoke, focus or set_value");
      }
    }
    return this.#enqueue(async () => {
      let current = observation;
      const completed = [];
      try {
        for (const step of plan) {
          if (this.#observations.get(targetKey(current.data.target)) !== current) throw new Error("Workflow source was superseded or consumed");
          if ("expectVisibleText" in step) {
            const included = current.data.accessibility?.context?.includes(step.expectVisibleText) ||
              current.data.elements?.some(node => node.name === step.expectVisibleText && node.states?.includes("showing"));
            if (!included) throw new Error("Expected visible text was not included; absence is not established");
            completed.push({ skipped: true, evidence: "included_visible_text" });
            continue;
          }
          if (current.data.truncated === true || current.data.accessibility?.truncated === true || current.data.accessibility?.traversal?.truncated === true) {
            throw new Error("Workflow selection evidence is incomplete; obtain a complete appropriate scope before planning name-based actions");
          }
          // A selection from THIS new snapshot, never relocation of an old ID.
          const matches = current.data.elements?.filter(node => node.name === step.element.name && node.role === step.element.role) ?? [];
          if (matches.length !== 1) throw new Error("Workflow requires one included exact name/role match; refusing ambiguous or incomplete evidence");
          const node = matches[0];
          const isCheck = ["check box", "radio button", "toggle button", "switch"].includes(node.role);
          if ("ensureChecked" in step) {
            if (!isCheck || !Array.isArray(node.states)) throw new Error("Workflow check state unavailable");
            if (node.states.includes("checked") === step.ensureChecked) { completed.push({ skipped: true }); continue; }
          }
          const capability = step.action.type === "set_value" ? node.capabilities?.set_value : node.capabilities?.[step.action.type];
          if (!(step.action.type === "set_value" ? ["text", "number"].includes(capability) : capability === true)) throw new Error("Workflow action is not advertised by this exact element");
          const next = await this.#actFrom(current, { type: "semantic", element_id: node.element_id, action: step.action });
          if (!next.observation) throw new Error("Workflow action returned no replacement observation; inspect before continuing");
          current = next.observation;
          completed.push({ skipped: false, target: current.data.target, observation_id: current.data.observation_id });
          if ("ensureChecked" in step) {
            const verified = current.data.elements?.filter(value => value.name === step.element.name && value.role === step.element.role) ?? [];
            if (verified.length !== 1 || !Array.isArray(verified[0].states) || verified[0].states.includes("checked") !== step.ensureChecked) throw new Error("Workflow check state was not verified after dispatch");
          }
        }
        return { observation: current, steps: completed };
      } catch (error) {
        error.workflowCompletedSteps = completed.length;
        throw error; // Keep native outcomes; never replay a partial workflow.
      }
    });
  }

  close() {
    return this.#closing ??= (async () => {
      this.#observations.clear();
      const error = this.#failure ??= new Error("Desktop client closed; pending mutation outcomes may be unknown");
      for (const pending of this.#pending.values()) { clearTimeout(pending.timer); pending.reject(error); }
      this.#pending.clear();
      this.#child.stdin.end();
      // Native worker cleanup owns up to 20s plus 5s TERM escalation. Do not
      // truncate that owned-session teardown with a shorter client deadline.
      let forced = false;
      const timer = setTimeout(() => { forced = true; this.#child.kill("SIGKILL"); }, this.closeTimeoutMs);
      let status;
      try { status = await this.#exit; } finally { clearTimeout(timer); clearTimeout(this.#killTimer); }
      if (forced) throw new Error("Desktop shutdown deadline exceeded; forced stop, private-session cleanup is not verified");
      if (status.code !== 0 || status.signal) throw new Error(`Abnormal desktop shutdown (${status.signal ?? `exit ${status.code}`}); private-session cleanup is not verified`);
    })();
  }

  async [Symbol.asyncDispose]() { await this.close(); }
}

class Observation {
  constructor(client, data, result) {
    this.client = client;
    this.data = structuredClone(data);
    this.result = result;
    const freeze = value => { if (value && typeof value === "object") { Object.values(value).forEach(freeze); Object.freeze(value); } };
    freeze(this.data);
    Object.freeze(this);
  }
  act(operation) { return this.client.actFrom(this, operation); }
  semantic(elementId, action) {
    if (!this.data.elements?.some(element => element.element_id === elementId)) throw new Error("Element is not in this observation");
    return this.act({ type: "semantic", element_id: elementId, action });
  }
}
