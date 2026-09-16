# monitor

Periodic method stats, Arthas-style. Enables matching probes for the duration.

```bash
rthas monitor handle_request --interval 1 --count 3
rthas monitor --interval 1 --count 0 > /tmp/mon.log &
```

| Flag | Meaning |
|---|---|
| `--interval F` | Sample every F seconds |
| `--count N` | Number of rows (`0` means until cancelled) |
| `--seconds F` | Stop after F seconds |

`dashboard` already shows a short probe-activity table. Use `monitor` when you want that table on its own, with probes auto-enabled.

On eBPF attach, fail-rate is always 0.
