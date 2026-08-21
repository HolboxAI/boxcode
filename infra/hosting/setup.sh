#!/usr/bin/env bash
# Provisions the runner box: Docker with gVisor, the container networking that
# confines hosted apps, Postgres, and nginx for apps.boxcode.sh.
#
# Runs unattended from user-data on every launch -- including the 3am spot
# replacement nobody is awake for -- and by hand after any infra/hosting change.
# Idempotent throughout, which is not a nicety here: it is the recovery path.
#
# THIS BOX RUNS CODE STRANGERS UPLOADED. It shares nothing with boxcode-auth,
# which runs its control-plane as root with Postgres on `trust` -- defensible
# there because everything on it is a trusted third-party image, and not
# defensible for a minute here. Do not copy that box's privilege posture over.
#
# SKIP_TLS=1 sets up everything except the DNS check and certbot, so the rest
# can be proven over plain HTTP before apps.boxcode.sh has an A record. Same
# escape hatch infra/auth/setup.sh has, for the same reason.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SKIP_TLS="${SKIP_TLS:-0}"
DATA="${BOXCODE_DATA_DIR:-/opt/boxcode-hosting}"
DOMAIN=apps.boxcode.sh

# Docker's default address pools are 172.17.0.0/16 through 172.31.0.0/16, and
# this account's VPC is 172.31.0.0/16. Left alone, Docker will eventually hand a
# container network the same range as the VPC and the box silently loses its
# route to the auth box at 172.31.22.160 -- an outage that appears at container
# number N for no visible reason. Moving the pool out of the way costs nothing.
DOCKER_POOL=10.200.0.0/16
PG_NETWORK=boxcode-pg
PG_CONTAINER=boxcode-postgres
PG_IMAGE=postgres:16-alpine

echo "== data volume =="
if ! mountpoint -q "$DATA"; then
    echo "$DATA is not a mount point -- user-data attaches and mounts the data" >&2
    echo "volume before running this. Refusing to write project state to the" >&2
    echo "root volume, which is destroyed on every instance replacement." >&2
    exit 1
fi
sudo mkdir -p "$DATA"/{pgdata,apps,zips,builds,secrets}
sudo chmod 700 "$DATA/secrets"

echo "== packages =="
sudo dnf install -y docker nginx certbot python3-certbot-nginx bind-utils \
    jq unzip iptables-services >/dev/null

echo "== gvisor =="
# The replacement for the per-tenant Firecracker microVM that Lambda gave for
# free. Containers share the host kernel; runsc puts a user-space kernel in
# front of it so an exploit has to get through the Sentry first. Costs ~10-15%
# CPU, which is the price of the substrate change.
if ! command -v runsc >/dev/null 2>&1; then
    ARCH="$(uname -m)"
    URL="https://storage.googleapis.com/gvisor/releases/release/latest/${ARCH}"
    TMP="$(mktemp -d)"
    (
        cd "$TMP"
        # Checksums are verified, not decorated: this binary is the thing
        # standing between a hostile container and the host kernel, and it is
        # fetched over the internet on every fresh instance.
        curl -fsSLO "${URL}/runsc" -O "${URL}/runsc.sha512" \
            -O "${URL}/containerd-shim-runsc-v1" -O "${URL}/containerd-shim-runsc-v1.sha512"
        sha512sum -c runsc.sha512 containerd-shim-runsc-v1.sha512
        chmod a+rx runsc containerd-shim-runsc-v1
        sudo mv runsc containerd-shim-runsc-v1 /usr/local/bin/
    )
    rm -rf "$TMP"
fi
runsc --version | head -1

echo "== docker daemon =="
sudo mkdir -p /etc/docker
# runsc is registered as an available runtime, NOT as the default. Hosted app
# containers and the build sandbox ask for it explicitly; Postgres deliberately
# does not -- it is our own trusted image, and gVisor's overhead falls hardest
# on exactly the I/O a database does most of.
#
# live-restore keeps containers running across a daemon restart, so `systemctl
# restart docker` during maintenance does not take ten live demos down with it.
#
# Log rotation is not housekeeping either. A full disk is the outage this box
# will actually have -- it takes out all ten apps and Postgres at once, and it
# is caused by ordinary use rather than by anything going wrong.
sudo tee /etc/docker/daemon.json >/dev/null <<EOF
{
  "default-address-pools": [{"base": "${DOCKER_POOL}", "size": 24}],
  "runtimes": {"runsc": {"path": "/usr/local/bin/runsc"}},
  "live-restore": true,
  "log-driver": "json-file",
  "log-opts": {"max-size": "10m", "max-file": "3"}
}
EOF
sudo systemctl enable --now docker
sudo systemctl restart docker
sudo docker info --format '{{.ServerVersion}} runtimes={{range $k,$v := .Runtimes}}{{$k}} {{end}}'

echo "== egress backstop =="
# Belt and braces on the container networks below.
#
# Each app network is created --internal, which is what actually blocks egress.
# But Docker owns its iptables rules and rewrites them on daemon restart, and a
# control that a `systemctl restart docker` can silently remove is not a
# control. DOCKER-USER is the one chain Docker never flushes, so the rule goes
# there: anything forwarded FROM the app pool TO outside the app pool is
# dropped, whatever Docker's own rules happen to say at the time.
sudo tee /usr/local/sbin/boxcode-egress-backstop >/dev/null <<EOF
#!/usr/bin/env bash
set -euo pipefail
# -C tests for the rule and fails if absent, which is the idempotency check.
iptables -C DOCKER-USER -s ${DOCKER_POOL} '!' -d ${DOCKER_POOL} -j DROP 2>/dev/null || \\
    iptables -I DOCKER-USER 1 -s ${DOCKER_POOL} '!' -d ${DOCKER_POOL} -j DROP
EOF
sudo chmod +x /usr/local/sbin/boxcode-egress-backstop
sudo tee /etc/systemd/system/boxcode-egress-backstop.service >/dev/null <<'EOF'
[Unit]
Description=Drop egress from boxcode app networks, whatever Docker rewrote
After=docker.service
Requires=docker.service
PartOf=docker.service

[Service]
Type=oneshot
RemainAfterExit=yes
ExecStart=/usr/local/sbin/boxcode-egress-backstop

[Install]
WantedBy=multi-user.target
EOF
sudo systemctl daemon-reload
sudo systemctl enable --now boxcode-egress-backstop
sudo systemctl restart boxcode-egress-backstop

echo "== postgres =="
# The reason this box exists rather than a Lambda: a real wire protocol, so
# Prisma, SQLAlchemy, the Django ORM and every other ORM work untouched.
#
# Runs on the default runtime, not runsc -- see the daemon.json comment.
if ! sudo docker network inspect "$PG_NETWORK" >/dev/null 2>&1; then
    sudo docker network create --internal "$PG_NETWORK" >/dev/null
fi

PW_FILE="$DATA/secrets/postgres-super.pw"
if [ ! -f "$PW_FILE" ]; then
    # Generated once and kept on the data volume, so an instance replacement
    # comes back to a database it can still open.
    openssl rand -hex 32 | sudo tee "$PW_FILE" >/dev/null
    sudo chmod 600 "$PW_FILE"
fi

if ! sudo docker ps --format '{{.Names}}' | grep -qx "$PG_CONTAINER"; then
    sudo docker rm -f "$PG_CONTAINER" >/dev/null 2>&1 || true
    sudo docker run -d --name "$PG_CONTAINER" \
        --network "$PG_NETWORK" \
        --restart unless-stopped \
        -e POSTGRES_PASSWORD_FILE=/run/secrets/pw \
        -v "$PW_FILE":/run/secrets/pw:ro \
        -v "$DATA/pgdata":/var/lib/postgresql/data \
        --memory 768m --memory-swap 768m \
        "$PG_IMAGE" \
        -c shared_buffers=192MB \
        -c max_connections=100 \
        -c statement_timeout=10000 \
        -c idle_in_transaction_session_timeout=60000 >/dev/null
fi
# Wait for it rather than racing whatever runs next.
for _ in $(seq 1 30); do
    sudo docker exec "$PG_CONTAINER" pg_isready -q && break
    sleep 2
done
sudo docker exec "$PG_CONTAINER" pg_isready

echo "== nginx =="
sudo mkdir -p /etc/nginx/conf.d/app-projects
sudo cp "$SCRIPT_DIR/nginx/upgrade-map.conf" /etc/nginx/conf.d/upgrade-map.conf
# Only write the base vhost if it is not already there: once certbot has run it
# has edited this file in place, and copying the template over the top would
# silently destroy the TLS server block. Same trap infra/db/setup.sh documents.
if [ ! -f /etc/nginx/conf.d/apps.conf ]; then
    sudo cp "$SCRIPT_DIR/nginx/apps.conf.template" /etc/nginx/conf.d/apps.conf
fi
sudo nginx -t
sudo systemctl enable --now nginx
sudo systemctl reload nginx

if [ "$SKIP_TLS" = "1" ]; then
    echo "== SKIP_TLS=1: plain HTTP, no DNS check, no certbot =="
else
    echo "== dns check =="
    # certbot's HTTP-01 challenge fails opaquely without this, so say the useful
    # thing here rather than letting certbot's error be the first anyone sees.
    TOKEN=$(curl -sX PUT http://169.254.169.254/latest/api/token \
        -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')
    THIS_IP=$(curl -s http://169.254.169.254/latest/meta-data/public-ipv4 \
        -H "X-aws-ec2-metadata-token: $TOKEN")
    RESOLVED=$(dig +short "$DOMAIN" | tail -1)
    if [ "$RESOLVED" != "$THIS_IP" ]; then
        echo "$DOMAIN resolves to '$RESOLVED', not this box ($THIS_IP)." >&2
        echo "" >&2
        echo "On a spot replacement this is expected until the Elastic IP is" >&2
        echo "reassociated. Point $DOMAIN at the Elastic IP, not at an" >&2
        echo "instance address, or every replacement breaks TLS." >&2
        exit 1
    fi

    echo "== tls =="
    sudo certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos \
        --register-unsafely-without-email --redirect
fi

echo "== hosting control-plane =="
if [ -f "$SCRIPT_DIR/control-plane/index.mjs" ]; then
    sudo mkdir -p /opt/boxcode-hosting-cp
    sudo cp "$SCRIPT_DIR"/control-plane/*.mjs /opt/boxcode-hosting-cp/
    sudo cp "$SCRIPT_DIR/control-plane/boxcode-hosting-control-plane.service" \
        /etc/systemd/system/boxcode-hosting-control-plane.service
    sudo systemctl daemon-reload
    sudo systemctl enable --now boxcode-hosting-control-plane
    sudo systemctl restart boxcode-hosting-control-plane
    sudo systemctl --no-pager status boxcode-hosting-control-plane
else
    # Not an error. The box is provisioned and reachable; it just has nothing
    # to deploy into yet. Failing here would put the ASG into a replace loop
    # that no amount of retrying fixes.
    echo "no control-plane/ in this bundle -- box is provisioned but cannot"
    echo "accept deploys yet. This is expected until it ships."
fi

echo "== done: $(date -Is) =="
