import { fileURLToPath } from "node:url";

const binary = fileURLToPath(new URL("../vendor/computer-use-mcp", import.meta.url));
const skills = fileURLToPath(new URL("../skills", import.meta.url));

export default {
  id: "@mirsella/opencode-computer-use-mcp",
  server: async () => ({
    config(config) {
      config.mcp ??= {};
      config.mcp.computer_use ??= {
        type: "local",
        command: [binary, "mcp"],
        enabled: true,
        timeout: 90_000,
      };

      config.skills ??= {};
      config.skills.paths ??= [];
      if (!config.skills.paths.includes(skills)) config.skills.paths.push(skills);
    },
  }),
};
