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

| Arthas 能力 | Java 依赖 | Rust 可行方案 | 可行性 | 状态 |
|---|---|---|---|---|
| `trace` 调用链+耗时 | 字节码增强 | proc-macro 插桩 | ✅ | ✅ 已实现 |
| `watch` 入参/返回值 | 字节码增强 | proc-macro，直接拿真值 | ✅ | ✅ 已实现 |
| `stack` 调用来源 | JVMTI | 进程内 span 路径 + 原生栈 | ✅ | ✅ 已实现（`--native`） |
| `dashboard` / `thread` | JMX / JVMTI | 自采集：`/proc`、Mach、`getrusage` | ✅ | ✅ 已实现 |
| `monitor` 周期统计 | 字节码增强 | 环形缓冲按区间聚合 | ✅ | ✅ `dashboard` 内建 |
| 不重启 attach **已插桩**进程 | Attach API | 触发文件唤醒按需 agent | ✅ | ✅ 已实现 |
| 不重启 attach **未插桩**进程 | Attach API | 仅 eBPF uprobe（Linux + root + 符号表） | ⚠️ | ❌ 未实现 |
| `jad` 反编译 / `redefine` 热更 | 运行时重定义类 | 不可能（机器码不可改写） | ❌ | — |

`attach` 分成两半，各走各的路：

- **已插桩的一半已经做完了**：只要二进制带 `#[rthas::trace]`，就能在运行中接管它，不重启、不重编译、不要 root，Linux 和 macOS 都行 —— 见 [Attaching](#attaching-to-a-running-process)。
- **未插桩的一半还没做**：接管一个从没插过桩的 Rust 进程，只有 eBPF uprobe 一条路，需要 Linux 内核、root 或 `CAP_BPF`、以及带符号表的二进制。rthas 目前没有这个后端。

### 关于 `stack`

Arthas 的 `stack` 在 async 代码下也是碎的。`rthas` 做了两手：

- **逻辑调用路径**（默认）：直接复用进程内的 span 树，所以跨 `.await`、跨线程跳转后仍然完整 —— 这是 eBPF 和 JVMTI 都给不了的。
- **原生栈**（`--native`）：`std::backtrace` 在 span 打开时抓一次，对同步代码准确；对 async 代码它只能看到当前正在 poll 的那一帧，所以**以逻辑路径为准**。

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

### `dashboard` 输出

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

`CPU` / `MEM` / `load1` / `threads` 由 `sample.rs` 直接从 OS 读，不经过 JVM 之类的中间层；下半部分是把环形缓冲按刷新区间做增量聚合，等价于 Arthas 的 `monitor`。

平台差异：`/proc` 能给到每线程的精确 CPU 增量（Linux），Mach 只给瞬时占用率（macOS），且 macOS 的 RSS 取的是 `getrusage` 峰值而非当前值。`--by cpu` 在 macOS 上是瞬时值，其余字段一致。

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
