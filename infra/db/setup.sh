#!/usr/bin/env bash
# Adds the db control-plane to a box that has already run
# infra/auth/setup.sh -- this assumes nginx, its cert, and the base
# auth.boxcode.sh vhost already exist, and only adds this feature's own
# systemd service plus one new nginx route for it. Does not touch certbot,
# and deliberately does NOT touch /etc/nginx/conf.d/auth.conf itself:
# once certbot has run on this box, that file has its own TLS server block
# edited into it in place, and overwriting it from the base template (an
# earlier version of this script did exactly that) would silently destroy
# that edit. Instead this writes its route into
# /etc/nginx/conf.d/auth-projects/ -- the directory the base vhost already
# `include`s for per-project auth routes -- which certbot never touches at
# all, so there is nothing here that can go stale relative to it.
#
# Idempotent, same as infra/auth/setup.sh -- safe to re-run after any
# infra/db/ change.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR=/opt/boxcode-db
NODE_VERSION=v24.19.0
NODE_DIR=/opt/node24

if [ ! -f /etc/nginx/conf.d/auth.conf ]; then
    echo "no /etc/nginx/conf.d/auth.conf found -- run infra/auth/setup.sh on this box first." >&2
    exit 1
fi

echo "== node $NODE_VERSION (dedicated, not the system node) =="
# infra/auth/setup.sh's `dnf install nodejs` gets whatever AL2023's default
# repo carries -- confirmed live to be v18.20.8, which has no node:sqlite
# at all ("No such built-in module: node:sqlite"; the module wasn't added
# until v22.5), and `dnf module list nodejs` has no stream to switch to on
# this box either. Rather than upgrading the system node (which the auth
# control-plane already depends on and works fine with, on v18 -- no
# reason to put that at risk for a need only this service has), this pulls
# a real prebuilt tarball from nodejs.org into its own directory and
# points only this service's systemd unit at it.
if [ ! -x "$NODE_DIR/bin/node" ] || [ "$("$NODE_DIR/bin/node" --version)" != "$NODE_VERSION" ]; then
    TARBALL="node-$NODE_VERSION-linux-x64.tar.xz"
    curl -fsSLO "https://nodejs.org/dist/$NODE_VERSION/$TARBALL"
    sudo rm -rf "$NODE_DIR"
    sudo mkdir -p "$NODE_DIR"
    sudo tar -xJf "$TARBALL" -C "$NODE_DIR" --strip-components=1
    rm -f "$TARBALL"
fi
"$NODE_DIR/bin/node" --version

echo "== nginx routes for /db/query and /db/named-query =="
sudo mkdir -p /etc/nginx/conf.d/auth-projects
cat << 'EOF' | sudo tee /etc/nginx/conf.d/auth-projects/_db-route.conf >/dev/null
location = /db/query {
    proxy_pass http://127.0.0.1:8081/query;
    proxy_set_header Host $host;
}
location = /db/named-query {
    proxy_pass http://127.0.0.1:8081/named-query;
    proxy_set_header Host $host;
}
EOF
sudo nginx -t
sudo systemctl reload nginx

echo "== db control-plane service =="
sudo mkdir -p "$INSTALL_DIR/control-plane" "$INSTALL_DIR/data"
sudo cp "$SCRIPT_DIR/control-plane/index.mjs" "$INSTALL_DIR/control-plane/index.mjs"
# worker.mjs is not optional: index.mjs spawns it by path on startup, and a
# deploy that copied only index.mjs would leave the service crash-looping on
# a missing module. Copied in the same step for exactly that reason.
sudo cp "$SCRIPT_DIR/control-plane/worker.mjs" "$INSTALL_DIR/control-plane/worker.mjs"
sudo cp "$SCRIPT_DIR/control-plane/boxcode-db-control-plane.service" \
    /etc/systemd/system/boxcode-db-control-plane.service
sudo systemctl daemon-reload
sudo systemctl enable --now boxcode-db-control-plane
sudo systemctl restart boxcode-db-control-plane

echo "== nightly backup =="
# sqlite3's own .backup takes a consistent snapshot of a database that is
# being written to; a plain file copy of one with a live journal restores
# corrupt. See backup.sh.
if ! command -v sqlite3 >/dev/null 2>&1; then
    sudo dnf install -y sqlite >/dev/null
fi
sudo install -m 0755 "$SCRIPT_DIR/backup.sh" "$INSTALL_DIR/backup.sh"
sudo tee /etc/systemd/system/boxcode-db-backup.service >/dev/null <<EOF
[Unit]
Description=boxcode db nightly backup to S3

[Service]
Type=oneshot
Environment=DATA_DIR=$INSTALL_DIR/data
ExecStart=$INSTALL_DIR/backup.sh
EOF
sudo tee /etc/systemd/system/boxcode-db-backup.timer >/dev/null <<'EOF'
[Unit]
Description=Run the boxcode db backup nightly

[Timer]
OnCalendar=daily
# The box may be asleep or the unit may have been added after today's run;
# without this the first backup would wait until tomorrow.
Persistent=true

[Install]
WantedBy=timers.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now boxcode-db-backup.timer

echo "== done =="
sleep 1
sudo systemctl --no-pager status boxcode-db-control-plane
