#!/usr/bin/env bash
# Adds the change-request control-plane to a box that has already run
# infra/auth/setup.sh -- same assumption infra/db/setup.sh makes, and the
# same reason: nginx, its cert, and the base auth.boxcode.sh vhost already
# exist, and this only adds its own systemd service plus its own nginx
# routes. Does not touch certbot, and deliberately does NOT touch
# /etc/nginx/conf.d/auth.conf itself -- see infra/db/setup.sh's own header
# for why overwriting that file from a template is the mistake to avoid.
# Instead this writes its routes into /etc/nginx/conf.d/auth-projects/, the
# directory the base vhost already `include`s.
#
# Idempotent, same as infra/auth/setup.sh and infra/db/setup.sh -- safe to
# re-run after any infra/requests/ change.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR=/opt/boxcode-requests

if [ ! -f /etc/nginx/conf.d/auth.conf ]; then
    echo "no /etc/nginx/conf.d/auth.conf found -- run infra/auth/setup.sh on this box first." >&2
    exit 1
fi

echo "== nginx routes for /requests-widget.js, /requests, /requests/*/resolve =="
sudo mkdir -p /etc/nginx/conf.d/auth-projects
cat << 'EOF' | sudo tee /etc/nginx/conf.d/auth-projects/_requests-route.conf >/dev/null
location = /requests-widget.js {
    proxy_pass http://127.0.0.1:8082/requests-widget.js;
    proxy_set_header Host $host;
}
location = /requests {
    proxy_pass http://127.0.0.1:8082/requests;
    proxy_set_header Host $host;
}
location ~ ^/requests/[^/]+/resolve$ {
    proxy_pass http://127.0.0.1:8082;
    proxy_set_header Host $host;
}
EOF
sudo nginx -t
sudo systemctl reload nginx

echo "== requests control-plane service =="
sudo mkdir -p "$INSTALL_DIR/control-plane"
sudo cp "$SCRIPT_DIR/control-plane/index.mjs" "$INSTALL_DIR/control-plane/index.mjs"
sudo cp "$SCRIPT_DIR/control-plane/boxcode-requests-control-plane.service" \
    /etc/systemd/system/boxcode-requests-control-plane.service
sudo systemctl daemon-reload
sudo systemctl enable --now boxcode-requests-control-plane
sudo systemctl restart boxcode-requests-control-plane

echo "== done =="
sleep 1
sudo systemctl --no-pager status boxcode-requests-control-plane
