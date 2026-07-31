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
