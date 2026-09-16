# rthas

> **Arthas-flavoured runtime probe toolkit for Rust**  
> `trace` / `watch` / `stack` / `stats` / `top` / `dashboard` / `thread` / `profiler` / `attach --ebpf` — without a debugger.

**Docs:** https://xuqianjin-stars.github.io/rthas

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
| `dashboard` / `thread` | JMX / JVMTI | self-sampled: `/proc`, Mach, `getrusage` + SIGURG dump | ✅ | ✅ table + `thread --n` / `<tid>` / `--all` native stacks |
| `monitor` periodic stats | bytecode instrumentation | ring buffer aggregated per interval | ✅ | ✅ implemented (also in `dashboard`) |
| `tt` time tunnel | bytecode + object refs | indexed `Debug` snapshots | ✅ | ✅ record / list / inspect (no replay) |
| `sysenv` / `memory` / `session` / `options` / `version` / `stop` | JMX / agent | process env, OS memory, runtime knobs | ✅ | ✅ implemented |
| `jvm` / `sysprop` | JMX | rustc/os/features snapshot; read-only knobs | ✅ | ✅ implemented (`runtime` aliases `jvm`) |
| `auth` + pipes (`grep` / `tee` / `wc`) / `pwd` / `cat` | telnet session | `RTHAS_PASSWORD` per connection; in-process pipes | ✅ | ✅ implemented |
| `jobs` / `&` / `kill` / `>` | async jobs | background command + log file | ✅ | ✅ implemented (no fg/bg/ctrl-z) |
| `history` / `cls` / `base64` / batch `-f` | telnet / as.sh | in-process history; CLI `-f` / `-c` | ✅ | ✅ implemented |
| Restart-free attach to an **instrumented** process | Attach API | trigger file wakes a deferred agent | ✅ | ✅ implemented |
| `profiler` flame graph | async-profiler | SIGPROF (cpu) / ITIMER_REAL (wall) | ✅ | ✅ start / stop / status (`--event cpu\|wall`) |
| Restart-free attach to an **un-instrumented** process | Attach API | eBPF uprobe only (Linux + root + symbols) | ⚠️ | ✅ `attach --ebpf` (`trace`/`watch`/`stats`/`top`/`monitor`/`dashboard`/`thread` + `/proc`; no Debug args, no native stacks) |
| `jad` / `redefine` / `retransform` / `dump` / `mc` / `classloader` | runtime class redefinition | impossible (machine code is not rewritable) | ❌ | — |
| `ognl` / `getstatic` / `heapdump` / `mbean` / `jfr` / `vmtool` | JVM object model | no VM heap or bytecode | ❌ | — |
| `logger` / `perfcounter` / web-console / HTTP API | JVM / Spring / telnet UI | no JVM logger, JMX beans, or HTTP server | ❌ | — |
| `profiler --event alloc/lock` / `tt -p` replay | async-profiler / OGNL | no JVM alloc sampler; tt stores Debug strings | ❌ | — |

`attach` splits in two, and each half takes its own route:

- **The instrumented half is done**: as long as the binary carries `#[rthas::trace]`, you can take it over while it runs — no restart, no recompile, no root, on both Linux and macOS. See [Attaching](#attaching-to-a-running-process).
- **The un-instrumented half is Linux-only**: `rthas attach --ebpf <pid>` loads uprobes from outside the process. It needs a Linux kernel (5.5+), root or `CAP_BPF`, and a binary that still has symbols. You get function names and latency via `trace` / `watch` / `stats` / `top` / `monitor`, plus `dashboard` / `thread` from `/proc`; there are no `Debug` arguments, and async call trees will fragment. See [eBPF attach](#ebpf-attach-uninstrumented-processes).

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
cargo run --bin rthas -- thread --by cpu --n 3

# CPU sample for a few seconds (text tree; --format flamegraph writes SVG)
cargo run --bin rthas -- profiler --seconds 5
# Wall clock: samples even while the process is asleep in .await
cargo run --bin rthas -- profiler --seconds 5 --event wall

# Periodic method stats (enables probes for the duration)
cargo run --bin rthas -- monitor handle_request --interval 1 --count 3

# Time tunnel: record, then inspect later (no replay — Debug strings only)
cargo run --bin rthas -- tt handle_request --count 5
cargo run --bin rthas -- tt --list
cargo run --bin rthas -- tt --index 1000

# Runtime snapshot (Arthas jvm / sysprop analog; knobs are read-only)
cargo run --bin rthas -- jvm
cargo run --bin rthas -- sysprop rustc

# Interactive session
cargo run --bin rthas -- shell
#   rthas> list | grep handle
#   rthas> sysenv | grep -i path | wc
#   rthas> monitor handle --interval 1 --count 0 > /tmp/mon.log &
#   rthas> jobs
```

## Commands

| Command | Description |
|---|---|
| `ps` | List processes exposing an rthas agent |
| `ps --all [filter]` | List every process, flagging those built with `#[rthas::trace]` |
| `attach <pid>` | Start the agent inside a running process that deferred it |
| `attach --ebpf <pid>` | eBPF uprobes on an uninstrumented process (Linux + root + symbols) |
| `list [pattern]` | Enumerate instrumented functions |
| `on <pattern>` / `off [pattern]` | Toggle probes (off by default) |
| `trace <pattern> [--count N] [--seconds F] [--depth N] [--min-ms F] [--grace-ms N]` | Stream call trees |
| `watch <pattern> [--args S] [--ret S] [--error] [--success] [--count N]` | One line per call (`--error` / `--success` = Arthas `-e` / `-s`) |
| `stack <pattern> [--native] [--count N] [--depth N]` | Call path that reached each matching call |
| `stats [pattern]` | p50 / p95 / p99 / max (in-process: ring; eBPF: attach for `--seconds`, default 3s) |
| `top [pattern] [--n N] [--by total\|max\|count]` | Hottest or slowest functions |
| `monitor [pattern] [--interval F] [--count N] [--seconds F]` | Periodic method stats (eBPF: fail-rate always 0) |
| `tt <pattern> [--count N] [--args S] [--ret S]` | Record calls into the time tunnel |
| `tt --list [pattern]` / `tt --index N` / `tt --delete N` / `tt --clear` | List / inspect / drop fragments (no `tt -p` replay) |
| `dashboard [--interval F] [--count N] [--n N]` | Live process overview, refreshes until Ctrl-C |
| `thread [--n N] [--by tid\|cpu\|name] [<tid>] [--all] [--state S]` | Per-thread CPU + last span; `--n` / `<tid>` / `--all` dump native stacks (in-process only) |
| `profiler start\|stop\|status` | Sampling. `--event cpu` (default, SIGPROF) or `wall` (ITIMER_REAL). `--seconds F`; `--include`/`--exclude`; `--format text\|collapsed\|flamegraph` |
| `memory` | OS memory: rss / virt / threads / fds |
| `jvm` (`runtime`) | Process snapshot: os / arch / rustc / features / memory |
| `sysprop [NAME]` | Read-only knobs (`os`, `rustc`, `rthas`); no `System.setProperty` |
| `sysenv [NAME]` | Process environment (read-only) |
| `session` | pid, socket, probes, ring, tunnel, profiler, auth |
| `options` / `vmoption` `[name] [value]` | List or set runtime knobs (`max-str`, `tz-hours`) |
| `sm [pattern]` | Alias of `list` (Arthas search-method) |
| `jobs` / `kill N` | Background jobs; `<cmd> > FILE &` |
| `history [N]` / `history -c` | Command history |
| `cls` / `keymap` | Clear screen / supported keys |
| `base64 [-d] PATH` | Encode / decode a file (2 MiB cap) |
| `-f FILE` / `-c COMMAND` | Batch script (CLI) |
| `auth [password]` | Authenticate this connection when `RTHAS_PASSWORD` is set |
| `pwd` / `cat PATH` / `echo ...` | Process working directory and files (cat capped at 2 MiB) |
| `<cmd> \| grep PATTERN` | Pipe: `-i -v -n -c -m N -A N -B N -C N` (substring; no regex) |
| `<cmd> \| tee [-a] FILE` / `<cmd> \| wc` | Copy output to a file / count lines |
| `version` | rthas library version in the target process |
| `reset` | Disable all probes |
| `stop` | Unbind the agent; `rthas attach <pid>` restarts it |
| `clear` | Drop buffered events |
| `help [command]` | Full reference, or one command (`help watch`) |

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

`CPU` / `MEM` / `load1` / `threads` are read straight from the OS by `sample.rs`, with no JVM-like middle layer in between; the lower half aggregates the ring buffer incrementally per refresh interval. For Arthas-style method stats on their own (and to enable matching probes automatically), use `monitor`.

### `profiler` output

```text
$ rthas profiler --seconds 3
Started [cpu] profiling at 99 Hz for 3.0s
Stopped [cpu] profiling. samples=280 elapsed=3.0s hz=99
── rthas profiler [cpu] ── 3.0s ── 99 Hz ── 280 samples ──
  SHARE  SAMPLES  STACK
  100.0%     280  example_app::handle_request
   48.2%     135    example_app::crunch
   21.4%      60    example_app::lookup_metadata
```

Sampling uses SIGPROF (`pprof`) for `--event cpu` (the default) and ITIMER_REAL / SIGALRM for `--event wall`. Nothing is installed until `profiler start` or `--seconds`. `--include` / `--exclude` are comma-separated globs matched against any frame in a sample (`profiler --seconds 3 --include crunch --exclude tokio`). The CPU text tree omits tokio / std / pthread frames so your functions surface (`--full` keeps them); wall keeps those frames so you can see park / poll. Wall samples the thread that receives SIGALRM each tick (often the runtime park thread), not every worker. `--format collapsed` is input for speedscope / `flamegraph.pl`; `--format flamegraph` writes an SVG (`--file` or `/tmp/rthas-<pid>.svg`). This is a **native** stack sample — async work shows up as `poll`, not as the logical request. Use `trace` for that. Not available on `attach --ebpf`. CPU mode only ticks while the process is on CPU; a service that is mostly `.await`ing looks idle until you pass `--event wall`.

`thread` without flags is the CPU table. `thread --n 3`, `thread <tid>`, and `thread --all` interrupt those threads with SIGURG and print a native stack (most recent frame first, like Arthas). `thread --state running` (also `sleeping` / `disk` / `stopped` / `zombie`, plus JVM names like `RUNNABLE`) keeps one OS state. A thread that is asleep may not respond; `--full` keeps tokio/std/pthread frames. On eBPF attach the table comes from `/proc/<pid>/task` and there are no native stacks.

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

## eBPF attach (uninstrumented processes)

When the target was **not** built with `#[rthas::trace]`, the only attach path
is eBPF uprobes. This is Linux-only.

```bash
# Build the helper next to the CLI (Aya needs nightly + bpfel-unknown-none + bpf-linker)
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

Limits, on purpose:

- Function **name + latency** only. No `Debug` arguments or return values
  (`--args` / `--ret` are ignored). `stats` / `top` / `monitor` work, but
  ERR / fail-rate are always 0. `dashboard` / `thread` read `/proc/<pid>`
  (no native stacks, no last-span column).
- At most 64 symbols per `trace`/`watch`/`stats`/`top`/`monitor`. `std::` /
  `core::` / `alloc::` are skipped unless the pattern names them.
- Async call trees fragment: a uretprobe fires when `poll` returns, not when
  the future completes. Prefer `#[rthas::trace]` for async services.
- The binary must still have symbols (`strip` makes attach fail).
- macOS: `attach --ebpf` errors immediately; there is no kprobe/uprobe API.

The function-uprobe path will keep these limits. Protocol probes (HTTP/gRPC)
are the planned way to get fail-rate and stripped-binary visibility without
reading Rust values — see [Roadmap](#roadmap).

Toolchain for compiling `rthas-ebpf` on Linux:

```bash
rustup toolchain install nightly
rustup component add rust-src --toolchain nightly
rustup target add bpfel-unknown-none --toolchain nightly
cargo install bpf-linker
```

## Roadmap

rthas stays an Arthas-style diagnostic CLI for **one Rust process**. The next
work borrows protocol-level observation from [OpenTelemetry eBPF
Instrumentation](https://opentelemetry.io/docs/zero-code/obi/) (OBI / Beyla):
hook the request on the socket, not another function symbol. It does **not**
turn rthas into an APM backend (no web console, no cluster topology, no AI
root-cause product). In-process `#[rthas::trace]` remains the source of truth
for async call trees and `Debug` arguments.

Implement in this order. Each row is a slice that should land on its own.

| # | Slice | Why | Intended surface |
|---|---|---|---|
| 1 | **Protocol probes** on `attach --ebpf`: HTTP/1.1 then gRPC, via sock/kprobe (or sockops) filtered to the target pid. Userspace parses the request line / status. | Function uprobes cannot see “this process is slow on `/api`” or a 5xx. A request-boundary span does not fragment across tokio `poll`. | `watch http --status 5xx`, `stats http --path '/api/*'`, `top grpc --by max`, a RED row on `dashboard` |
| 2 | **Fail-rate from protocol status** on `stats` / `top` / `monitor` / `dashboard`. | eBPF `monitor` ERR is always 0 today because return values are not readable. HTTP 4xx/5xx and gRPC status fill that without `Debug`. | Same commands; ERR column becomes real on the protocol path |
| 3 | **Stripped binaries**: protocol attach must work when the function-uprobe path refuses to start. | `strip` currently makes `attach --ebpf` fail entirely. Protocol probes do not need the target's symbol table. | `attach --ebpf` succeeds; `list` shows `http` / `grpc` even with no function symbols; function `trace` still needs symbols |
| 4 | **Optional OTLP export** of in-process spans and protocol spans. Apache-2.0 in, OTLP out — the user picks Jaeger / Grafana / any collector. Do not vendor an APM UI (Databuff is AGPL-3.0). | README marks web-console / HTTP API as out of scope; a sidecar export is the license-safe substitute. | `RTHAS_OTLP_ENDPOINT=http://127.0.0.1:4318` (CLI unchanged) |
| 5 | **`net`**: peers of *this* pid (fd, proto, peer, bytes, optional RTT / retransmit). | Cluster service graphs are APM. A local connection table is `dashboard` / `thread` for sockets. | `rthas net --pid 1234` |
| 6 | **Read** W3C `traceparent` on captured HTTP (never inject / rewrite packets by default). | Lets an inbound request line up with an outbound call. Injecting headers mutates production traffic. | Extra column or filter on `watch http`; correlation id if OTLP export is on |
| 7 | **BTF / CO-RE** when the first kprobe/sock program lands. Function uprobes can stay on kernel 5.5; socket programs should follow OBI's bar (5.8+, or RHEL-family 4.18+ with backports). | Portable kprobe programs need BTF. Not a user-facing feature on its own. | Documented in the eBPF toolchain notes when that program exists |
| 8 | **Do not double-uprobe** a process that already has an in-process agent. If `/tmp/rthas-<pid>.sock` is a live rthas library, skip function symbols; protocol probes may still attach. | OBI disables duplicate OTel signals. `attach` vs `attach --ebpf` should auto-prefer the library when both are possible. | `attach --ebpf` prints that it is protocol-only because the process already has probes |
| 9 | **TLS**: OpenSSL / BoringSSL `SSL_read` / `SSL_write` uprobes first (plaintext HTTP over TLS). rustls has none of those symbols — the kernel sees ciphertext. rustls comes later, and only with symbols or a hyper/tokio hook. | Many Rust services terminate TLS in rustls, not OpenSSL. Claim plaintext HTTPS only where the probe can actually see it. | `watch https` when OpenSSL is present; rustls documented as a follow-up |

Leave out of rthas on purpose:

- A web console, Grafana app, or in-tree APM backend
- AI root-cause analysis (dump `trace` / `tt` to an external model if needed)
- Kubernetes DaemonSet / host-wide, language-agnostic collection
- Kafka / Mongo / GenAI / GPU protocol catalogues
- Restoring arbitrary Rust functions and `Debug` args from a stripped binary

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
│  │  tunnel.rs    — indexed tt fragments    │ │
│  │  pipe.rs      — grep / tee / wc pipes   │ │
│  │  jobs.rs      — background `&` / kill   │ │
│  │  history.rs   — command history         │ │
│  │  base64.rs    — encode / decode a file  │ │
│  │  tree.rs      — forest → render         │ │
│  │  probe.rs     — static sites + registry │ │
│  │  span.rs      — thread-local stack      │ │
│  │  sample.rs    — OS metrics: /proc, Mach │ │
│  │  runtime.rs   — jvm / sysprop snapshot  │ │
│  │  thread_dump.rs — SIGURG native stacks  │ │
│  │  profiler.rs  — SIGPROF CPU sampling    │ │
│  │  time.rs      — monotonic + wall-clock  │ │
│  └──────────────────────────────────────────┘ │
│  ┌─ rthas-ebpf (Linux helper) ────────────┐ │
│  │  uprobe/uretprobe → same socket proto  │ │
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
| `RTHAS_TT_CAPACITY` | `100` | Time-tunnel fragments retained |
| `RTHAS_MAX_STR` | `256` | Max chars per arg/return value (`options max-str` can change this later) |
| `RTHAS_TZ_HOURS` | `0` (UTC) | Display timezone offset (`options tz-hours` can change this later) |
| `RTHAS_PASSWORD` | unset (off) | If set, each connection must `auth` (or CLI `--password`) |
| `RTHAS_USERNAME` | `rthas` | Username checked by `auth --username` |
| `RTHAS_MACRO_DEBUG` | off | Print macro expansion to stderr |
| `RTHAS_DEBUG` | off | Log every event the agent ingests to stderr |

## License

Apache-2.0. See [LICENSE](LICENSE).
