# Quick Start

Follow these four steps to probe a live Rust service.

## 1. Add the crate

```toml
[dependencies]
rthas = { version = "0.1", features = ["tokio-task"] }
```

`tokio-task` records the current tokio task id on every event so async work that hops threads still groups as one request.

## 2. Instrument functions

```rust
use rthas::trace;

#[trace(send)]                          // `send` needed if you spawn this future
async fn handle_request(id: u64, path: &str) -> Result<usize, Error> {
    let meta = lookup_metadata(path).await?;
    let data = read_block(id, meta.block_id).await?;
    Ok(checksum(&data))
}

#[trace]
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().map(|b| u64::from(*b)).sum()
}
```

Macro options:

```rust
#[trace]                                    // default: capture all args
#[trace(name = "custom-name")]             // override displayed path
#[trace(skip(secret, password))]           // don't log these params
#[trace(self)]                             // also capture &self
#[trace(send)]                             // + Send on generated impl Future
```

## 3. Start the agent

```rust
#[tokio::main]
async fn main() {
    rthas::init();   // background control-plane thread
    // ... your service ...
}
```

To attach later without paying for a listener from the start, use `rthas::init_lazy()` — see [Attaching](/attach).

## 4. Inspect from another terminal

```bash
# Discover running agents
cargo run --bin rthas -- ps

# Stream call trees (auto-enables probes, restores state on exit)
cargo run --bin rthas -- trace handle_request --count 5

# One line per call, filter by return value
cargo run --bin rthas -- watch read_block --ret Err --count 10

# Who called it
cargo run --bin rthas -- stack checksum --native --count 2

# Percentiles over the rolling ring buffer
cargo run --bin rthas -- stats

# Slowest functions
cargo run --bin rthas -- top --n 10 --by max

# Live process overview
cargo run --bin rthas -- dashboard --interval 1

# Per-thread CPU and native stacks
cargo run --bin rthas -- thread --by cpu --n 3

# CPU sample (text tree; --format flamegraph writes SVG)
cargo run --bin rthas -- profiler --seconds 5
# Wall clock: samples even while the process is asleep in .await
cargo run --bin rthas -- profiler --seconds 5 --event wall

# Periodic method stats
cargo run --bin rthas -- monitor handle_request --interval 1 --count 3

# Time tunnel
cargo run --bin rthas -- tt handle_request --count 5
cargo run --bin rthas -- tt --list

# Runtime snapshot (Arthas jvm / sysprop analog)
cargo run --bin rthas -- jvm
cargo run --bin rthas -- sysprop rustc

# Interactive session
cargo run --bin rthas -- shell
#   rthas> list | grep handle
#   rthas> sysenv | grep -i path | wc
#   rthas> monitor handle --interval 1 --count 0 > /tmp/mon.log &
#   rthas> jobs
```

The workspace also ships `example-app` (instrumented) and `example-plain` (for [eBPF attach](/ebpf)).
