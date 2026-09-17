# Install

Two artefacts, like Arthas:

- **One CLI binary** — `rthas`, including the eBPF helper on Linux. Download and run.
- **A Rust crate** — `rthas` the library, linked into the target if you want `watch` arguments and async call trees.

The CLI does **not** replace `#[rthas::trace]` for Debug values. It *does* replace `cargo build -p rthas-cli -p rthas-ebpf` for operators.

## Quick install (recommended)

```bash
curl -fsSL https://github.com/XuQianJin-Stars/rthas/releases/latest/download/install.sh | bash
```

Installs to `~/.rthas/bin/rthas`. Then:

```bash
export PATH="$HOME/.rthas/bin:$PATH"
rthas

# Uninstrumented process (Linux, root / CAP_BPF, unstripped symbols)
sudo rthas attach --ebpf $(pgrep -n duckdb)
rthas trace 'goosefs_sdk::*' --count 5
```

Manual download (same files as `arthas-boot.jar`):

```bash
curl -fsSL -o rthas https://github.com/XuQianJin-Stars/rthas/releases/latest/download/rthas-linux-amd64
chmod +x rthas
sudo ./rthas attach --ebpf <pid>
```

| Asset | eBPF helper embedded |
|---|---|
| `rthas-linux-amd64` | yes |
| `rthas-linux-arm64` | yes |
| `rthas-darwin-arm64` | no (macOS has no uprobe API) |
| `rthas-darwin-amd64` | no |

Linux assets are **musl-static**. They do not need the host glibc, so TencentOS, CentOS, and RHEL work; a GNU build from Ubuntu 24.04 would require GLIBC 2.39.

You do **not** need nightly rustc or `bpf-linker` on the machine that runs the release binary. Those are only used when GitHub Actions **builds** the Linux assets.

`VERSION=v0.1.0 PREFIX=/usr/local` can be set before `install.sh`.

## Library (instrumented process)

For `watch` args / return values and async span trees, the target still links the crate:

```toml
[dependencies]
rthas = { version = "0.1", features = ["tokio-task"] }
```

From this repository:

```toml
[dependencies]
rthas = { git = "https://github.com/XuQianJin-Stars/rthas", features = ["tokio-task"] }
```

Call `rthas::init()` (or `rthas::init_lazy()`) once at startup. See [Quick Start](/quick-start).

## CLI from source

```bash
cargo install --path rthas-cli
# eBPF attach from a source tree also needs the helper next to the CLI:
cargo build -p rthas-cli -p rthas-ebpf --release
# Linux fat binary (same as the GitHub Release):
cargo build -p rthas-ebpf --release
RTHAS_EMBED_EBPF=$PWD/target/release/rthas-ebpf cargo build -p rthas-cli --release
```

The protocol is line-oriented text over a Unix socket, so `nc` works as a fallback client. The CLI crate has no extra dependencies; the helper is either a sibling file, `RTHAS_EBPF_HELPER`, or bytes embedded at compile time.

## Platforms

| Feature | Linux | macOS |
|---|---|---|
| In-process probes, `trace` / `watch` / `stack` | ✅ | ✅ |
| `attach` (deferred agent) | ✅ | ✅ |
| `dashboard` / `thread` CPU | `/proc` deltas | Mach occupancy ratio |
| `attach --ebpf` | kernel 5.5+, root / `CAP_BPF`, symbols | not available |

## eBPF helper (Linux, from source)

Only needed if you did **not** download a Release binary:

```bash
bash scripts/ci-bpf-toolchain.sh   # nightly + rust-src
cargo install bpf-linker
cargo build -p rthas-cli -p rthas-ebpf
```

`bpfel-unknown-none` has no prebuilt std. The helper compiles the BPF object with `rustup run nightly cargo -Z build-std=core`. Details: [eBPF attach](/ebpf).

## Docs site (this page)

```bash
cd site
npm install
npm run docs:dev
```
