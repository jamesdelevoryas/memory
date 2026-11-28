#!/bin/bash
# One run of a memory agent (the dreamer, or the nightly consolidator): Claude
# Code, headless, with no built-in tools (no shell, no files, no web) and only
# the memory server's tools, following the prompt file it's given. Installed
# as /usr/local/bin/memory-dreamer.
#
# Usage: memory-dreamer [prompt-file]   (default: the dreamer's prompt)
# Environment: MEMORY_URL, MEMORY_TOKEN, CLAUDE_CODE_OAUTH_TOKEN, DREAMER_MODEL
set -euo pipefail
umask 077
prompt=${1:-/usr/local/share/memory/dreamer.md}
mcp=$(mktemp)
trap 'rm -f "$mcp"' EXIT
printf '{"mcpServers":{"memory":{"type":"http","url":"%s/mcp","headers":{"Authorization":"Bearer %s"}}}}\n' \
  "$MEMORY_URL" "$MEMORY_TOKEN" >"$mcp"
"$HOME/.local/bin/claude" -p --model "${DREAMER_MODEL:-sonnet}" --tools "" \
  --strict-mcp-config --mcp-config "$mcp" --allowedTools mcp__memory \
  --no-session-persistence <"$prompt"
