# trace

Stream call trees for matching probes. Auto-enables matching probes and restores their previous state on exit.

```bash
rthas trace handle_request --count 5
rthas trace 'db::*' --min-ms 10 --depth 8
```

| Flag | Meaning |
|---|---|
| `--count N` | Stop after N root trees (default 20 in-process) |
| `--seconds F` | Stop after F seconds |
| `--depth N` | Truncate the printed tree |
| `--min-ms F` | Skip trees faster than this |
| `--grace-ms N` | Wait for in-flight calls after the count/time limit |

## Async

`trace` prints the **logical** span tree recorded inside the process. That tree stays intact across `.await` and thread hops. A native stack does not — see [stack](/stack).

On `attach --ebpf`, a uretprobe fires when `poll` returns, not when the future completes, so async trees fragment. Prefer `#[rthas::trace]` for async services.
