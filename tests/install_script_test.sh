#!/bin/bash
# Exercises install.sh's stale-binary PATH sweep in isolation: no cargo
# build, no sudo, no network. Regression test for the bug where a stale
# `tuisample-code` binary elsewhere on $PATH (not just /usr/local/bin or
# ~/.local/bin) could keep shadowing a freshly installed build.
set -euo pipefail

REPO_ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
source "$REPO_ROOT/install.sh"

fail() {
  echo "FAIL: $1" >&2
  exit 1
}

workdir=$(mktemp -d)
trap 'rm -rf "$workdir"' EXIT

dir_a="$workdir/custom-bin-a"
dir_b="$workdir/custom-bin-b"
dir_installed="$workdir/installed"
mkdir -p "$dir_a" "$dir_b" "$dir_installed"

# Stale copies sitting in directories the old installer never knew to check.
echo "old-a" > "$dir_a/tuisample-code"
chmod +x "$dir_a/tuisample-code"
echo "old-b" > "$dir_b/tuisample-code"
chmod +x "$dir_b/tuisample-code"

# The binary we just installed — must never be touched by the sweep.
echo "new" > "$dir_installed/tuisample-code"
chmod +x "$dir_installed/tuisample-code"

# Deliberately an isolated PATH: the test directories plus bare-minimum
# system dirs for `rm` itself -- never the real $PATH, or the sweep would
# delete real binaries (as happened once while writing this test).
PATH="$dir_installed:$dir_a:$dir_b:/bin:/usr/bin" \
  sweep_path_for_stale_copies "$dir_installed/tuisample-code"

[ -f "$dir_a/tuisample-code" ] && fail "stale copy in $dir_a should have been removed"
[ -f "$dir_b/tuisample-code" ] && fail "stale copy in $dir_b should have been removed"
[ -f "$dir_installed/tuisample-code" ] || fail "the newly installed binary must not be removed"

echo "PASS: sweep_path_for_stale_copies removes stale copies elsewhere on \$PATH but keeps the installed one"

# --- install_binary ---------------------------------------------------------
# `tuisample-code --upgrade` runs this installer from the very binary being
# replaced. Writing over the destination in place fails with ETXTBSY on Linux,
# so install_binary must swap the inode by rename instead.

bin_dir="$workdir/install-binary"
mkdir -p "$bin_dir"
echo "new-build" > "$bin_dir/src"
echo "old-build" > "$bin_dir/dest"
chmod 755 "$bin_dir/dest"

# Stand-in for the inode a running process holds open: a hard link to dest.
ln "$bin_dir/dest" "$bin_dir/held-open"

install_binary "$bin_dir/src" "$bin_dir/dest" || fail "install_binary returned non-zero"

[ "$(cat "$bin_dir/dest")" = "new-build" ] || fail "dest should hold the new build"
# Had install_binary written in place, the hard link would show the new content
# too — and on Linux the write would have failed outright with ETXTBSY.
[ "$(cat "$bin_dir/held-open")" = "old-build" ] ||
  fail "replacing a binary must swap the inode, not write in place"
[ -x "$bin_dir/dest" ] || fail "dest should be executable"
[ -z "$(ls "$bin_dir"/dest.new.* 2>/dev/null)" ] || fail "temp file should not be left behind"

# A destination that cannot be written must report failure, not half-install.
# Skipped as root, where directory permissions don't stop the write.
if [ "$(id -u)" -ne 0 ]; then
  unwritable="$workdir/unwritable"
  mkdir -p "$unwritable"
  chmod 500 "$unwritable"
  if install_binary "$bin_dir/src" "$unwritable/tuisample-code" 2>/dev/null; then
    chmod 700 "$unwritable"
    fail "install_binary should return non-zero when the destination is unwritable"
  fi
  chmod 700 "$unwritable"
  [ -z "$(ls "$unwritable"/tuisample-code.new.* 2>/dev/null)" ] ||
    fail "failed install should not leave a temp file behind"
fi

echo "PASS: install_binary replaces a running binary by rename and cleans up on failure"

# --- ping_install -------------------------------------------------------------
# Anonymous install ping. Must be silent and safe by default (no telemetry URL
# configured, the state every build ships in), and must never fail the install
# itself even when a URL is configured but nothing is listening -- see the
# function's own doc comment for the full reasoning.

fake_home=$(mktemp -d)
fake_bin="$workdir/fake-binary"
cat > "$fake_bin" <<'EOF'
#!/bin/bash
echo "tuisample-code 9.9.9"
EOF
chmod +x "$fake_bin"

# Disabled by default: no TUISAMPLE_TELEMETRY_URL set.
unset TUISAMPLE_TELEMETRY_URL
HOME="$fake_home" ping_install "$fake_bin" || fail "ping_install must return 0 even when disabled"
[ -f "$fake_home/.tuisample-code/device_id" ] &&
  fail "a disabled ping must not even generate a device id"

echo "PASS: ping_install does nothing with no telemetry URL configured"

# Enabled, but pointing at nothing reachable: must still return success and
# must still create the device id (idempotently, for install.sh's own use and
# for the app binary to reuse on first launch), without ever blocking on the
# network -- ping_install backgrounds the curl itself, so this call returns
# long before any 3s curl timeout could.
rm -rf "$fake_home"
mkdir -p "$fake_home"
start=$(date +%s)
TUISAMPLE_TELEMETRY_URL="http://127.0.0.1:1/nowhere" HOME="$fake_home" ping_install "$fake_bin" ||
  fail "ping_install must return 0 even when the endpoint is unreachable"
elapsed=$(( $(date +%s) - start ))
[ -f "$fake_home/.tuisample-code/device_id" ] || fail "device id must be created once enabled"
[ -s "$fake_home/.tuisample-code/device_id" ] || fail "device id file must not be empty"
[ "$elapsed" -lt 2 ] || fail "ping_install must not block on the network (took ${elapsed}s)"

first_id=$(cat "$fake_home/.tuisample-code/device_id")
TUISAMPLE_TELEMETRY_URL="http://127.0.0.1:1/nowhere" HOME="$fake_home" ping_install "$fake_bin"
second_id=$(cat "$fake_home/.tuisample-code/device_id")
[ "$first_id" = "$second_id" ] || fail "an existing device id must be reused, not regenerated"

rm -rf "$fake_home"
echo "PASS: ping_install is non-blocking, creates a stable device id once enabled, and never fails the install"
