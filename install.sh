#!/bin/sh
# Install predict-agent binary.
# Usage: curl -sSL https://raw.githubusercontent.com/predictAworknet/prediction-skill/main/install.sh | sh
set -e

REPO="predictAworknet/prediction-skill"
INSTALL_DIR="${INSTALL_DIR:-/usr/local/bin}"

# Detect platform
OS="$(uname -s)"
ARCH="$(uname -m)"

case "${OS}" in
  Linux)   OS_NAME="linux" ;;
  Darwin)  OS_NAME="darwin" ;;
  *)       echo "Error: unsupported OS: ${OS}"; exit 1 ;;
esac

case "${ARCH}" in
  x86_64|amd64)   ARCH_NAME="x86_64" ;;
  aarch64|arm64)   ARCH_NAME="aarch64" ;;
  *)               echo "Error: unsupported architecture: ${ARCH}"; exit 1 ;;
esac

# On Linux x86_64, prefer musl (static) build to avoid glibc version issues
if [ "${OS_NAME}" = "linux" ] && [ "${ARCH_NAME}" = "x86_64" ]; then
  BINARY_NAME="predict-agent-linux-x86_64-musl"
else
  BINARY_NAME="predict-agent-${OS_NAME}-${ARCH_NAME}"
fi

# Get latest release tag
echo "Fetching latest release..."
LATEST=$(curl -sSL -H "Accept: application/vnd.github+json" \
  "https://api.github.com/repos/${REPO}/releases/latest" \
  | grep '"tag_name"' | head -1 | sed 's/.*: "\(.*\)".*/\1/')

if [ -z "${LATEST}" ]; then
  echo "Error: could not find latest release. Check https://github.com/${REPO}/releases"
  exit 1
fi

URL="https://github.com/${REPO}/releases/download/${LATEST}/${BINARY_NAME}"

echo "Downloading predict-agent ${LATEST} for ${OS_NAME}/${ARCH_NAME}..."
echo "  ${URL}"

# Download
TMPFILE=$(mktemp)
HTTP_CODE=$(curl -sSL -w "%{http_code}" -o "${TMPFILE}" "${URL}")

if [ "${HTTP_CODE}" != "200" ]; then
  rm -f "${TMPFILE}"
  echo "Error: download failed (HTTP ${HTTP_CODE})"
  echo "Available binaries at: https://github.com/${REPO}/releases/tag/${LATEST}"
  exit 1
fi

chmod +x "${TMPFILE}"

# Install
if [ -w "${INSTALL_DIR}" ]; then
  mv "${TMPFILE}" "${INSTALL_DIR}/predict-agent"
else
  echo "Installing to ${INSTALL_DIR} (requires sudo)..."
  sudo mv "${TMPFILE}" "${INSTALL_DIR}/predict-agent"
fi

# macOS: remove quarantine attribute so Gatekeeper doesn't block it
if [ "${OS_NAME}" = "darwin" ]; then
  xattr -d com.apple.quarantine "${INSTALL_DIR}/predict-agent" 2>/dev/null || true
fi

echo ""
echo "predict-agent ${LATEST} installed to ${INSTALL_DIR}/predict-agent"
echo ""
echo "Verify: predict-agent --version"
