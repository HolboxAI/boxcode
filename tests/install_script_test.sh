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
# Anonymous install ping. Enabled by default (points at the real telemetry
# endpoint -- see ping_install's own doc comment), must be disableable by an
# explicit blank override, and must never fail the install itself even when a
# URL is configured but nothing is listening.
#
# Deliberately does NOT test "an unset TUISAMPLE_TELEMETRY_URL reaches the
# real production endpoint" by actually letting that curl fire -- these tests
# run on every `cargo test`/CI pass, and that would mean every test run sends
# a real ping to production. The URL-resolution logic itself (unset falls
# back to the default, an explicit blank overrides and disables even with a
# non-blank default) is what telemetry.rs's own `telemetry_url_given` tests
# cover on the Rust side; the bash side only needs to prove *this* script's
# substitution has the same "unset vs. explicitly blank" distinction, which
# the test below does by explicitly overriding to a non-production URL either
# way.

fake_home=$(mktemp -d)
fake_bin="$workdir/fake-binary"
cat > "$fake_bin" <<'EOF'
#!/bin/bash
echo "tuisample-code 9.9.9"
EOF
chmod +x "$fake_bin"

# An explicit blank override disables sending -- even though the default is
# now a real, non-blank endpoint. This is "${VAR-default}" (no colon) rather
# than "${VAR:-default}" specifically so this case is reachable at all: the
# colon form can't distinguish "unset" from "set to empty".
TUISAMPLE_TELEMETRY_URL="" HOME="$fake_home" ping_install "$fake_bin" ||
  fail "ping_install must return 0 even when explicitly disabled"
[ -f "$fake_home/.tuisample-code/device_id" ] &&
  fail "an explicitly blank override must disable sending despite a non-blank default"

echo "PASS: ping_install is disabled by an explicit blank TUISAMPLE_TELEMETRY_URL override"

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

# --- detect_os / detect_arch -------------------------------------------------
# Shadowing the `uname` builtin with a function is the standard bash trick for
# this: `source`d into the same shell as the test, it wins over the real
# command for every caller below without needing five different machines to
# exercise five different platforms.

uname() {
  case "$1" in
    -s) echo "$FAKE_UNAME_S" ;;
    -m) echo "$FAKE_UNAME_M" ;;
  esac
}

FAKE_UNAME_S="Darwin"; [ "$(detect_os)" = "macos" ] || fail "Darwin should map to macos"
FAKE_UNAME_S="Linux"; [ "$(detect_os)" = "linux" ] || fail "Linux should map to linux"
FAKE_UNAME_S="MINGW64_NT-10.0"; [ "$(detect_os)" = "windows" ] || fail "MINGW* should map to windows"
FAKE_UNAME_S="SunOS"; [ "$(detect_os)" = "unsupported" ] || fail "an unrecognised OS should map to unsupported"

FAKE_UNAME_M="x86_64"; [ "$(detect_arch)" = "x86_64" ] || fail "x86_64 should map to x86_64"
FAKE_UNAME_M="arm64"; [ "$(detect_arch)" = "aarch64" ] || fail "macOS arm64 should map to aarch64"
FAKE_UNAME_M="aarch64"; [ "$(detect_arch)" = "aarch64" ] || fail "Linux aarch64 should map to aarch64"
FAKE_UNAME_M="i686"; [ "$(detect_arch)" = "unsupported" ] || fail "32-bit x86 should map to unsupported"

unset -f uname

echo "PASS: detect_os/detect_arch map uname's output onto release.yml's asset-name components"

# --- asset_download_url ------------------------------------------------------
# The part of the prebuilt-binary fetch that is pure text-in, text-out --
# tested against a fixture shaped like the real GitHub "get the latest
# release" response, never against the network itself.

fixture_release_json='{
  "tag_name": "v0.9.0",
  "assets": [
    {
      "url": "https://api.github.com/repos/HolboxAI/tuisample-code/releases/assets/1",
      "id": 1,
      "node_id": "RA_1",
      "name": "tuisample-code-linux-x86_64",
      "label": null,
      "content_type": "application/octet-stream",
      "state": "uploaded",
      "size": 12345678,
      "download_count": 0,
      "created_at": "2026-08-06T00:00:00Z",
      "updated_at": "2026-08-06T00:00:00Z",
      "browser_download_url": "https://github.com/HolboxAI/tuisample-code/releases/download/v0.9.0/tuisample-code-linux-x86_64"
    },
    {
      "url": "https://api.github.com/repos/HolboxAI/tuisample-code/releases/assets/2",
      "id": 2,
      "node_id": "RA_2",
      "name": "tuisample-code-macos-aarch64",
      "label": null,
      "content_type": "application/octet-stream",
      "state": "uploaded",
      "size": 12345678,
      "download_count": 0,
      "created_at": "2026-08-06T00:00:00Z",
      "updated_at": "2026-08-06T00:00:00Z",
      "browser_download_url": "https://github.com/HolboxAI/tuisample-code/releases/download/v0.9.0/tuisample-code-macos-aarch64"
    },
    {
      "url": "https://api.github.com/repos/HolboxAI/tuisample-code/releases/assets/3",
      "id": 3,
      "node_id": "RA_3",
      "name": "SHA256SUMS.txt",
      "label": null,
      "content_type": "text/plain",
      "state": "uploaded",
      "size": 200,
      "download_count": 0,
      "created_at": "2026-08-06T00:00:00Z",
      "updated_at": "2026-08-06T00:00:00Z",
      "browser_download_url": "https://github.com/HolboxAI/tuisample-code/releases/download/v0.9.0/SHA256SUMS.txt"
    }
  ]
}'

url=$(asset_download_url "$fixture_release_json" "tuisample-code-linux-x86_64")
[ "$url" = "https://github.com/HolboxAI/tuisample-code/releases/download/v0.9.0/tuisample-code-linux-x86_64" ] ||
  fail "expected the linux-x86_64 asset's URL, got: $url"

url=$(asset_download_url "$fixture_release_json" "tuisample-code-macos-aarch64")
[ "$url" = "https://github.com/HolboxAI/tuisample-code/releases/download/v0.9.0/tuisample-code-macos-aarch64" ] ||
  fail "expected the macos-aarch64 asset's URL, got: $url"

url=$(asset_download_url "$fixture_release_json" "SHA256SUMS.txt")
[ "$url" = "https://github.com/HolboxAI/tuisample-code/releases/download/v0.9.0/SHA256SUMS.txt" ] ||
  fail "expected the checksums asset's URL, got: $url"

url=$(asset_download_url "$fixture_release_json" "tuisample-code-windows-x86_64.exe")
[ -z "$url" ] || fail "an asset that is not in the release must resolve to nothing, got: $url"

echo "PASS: asset_download_url finds the right asset's URL in a real-shaped release response"

# --- sha256_of ----------------------------------------------------------------
# Must agree with whatever the platform's own tool says, not a hardcoded
# digest -- the point is proving this repo's wrapper picks the right tool, not
# re-testing sha256sum/shasum itself.

sum_file="$workdir/sum-target"
echo -n "tuisample-code" > "$sum_file"
computed=$(sha256_of "$sum_file")
if command -v sha256sum &> /dev/null; then
  reference=$(sha256sum "$sum_file" | awk '{print $1}')
elif command -v shasum &> /dev/null; then
  reference=$(shasum -a 256 "$sum_file" | awk '{print $1}')
else
  reference=""
fi
if [ -n "$reference" ]; then
  [ "$computed" = "$reference" ] || fail "sha256_of disagreed with the platform's own tool"
  echo "PASS: sha256_of matches the platform's own sha256sum/shasum"
else
  echo "SKIP: sha256_of (neither sha256sum nor shasum is available on this machine)"
fi

# --- fetch_prebuilt_binary: failure falls back cleanly ------------------------
# A refused connection must fail fast and leave nothing behind -- this is the
# exact shape of "no release has been published yet", which main() must
# recover from by building from source, not by hanging or half-writing a file.

start=$(date +%s)
RELEASE_API_BASE="http://127.0.0.1:1" fetch_prebuilt_binary "linux" "x86_64" "$workdir/should-not-exist" &&
  fail "fetch_prebuilt_binary should fail when the API is unreachable"
elapsed=$(( $(date +%s) - start ))
[ ! -e "$workdir/should-not-exist" ] || fail "a failed fetch must not leave a partial file behind"
[ "$elapsed" -lt 5 ] || fail "a refused connection should fail fast, not wait out the full timeout (took ${elapsed}s)"

echo "PASS: fetch_prebuilt_binary fails fast and cleanly when no release is reachable, so main() can fall back to a source build"

# --- ensure_ddgs_available ----------------------------------------------------
# A fake `python3` -- shadowing it as a shell function works the same way
# shadowing `uname` did earlier, and `command -v` finds shell functions just
# like real executables, so ensure_ddgs_available cannot tell the difference.
#
# The fake tracks "is ddgs importable" via a marker file rather than call
# count, so it composes the same way the real thing does: `import ddgs`
# succeeds once (and only once) something has actually "installed" it.

fake_python3_marker=""
fake_python3_pip_behavior=""

python3() {
  if [ "$1" = "-c" ]; then
    [ -n "$fake_python3_marker" ] && [ -f "$fake_python3_marker" ]
    return $?
  fi
  if [ "$1" = "-m" ] && [ "$2" = "pip" ]; then
    case "$fake_python3_pip_behavior" in
      succeeds)
        touch "$fake_python3_marker"
        return 0
        ;;
      needs-break-system-packages)
        if printf '%s\n' "$@" | grep -q -- --break-system-packages; then
          touch "$fake_python3_marker"
          return 0
        fi
        return 1
        ;;
      always-fails)
        return 1
        ;;
    esac
  fi
  return 1
}

fake_python3_marker="$workdir/ddgs-already-there"
touch "$fake_python3_marker"
out=$(ensure_ddgs_available)
[ -z "$out" ] || fail "already-importable ddgs should not print anything, got: $out"
rm -f "$fake_python3_marker"

echo "PASS: ensure_ddgs_available does nothing when ddgs is already importable"

fake_python3_marker="$workdir/ddgs-plain-install"
rm -f "$fake_python3_marker"
fake_python3_pip_behavior="succeeds"
out=$(ensure_ddgs_available)
echo "$out" | grep -q "ddgs installed" || fail "expected a success message, got: $out"
[ -f "$fake_python3_marker" ] || fail "the plain pip install path should have run"

echo "PASS: ensure_ddgs_available installs ddgs with a plain pip install when that's enough"

fake_python3_marker="$workdir/ddgs-break-system-packages"
rm -f "$fake_python3_marker"
fake_python3_pip_behavior="needs-break-system-packages"
out=$(ensure_ddgs_available)
echo "$out" | grep -q "ddgs installed" || fail "expected a success message via the PEP 668 fallback, got: $out"

echo "PASS: ensure_ddgs_available falls back to --break-system-packages for externally-managed Pythons"

fake_python3_marker="$workdir/ddgs-never-appears"
rm -f "$fake_python3_marker"
fake_python3_pip_behavior="always-fails"
out=$(ensure_ddgs_available)
echo "$out" | grep -q "Could not install 'ddgs' automatically" || fail "expected the graceful-failure message, got: $out"
echo "$out" | grep -q "pip install ddgs" || fail "the failure message should still say how to install it manually"

echo "PASS: ensure_ddgs_available fails gracefully (not via set -e) when neither install attempt works"

unset -f python3

# No python3 at all: must return immediately without ever trying to install
# anything, and without touching `set -e`.
no_python_dir="$workdir/no-python-path"
mkdir -p "$no_python_dir"
# Deliberately just this one empty directory -- not /bin or /usr/bin, which
# have a real python3 on this machine. `[`, `command`, and `echo` are bash
# builtins, so an otherwise-empty PATH does not stop them from working.
out=$(PATH="$no_python_dir" ensure_ddgs_available)
[ -z "$out" ] || fail "with no python3 on PATH, nothing should be printed, got: $out"

echo "PASS: ensure_ddgs_available is a silent no-op when python3 isn't on PATH"

# The real thing, run against the actual `ddgs` package -- skipped rather
# than failed when Python isn't available, the same convention tools.rs's
# own live tests use. Since this machine already has ddgs installed (as
# confirmed while building the web_search feature), this exercises the
# "already there, nothing to do" path for real, not just against a fake.
if command -v python3 &> /dev/null; then
  real_out=$(ensure_ddgs_available)
  if python3 -c "import ddgs" &> /dev/null; then
    [ -z "$real_out" ] || fail "ddgs is genuinely already installed here, so there should be nothing to print, got: $real_out"
    echo "PASS: ensure_ddgs_available is a real no-op against this machine's actual ddgs install"
  else
    echo "$real_out" | grep -qE "ddgs installed|Could not install" ||
      fail "expected either a real install attempt or its failure message, got: $real_out"
    echo "PASS: ensure_ddgs_available really attempted to install ddgs on this machine"
  fi
else
  echo "SKIP: ensure_ddgs_available real-Python test (no python3 on this machine)"
fi
