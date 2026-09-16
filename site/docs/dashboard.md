# dashboard

Live process overview. Refreshes until Ctrl-C.

```bash
rthas dashboard --interval 1
rthas dashboard --count 1 --pid 1234
```

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

`CPU` / `MEM` / `load1` / `threads` come straight from the OS (`sample.rs`). The lower half aggregates the ring buffer per refresh interval. For method stats on their own, use [monitor](/monitor).

On eBPF attach, the OS half still works via `/proc/<pid>`; there is no last-span column.

Platform notes: Linux `/proc` gives exact per-thread CPU deltas. Mach reports an instantaneous occupancy ratio, and macOS RSS is the `getrusage` peak rather than the current value.
