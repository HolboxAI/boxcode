#!/usr/bin/env bash
# Adds the uploads control-plane to a box that has already run
# infra/auth/setup.sh -- same assumption infra/db/setup.sh and
# infra/requests/setup.sh make, and the same reason: nginx, its cert, and
# the base auth.boxcode.sh vhost already exist, and this only adds its own
# systemd service plus its own nginx route. Does not touch certbot, and
# deliberately does NOT touch /etc/nginx/conf.d/auth.conf itself -- see
# infra/db/setup.sh's own header for why overwriting that file from a
# template is the mistake to avoid.
#
# Unlike infra/requests, this endpoint is not reached through
# auth.boxcode.sh at all in the end -- the presigned URLs it hands back
# point straight at S3, and the *uploaded image itself* is served publicly
# through boxcode.sh's own CloudFront distribution (a new /uploads/*
# behavior added there, and a matching bucket-policy statement -- both
# outside this repo's own infra, done by hand against the account, see
# infra/uploads/README.md). What runs on this box only ever signs; nginx's
# route here is for the signing call itself
# (POST auth.boxcode.sh/uploads), same shape as db/requests.
#
# Idempotent, same as the other infra/*/setup.sh scripts -- safe to re-run
# after any infra/uploads/ change.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR=/opt/boxcode-uploads

if [ ! -f /etc/nginx/conf.d/auth.conf ]; then
    echo "no /etc/nginx/conf.d/auth.conf found -- run infra/auth/setup.sh on this box first." >&2
    exit 1
fi

echo "== nginx route for /uploads =="
sudo mkdir -p /etc/nginx/conf.d/auth-projects
cat << 'EOF' | sudo tee /etc/nginx/conf.d/auth-projects/_uploads-route.conf >/dev/null
location = /uploads {
    proxy_pass http://127.0.0.1:8083/uploads;
    proxy_set_header Host $host;
}
EOF
sudo nginx -t
sudo systemctl reload nginx

echo "== uploads control-plane service =="
sudo mkdir -p "$INSTALL_DIR/control-plane"
sudo cp "$SCRIPT_DIR/control-plane/index.mjs" "$INSTALL_DIR/control-plane/index.mjs"
sudo cp "$SCRIPT_DIR/control-plane/boxcode-uploads-control-plane.service" \
    /etc/systemd/system/boxcode-uploads-control-plane.service
sudo systemctl daemon-reload
sudo systemctl enable --now boxcode-uploads-control-plane
sudo systemctl restart boxcode-uploads-control-plane

echo "== done =="
sleep 1
sudo systemctl --no-pager status boxcode-uploads-control-plane
