#!/bin/bash
set -e

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
# Disabled by default (empty TUISAMPLE_TELEMETRY_URL, matching telemetry.rs's
# own DEFAULT_TELEMETRY_URL), and every failure mode -- no uuidgen, no curl,
# network down, endpoint unset or unreachable -- is swallowed. This must never
# be able to fail the install itself, so it always backgrounds the request and
# never lets a failure here reach `set -e`.
ping_install() {
  local binary="$1"
  local url="${TUISAMPLE_TELEMETRY_URL:-}"
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

main() {
echo "🚀 Installing tuisample-code..."
echo ""

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

# Clone repo
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

echo "📥 Cloning repository..."
git clone https://github.com/HolboxAI/tuisample-code.git "$TEMP_DIR"

echo "⚙️  Building tuisample-code (this takes 2-3 minutes)..."
cd "$TEMP_DIR"
cargo build --release

# Verify binary exists
BINARY_PATH="$TEMP_DIR/target/release/tuisample-code"
if [ ! -f "$BINARY_PATH" ]; then
  echo "❌ Error: Binary not found at $BINARY_PATH"
  echo "Build may have failed. Check the output above."
  exit 1
fi

echo "✓ Binary built successfully"

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
