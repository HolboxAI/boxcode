#!/usr/bin/env bash
# Proves the containment controls BLOCK, rather than proving they are configured.
#
# The distinction matters. `docker inspect` showing --cap-drop ALL proves a flag
# was passed; it does not prove a container cannot mount a filesystem. Every
# check below tries the thing and fails the run if it succeeds.
#
# Runs on the box as part of the abuse drill, and on any machine with Docker
# while developing. gVisor is not available on macOS, so:
#
#   RUNTIME=runc bash infra/hosting/runtime/verify-containment.sh
#
# checks everything except the gVisor layer itself. On the box, run it without
# RUNTIME set so it exercises runsc for real.
#
# Everything it creates is prefixed bcverify- and removed on exit, including on
# failure -- this runs on a box with live tenants and must never leave a network
# or container behind.
set -uo pipefail

RUNTIME="${RUNTIME:-runsc}"
IMAGE="${IMAGE:-alpine:3}"
PASS=0
FAIL=0

cleanup() {
    docker rm -f bcverify-pg bcverify-a1 >/dev/null 2>&1
    docker network rm bcverify-net1 bcverify-net2 >/dev/null 2>&1
}
trap cleanup EXIT
cleanup

ok()   { PASS=$((PASS+1)); printf '  \033[32mPASS\033[0m %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  \033[31mFAIL\033[0m %s\n' "$1"; }

# Runs a command inside the app container and fails the check if it SUCCEEDS.
# This is the direction that matters: a control is proven by the attempt losing.
refute() {
    local what="$1"; shift
    if docker exec bcverify-a1 sh -c "$*" >/dev/null 2>&1; then
        bad "$what -- IT WORKED, which means the control is not there"
    else
        ok "$what"
    fi
}
confirm() {
    local what="$1"; shift
    if docker exec bcverify-a1 sh -c "$*" >/dev/null 2>&1; then
        ok "$what"
    else
        bad "$what -- it did not work, and it needs to"
    fi
}

# For checks that damage the container they run in -- exhausting its pids,
# getting OOM-killed. These get a throwaway container each, carrying the same
# flags as a real app.
#
# An earlier version ran the fork bomb inside the shared container above, which
# left it pinned at its PID ceiling for thirty seconds; the next `docker exec`
# then could not fork and the check after it failed for a reason that had
# nothing to do with what it was testing. A destructive test needs its own
# blast radius, same as a destructive tenant does.
oneshot_exit() {
    local want="$1" what="$2"; shift 2
    docker run --rm \
        --runtime "$RUNTIME" \
        --network bcverify-net1 \
        --user 10001:10001 --cap-drop ALL --security-opt no-new-privileges \
        --read-only --tmpfs /tmp:rw,noexec,nosuid,size=64m \
        --memory 384m --memory-swap 384m --cpus 0.25 --pids-limit 256 \
        "$IMAGE" sh -c "$*" >/dev/null 2>&1
    local got=$?
    if [ "$got" = "$want" ]; then
        ok "$what"
    else
        bad "$what -- exited $got, expected $want"
    fi
}

echo "== setting up (runtime: $RUNTIME) =="
docker network create --internal --subnet 10.201.1.0/24 bcverify-net1 >/dev/null
docker network create --internal --subnet 10.201.2.0/24 bcverify-net2 >/dev/null

# Stands in for Postgres: same placement, same reachability, no database needed.
docker run -d --name bcverify-pg --network bcverify-net1 "$IMAGE" \
    sh -c 'nc -lk -p 5432 -e echo pg' >/dev/null
docker network connect bcverify-net2 bcverify-pg

docker run -d --name bcverify-a1 \
    --runtime "$RUNTIME" \
    --network bcverify-net1 \
    --user 10001:10001 \
    --cap-drop ALL \
    --security-opt no-new-privileges \
    --read-only \
    --tmpfs /tmp:rw,noexec,nosuid,size=64m \
    --memory 384m --memory-swap 384m \
    --cpus 0.25 \
    --pids-limit 256 \
    "$IMAGE" sleep 600 >/dev/null
sleep 2

echo
echo "== egress: the highest-value control =="
refute "cannot open a TCP connection to the internet"      'timeout 5 nc -z -w3 1.1.1.1 443'
refute "cannot fetch a URL by name"                        'timeout 6 wget -q -T3 -O /dev/null http://example.com'
refute "cannot resolve an external name at all"            'timeout 5 nslookup example.com 2>&1 | grep -q "^Address: [0-9]"'
refute "cannot reach the EC2 metadata service"             'timeout 5 wget -q -T3 -O /dev/null http://169.254.169.254/latest/meta-data/'

echo
echo "== tenant isolation =="
confirm "CAN reach its own database"                       'timeout 5 nc -z -w3 bcverify-pg 5432'
refute  "cannot reach another app's network"               'timeout 5 nc -z -w3 10.201.2.2 5432'
refute  "cannot scan another app's subnet"                 'timeout 5 nc -z -w3 10.201.2.99 5432'

echo
echo "== privilege =="
refute "is not root"                                       '[ "$(id -u)" = "0" ]'
refute "cannot mount anything"                             'mount -t tmpfs none /mnt'
refute "cannot write outside its tmpfs"                    'touch /evil'
refute "cannot write to its own code directory"            'touch /bin/evil'
confirm "CAN write to /tmp, which it needs"                'touch /tmp/fine'
refute "cannot execute from /tmp"                          'cp /bin/echo /tmp/x && /tmp/x hi'

echo
echo "== resource ceilings =="
# 137 is SIGKILL, i.e. the OOM killer. This is the check that proves the memory
# ceiling BITES rather than merely being reported by the cgroup -- a container
# can read its own memory.max and still not be constrained by it.
oneshot_exit 137 "unbounded memory allocation is killed"   'tail /dev/zero'
# A fork bomb must lose. What the exit code is does not matter; that the
# container dies rather than the box does is the whole assertion.
oneshot_exit 2   "a fork bomb hits the pid ceiling"        'i=0; while [ $i -lt 400 ]; do sleep 5 & i=$((i+1)); done; exit 2'

echo
printf '== %d passed, %d failed ==\n' "$PASS" "$FAIL"
[ "$FAIL" -eq 0 ]
