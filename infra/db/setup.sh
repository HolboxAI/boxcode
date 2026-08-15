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

if [ ! -f /etc/nginx/conf.d/auth.conf ]; then
    echo "no /etc/nginx/conf.d/auth.conf found -- run infra/auth/setup.sh on this box first." >&2
    exit 1
fi

echo "== nginx route for /db/query =="
sudo mkdir -p /etc/nginx/conf.d/auth-projects
cat << 'EOF' | sudo tee /etc/nginx/conf.d/auth-projects/_db-route.conf >/dev/null
location = /db/query {
    proxy_pass http://127.0.0.1:8081/query;
    proxy_set_header Host $host;
}
EOF
sudo nginx -t
sudo systemctl reload nginx

echo "== db control-plane service =="
sudo mkdir -p "$INSTALL_DIR/control-plane" "$INSTALL_DIR/data"
sudo cp "$SCRIPT_DIR/control-plane/index.mjs" "$INSTALL_DIR/control-plane/index.mjs"
sudo cp "$SCRIPT_DIR/control-plane/boxcode-db-control-plane.service" \
    /etc/systemd/system/boxcode-db-control-plane.service
sudo systemctl daemon-reload
sudo systemctl enable --now boxcode-db-control-plane
sudo systemctl restart boxcode-db-control-plane

echo "== done =="
sleep 1
sudo systemctl --no-pager status boxcode-db-control-plane
