#!/usr/bin/env bash
# Provisions the runner box into a Firecracker host: KVM, the firecracker and
# jailer binaries, a guest kernel, per-slot networking, Postgres and nginx.
#
# Each hosted project gets its own microVM with its own guest kernel, so tenants
# are separated by hardware virtualisation rather than by a shared kernel's
# permission checks. That is the same isolation Lambda and Fargate use, because
# it is the same hypervisor.
#
# THIS BOX RUNS CODE STRANGERS UPLOADED. It shares nothing with boxcode-auth,
# which runs its control-plane as root with Postgres on `trust`. That is
# defensible there, where every process is a trusted third-party image, and
# would be indefensible here.
#
# Idempotent -- meant to be re-run after any infra/hosting change.
#
# SKIP_TLS=1 does everything except the DNS check and certbot, so the rest can
# be proven over plain HTTP before apps.boxcode.sh has an A record.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SKIP_TLS="${SKIP_TLS:-0}"
DOMAIN=apps.boxcode.sh

FC_DIR=/opt/firecracker           # binaries and the guest kernel
JAIL_ROOT=/srv/jailer             # must match JAIL_ROOT in runtime/machine.mjs
APPS_DIR=/opt/boxcode-apps        # per-project rootfs images
SLOT_COUNT=16                     # must match SLOT_COUNT in runtime/network.mjs
SUBNET_PREFIX=10.200              # must match SUBNET_PREFIX in runtime/network.mjs

##############################################################################
# The check that has to come first
##############################################################################

echo "== kvm =="
# Everything below is pointless without this. Nested virtualization is a CPU
# option set at launch (see runner.tf); if it is off, or the instance type does
# not support it, /dev/kvm simply does not exist and Firecracker fails later
# with an error that reads like a permissions problem. Fail here instead, with
# the actual reason.
if [ ! -e /dev/kvm ]; then
    echo "/dev/kvm does not exist -- this box cannot run Firecracker." >&2
    echo "" >&2
    echo "Almost always one of:" >&2
    echo "  1. Nested virtualization is not enabled on the instance. It is a CPU" >&2
    echo "     option, can only be changed while the instance is STOPPED, and is" >&2
    echo "     set by runner.tf:" >&2
    echo "       aws ec2 stop-instances --instance-ids <id>" >&2
    echo "       aws ec2 modify-instance-cpu-options --instance-id <id> \\" >&2
    echo "           --nested-virtualization enabled" >&2
    echo "  2. The instance type does not support it. Supported: C8i M8i R8i" >&2
    echo "     C8id R8id M8id *-flex X8i C7i R7i M7i I7i. Graviton is NOT" >&2
    echo "     supported, so no *g types." >&2
    echo "" >&2
    echo "instance type: $(curl -s http://169.254.169.254/latest/meta-data/instance-type \
        -H "X-aws-ec2-metadata-token: $(curl -sX PUT http://169.254.169.254/latest/api/token \
        -H 'X-aws-ec2-metadata-token-ttl-seconds: 60')" 2>/dev/null || echo unknown)" >&2
    exit 1
fi
# Present is not the same as usable -- confirm the CPU actually exposes the
# virtualization extensions rather than trusting the device node.
if ! grep -qE '^flags.*\b(vmx|svm)\b' /proc/cpuinfo; then
    echo "/dev/kvm exists but the CPU reports no vmx/svm flag." >&2
    echo "Nested virtualization is not actually active on this instance." >&2
    exit 1
fi
echo "/dev/kvm present, CPU reports virtualization extensions"

##############################################################################
# Packages
##############################################################################

echo "== packages =="
sudo dnf install -y nginx postgresql15-server postgresql15 certbot \
    python3-certbot-nginx bind-utils jq unzip git tar xz e2fsprogs \
    iproute iptables-nft curl >/dev/null

echo "== node =="
# AL2023's default nodejs is v18 and the control-plane targets current Node.
# Same approach infra/db/setup.sh takes: a real tarball in its own directory,
# rather than fighting the distro's module streams.
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

##############################################################################
# Firecracker and the jailer
##############################################################################

echo "== firecracker =="
FC_VERSION="${FC_VERSION:-v1.13.1}"
ARCH="$(uname -m)"
sudo mkdir -p "$FC_DIR"

if [ ! -x /usr/bin/firecracker ] || [ "$(/usr/bin/firecracker --version | head -1 | awk '{print $2}')" != "$FC_VERSION" ]; then
    TMP="$(mktemp -d)"
    (
        cd "$TMP"
        REL="https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}"
        curl -fsSL "${REL}/firecracker-${FC_VERSION}-${ARCH}.tgz" -o fc.tgz
        # The checksum is verified, not decorated: these two binaries are what
        # stands between a hostile tenant and the host, and they are fetched
        # over the internet on a box that hosts other people's code.
        curl -fsSL "${REL}/firecracker-${FC_VERSION}-${ARCH}.tgz.sha256.txt" -o fc.sha256
        sha256sum -c fc.sha256
        tar -xzf fc.tgz
        sudo install -m 0755 "release-${FC_VERSION}-${ARCH}/firecracker-${FC_VERSION}-${ARCH}" /usr/bin/firecracker
        # The jailer is not optional hardening. A microVM is a strong boundary,
        # but the VMM process sits on the HOST side of it -- running it as root
        # would turn a Firecracker vulnerability into host root directly. The
        # jailer chroots it, drops it to an unprivileged uid, puts it in a cgroup
        # and a network namespace, and installs a seccomp filter.
        sudo install -m 0755 "release-${FC_VERSION}-${ARCH}/jailer-${FC_VERSION}-${ARCH}" /usr/bin/jailer
    )
    rm -rf "$TMP"
fi
/usr/bin/firecracker --version | head -1
/usr/bin/jailer --version | head -1

echo "== guest kernel =="
# Firecracker boots an uncompressed kernel image directly -- no bootloader, no
# firmware, no device probing. That is most of why a microVM boots in about a
# tenth of a second, and it is why a plain distro kernel package is not what is
# wanted here.
if [ ! -f "$FC_DIR/vmlinux" ]; then
    curl -fsSL "https://s3.amazonaws.com/spec.ccfc.min/firecracker-ci/v1.13/${ARCH}/vmlinux-6.1.141" \
        -o /tmp/vmlinux
    sudo install -m 0644 /tmp/vmlinux "$FC_DIR/vmlinux"
    rm -f /tmp/vmlinux
fi
ls -l "$FC_DIR/vmlinux"

echo "== jail root =="
sudo mkdir -p "$JAIL_ROOT" "$APPS_DIR"
sudo chmod 0700 "$JAIL_ROOT"

# One unprivileged user per slot, matching jailerIds() in runtime/machine.mjs.
# A single shared uid would let a VMM that escaped its chroot signal or ptrace
# the other fifteen, which is most of what escaping a chroot is good for.
echo "== per-slot jailer users =="
for slot in $(seq 0 $((SLOT_COUNT - 1))); do
    uid=$((30000 + slot))
    name="fcjail${slot}"
    if ! id -u "$name" >/dev/null 2>&1; then
        sudo groupadd -g "$uid" "$name"
        sudo useradd -u "$uid" -g "$uid" -M -s /sbin/nologin "$name"
    fi
done
echo "slots 0..$((SLOT_COUNT - 1)) have dedicated uids 30000..$((30000 + SLOT_COUNT - 1))"

##############################################################################
# Per-slot networking
##############################################################################

echo "== constants agree with runtime/network.mjs =="
# SLOT_COUNT and SUBNET_PREFIX are duplicated: this script creates the
# namespaces and TAP devices, and runtime/network.mjs decides what the
# control-plane looks for. A silent disagreement puts devices where nothing
# looks for them, and shows up as microVMs that boot with no network rather than
# as anything resembling a configuration error. So assert it instead.
FROM_JS=$("$NODE_DIR/bin/node" -e "
  import('file://$SCRIPT_DIR/runtime/network.mjs').then(m =>
    console.log(m.SLOT_COUNT, m.SUBNET_PREFIX, m.tapName(0), m.netnsName(0)));
")
read -r JS_SLOTS JS_PREFIX JS_TAP0 JS_NS0 <<<"$FROM_JS"
if [ "$JS_SLOTS" != "$SLOT_COUNT" ] || [ "$JS_PREFIX" != "$SUBNET_PREFIX" ] \
   || [ "$JS_TAP0" != "fc-tap0" ] || [ "$JS_NS0" != "fcns0" ]; then
    echo "setup.sh and runtime/network.mjs disagree:" >&2
    echo "  setup.sh:    SLOT_COUNT=$SLOT_COUNT SUBNET_PREFIX=$SUBNET_PREFIX tap0=fc-tap0 ns0=fcns0" >&2
    echo "  network.mjs: SLOT_COUNT=$JS_SLOTS SUBNET_PREFIX=$JS_PREFIX tap0=$JS_TAP0 ns0=$JS_NS0" >&2
    exit 1
fi
echo "slots=$SLOT_COUNT prefix=$SUBNET_PREFIX naming=fc-tapN/fcnsN -- agreed"

echo "== slot networking =="
# One network namespace and one TAP device per slot, matching runtime/network.mjs.
# Each guest sits on a point-to-point /30 with the host, so a guest has no route
# to another guest -- that is structural rather than a firewall rule that could
# be edited away.
#
# The host deliberately does NOT enable ip_forward for these. With no forwarding
# and no NAT there is no route off the box at all: no mining pool, no C2, no
# spam relay, nowhere to exfiltrate to.
sudo tee /usr/local/sbin/boxcode-slot-net >/dev/null <<SLOTEOF
#!/usr/bin/env bash
# Create or tear down one slot's namespace and TAP device. Idempotent.
set -euo pipefail
action="\${1:?up or down}"
slot="\${2:?slot number}"
ns="fcns\${slot}"
tap="fc-tap\${slot}"
host_ip="${SUBNET_PREFIX}.\${slot}.1"

# Test for the namespace by its file, not by parsing \`ip netns list\` -- that
# prints "fcns0 (id: 0)", so an exact-line grep never matches and every re-run
# would try to add a namespace that already exists and die under set -e. The
# file is also exactly what jailer --netns is pointed at.
if [ "\$action" = "up" ]; then
    [ -e "/var/run/netns/\$ns" ] || ip netns add "\$ns"
    ip netns exec "\$ns" ip link show "\$tap" >/dev/null 2>&1 || {
        ip netns exec "\$ns" ip tuntap add dev "\$tap" mode tap
        ip netns exec "\$ns" ip addr add "\$host_ip/30" dev "\$tap"
        ip netns exec "\$ns" ip link set "\$tap" up
        ip netns exec "\$ns" ip link set lo up
    }
elif [ "\$action" = "down" ]; then
    [ -e "/var/run/netns/\$ns" ] && ip netns delete "\$ns" || true
else
    echo "usage: \$0 up|down <slot>" >&2; exit 2
fi
SLOTEOF
sudo chmod 0755 /usr/local/sbin/boxcode-slot-net

# Explicitly off, and asserted rather than assumed. A guest reaching the
# internet would need this on; leaving it at whatever the AMI shipped is how a
# no-egress guarantee quietly stops being true.
sudo sysctl -qw net.ipv4.ip_forward=0
sudo tee /etc/sysctl.d/99-boxcode-no-forward.conf >/dev/null <<'EOF'
# Guests must have no route off this box. Nothing here forwards for them.
net.ipv4.ip_forward = 0
EOF
echo "ip_forward = $(cat /proc/sys/net/ipv4/ip_forward) (must be 0)"

##############################################################################
# Postgres
##############################################################################

echo "== postgres =="
# The reason this platform is not on Lambda: a real wire protocol, so Prisma,
# SQLAlchemy and the Django ORM work untouched.
if [ ! -f /var/lib/pgsql/data/PG_VERSION ]; then
    sudo postgresql-setup --initdb
fi

# Listens on the slot subnet so guests can reach it, with scram-sha-256 and a
# role per project -- never `trust`. Each project's role has CONNECT revoked on
# every database but its own, so reaching the port is not reaching the data.
sudo tee /var/lib/pgsql/data/pg_hba.conf >/dev/null <<EOF
local   all   postgres                      peer
host    all   all      127.0.0.1/32         scram-sha-256
host    all   all      ${SUBNET_PREFIX}.0.0/16  scram-sha-256
EOF
sudo tee -a /var/lib/pgsql/data/postgresql.conf >/dev/null <<EOF

# --- boxcode hosting ---
listen_addresses = 'localhost,${SUBNET_PREFIX}.0.1'
shared_buffers = 128MB
max_connections = 60
password_encryption = scram-sha-256
statement_timeout = 10000
idle_in_transaction_session_timeout = 60000
EOF
sudo systemctl enable --now postgresql
sudo systemctl restart postgresql
sudo -u postgres psql -qtc 'select version()' | head -1

##############################################################################
# nginx
##############################################################################

echo "== nginx =="
sudo mkdir -p /etc/nginx/conf.d/app-projects
if [ ! -f /etc/nginx/conf.d/apps.conf ]; then
    sudo cp "$SCRIPT_DIR/nginx/apps.conf.template" /etc/nginx/conf.d/apps.conf
fi
sudo nginx -t
sudo systemctl enable --now nginx
sudo systemctl reload nginx

echo "== journald size cap =="
# A full disk takes out every microVM and Postgres at once, is caused by
# ordinary use, and is the outage this box will actually have.
sudo mkdir -p /etc/systemd/journald.conf.d
sudo tee /etc/systemd/journald.conf.d/boxcode.conf >/dev/null <<'EOF'
[Journal]
SystemMaxUse=2G
MaxRetentionSec=3day
EOF
sudo systemctl restart systemd-journald

##############################################################################
# TLS
##############################################################################

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
        echo "Point it at the Elastic IP, then re-run; or re-run with SKIP_TLS=1" >&2
        echo "to prove everything else over plain HTTP first." >&2
        exit 1
    fi
    echo "== tls =="
    sudo certbot --nginx -d "$DOMAIN" --non-interactive --agree-tos \
        --register-unsafely-without-email --redirect
fi

##############################################################################
# Control plane
##############################################################################

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
    # Not a failure. The box is a working Firecracker host; it just has nothing
    # driving it yet.
    echo "no control-plane/ yet -- box is provisioned but cannot accept deploys."
fi

echo "== done: $(date -Is) =="
