#!/bin/bash
# Pre-register an OAuth client (for apps like Gemini that ask for a client ID
# and secret) on the production server, and store its secret in the macOS
# keychain (service "memory-oauth-secret", account = client ID). The server
# keeps only the secret's hash. With no redirect URIs any https one is
# accepted; pass the app's to pin them. Re-running replaces the client.
#
# Usage: deploy/oauth-client.sh <client-id> [redirect-uri...]
# Read it back: security find-generic-password -s memory-oauth-secret -a <client-id> -w
set -euo pipefail
. "$(dirname "$0")/lib.sh"
[ $# -ge 1 ] || { sed -n 's/^# \{0,1\}//; 2,9p' "$0" >&2; exit 2; }
id=$1

secret=$(ssh "$HOST" bash -s -- "$@" <<'REMOTE'
set -euo pipefail
err=$(mktemp); trap 'rm -f $err' EXIT
line=$(memory oauth-client "$@" 2>"$err") || { cat "$err" >&2; exit 1; }
f=/etc/memory/oauth-clients
touch $f && chown root:memory $f && chmod 0640 $f
{ grep -v "^$1 " $f || true; echo "$line"; } > $f.new
cat $f.new > $f && rm $f.new
systemctl reload memory
sed -n 2p "$err"
REMOTE
)
security add-generic-password -U -s memory-oauth-secret -a "$id" -w "$secret"
echo "registered OAuth client $id; secret stored in keychain (memory-oauth-secret / $id)" >&2
