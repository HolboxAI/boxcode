#!/usr/bin/env bash
# Bootstraps a fresh Amazon Linux 2023 box into the auth control-plane host:
# Docker (for per-project GoTrue containers), Postgres (one shared instance,
# one database per project), nginx + a Let's Encrypt cert for auth.boxcode.sh
# (routes auth.boxcode.sh/<id>/* to the right container), and the
# control-plane service itself under systemd.
#
# A real, owned domain is not optional here, and auth.boxcode.sh's DNS A
# record has to already point at this box's public IP before this script
# reaches the certbot step, or that step fails. An earlier version of this
# script tried to sidestep needing one by using the EC2 instance's own
# AWS-assigned hostname (ec2-x-x-x-x.compute-1.amazonaws.com) -- Let's
# Encrypt refuses to issue for `*.compute.amazonaws.com` outright
# ("forbidden by policy"), specifically because anyone can obtain a
# matching hostname for free, so it does not count as proof of ownership.
# AWS Certificate Manager has the identical ownership requirement and, on
# top of that, cannot hand a plain nginx process its private key at all
# (only ALB/CloudFront/API Gateway can use an ACM cert directly) -- so it
# is not a way around this either. There genuinely is no substitute for an
# owned domain.
#
# Idempotent -- every step here is safe to run again on a box that already
# has some or all of this, which is the point: this is meant to be re-run
# after `infra/auth/` changes, not just once at instance creation.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR=/opt/boxcode-auth
DOMAIN=auth.boxcode.sh

echo "== packages =="
sudo dnf install -y docker postgresql15-server postgresql15 nginx nodejs certbot python3-certbot-nginx bind-utils git >/dev/null

echo "== docker =="
sudo systemctl enable --now docker

echo "== postgres =="
# initdb refuses to run against an already-initialized data directory, which
# is exactly the idempotency check this needs -- no separate "already done"
# flag to maintain and drift out of sync with reality.
if [ ! -f /var/lib/pgsql/data/PG_VERSION ]; then
    sudo postgresql-setup --initdb
fi
sudo systemctl enable --now postgresql
# Peer auth for the local `postgres` OS user only -- the control-plane
# service always connects as `sudo -u postgres`, never over the network, so
# there is no password to manage or leak. Widening this to accept network
# connections is the first thing to change if Postgres ever needs to be
# reached from anywhere but this box.
echo "local all postgres peer" | sudo tee /var/lib/pgsql/data/pg_hba.conf >/dev/null
sudo systemctl restart postgresql

echo "== nginx =="
sudo mkdir -p /etc/nginx/conf.d/auth-projects
sudo cp "$SCRIPT_DIR/nginx/auth.conf.template" /etc/nginx/conf.d/auth.conf
sudo nginx -t
sudo systemctl enable --now nginx
sudo systemctl reload nginx

echo "== DNS check =="
# certbot's HTTP-01 challenge fails opaquely if this is not already true, so
# check it here with a clear message rather than letting certbot's own error
# be the first anyone hears about it.
RESOLVED=$(dig +short "$DOMAIN" | tail -1)
THIS_IP=$(curl -s http://169.254.169.254/latest/meta-data/public-ipv4 \
    -H "X-aws-ec2-metadata-token: $(curl -s -X PUT http://169.254.169.254/latest/api/token -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')")
if [ "$RESOLVED" != "$THIS_IP" ]; then
    echo "$DOMAIN resolves to '$RESOLVED', not this box's IP ($THIS_IP)." >&2
    echo "Add/update the A record at whoever manages boxcode.sh's DNS before re-running this." >&2
    exit 1
fi

echo "== TLS (Let's Encrypt, for $DOMAIN) =="
# --redirect adds the plain-80-to-443 redirect and the whole HTTPS server
# block to /etc/nginx/conf.d/auth.conf itself -- run once per box, safe to
# re-run (certbot renews in place rather than erroring on an existing cert).
sudo certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos \
    --register-unsafely-without-email --redirect

echo "== control-plane service =="
sudo mkdir -p "$INSTALL_DIR/control-plane"
sudo cp "$SCRIPT_DIR/control-plane/index.mjs" "$INSTALL_DIR/control-plane/index.mjs"
sudo cp "$SCRIPT_DIR/control-plane/boxcode-auth-control-plane.service" \
    /etc/systemd/system/boxcode-auth-control-plane.service
sudo systemctl daemon-reload
sudo systemctl enable --now boxcode-auth-control-plane
sudo systemctl restart boxcode-auth-control-plane

echo "== done =="
sleep 1
sudo systemctl --no-pager status boxcode-auth-control-plane
