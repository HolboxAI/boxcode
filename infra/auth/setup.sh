#!/usr/bin/env bash
# Bootstraps a fresh Amazon Linux 2023 box into the auth control-plane host:
# Docker (for per-project GoTrue containers), Postgres (one shared instance,
# one database per project), nginx + a Let's Encrypt cert for the box's own
# AWS-assigned public hostname (routes <hostname>/<id>/* to the right
# container), and the control-plane service itself under systemd.
#
# No custom domain, deliberately: the published page needs an HTTPS origin
# to call, and EC2 already hands out a real, publicly-resolvable hostname
# for free the moment the instance launches -- Let's Encrypt only needs
# that, not a registrar-managed subdomain. If this box gets a nicer domain
# later, that is a DNS change plus a `certbot --nginx -d <domain>` re-run,
# not a change to anything in this script.
#
# Idempotent -- every step here is safe to run again on a box that already
# has some or all of this, which is the point: this is meant to be re-run
# after `infra/auth/` changes, not just once at instance creation.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALL_DIR=/opt/boxcode-auth

echo "== packages =="
sudo dnf install -y docker postgresql15-server postgresql15 nginx nodejs certbot python3-certbot-nginx >/dev/null

echo "== this box's own public hostname =="
# IMDSv2: a token is required before the metadata service answers anything,
# so a plain unauthenticated GET (IMDSv1) is not an option here even for a
# read this harmless.
IMDS_TOKEN=$(curl -s -X PUT "http://169.254.169.254/latest/api/token" \
    -H "X-aws-ec2-metadata-token-ttl-seconds: 60")
HOSTNAME_PUBLIC=$(curl -s -H "X-aws-ec2-metadata-token: $IMDS_TOKEN" \
    "http://169.254.169.254/latest/meta-data/public-hostname")
if [ -z "$HOSTNAME_PUBLIC" ]; then
    echo "could not determine this instance's public hostname from IMDS -- is it in a public subnet with a public IP?" >&2
    exit 1
fi
echo "$HOSTNAME_PUBLIC"

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
sed "s/__HOSTNAME__/$HOSTNAME_PUBLIC/" "$SCRIPT_DIR/nginx/auth.conf.template" | sudo tee /etc/nginx/conf.d/auth.conf >/dev/null
sudo nginx -t
sudo systemctl enable --now nginx
sudo systemctl reload nginx

echo "== TLS (Let's Encrypt, for $HOSTNAME_PUBLIC) =="
# --redirect adds the plain-80-to-443 redirect and the whole HTTPS server
# block to /etc/nginx/conf.d/auth.conf itself -- run once per box, safe to
# re-run (certbot renews in place rather than erroring on an existing cert).
sudo certbot --nginx -d "$HOSTNAME_PUBLIC" --non-interactive --agree-tos \
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
