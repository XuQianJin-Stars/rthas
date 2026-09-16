# Introduction

<p class="intro-brand">rthas</p>

rthas is an Arthas-flavoured runtime probe toolkit for Rust. It helps you troubleshoot a **running** Rust process — call trees, arguments, latency, threads, flame graphs — without a debugger and without restarting the server.

```text
  #[rthas::trace]              →  rthas::init()          →  rthas trace 'db::*'
  compile-time probe points       background agent            call trees, live
```

## Background

Java gets [Arthas](https://github.com/alibaba/arthas) because the JVM can rewrite bytecode at runtime (Instrumentation), attach to a live process (Attach API), and redefine classes on the fly (JVMTI). Rust is ahead-of-time compiled to machine code with no VM layer, so those tricks are simply unavailable.

Production networks are often unreachable from a laptop IDE. Remote debugging is worse: it suspends threads. Adding logs means another build, staging, and deploy — and the bug may vanish after restart.

rthas takes the part that **is achievable** in Rust and makes it pleasant. A developer can inspect a live process as an observer: probes start disabled, the agent never stops your threads, and there is no `ptrace`.

## Key features

- Trace method invocation to find slow sub-calls, including async span trees that survive `.await`
- Watch parameters, return values, and errors (`Debug` snapshots)
- Print the logical call path that reached a function, plus an optional native stack
- Monitor method statistics: count, error rate, p50 / p95 / p99 / max
- Dashboard for CPU, memory, threads, and probe activity
- Dump native stacks from running threads (`thread --n` / `<tid>` / `--all`)
- CPU and wall-clock profiler with text trees and flame-graph SVG
- Time tunnel: record calls, list and inspect later (no replay)
- Interactive shell with pipes (`grep` / `tee` / `wc`), jobs, history, and session auth
- Restart-free attach to an instrumented process (Linux and macOS)
- eBPF attach to an **uninstrumented** Linux process (root / `CAP_BPF` + symbols)

## What you get vs Arthas

| Arthas capability | Java relies on | Rust approach | Feasible | Status |
|---|---|---|---|---|
| `trace` call path + latency | bytecode instrumentation | proc-macro probes | ✅ | implemented |
| `watch` args / return value | bytecode instrumentation | proc-macro, reads the real value | ✅ | implemented |
| `stack` who called me | JVMTI | in-process span path + native stack | ✅ | implemented (`--native`) |
| `dashboard` / `thread` | JMX / JVMTI | `/proc`, Mach, `getrusage` + SIGURG dump | ✅ | table + native stacks |
| `monitor` periodic stats | bytecode instrumentation | ring buffer aggregated per interval | ✅ | implemented |
| `tt` time tunnel | bytecode + object refs | indexed `Debug` snapshots | ✅ | record / list / inspect (no replay) |
| `sysenv` / `memory` / `session` / `options` / `version` / `stop` | JMX / agent | process env, OS memory, runtime knobs | ✅ | implemented |
| `jvm` / `sysprop` | JMX | rustc / os / features snapshot | ✅ | implemented (`runtime` aliases `jvm`) |
| `auth` + pipes / `pwd` / `cat` | telnet session | `RTHAS_PASSWORD`; in-process pipes | ✅ | implemented |
| `jobs` / `&` / `kill` / `>` | async jobs | background command + log file | ✅ | implemented (no fg/bg/ctrl-z) |
| Restart-free attach (instrumented) | Attach API | trigger file wakes a deferred agent | ✅ | implemented |
| `profiler` flame graph | async-profiler | SIGPROF (cpu) / ITIMER_REAL (wall) | ✅ | `--event cpu\|wall` |
| Restart-free attach (uninstrumented) | Attach API | eBPF uprobe (Linux + root + symbols) | ⚠️ | `attach --ebpf` |
| `jad` / `redefine` / `retransform` / `dump` / `mc` / `classloader` | runtime class redefinition | impossible (machine code is not rewritable) | ❌ | — |
| `ognl` / `getstatic` / `heapdump` / `mbean` / `jfr` / `vmtool` | JVM object model | no VM heap or bytecode | ❌ | — |
| `logger` / `perfcounter` / web-console / HTTP API | JVM / Spring / telnet UI | no JVM logger, JMX beans, or HTTP server | ❌ | — |
| `profiler --event alloc/lock` / `tt -p` replay | async-profiler / OGNL | no JVM alloc sampler; tt stores Debug strings | ❌ | — |

`attach` splits in two:

- **Instrumented** (done): the binary carries `#[rthas::trace]`. Take it over while it runs — no restart, no recompile, no root, Linux and macOS. See [Attaching](/attach).
- **Uninstrumented** (Linux-only): `rthas attach --ebpf <pid>` loads uprobes from outside. Kernel 5.5+, root or `CAP_BPF`, unstripped binary. Function names and latency only. See [eBPF attach](/ebpf).
