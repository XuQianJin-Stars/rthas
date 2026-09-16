# stack

Who called this function, and how the request got there.

```bash
rthas stack checksum --native --count 2
rthas stack handle_request --depth 6
```

| Flag | Meaning |
|---|---|
| `--native` | Also print `std::backtrace` captured when the span opened |
| `--count N` | Stop after N hits |
| `--depth N` | Truncate the logical path |

## Logical vs native

Arthas's `stack` is also fragmented under async code. rthas covers both angles:

- **Logical call path** (default): reuses the in-process span tree, so it stays intact across `.await` points and thread hops — something neither eBPF nor JVMTI can give you.
- **Native stack** (`--native`): `std::backtrace` captured once when the span opens. Accurate for synchronous code; for async it only sees the frame currently being polled. **Treat the logical path as the source of truth.**

## Why async stacks fragment

`tokio` futures are state machines. After an `.await`, the current stack frame is gone — only the future's saved state remains. A native stack capture sees whichever task the worker thread happens to be polling, not the logical request that called you.

The fix: rthas records a **span tree inside the process**. Each span carries a tokio task id (`task#N` column), so spans that complete on different threads still group into one request.
