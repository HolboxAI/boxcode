#!/bin/bash
set -e

echo "Installing tuisample-code..."

# Check for Rust/Cargo
if ! command -v cargo &> /dev/null; then
  echo "Error: Rust/Cargo not found. Install from https://rustup.rs/"
  exit 1
fi

# Clone repo
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

echo "Cloning repository..."
git clone https://github.com/HolboxAI/tuisample-code.git "$TEMP_DIR"

echo "Building tuisample-code (this may take 2-3 minutes)..."
cd "$TEMP_DIR"
cargo build --release

# Install binary
echo "Installing to /usr/local/bin..."
sudo cp target/release/tuisample-code /usr/local/bin/tuisample-code
sudo chmod +x /usr/local/bin/tuisample-code

echo "✓ Installed successfully!"
echo ""
echo "Next steps:"
echo "1. Configure your LLM endpoint:"
echo "   export TUISAMPLE_ENDPOINT=https://llm.company.internal:8443"
echo "   export TUISAMPLE_MODEL=company-llm-70b-v1.2"
echo "   export TUISAMPLE_API_KEY=sk_company_xxx"
echo ""
echo "2. Run tuisample-code:"
echo "   tuisample-code"
echo ""
