#!/usr/bin/env bash
# Proves the whole pipeline works, on the box, without CloudFront or DNS.
#
#   bash infra/hosting/smoke-test.sh
#
# Builds a tiny Express app, takes it through every stage a real deploy goes
# through, checks it actually serves and actually reaches its database, then
# removes it. Nothing is left behind.
#
# This is the answer to "how do I know it works" before pointing a domain at
# anything. It needs a host with /dev/kvm -- so it runs on the runner box and
# nowhere else. Everything that CAN be checked without KVM already is, in
# `node --test infra/hosting/runtime/*.test.mjs`.
set -uo pipefail

REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
NODE="${NODE:-/opt/node22/bin/node}"
APPS_DIR="${APPS_DIR:-/opt/boxcode-apps}"
ID="${SMOKE_ID:-smoke2test}"
SLOT="${SMOKE_SLOT:-9}"

PASS=0; FAIL=0
ok()  { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
bad() { FAIL=$((FAIL+1)); printf '  \033[31mFAIL\033[0m %s\n' "$1"; }
step() { printf '\n== %s ==\n' "$1"; }

cleanup() {
    step "cleaning up"
    bash "$REPO/lifecycle/vm.sh" stop "$ID" "$SLOT" >/dev/null 2>&1 || true
    bash "$REPO/lifecycle/database.sh" drop "$ID" >/dev/null 2>&1 || true
    sudo rm -rf "${APPS_DIR:?}/$ID"
    echo "   removed $ID"
}

step "preflight"
if [ ! -e /dev/kvm ]; then
    echo "/dev/kvm is missing -- this must run on the runner box." >&2
    echo "Everything testable without KVM: node --test infra/hosting/runtime/*.test.mjs" >&2
    exit 1
fi
ok "/dev/kvm present"
[ -f /opt/firecracker/vmlinux ] && ok "guest kernel installed" || bad "no guest kernel -- run setup.sh"
[ -d /opt/firecracker/base/node22 ] && ok "node base image built" || bad "no base image -- run setup.sh"
command -v jailer >/dev/null && ok "jailer installed" || bad "no jailer -- run setup.sh"

# Armed only now. Installed before the preflight it would fire on a machine
# that never created anything, and ask for a sudo password to delete a
# directory that does not exist.
trap cleanup EXIT

step "a project that uses its database and reads PORT"
SRC="$APPS_DIR/$ID/src"
sudo rm -rf "${APPS_DIR:?}/$ID"
sudo mkdir -p "$SRC"
sudo tee "$SRC/package.json" >/dev/null <<'EOF'
{ "name": "smoke", "version": "1.0.0", "main": "server.js",
  "dependencies": { "pg": "^8.11.5" } }
EOF
# No express: one fewer thing to download, and http is what actually has to work.
# pg is here because reaching the database across the point-to-point link is the
# part most likely to be wrong.
sudo tee "$SRC/server.js" >/dev/null <<'EOF'
const http = require("http");
const { Client } = require("pg");

http.createServer(async (req, res) => {
  if (req.url === "/db") {
    const c = new Client({ connectionString: process.env.DATABASE_URL });
    try {
      await c.connect();
      const { rows } = await c.query("select 1 as ok");
      res.writeHead(200, { "content-type": "application/json" });
      res.end(JSON.stringify({ db: rows[0].ok }));
    } catch (e) {
      res.writeHead(500); res.end(String(e.message));
    } finally { try { await c.end(); } catch {} }
    return;
  }
  if (req.url === "/egress") {
    // Must fail. This is the highest-value control in the whole design.
    const t = setTimeout(() => { res.writeHead(200); res.end("blocked"); }, 4000);
    http.get("http://1.1.1.1/", () => {
      clearTimeout(t); res.writeHead(200); res.end("REACHED");
    }).on("error", () => { clearTimeout(t); res.writeHead(200); res.end("blocked"); });
    return;
  }
  res.writeHead(200, { "content-type": "text/plain" });
  res.end("hello from a microVM\n");
}).listen(process.env.PORT || 3000, "0.0.0.0");
EOF
ok "project written to $SRC"

step "database"
DB_URL="$(bash "$REPO/lifecycle/database.sh" provision "$ID" "$SLOT" 2>/dev/null)"
if [ -n "$DB_URL" ]; then ok "provisioned, url captured"; else bad "database.sh produced no url"; fi

step "rootfs"
if BOXCODE_APP_ENV="{\"DATABASE_URL\":\"$DB_URL\"}" \
   bash "$REPO/rootfs/assemble.sh" "$ID" node "$SRC" /usr/bin/node server.js >/dev/null 2>&1; then
    ok "image assembled"
else
    bad "assemble.sh failed"; exit 1
fi
[ -f "$APPS_DIR/$ID/rootfs.ext4" ] && ok "rootfs.ext4 exists" || bad "no image produced"

step "dependencies, in a build microVM"
if bash "$REPO/rootfs/install-deps.sh" "$ID" >/tmp/smoke-build.log 2>&1; then
    ok "build VM installed dependencies"
else
    bad "install-deps.sh failed -- see /tmp/smoke-build.log"
    tail -20 /tmp/smoke-build.log
    exit 1
fi
# Proves the build actually wrote into the image, without mounting it.
if sudo debugfs -R "ls /app/node_modules" "$APPS_DIR/$ID/rootfs.ext4" 2>/dev/null | grep -q pg; then
    ok "node_modules/pg is in the image"
else
    bad "dependencies are not in the image"
fi

step "starting it"
if bash "$REPO/lifecycle/vm.sh" start "$ID" "$SLOT" >/dev/null 2>&1; then
    ok "microVM started"
else
    bad "vm.sh start failed"; exit 1
fi
GUEST="10.200.$SLOT.2"
for _ in $(seq 1 30); do
    curl -sf -m 2 "http://$GUEST:8080/" >/dev/null 2>&1 && break
    sleep 1
done

step "does it actually work"
if curl -sf -m 5 "http://$GUEST:8080/" | grep -q "hello from a microVM"; then
    ok "the guest serves HTTP"
else
    bad "the guest did not answer on $GUEST:8080"
fi

if curl -sf -m 15 "http://$GUEST:8080/db" | grep -q '"db":1'; then
    ok "the guest reached its PostgreSQL database"
else
    bad "the guest could not reach its database"
fi

# The control that matters most. A pass here means a hosted project cannot
# reach a mining pool, a C2 server, or anywhere to send what it collected.
#
# Read carefully, because the obvious version of this test is wrong: a guest
# that is not serving at all makes this curl fail, and treating any non-"blocked"
# answer as "reached" turns a dead app into a reported security failure. Only
# the literal string "REACHED" means egress worked.
EGRESS="$(curl -sf -m 15 "http://$GUEST:8080/egress" || echo "no-answer")"
case "$EGRESS" in
    blocked)   ok "the guest has NO outbound internet" ;;
    REACHED)   bad "THE GUEST REACHED THE INTERNET -- the no-egress guarantee is broken" ;;
    *)         bad "could not test egress: the guest answered '$EGRESS'" ;;
esac

step "isolation"
OTHER=$(( SLOT == 0 ? 1 : 0 ))
if curl -sf -m 3 "http://10.200.$OTHER.2:8080/" >/dev/null 2>&1; then
    bad "another slot's address answered -- slots are not isolated"
else
    ok "another slot's address is unreachable"
fi

if bash "$REPO/lifecycle/vm.sh" list | grep -q "boxcode-app-$ID"; then
    ok "the VM is visible to reconciliation"
else
    bad "vm.sh list does not see it -- reconciliation would stop it as a leak"
fi

printf '\n== %d passed, %d failed ==\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
