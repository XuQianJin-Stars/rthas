# Attaching to a running process

Arthas can attach to any JVM because a JVM will load an agent on demand. Rust is ahead-of-time compiled, so rthas asks for one thing up front: the binary must carry `#[rthas::trace]` probes. Given that, the control plane can be created **after** the process is already running.

## Defer the agent

```rust
#[tokio::main]
async fn main() {
    rthas::init_lazy();   // or RTHAS_AGENT=lazy with a plain rthas::init()
    // ... your service ...
}
```

A deferred process costs one idle thread that stats a file five times a second, and nothing else — no socket, no listener, no per-call overhead.

## Attach

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

`attach` drops a trigger file into the socket directory; the deferred thread sees it and binds. No signals — a library has no business owning `SIGUSR2` in somebody else's process — no `ptrace`, and no privileges beyond write access to the socket directory.

## Notes

- `ps --all` takes a name filter because detecting probes means reading the binary. Scanning everything on a desktop is gigabytes of I/O; with a filter it is milliseconds.
- The marker `ps --all` and `attach` look for lives in read-only data, not in the symbol table, so a **stripped** release binary is still recognised (this is the instrumented path; [eBPF attach](/ebpf) still needs symbols).

For a process that was never built with `#[rthas::trace]`, see [eBPF attach](/ebpf).
