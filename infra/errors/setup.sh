#!/usr/bin/env bash
# Adds the runtime-error control-plane to a box that has already run
# infra/auth/setup.sh -- same assumption infra/requests/setup.sh makes, and
# the same reason: nginx, its cert, and the base auth.boxcode.sh vhost
# already exist, and this only adds its own systemd service plus its own
# nginx routes. Does not touch certbot, and deliberately does NOT touch
# /etc/nginx/conf.d/auth.conf itself -- see infra/db/setup.sh's own header
# for why overwriting that file from a template is the mistake to avoid.
# Instead this writes its routes into /etc/nginx/conf.d/auth-projects/, the
# directory the base vhost already `include`s.
#
# Idempotent, same as the other control-planes' setup.sh -- safe to re-run
# after any infra/errors/ change.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR=/opt/boxcode-errors

if [ ! -f /etc/nginx/conf.d/auth.conf ]; then
    echo "no /etc/nginx/conf.d/auth.conf found -- run infra/auth/setup.sh on this box first." >&2
    exit 1
fi

echo "== nginx routes for /errors-beacon.js, /errors, /errors/*/resolve =="
sudo mkdir -p /etc/nginx/conf.d/auth-projects
cat << 'EOF' | sudo tee /etc/nginx/conf.d/auth-projects/_errors-route.conf >/dev/null
location = /errors-beacon.js {
    proxy_pass http://127.0.0.1:8083/errors-beacon.js;
    proxy_set_header Host $host;
}
location = /errors {
    proxy_pass http://127.0.0.1:8083/errors;
    proxy_set_header Host $host;
}
location ~ ^/errors/[^/]+/resolve$ {
    proxy_pass http://127.0.0.1:8083;
    proxy_set_header Host $host;
}
EOF
sudo nginx -t
sudo systemctl reload nginx

echo "== errors control-plane service =="
sudo mkdir -p "$INSTALL_DIR/control-plane"
sudo cp "$SCRIPT_DIR/control-plane/index.mjs" "$INSTALL_DIR/control-plane/index.mjs"
sudo cp "$SCRIPT_DIR/control-plane/boxcode-errors-control-plane.service" \
    /etc/systemd/system/boxcode-errors-control-plane.service
sudo systemctl daemon-reload
sudo systemctl enable --now boxcode-errors-control-plane
sudo systemctl restart boxcode-errors-control-plane

echo "== done =="
sleep 1
sudo systemctl --no-pager status boxcode-errors-control-plane
