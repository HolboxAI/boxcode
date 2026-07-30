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
echo "📍 Installing to /usr/local/bin..."
if ! sudo cp "$BINARY_PATH" /usr/local/bin/tuisample-code 2>/dev/null; then
  echo "⚠️  Failed to install to /usr/local/bin (permission issue)"
  echo "Trying alternative installation to ~/.local/bin..."

  mkdir -p ~/.local/bin
  cp "$BINARY_PATH" ~/.local/bin/tuisample-code
  chmod +x ~/.local/bin/tuisample-code

  # Check if ~/.local/bin is in PATH
  if [[ ":$PATH:" == *":$HOME/.local/bin:"* ]]; then
    echo "✓ Installed to ~/.local/bin (already in PATH)"
  else
    echo "⚠️  Installed to ~/.local/bin"
    echo "Add to your PATH: export PATH=\"\$HOME/.local/bin:\$PATH\""
  fi
else
  sudo chmod +x /usr/local/bin/tuisample-code
  echo "✓ Installed to /usr/local/bin"
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
