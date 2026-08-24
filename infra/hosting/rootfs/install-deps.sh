#!/usr/bin/env bash
# Installs a project's dependencies by booting its own image as a microVM.
#
#   install-deps.sh <id>
#
# `npm install` runs arbitrary postinstall code. That is true of every CI system
# ever built and it is not preventable -- containment is the only answer, and it
# has to be at least as strong as what the app itself gets, or the build becomes
# the soft way into a platform whose entire premise is per-tenant hardware
# isolation. So the build is a microVM too.
#
# It boots the image assemble.sh already produced, with init=/sbin/build-init on
# the kernel command line instead of the default. Same disk, different pid 1. The
# dependencies land in /app and are simply there when the app boots normally --
# nothing is copied out, nothing is mounted, and there is no second block device.
#
# Three differences from an app microVM, and only three:
#   - it has network, through the build slot's NAT'd namespace
#   - it has 1 GiB instead of 256 MiB, because npm install peaks high
#   - it boots build-init and powers itself off when done
set -euo pipefail

REPO_RUNTIME="$(cd "$(dirname "${BASH_SOURCE[0]}")/../runtime" && pwd)"
NODE="${NODE:-/opt/node22/bin/node}"
FC_DIR="${FC_DIR:-/opt/firecracker}"
APPS_DIR="${APPS_DIR:-/opt/boxcode-apps}"
JAIL_ROOT="${JAIL_ROOT:-/srv/jailer}"

id="${1:?usage: install-deps.sh <id>}"
case "$id" in *[!a-z2-9]* | "" ) echo "invalid project id: $id" >&2; exit 2 ;; esac
[ "${#id}" -ge 4 ] && [ "${#id}" -le 16 ] || { echo "invalid project id length: $id" >&2; exit 2; }

image="$APPS_DIR/$id/rootfs.ext4"
[ -f "$image" ] || { echo "no image at $image -- run rootfs/assemble.sh first" >&2; exit 1; }

read -r BUILD_SLOT BUILD_MEM BUILD_TIMEOUT STATUS_PATH STARTED_PATH <<<"$("$NODE" -e "
  import('file://$REPO_RUNTIME/build.mjs').then(m => console.log(
    m.BUILD_SLOT, m.BUILD_MEM_MIB, m.BUILD_TIMEOUT_S, m.STATUS_PATH, m.STARTED_PATH));
")"

echo "== $id: build slot $BUILD_SLOT =="
sudo /usr/local/sbin/boxcode-slot-net up "$BUILD_SLOT"
sudo /usr/local/sbin/boxcode-build-net

# The jailer derives the chroot from --id and will create
# <chroot-base>/firecracker/<id>/root itself. This path has to be exactly that,
# or the files staged below land somewhere the VM never looks. Taken from
# machine.mjs rather than rebuilt here, so the two cannot drift.
jail="$("$NODE" -e "
  import('file://$REPO_RUNTIME/machine.mjs').then(async m => {
    const b = await import('file://$REPO_RUNTIME/build.mjs');
    console.log(m.jailPath(process.argv[1], b.BUILD_SLOT));
  });
" "$id")"

# A build VM is never reused. Whatever the last one left behind -- a socket, a
# stale config, a copy of somebody else's image -- is not something to inherit.
sudo rm -rf "$jail"
sudo mkdir -p "$jail"

# The jailer expects everything the VM touches to be inside its chroot and will
# not follow a symlink out, so the image is copied in and moved back on success.
#
# Deliberately NOT a hard link, which was the first version of this and was
# wrong: hard links share an inode, so the `chown` below would have changed the
# ownership of the real image in $APPS_DIR to the build slot's uid, and the app
# VM -- a different slot, a different uid -- would then have been unable to read
# its own disk. --reflink=auto makes the copy near-free on xfs anyway.
sudo cp --reflink=auto "$image" "$jail/rootfs.ext4"
sudo cp "$FC_DIR/vmlinux" "$jail/vmlinux"

echo "== $id: firecracker config =="
"$NODE" -e "
  import('file://$REPO_RUNTIME/machine.mjs').then(async m => {
    const b = await import('file://$REPO_RUNTIME/build.mjs');
    const cfg = m.machineConfig({
      id: process.argv[1],
      slot: b.BUILD_SLOT,
      memMib: b.BUILD_MEM_MIB,
      kernel: '/vmlinux',
      rootfs: '/rootfs.ext4',
      extraBootArgs: 'init=/sbin/build-init',
    });
    console.log(JSON.stringify(cfg, null, 2));
  });
" "$id" | sudo tee "$jail/vm.json" >/dev/null

# The uid the jailer drops to has to be able to read what it was given.
uid=$((30000 + BUILD_SLOT))
sudo chown -R "$uid:$uid" "$jail"

echo "== $id: booting the build VM (${BUILD_MEM} MiB, timeout ${BUILD_TIMEOUT}s) =="

# Kept as an array rather than word-split from a string: the values are all
# validated, but splitting on whitespace is the kind of thing that works right
# up until one value contains a space.
mapfile -t jailer_args < <("$NODE" -e "
  import('file://$REPO_RUNTIME/machine.mjs').then(async m => {
    const b = await import('file://$REPO_RUNTIME/build.mjs');
    console.log(m.jailerArgs({
      id: process.argv[1], slot: b.BUILD_SLOT, configFile: 'vm.json',
    }).join('\n'));
  });
" "$id")

# The guest enforces its own timeout too. This one is the backstop for a guest
# that never reached its init at all -- a kernel panic, an unbootable image --
# where there is nothing left inside to enforce anything. Hence the extra 30s:
# it should only ever fire when the in-guest timeout could not.
set +e
sudo timeout --kill-after=10 "$((BUILD_TIMEOUT + 30))" \
    /usr/bin/jailer "${jailer_args[@]}" >"/tmp/build-$id.log" 2>&1
boot_rc=$?
set -e
echo "   jailer exited $boot_rc"

echo "== $id: reading the result =="
# debugfs reads the image without mounting it. The host must never mount a
# filesystem a stranger's build has just finished writing to.
read_guest() {
    sudo debugfs -R "cat $1" "$jail/rootfs.ext4" 2>/dev/null || true
}
started="$(read_guest "$STARTED_PATH")"
status="$(read_guest "$STATUS_PATH")"

result="$("$NODE" -e "
  import('file://$REPO_RUNTIME/build.mjs').then(m => {
    const r = m.parseStatus({ started: process.argv[1], status: process.argv[2] });
    console.log(JSON.stringify(r));
  });
" "$started" "$status")"

ok="$(echo "$result" | "$NODE" -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>console.log(JSON.parse(s).ok))")"
reason="$(echo "$result" | "$NODE" -e "let s='';process.stdin.on('data',d=>s+=d).on('end',()=>console.log(JSON.parse(s).reason))")"

if [ "$ok" = "true" ]; then
    # Only now does the built image replace the original. A failed build leaves
    # the project exactly as it was rather than half-installed, which matters
    # because a redeploy retries from a known state instead of from whatever
    # the last attempt managed before it died.
    sudo chown root:root "$jail/rootfs.ext4"
    sudo mv "$jail/rootfs.ext4" "$image"
fi

sudo rm -rf "$jail"
sudo /usr/local/sbin/boxcode-slot-net down "$BUILD_SLOT" || true

if [ "$ok" = "true" ]; then
    echo "== $id: $reason =="
    ls -lh "$image"
    exit 0
fi

echo "$id: $reason" >&2
echo "" >&2
echo "last 40 lines of the build VM's console:" >&2
tail -40 "/tmp/build-$id.log" >&2 || true
exit 1
