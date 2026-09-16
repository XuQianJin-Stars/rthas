#!/usr/bin/env bash
# Install nightly rust-src + bpf-linker for compiling rthas-ebpf-prog.
set -euo pipefail

try_install() {
  rustup toolchain install "$1" --profile minimal --component rust-src
}

if try_install nightly; then
  rustc +nightly -vV
else
  echo "::warning::latest nightly is missing rust-src; walking back dates"
  found=0
  for i in $(seq 1 14); do
    spec="nightly-$(date -u -d "$i days ago" +%F)"
    if try_install "$spec"; then
      rustup toolchain link nightly "$(rustc "+$spec" --print sysroot)"
      rustc +nightly -vV
      found=1
      break
    fi
  done
  if [ "$found" -ne 1 ]; then
    echo "::error::no nightly with rust-src in the last 14 days"
    exit 1
  fi
fi
