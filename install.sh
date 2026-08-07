#!/bin/bash
set -e

# Where release assets and checksums are published. Overridable so a fork or
# an internal mirror can serve its own builds -- mirrors how upgrade.rs lets
# TUISAMPLE_UPGRADE_URL_BASE redirect its own fetches.
RELEASE_API_BASE="${TUISAMPLE_RELEASE_API_BASE:-https://api.github.com/repos/HolboxAI/tuisample-code}"

# Remove any other "tuisample-code" executable found on $PATH so a stale
# build from a previous install can't shadow (or be shadowed by) the one we
# just installed, regardless of which directory it ended up in.
sweep_path_for_stale_copies() {
  local installed_at="$1"
  local dir candidate found=0
  local saved_ifs="$IFS"
  IFS=':'
  for dir in $PATH; do
    IFS="$saved_ifs"
    [ -n "$dir" ] || continue
    candidate="$dir/tuisample-code"
    if [ -f "$candidate" ] && [ "$candidate" != "$installed_at" ]; then
      found=1
      echo "🧹 Removing stale copy on PATH: $candidate"
      if [ -w "$dir" ]; then
        rm -f "$candidate" || echo "⚠️  Could not remove it. Run: rm $candidate"
      else
        sudo rm -f "$candidate" || echo "⚠️  Could not remove it. Run: sudo rm $candidate"
      fi
    fi
    IFS=':'
  done
  IFS="$saved_ifs"
  return 0
}

# Fire a single, best-effort "an install happened" ping -- the bash-side
# counterpart to telemetry.rs's `active` ping, which this binary hasn't run
# yet to send. Anonymous: a random ID written to $HOME/.tuisample-code/device_id
# that labels this machine, not the person running it -- the same file
# telemetry.rs reads and reuses on later runs rather than generating a second,
# conflicting ID. No other data leaves this machine from here.
#
# Defaults to the same endpoint telemetry.rs's DEFAULT_TELEMETRY_URL points
# at -- keep the two in sync if that ever changes. TUISAMPLE_TELEMETRY_URL
# overrides it; note the missing ":" in the substitution below is deliberate,
# not a typo -- "${VAR-default}" falls back only when the variable is unset,
# so an explicit TUISAMPLE_TELEMETRY_URL="" still disables sending rather than
# silently reverting to the default, matching telemetry.rs's own handling of
# an explicit blank override. Every failure mode -- no uuidgen, no curl,
# network down, endpoint unreachable -- is swallowed either way. This must
# never be able to fail the install itself, so it always backgrounds the
# request and never lets a failure here reach `set -e`.
ping_install() {
  local binary="$1"
  local default_url="https://tui-telemetry.dhruvm307.workers.dev"
  local url="${TUISAMPLE_TELEMETRY_URL-$default_url}"
  [ -n "$url" ] || return 0

  local state_dir="$HOME/.tuisample-code"
  local id_file="$state_dir/device_id"
  mkdir -p "$state_dir" 2>/dev/null || return 0

  if [ ! -s "$id_file" ]; then
    if command -v uuidgen &> /dev/null; then
      uuidgen > "$id_file" 2>/dev/null || return 0
    else
      # No uuidgen (rare, mainly minimal Linux images): unique enough to
      # count installs by is all this needs to be, not cryptographically
      # random -- see telemetry.rs's own fallback for the same reasoning.
      echo "$(date +%s%N)-$$-$RANDOM" > "$id_file" 2>/dev/null || return 0
    fi
  fi
  local device_id
  device_id=$(cat "$id_file" 2>/dev/null) || return 0
  [ -n "$device_id" ] || return 0

  local version
  version=$("$binary" --version 2>/dev/null | awk '{print $2}')

  ( curl -s -m 3 -X POST "$url" \
      -H "Content-Type: application/json" \
      -d "{\"anon_id\":\"$device_id\",\"event\":\"install\",\"version\":\"${version:-unknown}\",\"os\":\"$(uname -s)\"}" \
      >/dev/null 2>&1 & ) || true
  return 0
}

# `web_search` needs Python's `ddgs` package -- see tools.rs's own doc comment
# for why it shells out to Python rather than a pure-Rust HTTP call. Whether
# it actually works has so far depended on ddgs already happening to be on
# the machine for some unrelated reason; this makes that a real, install-time
# guarantee instead of a coincidence, the same way Rust itself gets installed
# automatically below if it's missing.
#
# Best-effort in every direction, and never allowed to fail the install: no
# python3 means web_search simply won't work (the tool itself already
# explains that clearly when it's actually used, so there is nothing more
# useful to say here), and a failed pip install is reported but not fatal --
# the app remains fully usable without web_search either way.
ensure_ddgs_available() {
  if ! command -v python3 &> /dev/null; then
    return 0
  fi
  if python3 -c "import ddgs" &> /dev/null; then
    return 0
  fi

  echo "🔎 Installing the 'ddgs' Python package (needed for web_search)..."
  # Plain `pip install` first -- works everywhere it's allowed to. Many
  # current Linux distros mark their system Python as "externally managed"
  # (PEP 668) and refuse that outright, even with `--user`; retried with
  # `--break-system-packages` for exactly that case; a single small package
  # in the user's own site-packages is what `--user` was already asking for,
  # this just gets past the distro's opt-out-required guard rail for it.
  # `-m pip` rather than a bare `pip3`, since some systems have python3 but
  # no separate `pip3` executable on PATH.
  if python3 -m pip install --user ddgs &> /dev/null; then
    :
  elif python3 -m pip install --user --break-system-packages ddgs &> /dev/null; then
    :
  fi

  if python3 -c "import ddgs" &> /dev/null; then
    echo "✓ ddgs installed"
  else
    echo "⚠️  Could not install 'ddgs' automatically. web_search will explain how"
    echo "   to install it yourself (pip install ddgs) if you end up using it."
  fi
  return 0
}

# Put a binary at $dest, replacing whatever is there.
#
# Writing straight over the destination breaks `tuisample-code --upgrade`: that
# runs this script from the very binary being replaced, and on Linux writing to
# a running executable fails with ETXTBSY. So write a sibling temp file and
# rename it over the target — rename swaps the inode atomically, leaving the
# still-running process on the old one and never exposing a half-copied binary.
#
# $3 is the privilege escalation command ("sudo", or empty for none).
install_binary() {
  local src="$1" dest="$2" runner="${3:-}"
  local tmp="$dest.new.$$"
  if $runner cp "$src" "$tmp" && $runner chmod 755 "$tmp" && $runner mv -f "$tmp" "$dest"; then
    return 0
  fi
  $runner rm -f "$tmp" 2>/dev/null || true
  return 1
}

# --- prebuilt-binary fetch ---------------------------------------------------
#
# Building from source works everywhere but costs a Rust toolchain install (if
# missing) plus 2-3 minutes of compilation -- for the five platforms release.yml
# already builds on every tagged release, that's pure waste. This section tries
# a direct binary download first; `main` falls back to the source build below
# for anything it can't satisfy (an unsupported platform, no published release
# yet, a network hiccup), so the install never becomes *less* capable than it
# was before, only faster when a matching binary exists.

# Maps `uname -s`/`uname -m` onto the asset-name components release.yml uses
# (`tuisample-code-$os-$arch`). Isolated in their own functions, rather than
# inlined into main, purely so tests can shadow the `uname` builtin and drive
# every branch without needing to run on five different machines.
detect_os() {
  case "$(uname -s)" in
    Darwin) echo "macos" ;;
    Linux) echo "linux" ;;
    MINGW* | MSYS* | CYGWIN*) echo "windows" ;;
    *) echo "unsupported" ;;
  esac
}

detect_arch() {
  case "$(uname -m)" in
    x86_64 | amd64) echo "x86_64" ;;
    arm64 | aarch64) echo "aarch64" ;;
    *) echo "unsupported" ;;
  esac
}

# Pulls the download URL for one named asset out of a GitHub "get the latest
# release" API response. No `jq` dependency, on purpose -- this has to run on
# a bare-minimum machine that may have nothing but bash and curl. Relies on
# the API's stable pretty-printed layout (one field per line, `name` before
# `browser_download_url` within the same asset object) rather than parsing
# JSON properly; a generous line window after the match keeps this from
# depending on the exact field count GitHub happens to emit.
#
# Deliberately pure -- takes the JSON as a string, does no network I/O itself
# -- so this is the part of the fetch path tests can cover with a fixture
# response instead of needing the real GitHub API to be reachable.
asset_download_url() {
  local json="$1" asset_name="$2"
  # `|| true`: "no such asset" is an expected, common outcome (this platform
  # has no prebuilt binary, or the release predates SHA256SUMS.txt), and
  # `grep` finding nothing exits non-zero. Left unguarded, that would trip
  # `set -e` at the call site under `main` and abort the whole install
  # instead of falling back to a source build -- the exact case this
  # function exists to handle gracefully.
  echo "$json" |
    grep -A 30 "\"name\": \"$asset_name\"" |
    grep '"browser_download_url"' |
    head -1 |
    sed -E 's/.*"browser_download_url": *"([^"]*)".*/\1/' || true
}

# Whichever SHA-256 tool this platform actually has -- Linux ships
# `sha256sum`, macOS ships `shasum -a 256`, and asking for the wrong one on
# either platform is a silent no-op that would let a corrupted download
# through. Prints nothing (and the caller treats that as "cannot verify") if
# neither is present.
sha256_of() {
  local file="$1"
  if command -v sha256sum &> /dev/null; then
    sha256sum "$file" | awk '{print $1}'
  elif command -v shasum &> /dev/null; then
    shasum -a 256 "$file" | awk '{print $1}'
  fi
  # Explicit, and after the branches rather than folded into an `|| true` on
  # each: neither tool being present must still return success with empty
  # output (the "cannot verify" case), not fail the bare assignment at the
  # call site under `set -e`.
  return 0
}

# Downloads the release asset matching `$os-$arch` into `$dest`, verifying it
# against SHA256SUMS.txt when the release publishes one (older releases, from
# before this file existed, will not -- that is a missed check, not a reason
# to refuse an otherwise-good binary). Echoes nothing; the caller checks the
# file it asked for.
#
# Returns non-zero on anything short of a verified (or unverifiable-but-
# present) binary landing at `$dest` -- no release yet, no asset for this
# platform, a network failure, or a checksum mismatch. Every one of those is
# meant to be silently recoverable by falling back to a source build, not a
# reason to abort the install, so this function itself never calls `exit`.
fetch_prebuilt_binary() {
  local os="$1" arch="$2" dest="$3"
  local asset_name="tuisample-code-$os-$arch"

  local release_json
  release_json=$(curl -fsSL -m 15 "$RELEASE_API_BASE/releases/latest" 2>/dev/null) || return 1
  [ -n "$release_json" ] || return 1

  local download_url
  download_url=$(asset_download_url "$release_json" "$asset_name")
  [ -n "$download_url" ] || return 1

  curl -fsSL -m 60 -o "$dest" "$download_url" || return 1
  [ -s "$dest" ] || return 1

  local sums_url
  sums_url=$(asset_download_url "$release_json" "SHA256SUMS.txt")
  if [ -n "$sums_url" ]; then
    local expected actual
    # `|| true`: this asset having no line in SHA256SUMS.txt is possible (a
    # release published before this check existed) and must fall through to
    # "cannot verify", not abort the install via `set -e`.
    expected=$(curl -fsSL -m 15 "$sums_url" 2>/dev/null | grep " $asset_name\$" | awk '{print $1}' || true)
    actual=$(sha256_of "$dest")
    if [ -n "$expected" ] && [ -n "$actual" ] && [ "$expected" != "$actual" ]; then
      echo "⚠️  Checksum mismatch for $asset_name -- refusing to install a corrupted download." >&2
      rm -f "$dest"
      return 1
    fi
  fi

  chmod +x "$dest"
  return 0
}

main() {
echo "🚀 Installing tuisample-code..."
echo ""

TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

BINARY_PATH=""
OS_NAME=$(detect_os)
ARCH_NAME=$(detect_arch)

if [ "$OS_NAME" != "unsupported" ] && [ "$ARCH_NAME" != "unsupported" ]; then
  echo "🔍 Looking for a prebuilt $OS_NAME-$ARCH_NAME binary..."
  CANDIDATE="$TEMP_DIR/tuisample-code"
  if fetch_prebuilt_binary "$OS_NAME" "$ARCH_NAME" "$CANDIDATE"; then
    BINARY_PATH="$CANDIDATE"
    echo "✓ Downloaded a prebuilt binary — no build required"
  else
    echo "⚠️  No usable prebuilt binary for $OS_NAME-$ARCH_NAME yet. Building from source instead..."
  fi
else
  echo "⚠️  No prebuilt binary is published for this platform ($(uname -s) $(uname -m)). Building from source instead..."
fi
echo ""

if [ -z "$BINARY_PATH" ]; then
  # Check for Rust/Cargo, install if needed
  if ! command -v cargo &> /dev/null; then
    echo "📦 Rust not found. Installing Rust automatically..."
    echo "   (This takes 1-2 minutes on first install)"
    echo ""

    # Install Rust
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y

    # Load Rust into current shell
    export PATH="$HOME/.cargo/bin:$PATH"
    source "$HOME/.cargo/env" 2>/dev/null || true

    echo ""
    echo "✓ Rust installed successfully"
    echo ""
  fi

  # Verify cargo works
  if ! command -v cargo &> /dev/null; then
    echo "Error: Could not install Rust. Please visit https://rustup.rs/"
    exit 1
  fi

  echo "📥 Cloning repository..."
  git clone https://github.com/HolboxAI/tuisample-code.git "$TEMP_DIR/src"

  echo "⚙️  Building tuisample-code (this takes 2-3 minutes)..."
  (cd "$TEMP_DIR/src" && cargo build --release)

  BINARY_PATH="$TEMP_DIR/src/target/release/tuisample-code"
  if [ ! -f "$BINARY_PATH" ]; then
    echo "❌ Error: Binary not found at $BINARY_PATH"
    echo "Build may have failed. Check the output above."
    exit 1
  fi
  echo "✓ Binary built successfully"
fi

# Install binary
SYSTEM_BIN=/usr/local/bin/tuisample-code
USER_BIN="$HOME/.local/bin/tuisample-code"

echo "📍 Installing to /usr/local/bin..."
if install_binary "$BINARY_PATH" "$SYSTEM_BIN" sudo; then
  INSTALLED_AT="$SYSTEM_BIN"
  OTHER_COPY="$USER_BIN"
  echo "✓ Installed to /usr/local/bin"
else
  echo "⚠️  Could not write to /usr/local/bin, using ~/.local/bin instead..."

  mkdir -p "$HOME/.local/bin"
  install_binary "$BINARY_PATH" "$USER_BIN" || {
    echo "❌ Error: could not install to $USER_BIN"
    exit 1
  }
  INSTALLED_AT="$USER_BIN"
  OTHER_COPY="$SYSTEM_BIN"

  if [[ ":$PATH:" == *":$HOME/.local/bin:"* ]]; then
    echo "✓ Installed to ~/.local/bin (already in PATH)"
  else
    echo "⚠️  Installed to ~/.local/bin"
    echo "Add to your PATH: export PATH=\"\$HOME/.local/bin:\$PATH\""
  fi
fi

# An older install in the *other* location would shadow (or be shadowed by) the
# copy we just wrote, depending on PATH order — leaving users on a stale build.
if [ -f "$OTHER_COPY" ]; then
  echo ""
  echo "🧹 Removing stale copy at $OTHER_COPY..."
  if [ "$OTHER_COPY" = "$SYSTEM_BIN" ]; then
    sudo rm -f "$OTHER_COPY" || echo "⚠️  Could not remove it. Run: sudo rm $OTHER_COPY"
  else
    rm -f "$OTHER_COPY" || echo "⚠️  Could not remove it. Run: rm $OTHER_COPY"
  fi
fi

# A stale copy can also live in some *other* directory entirely (a custom bin
# dir, homebrew, etc.) — not just the two locations above.
sweep_path_for_stale_copies "$INSTALLED_AT"

# Confirm the shell actually resolves to what we just installed.
hash -r 2>/dev/null || true
RESOLVED=$(command -v tuisample-code || true)
if [ -z "$RESOLVED" ]; then
  echo "⚠️  tuisample-code is not on your PATH yet. Open a new shell and retry."
elif [ "$RESOLVED" != "$INSTALLED_AT" ]; then
  echo ""
  echo "⚠️  WARNING: 'tuisample-code' resolves to $RESOLVED,"
  echo "   but this build was installed to $INSTALLED_AT."
  echo "   Remove the other copy, or fix your PATH order, or you will keep"
  echo "   running the old version."
else
  echo "✓ Verified: $RESOLVED ($("$RESOLVED" --version 2>/dev/null || echo 'version unknown'))"
fi

# Best-effort either way -- see ensure_ddgs_available's own doc comment.
ensure_ddgs_available

# Best-effort and silent either way -- see ping_install's own doc comment.
ping_install "$INSTALLED_AT"

echo ""
echo "✅ Installation complete!"
echo ""
echo "🎯 Next steps:"
echo "1. Configure your LLM endpoint:"
echo "   export TUISAMPLE_ENDPOINT=https://api.openai.com"
echo "   export TUISAMPLE_MODEL=gpt-4"
echo "   export TUISAMPLE_API_KEY=sk-..."
echo ""
echo "2. Run tuisample-code:"
echo "   tuisample-code"
echo ""
echo "📖 For more info: https://github.com/HolboxAI/tuisample-code"
echo ""
}

# Guarded so tests can `source` this file (to reach sweep_path_for_stale_copies)
# without triggering a full install.
if [[ "${BASH_SOURCE[0]:-$0}" == "${0}" ]]; then
  main "$@"
fi
