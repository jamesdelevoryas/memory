#!/bin/bash
# One-time setup for the nightly consolidator on the server (after
# dreamer-setup.sh): it runs as the dreamer's user with the same Claude Code
# login, but under its own memory token (source "consolidator", consolidate,
# private), minted here unless the keychain has one.
#
# Usage: deploy/consolidator-setup.sh
set -euo pipefail
. "$(dirname "$0")/lib.sh"
if ! memory_token=$(security find-generic-password -s memory-mcp-token -a consolidator -w 2>/dev/null); then
  "$(dirname "$0")/token.sh" consolidator consolidate private
  memory_token=$(security find-generic-password -s memory-mcp-token -a consolidator -w)
fi
printf 'MEMORY_URL=https://%s\nMEMORY_TOKEN=%s\n' "$MEMORY_DOMAIN" "$memory_token" |
  ssh "$HOST" 'set -e
    [ -f /etc/memory-dreamer/env ] || { echo "set up the dreamer first: deploy/dreamer-setup.sh" >&2; exit 1; }
    install -d -o root -g dreamer -m 0750 /etc/memory-consolidator
    { cat; grep "^CLAUDE_CODE_OAUTH_TOKEN=" /etc/memory-dreamer/env; } |
      install -m 0640 -o root -g dreamer /dev/stdin /etc/memory-consolidator/env
    systemctl enable --now memory-consolidator.timer
    systemctl list-timers --no-pager memory-consolidator.timer
    echo "consolidator enabled; run it now with: systemctl start memory-consolidator"'
