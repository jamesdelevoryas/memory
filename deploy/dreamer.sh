#!/bin/bash
# One dreamer run, started by memory-dreamer.service: Claude Code, headless,
# with no built-in tools (no shell, no files, no web) and only the memory
# server's tools. Installed as /usr/local/bin/memory-dreamer.
set -euo pipefail
umask 077
mcp=$(mktemp)
trap 'rm -f "$mcp"' EXIT
printf '{"mcpServers":{"memory":{"type":"http","url":"%s/mcp","headers":{"Authorization":"Bearer %s"}}}}\n' \
  "$MEMORY_URL" "$MEMORY_TOKEN" >"$mcp"
"$HOME/.local/bin/claude" -p --model "${DREAMER_MODEL:-sonnet}" --tools "" \
  --strict-mcp-config --mcp-config "$mcp" --allowedTools mcp__memory \
  --no-session-persistence </usr/local/share/memory/dreamer.md
