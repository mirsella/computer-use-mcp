import { Plugin, Skill } from "@opencode/plugin";
import { readFile } from "node:fs/promises";
import { fileURLToPath } from "node:url";
import { parse } from "yaml";

const binary = fileURLToPath(new URL("../vendor/bin/computer-use-mcp", import.meta.url));
const path = fileURLToPath(new URL("../skills/computer-use-mcp/SKILL.md", import.meta.url));
const tools = ["list_desktop", "launch_application", "activate_window", "observe", "act", "wait_for"].map(name => `computer_use_${name}`);

export default Plugin.define({
  id: "@mirsella/opencode-computer-use-mcp",
  async setup(ctx) {
    const { enabled = true, compactTools = true } = ctx.options;
    if (typeof enabled !== "boolean") throw new Error("enabled must be a boolean");
    if (typeof compactTools !== "boolean") throw new Error("compactTools must be a boolean");
    let compact = compactTools;
    let disposed = false;
    let existingServer;
    await ctx.mcp.transform(editor => {
      existingServer = editor.get("computer_use");
      if (existingServer) return;
      editor.set("computer_use", {
        type: "local", command: [binary, "mcp", ...(compact ? ["--compact-tools"] : [])],
        disabled: !enabled, codemode: false,
        timeout: { startup: 30_000, catalog: 30_000, execution: 150_000 },
      });
    });
    if (existingServer ? existingServer.disabled : !enabled) return;
    const text = await readFile(path, "utf8");
    const frontmatter = text.match(/^---\r?\n([\s\S]*?)\r?\n---(?:\r?\n|$)/);
    if (!frontmatter) throw new Error(`Missing skill frontmatter: ${path}`);
    const meta = parse(frontmatter[1]);
    if (!meta || typeof meta.name !== "string" || typeof meta.description !== "string") throw new Error(`Invalid skill metadata: ${path}`);
    const skill = Skill.Info.make({ id: Skill.ID.make(meta.name), name: Skill.Name.make(meta.name), description: meta.description,
      path: Skill.Info.fields.path.make(path), content: text.slice(frontmatter[0].length) });
    await ctx.skill.transform(editor => editor.add(skill));
    // Custom registrations own their tool mode; reloading cannot change it.
    if (existingServer || !compactTools) return;
    // Reading catalogs during setup would wait on our own bootstrap.
    const checkPolicy = async () => {
      if (disposed) return;
      const agents = (await ctx.agent.list()).data;
      const partial = agents.some(agent => agent.permissions.some(({ action }) => {
        const regex = new RegExp(`^${action.replace(/[.+^${}()|[\]\\]/g, "\\$&").replaceAll("*", ".*").replaceAll("?", ".")}$`);
        return tools.some(tool => regex.test(tool)) && !(regex.test("computer_use_dispatch") && tools.every(tool => regex.test(tool)));
      }));
      const next = !partial;
      if (disposed || compact === next) return;
      compact = next;
      if (!compact) console.warn("Computer Use MCP uses direct tools to preserve per-tool permissions");
      await ctx.mcp.reload();
    };
    const policyReady = Promise.resolve().then(checkPolicy);
    void policyReady.catch(error => console.error("Computer Use MCP permission initialization failed", error));
    await ctx.session.hook("prompt", async () => { await policyReady; await checkPolicy(); });
    return () => { disposed = true; };
  },
});
