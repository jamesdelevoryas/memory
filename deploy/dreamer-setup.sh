#!/bin/bash
# One-time setup for the dreamer on the server (after deploy.sh has run).
# It runs Claude Code on your subscription: first get a long-lived token with
# `claude setup-token`; this script asks for it. The dreamer's own memory
# token (consolidate) comes from the keychain, or is minted with token.sh.
#
# Usage: deploy/dreamer-setup.sh
set -euo pipefail
. "$(dirname "$0")/lib.sh"
read -rsp "Token from 'claude setup-token': " claude_token
echo
[ -n "$claude_token" ] || { echo "no token given" >&2; exit 1; }
if ! memory_token=$(security find-generic-password -s memory-mcp-token -a dreamer -w 2>/dev/null); then
  "$(dirname "$0")/token.sh" dreamer consolidate
  memory_token=$(security find-generic-password -s memory-mcp-token -a dreamer -w)
fi
printf 'MEMORY_URL=https://%s\nMEMORY_TOKEN=%s\nCLAUDE_CODE_OAUTH_TOKEN=%s\n' \
  "$MEMORY_DOMAIN" "$memory_token" "$claude_token" |
  ssh "$HOST" 'set -e
    install -m 0640 -o root -g dreamer /dev/stdin /etc/memory-dreamer/env
    systemctl enable --now memory-dreamer.timer memory-dreamer.path
    systemctl start --no-block memory-dreamer.service
    echo "dreamer enabled; watch it with: journalctl -fu memory-dreamer"'
