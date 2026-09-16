#!/usr/bin/env bash
# Arthas-style installer for the rthas CLI (and, on Linux, the embedded eBPF helper).
#
#   curl -fsSL https://github.com/XuQianJin-Stars/rthas/releases/latest/download/install.sh | bash
#
# Override with:
#   PREFIX=$HOME/.rthas VERSION=v0.1.0 bash install.sh

set -euo pipefail

REPO="${RTHAS_REPO:-XuQianJin-Stars/rthas}"
PREFIX="${PREFIX:-${HOME}/.rthas}"
BIN_DIR="${PREFIX}/bin"
VERSION="${VERSION:-latest}"

need() {
  command -v "$1" >/dev/null 2>&1 || {
    echo "rthas: need '$1' on PATH" >&2
    exit 1
  }
}

need curl
need uname

os="$(uname -s)"
arch="$(uname -m)"
case "${os}" in
  Linux) os=linux ;;
  Darwin) os=darwin ;;
  *)
    echo "rthas: unsupported OS '${os}' (Linux or macOS)" >&2
    exit 1
    ;;
esac
case "${arch}" in
  x86_64 | amd64) arch=amd64 ;;
  aarch64 | arm64) arch=arm64 ;;
  *)
    echo "rthas: unsupported arch '${arch}'" >&2
    exit 1
    ;;
esac

asset="rthas-${os}-${arch}"
if [ "${VERSION}" = "latest" ]; then
  url="https://github.com/${REPO}/releases/latest/download/${asset}"
else
  url="https://github.com/${REPO}/releases/download/${VERSION}/${asset}"
fi

mkdir -p "${BIN_DIR}"
tmp="$(mktemp "${TMPDIR:-/tmp}/rthas.XXXXXX")"
trap 'rm -f "${tmp}"' EXIT

echo "rthas: downloading ${asset} (${VERSION})"
if ! curl -fL --retry 3 --retry-delay 1 -o "${tmp}" "${url}"; then
  echo "rthas: download failed: ${url}" >&2
  echo "rthas: publish a GitHub Release (tag v*) so the binary exists." >&2
  exit 1
fi
chmod +x "${tmp}"
mv "${tmp}" "${BIN_DIR}/rthas"
trap - EXIT

echo
echo "rthas installed to ${BIN_DIR}/rthas"
echo
if ! command -v rthas >/dev/null 2>&1 || [ "$(command -v rthas)" != "${BIN_DIR}/rthas" ]; then
  echo "Add to PATH:"
  echo "  export PATH=\"${BIN_DIR}:\$PATH\""
  echo
fi
echo "Next:"
echo "  rthas"
echo "  sudo rthas attach --ebpf <pid>    # Linux, uninstrumented process"
echo "  rthas attach <pid>                # process built with #[rthas::trace]"
echo
echo "Docs: https://xuqianjin-stars.github.io/rthas/install"
