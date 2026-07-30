#!/bin/bash
set -e

echo "Installing tuisample-code..."

# Detect OS and architecture
OS=$(uname -s)
ARCH=$(uname -m)

case "$OS" in
  Darwin)
    OS_NAME="macos"
    ;;
  Linux)
    OS_NAME="linux"
    ;;
  MINGW64_NT*|MSYS_NT*)
    OS_NAME="windows"
    ;;
  *)
    echo "Unsupported OS: $OS"
    exit 1
    ;;
esac

case "$ARCH" in
  x86_64|amd64)
    ARCH_NAME="x86_64"
    ;;
  arm64|aarch64)
    ARCH_NAME="aarch64"
    ;;
  *)
    echo "Unsupported architecture: $ARCH"
    exit 1
    ;;
esac

# Get latest release tag (macOS and Linux compatible)
LATEST_TAG=$(curl -fsSL https://api.github.com/repos/HolboxAI/tuisample-code/releases/latest | grep '"tag_name"' | sed -E 's/.*"tag_name": "([^"]+)".*/\1/' | head -1)
if [ -z "$LATEST_TAG" ]; then
  echo "Error: Could not fetch latest release from GitHub"
  exit 1
fi

# Download latest release binary
RELEASE_URL="https://github.com/HolboxAI/tuisample-code/releases/download/${LATEST_TAG}/tuisample-code-${OS_NAME}-${ARCH_NAME}"
if [ "$OS_NAME" = "windows" ]; then
  RELEASE_URL="${RELEASE_URL}.exe"
fi

echo "Downloading tuisample-code ${LATEST_TAG} for ${OS_NAME}-${ARCH_NAME}..."

# Create temporary directory
TEMP_DIR=$(mktemp -d)
trap "rm -rf $TEMP_DIR" EXIT

# Download binary
if ! curl -fsSL -o "$TEMP_DIR/tuisample-code" "$RELEASE_URL"; then
  echo "Error: Failed to download binary from $RELEASE_URL"
  echo "Binary not found for ${OS_NAME}-${ARCH_NAME}"
  exit 1
fi

chmod +x "$TEMP_DIR/tuisample-code"

# Install to /usr/local/bin
echo "Installing to /usr/local/bin..."
sudo mv "$TEMP_DIR/tuisample-code" /usr/local/bin/tuisample-code

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
