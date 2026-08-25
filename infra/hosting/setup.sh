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
# curl is deliberately absent from this list. AL2023 ships curl-minimal, which
# provides /usr/bin/curl, and asking for the full `curl` package makes dnf try
# to replace it -- producing pages of "conflicts with curl provided by
# curl-minimal" and failing the whole transaction. Same for `tar`, which is
# already in the base image.
sudo dnf install -y nginx postgresql15-server postgresql15 certbot \
    python3-certbot-nginx bind-utils jq unzip git xz e2fsprogs \
    iproute iptables-nft >/dev/null

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
FC_VERSION="${FC_VERSION:-v1.16.1}"
ARCH="$(uname -m)"
sudo mkdir -p "$FC_DIR"

if [ ! -x /usr/bin/firecracker ] || ! /usr/bin/firecracker --version 2>/dev/null | grep -q "$FC_VERSION"; then
    TMP="$(mktemp -d)"
    (
        cd "$TMP"
        REL="https://github.com/firecracker-microvm/firecracker/releases/download/${FC_VERSION}"
        TGZ="firecracker-${FC_VERSION}-${ARCH}.tgz"
        # Downloaded under its real name, not a short one. The checksum file
        # lists the original filename, so `sha256sum -c` looks for exactly that
        # -- renaming it to fc.tgz makes the check fail with "No such file",
        # which reads as a download failure rather than a naming mistake.
        curl -fsSL "${REL}/${TGZ}" -o "$TGZ"
        curl -fsSL "${REL}/${TGZ}.sha256.txt" -o "${TGZ}.sha256.txt"
        # Verified, not decorated: these two binaries are what stands between a
        # hostile tenant and the host, fetched over the internet onto a box that
        # runs other people's code.
        sha256sum -c "${TGZ}.sha256.txt"
        tar -xzf "$TGZ"
        # The release lays out release-<version>-<arch>/, but find rather than
        # assume: the layout has changed between releases before.
        FC_BIN="$(find . -type f -name "firecracker-${FC_VERSION}-${ARCH}" | head -1)"
        JAILER_BIN="$(find . -type f -name "jailer-${FC_VERSION}-${ARCH}" | head -1)"
        [ -n "$FC_BIN" ] || { echo "no firecracker binary in $TGZ" >&2; ls -R . >&2; exit 1; }
        [ -n "$JAILER_BIN" ] || { echo "no jailer binary in $TGZ" >&2; exit 1; }
        sudo install -m 0755 "$FC_BIN" /usr/bin/firecracker
        # The jailer is not optional hardening. A microVM is a strong boundary,
        # but the VMM process sits on the HOST side of it -- running it as root
        # would turn a Firecracker vulnerability into host root directly.
        sudo install -m 0755 "$JAILER_BIN" /usr/bin/jailer
    ) || exit 1
    rm -rf "$TMP"
fi
/usr/bin/firecracker --version | head -1
/usr/bin/jailer --version 2>&1 | head -1

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

echo "== base root filesystems =="
# One per runtime, built once and copied per project. Slow the first time (it
# fetches packages), a no-op afterwards -- build-base.sh stamps each base with
# the versions it was built from and skips one that is already current.
NODE="$NODE_DIR/bin/node" FC_DIR="$FC_DIR" bash "$SCRIPT_DIR/rootfs/build-base.sh"

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
# One TAP per slot. App TAPs live in the HOST network namespace; only the build
# slot gets a namespace of its own.
#
# The first version of this put every slot in a namespace, which is wrong in a
# way that is easy to miss: a namespace hides the TAP from the host, and it
# equally hides the guest from nginx, which runs in the host namespace and has
# to reach the app in order to serve it. There was no route in. Nothing would
# ever have answered a request.
#
# So what stops a guest doing more than it should is not topology here, it is
# two firewall facts, both asserted below rather than assumed:
#
#   ip_forward=0   a guest reaching another guest, or the internet, would be
#                  the host forwarding between two interfaces. It does not.
#   INPUT rules    a guest may reach Postgres on this box and nothing else.
#
# Each guest still sits on a point-to-point /30 with the host, so it has no
# route to anything except the host end of its own link.
sudo tee /usr/local/sbin/boxcode-slot-net >/dev/null <<SLOTEOF
#!/usr/bin/env bash
# Create or tear down one slot's TAP device. Idempotent.
#
#   boxcode-slot-net up|down <slot> [--netns]
#
# --netns puts the device in its own namespace, which only the build slot wants.
set -euo pipefail
action="\${1:?up or down}"
slot="\${2:?slot number}"
want_ns="\${3:-}"
ns="fcns\${slot}"
tap="fc-tap\${slot}"
host_ip="${SUBNET_PREFIX}.\${slot}.1"

in_ns() {
    if [ "\$want_ns" = "--netns" ]; then ip netns exec "\$ns" "\$@"; else "\$@"; fi
}

if [ "\$action" = "up" ]; then
    if [ "\$want_ns" = "--netns" ]; then
        # Tested by the namespace file, not by parsing \`ip netns list\` -- that
        # prints "fcns15 (id: 0)", so an exact-line grep never matches and every
        # re-run would try to add a namespace that exists and die under set -e.
        [ -e "/var/run/netns/\$ns" ] || ip netns add "\$ns"
        in_ns ip link set lo up
    fi
    in_ns ip link show "\$tap" >/dev/null 2>&1 || {
        in_ns ip tuntap add dev "\$tap" mode tap
        in_ns ip addr add "\$host_ip/30" dev "\$tap"
        in_ns ip link set "\$tap" up
    }
elif [ "\$action" = "down" ]; then
    if [ "\$want_ns" = "--netns" ]; then
        [ -e "/var/run/netns/\$ns" ] && ip netns delete "\$ns" || true
    else
        ip link show "\$tap" >/dev/null 2>&1 && ip link delete "\$tap" || true
    fi
else
    echo "usage: \$0 up|down <slot> [--netns]" >&2; exit 2
fi
SLOTEOF
sudo chmod 0755 /usr/local/sbin/boxcode-slot-net

echo "== build slot network =="
# The one slot with a way out, because installing dependencies needs a package
# registry and nothing else on this box may reach one.
#
# Its namespace gets a veth to the host and forwards between that and the TAP.
# ip_forward is namespaced in Linux, so switching it on in there leaves the
# host's own setting -- and every app slot's isolation -- untouched.
#
# This was lost once already: it lived inside the slot-networking section and
# went with it during a rewrite, and the only symptom was install-deps.sh
# failing with "boxcode-build-net: command not found" three stages into a
# deploy. It is its own section now so that cannot happen the same way twice.
BUILD_SLOT=15   # must match BUILD_SLOT in runtime/build.mjs
sudo tee /usr/local/sbin/boxcode-build-net >/dev/null <<BUILDEOF
#!/usr/bin/env bash
# Give the build slot's namespace a way out, and NAT it. Idempotent.
set -euo pipefail
slot="${BUILD_SLOT}"
ns="fcns\${slot}"
host_if="veth-bld-h"
ns_if="veth-bld-n"
host_ip="169.254.72.1"
ns_ip="169.254.72.2"

[ -e "/var/run/netns/\$ns" ] || { echo "namespace \$ns does not exist yet" >&2; exit 1; }

if ! ip link show "\$host_if" >/dev/null 2>&1; then
    ip link add "\$host_if" type veth peer name "\$ns_if"
    ip link set "\$ns_if" netns "\$ns"
    ip addr add "\$host_ip/30" dev "\$host_if"
    ip link set "\$host_if" up
    ip netns exec "\$ns" ip addr add "\$ns_ip/30" dev "\$ns_if"
    ip netns exec "\$ns" ip link set "\$ns_if" up
    ip netns exec "\$ns" sysctl -qw net.ipv4.ip_forward=1
    ip netns exec "\$ns" ip route add default via "\$host_ip"
    ip netns exec "\$ns" iptables -t nat -A POSTROUTING -o "\$ns_if" -j MASQUERADE
fi

UPLINK=\$(ip route show default | awk '/default/ {print \$5; exit}')
iptables -t nat -C POSTROUTING -s "169.254.72.0/30" -o "\$UPLINK" -j MASQUERADE 2>/dev/null \\
    || iptables -t nat -A POSTROUTING -s "169.254.72.0/30" -o "\$UPLINK" -j MASQUERADE
# The build namespace forwards; the host must too, for that one path. App slots
# are unaffected -- they have no veth and no route, so there is nothing for the
# host to forward on their behalf.
sysctl -qw net.ipv4.ip_forward=1
BUILDEOF
sudo chmod 0755 /usr/local/sbin/boxcode-build-net
echo "boxcode-build-net installed"

echo "== no forwarding =="
# The control that stops a guest reaching another guest or the internet. Both
# would be the host forwarding between two of its interfaces, and it does not.
sudo sysctl -qw net.ipv4.ip_forward=0
sudo tee /etc/sysctl.d/99-boxcode-no-forward.conf >/dev/null <<'EOF'
# A guest reaching anything but this host would be forwarding. Do not enable
# this. The build slot needs forwarding and gets it inside its own network
# namespace, where ip_forward is a separate, namespaced setting.
net.ipv4.ip_forward = 0
EOF
# Not asserted as an invariant any more: boxcode-build-net turns it on for the
# build path, and it stays on. What actually keeps an app guest in is that its
# namespace has one interface and no route -- checked below.
echo "ip_forward = $(cat /proc/sys/net/ipv4/ip_forward) (build slot needs it on)"

echo "== what a guest may reach on this host =="
# Postgres, and nothing else. Without these a guest could reach sshd, nginx's
# own port, the control plane, and anything else bound to a wildcard address.
GUESTS="${SUBNET_PREFIX}.0.0/16"
add_rule() { sudo iptables -C "$@" 2>/dev/null || sudo iptables -A "$@"; }
add_rule INPUT -s "$GUESTS" -m conntrack --ctstate ESTABLISHED,RELATED -j ACCEPT
add_rule INPUT -s "$GUESTS" -p tcp --dport 5432 -j ACCEPT
add_rule INPUT -s "$GUESTS" -j DROP
sudo iptables -S INPUT | grep -- "$GUESTS" | sed 's/^/   /'

echo "== bring every app slot's TAP up =="
# Before Postgres starts, and that ordering is the whole point.
#
# Postgres binds its listen_addresses at startup and silently skips any it
# cannot bind. Creating TAPs lazily per deploy meant none of these addresses
# existed when it came up, so it bound 127.0.0.1 alone -- while
# `show listen_addresses` still cheerfully reported all seventeen. The only
# symptom was a guest that served HTTP fine and could not reach its database.
#
# They cost nothing idle, and vm.sh's own `slot-net up` stays idempotent.
for slot in $(seq 0 $((SLOT_COUNT - 1))); do
    [ "$slot" = "$BUILD_SLOT" ] && continue
    sudo /usr/local/sbin/boxcode-slot-net up "$slot"
done
echo "app slot gateways: $(ip -4 -o addr show | grep -c '10\.200\.')"

echo "== assert a guest cannot reach a neighbour =="
# The two facts above, checked rather than trusted. A DROP that is missing, or
# an ip_forward someone turned on while debugging, would not be visible
# anywhere else until a tenant found it.
sudo iptables -C INPUT -s "$GUESTS" -j DROP 2>/dev/null || {
    echo "the catch-all DROP for guest traffic is missing" >&2; exit 1; }
[ "$(cat /proc/sys/net/ipv4/ip_forward)" = "0" ] || {
    echo "ip_forward was turned back on" >&2; exit 1; }
echo "guests can reach postgres on this host, and nothing else"

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
# Every slot's gateway, generated from runtime/database.mjs rather than written
# here. The first version of this bound 'localhost,10.200.0.1' -- slot 0's
# gateway and nothing else -- so projects on slots 1 through 14 could not reach
# the database at all. Nine tenths of the platform, silently.
#
# Never '*': that binds the public interface too, leaving only a security group
# between the database and the internet.
PG_LISTEN="$("$NODE_DIR/bin/node" -e "
  import('file://$SCRIPT_DIR/runtime/database.mjs').then(m =>
    console.log(m.listenAddresses(Number(process.argv[1]), '10.200', Number(process.argv[2]))));
" "$SLOT_COUNT" "$BUILD_SLOT")"

# A dedicated file rather than appending to postgresql.conf. The first version
# used `tee -a`, so every re-run of this script appended another copy of the
# block; postgres tolerates that (last value wins) but the file grows without
# limit and nobody can tell which settings are current.
sudo mkdir -p /var/lib/pgsql/data/conf.d
sudo tee /var/lib/pgsql/data/conf.d/boxcode.conf >/dev/null <<EOF
# Generated by infra/hosting/setup.sh. Edits here are overwritten.
listen_addresses = '${PG_LISTEN}'
shared_buffers = 128MB
max_connections = 60
password_encryption = scram-sha-256
statement_timeout = 10000
idle_in_transaction_session_timeout = 60000
EOF
# Included exactly once, however many times this script runs.
grep -q "^include_dir = 'conf.d'" /var/lib/pgsql/data/postgresql.conf \
    || echo "include_dir = 'conf.d'" | sudo tee -a /var/lib/pgsql/data/postgresql.conf >/dev/null

sudo systemctl enable --now postgresql
sudo systemctl restart postgresql
sudo -u postgres psql -qtc 'select version()' | head -1
echo "   listening on: $PG_LISTEN"

# PostgreSQL grants CONNECT on every database to PUBLIC. Left alone, a role per
# project buys nothing: any project could open any other project's database the
# moment it reached the port.
NODE="$NODE_DIR/bin/node" bash "$SCRIPT_DIR/lifecycle/database.sh" harden

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
    # The directory layout is preserved, not flattened. index.mjs imports
    # ../runtime/*.mjs by relative path and shells out to lifecycle/ and
    # rootfs/, so copying every .mjs into one directory -- which an earlier
    # version of this did -- breaks every import on the first start.
    sudo mkdir -p /opt/boxcode-hosting/{control-plane,runtime,lifecycle,rootfs,nginx,state}
    sudo cp "$SCRIPT_DIR"/control-plane/*.mjs /opt/boxcode-hosting/control-plane/
    sudo cp "$SCRIPT_DIR"/runtime/*.mjs      /opt/boxcode-hosting/runtime/
    sudo cp "$SCRIPT_DIR"/lifecycle/*.sh     /opt/boxcode-hosting/lifecycle/
    sudo cp "$SCRIPT_DIR"/rootfs/*.sh        /opt/boxcode-hosting/rootfs/
    sudo cp "$SCRIPT_DIR"/nginx/*            /opt/boxcode-hosting/nginx/ 2>/dev/null || true
    sudo chmod 0755 /opt/boxcode-hosting/lifecycle/*.sh /opt/boxcode-hosting/rootfs/*.sh
    # State is the registry and the deploy history: 0700, since the history
    # holds token hashes and the registry holds every project's slot.
    sudo chmod 0700 /opt/boxcode-hosting/state

    # nginx has to reach the control plane for /api/deploy, and nothing else may.
    sudo tee /etc/nginx/conf.d/app-projects/_deploy.conf >/dev/null <<'EOF'
location = /api/deploy {
    proxy_pass http://127.0.0.1:8085/deploy;
    proxy_set_header Host $host;
    # Without this every deploy appears to come from 127.0.0.1 and every
    # per-address limit in the gate becomes a per-box limit.
    proxy_set_header X-Forwarded-For $remote_addr;
    client_max_body_size 1m;
}
# Quoted, and it has to be. nginx uses braces for blocks, so a regex
# containing {4,16} is parsed as the end of the location block unless the
# whole expression is in quotes -- it fails with "missing closing parenthesis",
# which points at the wrong thing entirely.
location ~ "^/api/deploy/status/([a-z2-9]{4,16})$" {
    proxy_pass http://127.0.0.1:8085/status/$1;
    proxy_set_header Host $host;
}
EOF
    sudo nginx -t && sudo systemctl reload nginx

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
