# eBPF attach (uninstrumented processes)

When the target was **not** built with `#[rthas::trace]`, the only attach path is eBPF uprobes. This is Linux-only.

```bash
# Build the helper next to the CLI (Aya needs nightly + rust-src + bpf-linker)
cargo build -p rthas-cli -p rthas-ebpf -p example-plain

./target/debug/example-plain
# example-plain pid=1234 (no rthas probes)

sudo ./target/debug/rthas attach --ebpf 1234
rthas list --pid 1234
rthas trace handle_request --count 3
rthas watch handle_request --count 5
rthas stats handle_request --seconds 3
rthas top handle_request --n 10
rthas monitor handle_request --interval 1 --count 3
rthas dashboard --count 1 --pid 1234
rthas thread --pid 1234
rthas stop --pid 1234
```

## Limits, on purpose

- Function **name + latency** only. No `Debug` arguments or return values (`--args` / `--ret` are ignored). `stats` / `top` / `monitor` work, but ERR / fail-rate are always 0. `dashboard` / `thread` read `/proc/<pid>` (no native stacks, no last-span column).
- At most 64 symbols per `trace` / `watch` / `stats` / `top` / `monitor`. `std::` / `core::` / `alloc::` are skipped unless the pattern names them.
- Async call trees fragment: a uretprobe fires when `poll` returns, not when the future completes. Prefer `#[rthas::trace]` for async services.
- The binary must still have symbols (`strip` makes function-uprobe attach fail).
- macOS: `attach --ebpf` errors immediately; there is no kprobe/uprobe API.

The function-uprobe path will keep these limits. Protocol probes (HTTP/gRPC) are the planned way to get fail-rate and stripped-binary visibility without reading Rust values — see [Roadmap](/roadmap).

## Toolchain

```bash
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
# Optional: rustup target add bpfel-unknown-none --toolchain nightly
# That target has no prebuilt std; build.rs uses -Z build-std=core either way.
cargo install bpf-linker
```
