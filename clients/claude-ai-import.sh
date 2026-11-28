#!/bin/bash
# Imports claude.ai data exports as they land in ~/Downloads, so memory has an
# exact copy of claude.ai conversations even when the model skipped
# memory_log_turn. Request an export in claude.ai (Settings → Privacy → Export
# data) now and then; download the zip from the email and this does the rest.
# Re-importing skips what the server already has, and each zip is imported once.
#
# Run by launchd when ~/Downloads changes (clients/install-macos.sh sets it up).
# Needs MEMORY_URL, and the claude-ai-export token in the keychain
# (deploy/token.sh claude-ai-export add).
set -euo pipefail
downloads=${1:-$HOME/Downloads}
state=${XDG_STATE_HOME:-$HOME/.local/state}/memory/claude-ai-imported
memory=${MEMORY_BIN:-$HOME/.cargo/bin/memory}
mkdir -p "$(dirname "$state")"
touch "$state"

shopt -s nullglob
for zip in "$downloads"/data-*.zip; do
  id="$(basename "$zip") $(stat -f %z "$zip")"
  grep -qxF "$id" "$state" && continue
  # A claude.ai export holds conversations.json (other exports don't).
  unzip -l "$zip" conversations.json >/dev/null 2>&1 || { echo "$id" >>"$state"; continue; }
  tmp=$(mktemp -d)
  trap 'rm -rf "$tmp"' EXIT
  unzip -p "$zip" conversations.json >"$tmp/conversations.json"
  MEMORY_TOKEN=$(security find-generic-password -s memory-mcp-token -a claude-ai-export -w) \
    "$memory" import claude-ai "$tmp/conversations.json"
  rm -rf "$tmp"
  echo "$id" >>"$state"
  echo "imported $zip"
done
