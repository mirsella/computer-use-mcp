import { fileURLToPath } from "node:url";

const binary = fileURLToPath(new URL("../vendor/bin/computer-use-mcp", import.meta.url));
const skills = fileURLToPath(new URL("../skills", import.meta.url));
const directTools = [
  "list_desktop", "launch_application", "activate_window", "observe", "act", "wait_for",
].map((name) => `computer_use_${name}`);

function hasPartialPolicy(policy) {
  if (!policy || typeof policy !== "object" || Array.isArray(policy)) return false;
  return Object.keys(policy).some((pattern) => {
    const expression = pattern.replace(/[.+^${}()|[\]\\]/g, "\\$&")
      .replaceAll("*", ".*").replaceAll("?", ".");
    const regex = new RegExp(`^${expression}$`);
    return directTools.some((tool) => regex.test(tool))
      && !(regex.test("computer_use_dispatch") && directTools.every((tool) => regex.test(tool)));
  });
}

function requiresDirectTools(config) {
  return [config, ...Object.values(config.agent ?? {})].some(
    (scope) => hasPartialPolicy(scope.permission) || hasPartialPolicy(scope.tools),
  );
}

export default {
  id: "@mirsella/opencode-computer-use-mcp",
  server: async (_input, options) => {
    const { compactTools = true } = options ?? {};
    if (typeof compactTools !== "boolean") throw new Error("compactTools must be a boolean");
    return {
      config(config) {
        config.mcp ??= {};
        if (!config.mcp.computer_use) {
          const compact = compactTools && !requiresDirectTools(config);
          if (compactTools && !compact) {
            console.warn("@mirsella/opencode-computer-use-mcp uses direct tools to preserve per-tool permissions");
          }
          config.mcp.computer_use = {
            type: "local",
            command: compact ? [binary, "mcp", "--compact-tools"] : [binary, "mcp"],
            enabled: true,
            timeout: 150_000,
          };
        }

        config.skills ??= {};
        config.skills.paths ??= [];
        if (!config.skills.paths.includes(skills)) config.skills.paths.push(skills);
      },
    };
  },
};
