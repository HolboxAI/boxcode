#!/bin/bash
set -e

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
if sudo cp "$BINARY_PATH" "$SYSTEM_BIN"; then
  sudo chmod +x "$SYSTEM_BIN"
  INSTALLED_AT="$SYSTEM_BIN"
  OTHER_COPY="$USER_BIN"
  echo "✓ Installed to /usr/local/bin"
else
  echo "⚠️  Could not write to /usr/local/bin, using ~/.local/bin instead..."

  mkdir -p "$HOME/.local/bin"
  cp "$BINARY_PATH" "$USER_BIN"
  chmod +x "$USER_BIN"
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
