#!/usr/bin/env bash
set -euo pipefail

VERSION="${1:?Usage: install.sh <version> (e.g. v0.1.0)}"
REPO="Haut/packet-prism"
ARCH="$(uname -m)"

case "$ARCH" in
  x86_64)  ARCH="x86_64" ;;
  aarch64) ARCH="aarch64" ;;
  *) echo "Unsupported architecture: $ARCH"; exit 1 ;;
esac

URL="https://github.com/${REPO}/releases/download/${VERSION}/packet-prism_${VERSION#v}_linux_${ARCH}.tar.gz"

echo "Downloading packet-prism ${VERSION} (linux/${ARCH})..."
curl -fsSL "$URL" | tar xz -C /tmp packet-prism
sudo mv /tmp/packet-prism /usr/local/bin/packet-prism
sudo chmod +x /usr/local/bin/packet-prism

echo "Installing systemd service..."
sudo mkdir -p /etc/packet-prism
sudo cp "$(dirname "$0")/packet-prism.service" /etc/systemd/system/packet-prism.service
sudo systemctl daemon-reload

if [ ! -f /etc/packet-prism/.env ]; then
  sudo cp "$(dirname "$0")/../.env.example" /etc/packet-prism/.env
  echo "Created /etc/packet-prism/.env from example — edit it with your values."
else
  echo "/etc/packet-prism/.env already exists, skipping."
fi

echo ""
echo "Done. Next steps:"
echo "  1. Edit /etc/packet-prism/.env"
echo "  2. sudo systemctl enable --now packet-prism"
echo "  3. sudo journalctl -u packet-prism -f"
