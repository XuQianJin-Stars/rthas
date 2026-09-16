# watch

One line per matching call. Unlike eBPF attach, the in-process probe reads the **real** argument and return value through `Debug`.

```bash
rthas watch read_block --ret Err --count 10
rthas watch handle_request --error
rthas watch handle_request --args secret --count 5   # still printed unless #[trace(skip(...))]
```

| Flag | Meaning |
|---|---|
| `--args S` | Keep lines whose formatted args contain `S` |
| `--ret S` | Keep lines whose formatted return value contains `S` |
| `--error` | Only failing calls (Arthas `-e`) |
| `--success` | Only successful calls (Arthas `-s`) |
| `--count N` | Stop after N lines |

On `attach --ebpf`, `--args` / `--ret` are ignored: there is no `Debug`. You still get function name and latency.
