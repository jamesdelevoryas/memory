#!/bin/bash
# Build on the VM and (re)install. Usage: deploy/deploy.sh [host]
set -euo pipefail
. "$(dirname "$0")/lib.sh"
HOST=${1:-$HOST}
cd "$(dirname "$0")/.."
ssh "$HOST" mkdir -p /root/src/memory
rsync -az --delete --exclude target --exclude .git --exclude '*.db*' --exclude /tokens --exclude /deploy/config ./ "$HOST:/root/src/memory/"
ssh "$HOST" bash -se -- "$MEMORY_DOMAIN" <<'REMOTE'
set -euo pipefail
DOMAIN=$1
cd /root/src/memory
~/.cargo/bin/cargo build --release --locked
install -m 0755 target/release/memory /usr/local/bin/memory
install -m 0755 deploy/backup.sh /usr/local/bin/memory-backup
install -m 0644 deploy/memory-backup.service deploy/memory-backup.timer /etc/systemd/system/
# The domain lives in the untracked deploy/config, not in the repo.
sed "s/@DOMAIN@/$DOMAIN/g" deploy/memory.service > /etc/systemd/system/memory.service
sed "s/@DOMAIN@/$DOMAIN/g" deploy/Caddyfile > /etc/caddy/Caddyfile
touch /etc/memory/tokens && chown root:memory /etc/memory/tokens && chmod 0640 /etc/memory/tokens
# The dreamer: its own user, with Claude Code installed in its home.
id dreamer >/dev/null 2>&1 || useradd --system --home-dir /var/lib/memory-dreamer --shell /usr/sbin/nologin dreamer
install -d -o dreamer -g dreamer -m 0700 /var/lib/memory-dreamer
install -d -o root -g dreamer -m 0750 /etc/memory-dreamer
[ -x /var/lib/memory-dreamer/.local/bin/claude ] ||
  sudo -u dreamer -H bash -c 'curl -fsSL https://claude.ai/install.sh | bash' >/dev/null
install -m 0755 deploy/dreamer.sh /usr/local/bin/memory-dreamer
install -D -m 0644 dreamer.md /usr/local/share/memory/dreamer.md
install -m 0644 deploy/memory-dreamer.service deploy/memory-dreamer.timer deploy/memory-dreamer.path /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now memory memory-backup.timer
systemctl restart memory
systemctl reload caddy
if [ -f /etc/memory-dreamer/env ]; then
  systemctl enable --now memory-dreamer.timer memory-dreamer.path
else
  echo "The dreamer isn't set up yet: run deploy/dreamer-setup.sh"
fi
systemctl --no-pager --lines=5 status memory
REMOTE
