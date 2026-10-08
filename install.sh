#!/bin/sh
# Install the latest Carpool release into ~/.local/bin (or $CARPOOL_INSTALL_DIR).
set -eu
repo="grollat/carpool"
dir="${CARPOOL_INSTALL_DIR:-$HOME/.local/bin}"
case "$(uname -s)-$(uname -m)" in
  Darwin-arm64) target=aarch64-apple-darwin ;;
  Darwin-x86_64) target=x86_64-apple-darwin ;;
  Linux-x86_64) target=x86_64-unknown-linux-gnu ;;
  Linux-aarch64|Linux-arm64) target=aarch64-unknown-linux-gnu ;;
  *) echo "unsupported platform $(uname -s)-$(uname -m); build from source: cargo install --git https://github.com/$repo" >&2; exit 1 ;;
esac
url="https://github.com/$repo/releases/latest/download/carpool-$target.tar.gz"
mkdir -p "$dir"
echo "downloading $url"
curl -fsSL "$url" | tar -xz -C "$dir" carpool
chmod +x "$dir/carpool"
echo "installed $dir/carpool"
case ":$PATH:" in *":$dir:"*) ;; *) echo "add $dir to your PATH" ;; esac
"$dir/carpool" --version
