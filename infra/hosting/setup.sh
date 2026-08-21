#!/usr/bin/env bash
# Provisions the runner box: nginx, Postgres, and the per-app plumbing that
# hosted backends run under.
#
# One dedicated t3.medium. No Docker, no container runtime, no orchestrator --
# apps run as ordinary systemd services, which is both the simplest thing that
# works and, because of what systemd can lock a service out of, the thing that
# gives each tenant a real sandbox. See runtime/unit.mjs.
#
# THIS BOX RUNS CODE STRANGERS UPLOADED. It shares nothing with boxcode-auth,
# which runs its control-plane as root with Postgres on `trust`. That is fine
# there, where every process is a trusted third-party image, and would be
# indefensible here. Do not carry that box's posture across.
#
# Idempotent -- meant to be re-run after any infra/hosting change.
#
# SKIP_TLS=1 does everything except the DNS check and certbot, so the rest can
# be proven over plain HTTP before apps.boxcode.sh has an A record.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SKIP_TLS="${SKIP_TLS:-0}"
APPS_DIR=/opt/boxcode-apps
DOMAIN=apps.boxcode.sh

echo "== packages =="
sudo dnf install -y nginx postgresql15-server postgresql15 certbot \
    python3-certbot-nginx bind-utils jq unzip git tar xz >/dev/null

echo "== node =="
# AL2023's default nodejs is v18, and this platform hosts apps written against
# current Node. Same approach infra/db/setup.sh takes: a real tarball in its own
# directory rather than fighting the distro's module streams.
NODE_VERSION=v22.20.0
NODE_DIR=/opt/node22
if [ ! -x "$NODE_DIR/bin/node" ] || [ "$("$NODE_DIR/bin/node" --version)" != "$NODE_VERSION" ]; then
    TARBALL="node-$NODE_VERSION-linux-x64.tar.xz"
    curl -fsSLO "https://nodejs.org/dist/$NODE_VERSION/$TARBALL"
    sudo rm -rf "$NODE_DIR" && sudo mkdir -p "$NODE_DIR"
    sudo tar -xJf "$TARBALL" -C "$NODE_DIR" --strip-components=1
    rm -f "$TARBALL"
fi
"$NODE_DIR/bin/node" --version

echo "== app directories and the shared group =="
# Every app's unix user is in this group; nothing else is. It exists so nginx
# can be granted what it needs without any app being able to reach another's
# files -- the directories below are 0750 and owned per-app.
getent group bcapp >/dev/null || sudo groupadd --system bcapp
sudo mkdir -p "$APPS_DIR"
sudo chmod 0755 "$APPS_DIR"

echo "== postgres =="
# The reason this box exists rather than a Lambda: a real wire protocol, so
# Prisma, SQLAlchemy and the Django ORM work untouched.
if [ ! -f /var/lib/pgsql/data/PG_VERSION ]; then
    sudo postgresql-setup --initdb
fi

# scram-sha-256 on loopback, NOT trust. Every app reaches Postgres over
# 127.0.0.1 with its own role and its own generated password, and CONNECT is
# revoked on every database but its own. `trust` here would mean any app could
# open any other app's database by name.
sudo tee /var/lib/pgsql/data/pg_hba.conf >/dev/null <<'EOF'
local   all   postgres              peer
host    all   all      127.0.0.1/32 scram-sha-256
host    all   all      ::1/128      scram-sha-256
EOF
# Sized for a 4 GiB box that also has to hold ten apps.
sudo tee -a /var/lib/pgsql/data/postgresql.conf >/dev/null <<'EOF'

# --- boxcode hosting ---
listen_addresses = '127.0.0.1'
shared_buffers = 128MB
max_connections = 60
password_encryption = scram-sha-256
statement_timeout = 10000
idle_in_transaction_session_timeout = 60000
EOF
sudo systemctl enable --now postgresql
sudo systemctl restart postgresql
sudo -u postgres psql -qtc 'select version()' | head -1

echo "== nginx =="
sudo mkdir -p /etc/nginx/conf.d/app-projects
if [ ! -f /etc/nginx/conf.d/apps.conf ]; then
    sudo cp "$SCRIPT_DIR/nginx/apps.conf.template" /etc/nginx/conf.d/apps.conf
fi
sudo nginx -t
sudo systemctl enable --now nginx
sudo systemctl reload nginx

echo "== journald size cap =="
# A full disk is the outage this box will actually have: it takes out every app
# and Postgres at once, and ordinary use causes it. Ten apps logging into the
# journal unbounded is the most likely way to get there.
sudo mkdir -p /etc/systemd/journald.conf.d
sudo tee /etc/systemd/journald.conf.d/boxcode.conf >/dev/null <<'EOF'
[Journal]
SystemMaxUse=2G
MaxRetentionSec=3day
EOF
sudo systemctl restart systemd-journald

if [ "$SKIP_TLS" = "1" ]; then
    echo "== SKIP_TLS=1: plain HTTP, no DNS check, no certbot =="
else
    echo "== dns check =="
    TOKEN=$(curl -sX PUT http://169.254.169.254/latest/api/token \
        -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
    THIS_IP=$(curl -s http://169.254.169.254/latest/meta-data/public-ipv4 \
        -H "X-aws-ec2-metadata-token: $TOKEN")
    RESOLVED=$(dig +short "$DOMAIN" | tail -1)
    if [ "$RESOLVED" != "$THIS_IP" ]; then
        echo "$DOMAIN resolves to '$RESOLVED', not this box ($THIS_IP)." >&2
        echo "Point it at this instance's Elastic IP, then re-run; or re-run" >&2
        echo "with SKIP_TLS=1 to prove everything else over plain HTTP first." >&2
        exit 1
    fi
    echo "== tls =="
    sudo certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos \
        --register-unsafely-without-email --redirect
fi

echo "== hosting control-plane =="
if [ -f "$SCRIPT_DIR/control-plane/index.mjs" ]; then
    sudo mkdir -p /opt/boxcode-hosting/control-plane
    sudo cp "$SCRIPT_DIR"/control-plane/*.mjs /opt/boxcode-hosting/control-plane/
    sudo cp "$SCRIPT_DIR"/runtime/*.mjs /opt/boxcode-hosting/control-plane/
    sudo cp "$SCRIPT_DIR/control-plane/boxcode-hosting-control-plane.service" \
        /etc/systemd/system/
    sudo systemctl daemon-reload
    sudo systemctl enable --now boxcode-hosting-control-plane
    sudo systemctl restart boxcode-hosting-control-plane
    sudo systemctl --no-pager status boxcode-hosting-control-plane
else
    # Not a failure. The box is provisioned and reachable; it has nothing to
    # deploy into yet.
    echo "no control-plane/ yet -- box is provisioned but cannot accept deploys."
fi

echo "== done: $(date -Is) =="
