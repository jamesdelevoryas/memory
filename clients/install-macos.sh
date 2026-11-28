#!/bin/bash
# Installs the client side on a Mac: the `memory` CLI, and a launchd agent that
# imports claude.ai data exports from ~/Downloads (clients/claude-ai-import.sh).
# Claude Code and Codex hooks are in the README.
#
# Usage: clients/install-macos.sh https://memory.example.com
set -euo pipefail
[ $# -eq 1 ] || { sed -n 's/^# \{0,1\}//; 2,6p' "$0" >&2; exit 2; }
url=$1
cd "$(dirname "$0")/.."
cargo install --quiet --locked --path .
install -d "$HOME/.local/bin" "$HOME/Library/LaunchAgents" "$HOME/Library/Logs"
install -m 0755 clients/claude-ai-import.sh "$HOME/.local/bin/memory-claude-ai-import"
label=memory.claude-ai-import
plist=$HOME/Library/LaunchAgents/$label.plist
cat >"$plist" <<PLIST
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>$label</string>
  <key>ProgramArguments</key>
  <array><string>$HOME/.local/bin/memory-claude-ai-import</string></array>
  <key>EnvironmentVariables</key>
  <dict><key>MEMORY_URL</key><string>$url</string></dict>
  <key>WatchPaths</key>
  <array><string>$HOME/Downloads</string></array>
  <key>RunAtLoad</key><true/>
  <key>StandardOutPath</key><string>$HOME/Library/Logs/memory-claude-ai-import.log</string>
  <key>StandardErrorPath</key><string>$HOME/Library/Logs/memory-claude-ai-import.log</string>
</dict>
</plist>
PLIST
launchctl bootout "gui/$(id -u)/$label" 2>/dev/null || true
launchctl bootstrap "gui/$(id -u)" "$plist"
echo "installed; log: ~/Library/Logs/memory-claude-ai-import.log"
