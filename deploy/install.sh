#!/bin/sh
# Installs chriscord-server as a systemd service.
#
#   cargo build --release
#   sudo deploy/install.sh                      # fresh install
#   sudo deploy/install.sh /path/to/old/dir     # …and move your existing data
#
# Run it again after pulling and rebuilding to update the binary; it keeps
# your data and settings and restarts the service.
#
# What it does:
#   - creates a "chriscord" system user to run the server
#   - installs the binary to /usr/local/bin/chriscord-server
#   - uses /var/lib/chriscord for the database, uploads and images
#     (copying them from the directory you give, if any)
#   - installs the unit to /etc/systemd/system/chriscord.service and
#     optional settings to /etc/chriscord/chriscord.env
#   - enables and (re)starts the service, then prints the owner key
set -eu

DATA=/var/lib/chriscord
BIN=/usr/local/bin/chriscord-server
HERE=$(cd "$(dirname "$0")" && pwd)
REPO=$(dirname "$HERE")

[ "$(id -u)" -eq 0 ] || { echo "Run this with sudo." >&2; exit 1; }
[ -x "$REPO/target/release/chriscord-server" ] || {
  echo "Build it first:  cargo build --release" >&2; exit 1; }
command -v systemctl >/dev/null || { echo "systemd (systemctl) wasn't found on this machine." >&2; exit 1; }

# 1. The user the server runs as (no login, no home directory of its own).
if ! id chriscord >/dev/null 2>&1; then
  NOLOGIN=$(command -v nologin || echo /sbin/nologin)
  useradd --system --home-dir "$DATA" --no-create-home --shell "$NOLOGIN" chriscord
  echo "Created system user: chriscord"
fi

# 2. Stop it while files are replaced (fine if it isn't installed yet).
systemctl stop chriscord 2>/dev/null || true

# 3. The program.
install -m 0755 "$REPO/target/release/chriscord-server" "$BIN"
echo "Installed $BIN"

# 4. The data directory, and moving existing data into it.
install -d -m 0750 -o chriscord -g chriscord "$DATA"
if [ "${1:-}" != "" ]; then
  OLD=$1
  [ -f "$OLD/chriscord.db" ] || { echo "No chriscord.db in $OLD — nothing to move." >&2; exit 1; }
  if [ -f "$DATA/chriscord.db" ]; then
    echo "$DATA already has a database; not overwriting it with the one in $OLD." >&2; exit 1
  fi
  for f in chriscord.db chriscord.db-wal chriscord.db-shm uploads pfps server_assets; do
    [ -e "$OLD/$f" ] && cp -a "$OLD/$f" "$DATA/"
  done
  echo "Copied your data from $OLD (the originals are left where they were)."
fi
chown -R chriscord:chriscord "$DATA"

# 5. Settings file (kept if you already have one) and the unit.
install -d -m 0755 /etc/chriscord
if [ ! -f /etc/chriscord/chriscord.env ]; then
  install -m 0640 -g chriscord "$HERE/chriscord.env.example" /etc/chriscord/chriscord.env
fi
install -m 0644 "$HERE/chriscord.service" /etc/systemd/system/chriscord.service
install -d /usr/local/share/doc/chriscord
[ -f "$HERE/README.txt" ] && install -m 0644 "$HERE/README.txt" /usr/local/share/doc/chriscord/README.txt

# 6. Start it, now and at every boot.
systemctl daemon-reload
systemctl enable --now chriscord
sleep 1
systemctl --no-pager --lines=0 status chriscord || true

echo
echo "Owner key (for the admin panel at http://<this machine>:7070/admin):"
sudo -u chriscord "$BIN" --data-dir "$DATA" owner-key 2>/dev/null \
  || runuser -u chriscord -- "$BIN" --data-dir "$DATA" owner-key 2>/dev/null
echo
echo "See it again any time:  sudo -u chriscord $BIN --data-dir $DATA owner-key"
