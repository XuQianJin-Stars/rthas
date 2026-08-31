# rthas

> **Arthas-flavoured runtime probe toolkit for Rust**  
> `trace` / `watch` / `stack` / `stats` / `top` / `dashboard` / `thread` — without a debugger.

Java gets [Arthas](https://github.com/alibaba/arthas) because the JVM can rewrite bytecode at runtime (Instrumentation), attach to a live process (Attach API), and redefine classes on the fly (JVMTI). Rust is ahead-of-time compiled to machine code with no VM layer, so those tricks are simply unavailable.

`rthas` takes the part that **is achievable** in Rust and makes it pleasant:

```text
  #[rthas::trace]              →  rthas::init()          →  rthas trace 'db::*'
  compile-time probe points       background agent            call trees, live
```

## What you get vs Arthas

| Arthas capability | Java relies on | Rust approach | Feasible | Status |
|---|---|---|---|---|
| `trace` call path + latency | bytecode instrumentation | proc-macro probes | ✅ | ✅ implemented |
| `watch` args / return value | bytecode instrumentation | proc-macro, reads the real value | ✅ | ✅ implemented |
| `stack` who called me | JVMTI | in-process span path + native stack | ✅ | ✅ implemented (`--native`) |
| `dashboard` / `thread` | JMX / JVMTI | self-sampled: `/proc`, Mach, `getrusage` | ✅ | ✅ implemented |
| `monitor` periodic stats | bytecode instrumentation | ring buffer aggregated per interval | ✅ | ✅ built into `dashboard` |
| Restart-free attach to an **instrumented** process | Attach API | trigger file wakes a deferred agent | ✅ | ✅ implemented |
| Restart-free attach to an **un-instrumented** process | Attach API | eBPF uprobe only (Linux + root + symbols) | ⚠️ | ❌ not implemented |
| `jad` decompile / `redefine` hot swap | runtime class redefinition | impossible (machine code is not rewritable) | ❌ | — |

`attach` splits in two, and each half takes its own route:

- **The instrumented half is done**: as long as the binary carries `#[rthas::trace]`, you can take it over while it runs — no restart, no recompile, no root, on both Linux and macOS. See [Attaching](#attaching-to-a-running-process).
- **The un-instrumented half is not**: taking over a Rust process that was never instrumented has only one route, eBPF uprobes, and it needs a Linux kernel, root or `CAP_BPF`, and a binary with symbols. rthas has no such backend today.

### About `stack`

Arthas's `stack` is also fragmented under async code. `rthas` covers both angles:

- **Logical call path** (default): reuses the in-process span tree, so it stays intact across `.await` points and thread hops — something neither eBPF nor JVMTI can give you.
- **Native stack** (`--native`): `std::backtrace` captured once when the span opens; accurate for synchronous code, but for async it only sees the frame currently being polled, so **treat the logical path as the source of truth**.

### Why async stacks are fragmented

`tokio` futures are state machines. After an `.await`, the current stack frame is gone — only the future's saved state remains. A native stack capture (`backtrace-rs`) sees whichever task the worker thread happens to be polling right now, not the logical request that called you.

**The fix**: `rthas` records a **span tree inside the process**, not from outside. Each span carries a tokio task id (`task#N` column), so even when spans complete on different threads they group back into one logical request. This is why process-level instrumentation beats eBPF for async code.

## Quick start

### 1. Add dependencies

```toml
[dependencies]
rthas = { version = "0.1", features = ["tokio-task"] }
```

### 2. Instrument your functions

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

### 3. Start the agent

```rust
#[tokio::main]
async fn main() {
    rthas::init();   // ← starts background control-plane thread
    // ... your service ...
}
```

### 4. Inspect from another terminal

```bash
# Discover running agents
cargo run --bin rthas -- ps

# Stream call trees (auto-enables probes, restores state on exit)
cargo run --bin rthas -- trace handle_request --count 5

# One line per call, filter by return value
cargo run --bin rthas -- watch read_block --ret Err --count 10

# Who called it, and how the request got there
cargo run --bin rthas -- stack checksum --native --count 2

# Percentiles over the rolling ring buffer
cargo run --bin rthas -- stats

# Slowest functions
cargo run --bin rthas -- top --n 10 --by max

# Live process overview (refreshes until Ctrl-C)
cargo run --bin rthas -- dashboard --interval 1

# Per-thread CPU and what each thread last did
cargo run --bin rthas -- thread --by cpu --n 5

# Interactive session
cargo run --bin rthas -- shell
```

## Commands

| Command | Description |
|---|---|
| `ps` | List processes exposing an rthas agent |
| `ps --all [filter]` | List every process, flagging those built with `#[rthas::trace]` |
| `attach <pid>` | Start the agent inside a running process that deferred it |
| `list [pattern]` | Enumerate instrumented functions |
| `on <pattern>` / `off [pattern]` | Toggle probes (off by default) |
| `trace <pattern> [--count N] [--seconds F] [--depth N] [--min-ms F] [--grace-ms N]` | Stream call trees |
| `watch <pattern> [--args S] [--ret S] [--count N]` | One line per call |
| `stack <pattern> [--native] [--count N] [--depth N]` | Call path that reached each matching call |
| `stats [pattern]` | p50 / p95 / p99 / max over ring buffer |
| `top [pattern] [--n N] [--by total\|max\|count]` | Hottest or slowest functions |
| `dashboard [--interval F] [--count N] [--n N]` | Live process overview, refreshes until Ctrl-C |
| `thread [--n N] [--by tid\|cpu\|name]` | Per-thread CPU plus last recorded span |
| `clear` | Drop buffered events |
| `help` | Full reference |

Patterns use shell-style globbing: `*` is wildcard, no-`*` matches by substring. So `get_status` finds `goosefs_sdk::client::master::MasterClient::get_status`. A pattern *with* a `*` is tried at every position, so `MasterClient::*` finds it too.

### `dashboard` output

```text
── rthas dashboard ── pid 14976 ── up 00:00:09 ── 14 cores ──────────────
  CPU           0.6%                load1          5.84
  MEM        3.7 MiB                threads          17
  PROBES  6 registered · 6 enabled · 443 events buffered · 0 evicted

  probe activity over the last 0.50s
  PATH                                       COUNT    ERR       P50       MAX      TOTAL
  example_app::handle_request                    4      1  48.581ms  49.139ms  174.907ms
  example_app::lookup_metadata                   4      1  34.132ms  34.140ms  128.680ms
```

`CPU` / `MEM` / `load1` / `threads` are read straight from the OS by `sample.rs`, with no JVM-like middle layer in between; the lower half aggregates the ring buffer incrementally per refresh interval, which is the equivalent of Arthas's `monitor`.

Platform differences: `/proc` gives exact per-thread CPU deltas (Linux), while Mach only reports an instantaneous occupancy ratio (macOS), and on macOS the RSS figure is the `getrusage` peak rather than the current value. `--by cpu` is therefore an instantaneous reading on macOS; every other field is identical.

## Attaching to a running process

Arthas can attach to any JVM because a JVM will load an agent on demand. Rust is
ahead-of-time compiled, so `rthas` asks for one thing up front: the binary must
carry `#[rthas::trace]` probes. Given that, the control plane can be created
*after* the process is already running.

Start it with the agent deferred:

```rust
#[tokio::main]
async fn main() {
    rthas::init_lazy();   // or RTHAS_AGENT=lazy with a plain rthas::init()
    // ... your service ...
}
```

A deferred process costs one idle thread that stats a file five times a second,
and nothing else — no socket, no listener, no per-call overhead.

Then attach from anywhere:

```bash
# Which of my processes even carry probes?
rthas ps --all my-service
#   PID      AGENT   PROBES COMMAND
#   4711     -       yes    ./target/release/my-service

rthas attach 4711
#   attached to pid 4711 at /tmp/rthas-4711.sock
#   next: rthas list --pid 4711

rthas list --pid 4711
rthas trace handle_request --pid 4711 --count 5
```

`attach` drops a trigger file into the socket directory; the deferred thread
sees it and binds. No signals — a library has no business owning `SIGUSR2` in
somebody else's process — no `ptrace`, and no privileges beyond write access to
the socket directory.

Two notes on the mechanics:

- `ps --all` takes a name filter because detecting probes means reading the
  binary. Scanning everything on a desktop is gigabytes of I/O; with a filter it
  is milliseconds.
- The marker `ps --all` and `attach` look for lives in read-only data, not in
  the symbol table, so a stripped release binary is still recognised.

## Macro options

```rust
#[trace]                                    // default: capture all args
#[trace(name = "custom-name")]             // override displayed path
#[trace(skip(secret, password))]           // don't log these params
#[trace(self)]                             // also capture &self
#[trace(send)]                             // + Send on generated impl Future
```

## Performance

| State | Cost per call |
|---|---|
| Probe disabled (default) | ~1 ns (one relaxed atomic load) |
| Probe enabled, no args captured | ~20–40 ns (Instant + Mutex + fmt) |
| Probe enabled, args formatted | depends on Debug impl size |

A disabled probe has **zero allocation** and is always branch-predicted taken. Argument formatting runs inside a closure that is only called when the probe is on, so the steady-state cost is the single load.

## Architecture

```
┌──────────────────────────────────────────────┐
│  rthas-cli  (zero-dep, nc-compatible client) │
│         ↕  Unix socket  (line-oriented text) │
│                                              │
│  ┌─ rthas (library) ──────────────────────┐ │
│  │  agent.rs     — control-plane thread    │ │
│  │  event.rs     — bounded FIFO (16K)      │ │
│  │  tree.rs      — forest → render         │ │
│  │  probe.rs     — static sites + registry │ │
│  │  span.rs      — thread-local stack      │ │
│  │  sample.rs    — OS metrics: /proc, Mach │ │
│  │  time.rs      — monotonic + wall-clock  │ │
│  └──────────────────────────────────────────┘ │
│         ↑ inventory (link-time collection)    │
│  ┌─ rthas-macros (proc-macro) ───────────┐ │
│  │  #[trace] → static Probe + SpanGuard    │ │
│  └──────────────────────────────────────────┘ │
└──────────────────────────────────────────────┘
```

## Environment variables

| Variable | Default | Purpose |
|---|---|---|
| `RTHAS_SOCK` | auto (`/tmp/rthas-<pid>.sock`) | Agent socket path |
| `RTHAS_SOCK_DIR` | `/tmp` | Socket directory |
| `RTHAS_AGENT` | `1` | `0` skips the agent; `lazy` defers it until `rthas attach` |
| `RTHAS_CAPACITY` | `16384` | Ring buffer size (events retained) |
| `RTHAS_MAX_STR` | `256` | Max chars per arg/return value |
| `RTHAS_TZ_HOURS` | `0` (UTC) | Display timezone offset |
| `RTHAS_MACRO_DEBUG` | off | Print macro expansion to stderr |
| `RTHAS_DEBUG` | off | Log every event the agent ingests to stderr |

## License

Apache-2.0. See [LICENSE](LICENSE).
