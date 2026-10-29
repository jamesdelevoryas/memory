#!/bin/bash
# Build on the VM and (re)install. Usage: deploy/deploy.sh [host]
set -euo pipefail
. "$(dirname "$0")/lib.sh"
HOST=${1:-$HOST}
cd "$(dirname "$0")/.."
ssh "$HOST" mkdir -p /root/src/memory
rsync -az --delete --exclude target --exclude .git --exclude '*.db*' --exclude /tokens --exclude /deploy/config ./ "$HOST:/root/src/memory/"
ssh "$HOST" bash -se -- "$MEMORY_DOMAIN" "$MEMORY_PRIVATE_IP" <<'REMOTE'
set -euo pipefail
DOMAIN=$1
PRIVATE_IP=$2
cd /root/src/memory
~/.cargo/bin/cargo build --release --locked
install -m 0755 target/release/memory /usr/local/bin/memory
install -m 0755 deploy/backup.sh /usr/local/bin/memory-backup
install -m 0644 deploy/memory-backup.service deploy/memory-backup.timer /etc/systemd/system/
# The domain and private IP live in the untracked deploy/config, not in the repo.
sed "s/@DOMAIN@/$DOMAIN/g; s/@PRIVATE_IP@/$PRIVATE_IP/g" deploy/memory.service > /etc/systemd/system/memory.service
# The edge terminates TLS for the domain and proxies here over the private
# network.
ufw allow from 10.0.0.0/16 to any port 8750 proto tcp >/dev/null
touch /etc/memory/tokens && chown root:memory /etc/memory/tokens && chmod 0640 /etc/memory/tokens
systemctl daemon-reload
systemctl enable --now memory memory-backup.timer
systemctl restart memory
systemctl --no-pager --lines=5 status memory
REMOTE
