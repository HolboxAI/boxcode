#!/usr/bin/env bash
# Builds the base root filesystems every project's microVM starts from. One per
# runtime, built once when the box is provisioned and reused for every deploy.
#
# Alpine rather than Amazon Linux, for one reason that matters here: a minimal
# Alpine root is about 8 MiB against roughly 200 for a dnf --installroot of
# AL2023. Every megabyte is paid ten times over -- once per project image on a
# 50 GiB disk -- and again in the seconds a deploy takes to copy it.
#
# Built with `apk --root`, which populates a directory without chroot and
# without root's help beyond writing the directory. Nothing here mounts
# anything.
#
# Idempotent: a base that already exists at the right version is left alone.
# Re-run after changing ALPINE_VERSION or a package list.
set -euo pipefail

FC_DIR="${FC_DIR:-/opt/firecracker}"
BASE_DIR="$FC_DIR/base"
ALPINE_VERSION="${ALPINE_VERSION:-3.21}"
ALPINE_ARCH="${ALPINE_ARCH:-x86_64}"
REPO="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/main"
COMMUNITY="https://dl-cdn.alpinelinux.org/alpine/v${ALPINE_VERSION}/community"

# Every base carries these.
#
# su-exec is not optional: the generated init uses it to drop from root to uid
# 1000 before starting the app. Without it every project runs as root inside its
# guest -- survivable, since the microVM is the boundary that matters, but a
# guest kernel exploit is easier from uid 0 and dropping costs nothing.
COMMON="alpine-baselayout busybox busybox-suid musl ca-certificates su-exec"

need() { command -v "$1" >/dev/null 2>&1 || { echo "$1 is required" >&2; exit 1; }; }
need apk || true

# Amazon Linux has no apk, so fetch the static one. It runs fine on glibc and is
# the supported way to build an Alpine root from a non-Alpine host.
APK_BIN="$FC_DIR/apk.static"
if [ ! -x "$APK_BIN" ]; then
    echo "== fetching apk.static =="
    sudo mkdir -p "$FC_DIR"
    APK_TOOLS=$(curl -fsSL "${REPO}/${ALPINE_ARCH}/" \
        | grep -o 'apk-tools-static-[0-9][^"]*\.apk' | sort -V | tail -1)
    [ -n "$APK_TOOLS" ] || { echo "could not find apk-tools-static in $REPO" >&2; exit 1; }
    TMP=$(mktemp -d)
    curl -fsSL "${REPO}/${ALPINE_ARCH}/${APK_TOOLS}" -o "$TMP/apk.apk"
    tar -xzf "$TMP/apk.apk" -C "$TMP" 2>/dev/null || true
    sudo install -m 0755 "$TMP/sbin/apk.static" "$APK_BIN"
    rm -rf "$TMP"
fi
"$APK_BIN" --version | head -1

build_base() {
    local name="$1"; shift
    local packages="$*"
    local root="$BASE_DIR/$name"
    local stamp="$root/.boxcode-base"
    local want="alpine=${ALPINE_VERSION} packages=${COMMON} ${packages}"

    if [ -f "$stamp" ] && [ "$(cat "$stamp")" = "$want" ]; then
        echo "== $name: already built and unchanged =="
        return
    fi

    echo "== building base: $name =="
    sudo rm -rf "$root"
    sudo mkdir -p "$root"
    # shellcheck disable=SC2086 -- word splitting of the package lists is wanted
    sudo "$APK_BIN" --root "$root" --initdb --no-cache \
        --repository "$REPO" --repository "$COMMUNITY" \
        --allow-untrusted add $COMMON $packages

    # A microVM has no DNS unless we give it one. There is no resolver on the
    # host side of the point-to-point link and no route off the box anyway, so
    # this exists to make failures fast and obvious rather than to work: a
    # project calling an external API should get "no such host" immediately,
    # not hang until its request times out.
    echo "# boxcode: hosted projects have no outbound internet." \
        | sudo tee "$root/etc/resolv.conf" >/dev/null

    # Where the project's code is mounted into the image, and the uid it runs as.
    sudo mkdir -p "$root/app"
    sudo chroot "$root" /bin/busybox adduser -D -u 1000 -H -s /sbin/nologin app 2>/dev/null \
        || echo "app:x:1000:1000::/app:/sbin/nologin" | sudo tee -a "$root/etc/passwd" >/dev/null

    echo "$want" | sudo tee "$stamp" >/dev/null
    echo "   $(sudo du -sh "$root" | cut -f1) at $root"
}

sudo mkdir -p "$BASE_DIR"

# Must stay in step with BASES in runtime/rootfs.mjs -- a runtime named there
# with no base here fails at deploy rather than here.
build_base node22    nodejs npm
build_base python312 python3 py3-pip

echo
echo "== bases =="
sudo du -sh "$BASE_DIR"/*/ 2>/dev/null || true
