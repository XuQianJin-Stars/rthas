# Install

rthas is a Rust workspace: a library you link into the target process, plus a CLI that talks to it over a Unix socket.

## Library

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

## CLI

From the repo root:

```bash
cargo run --bin rthas -- --help
# or
cargo install --path rthas-cli
rthas ps
```

The protocol is line-oriented text over a Unix socket, so `nc` works as a fallback client. The CLI has no extra crates.

## Platforms

| Feature | Linux | macOS |
|---|---|---|
| In-process probes, `trace` / `watch` / `stack` | ✅ | ✅ |
| `attach` (deferred agent) | ✅ | ✅ |
| `dashboard` / `thread` CPU | `/proc` deltas | Mach occupancy ratio |
| `attach --ebpf` | kernel 5.5+, root / `CAP_BPF`, symbols | not available |

## eBPF helper (Linux)

Only needed for `rthas attach --ebpf` on a process **without** `#[rthas::trace]`:

```bash
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
rustup target add bpfel-unknown-none --toolchain nightly
cargo install bpf-linker
cargo build -p rthas-cli -p rthas-ebpf
```

`bpfel-unknown-none` has no prebuilt std. The helper compiles the BPF object with `cargo +nightly -Z build-std=core`. Details: [eBPF attach](/ebpf).

## Docs site (this page)

```bash
cd site
npm install
npm run docs:dev
```
