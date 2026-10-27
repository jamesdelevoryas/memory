#!/bin/bash
# Claude Code hook that keeps the memory server's copy of your sessions current.
#   Stop:          claude-code-hook.sh stop    uploads this session's new turns
#   SessionStart:  claude-code-hook.sh start   catches up recent sessions, then asks for a dreamer run
# Needs MEMORY_URL (set it in settings.json's "env") and the `memory` binary on PATH.
# The token comes from MEMORY_TOKEN, or else the macOS keychain entry deploy/token.sh saved.
set -euo pipefail
export MEMORY_URL=${MEMORY_URL:?set MEMORY_URL to the memory server, e.g. https://memory.example.com}
if [ -z "${MEMORY_TOKEN:-}" ]; then
  MEMORY_TOKEN=$(security find-generic-password -s memory-mcp-token -a claude-code -w)
  export MEMORY_TOKEN
fi
case ${1:-} in
  stop) exec memory upload claude-code ;;
  start)
    memory upload claude-code --recent
    # Fails harmlessly until the server has a dreamer.
    memory dream 2>/dev/null || true
    ;;
  *) echo "usage: $0 stop|start" >&2; exit 64 ;;
esac
