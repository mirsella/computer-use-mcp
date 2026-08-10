#!/usr/bin/env bash
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
packaged_skill="$root/crates/computer-use-mcp/guidance/skill.md"
workspace_skill="$root/.agents/skills/computer-use-mcp/SKILL.md"

cmp -- "$packaged_skill" "$workspace_skill"
