# Architecture

```text
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

## Performance

| State | Cost per call |
|---|---|
| Probe disabled (default) | ~1 ns (one relaxed atomic load) |
| Probe enabled, no args captured | ~20–40 ns (Instant + Mutex + fmt) |
| Probe enabled, args formatted | depends on Debug impl size |

A disabled probe has **zero allocation** and is always branch-predicted taken. Argument formatting runs inside a closure that is only called when the probe is on, so the steady-state cost is the single load.
