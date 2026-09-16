# stats / top

Percentiles and hottest functions over the rolling ring buffer.

```bash
rthas stats
rthas stats handle_request
rthas top --n 10 --by max
rthas top handle --by count
```

| Command | Meaning |
|---|---|
| `stats [pattern]` | p50 / p95 / p99 / max |
| `top [pattern] [--n N] [--by total\|max\|count]` | Hottest or slowest functions |

In-process, both read the ring that `trace` / `watch` / enabled probes already fill. On `attach --ebpf` there is no ring: `stats` / `top` attach for `--seconds` (default 3s) unless `--count` is set.

eBPF cannot see return values, so ERR / fail-rate are always 0. Protocol probes on the [roadmap](/roadmap) are the planned fix.
