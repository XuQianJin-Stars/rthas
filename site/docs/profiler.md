# profiler

Sampling profiler. Nothing is installed until `profiler start` or `--seconds`.

```bash
rthas profiler --seconds 5
rthas profiler --seconds 5 --event wall
rthas profiler --seconds 3 --include crunch --exclude tokio
rthas profiler --seconds 3 --format flamegraph
rthas profiler start
rthas profiler status
rthas profiler stop
```

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

| Flag | Meaning |
|---|---|
| `--event cpu` | SIGPROF (default). Only ticks while the process is on CPU |
| `--event wall` | ITIMER_REAL / SIGALRM. Samples even during `.await` |
| `--seconds F` | Start, wait, stop |
| `--include` / `--exclude` | Comma-separated globs against any frame |
| `--format text\|collapsed\|flamegraph` | Text tree, speedscope input, or SVG |
| `--file PATH` | SVG output (`/tmp/rthas-<pid>.svg` if omitted) |
| `--full` | Keep tokio / std / pthread frames (CPU mode omits them by default) |

This is a **native** stack sample — async work shows up as `poll`, not as the logical request. Use [trace](/trace) for that. Not available on `attach --ebpf`.

A service that is mostly `.await`ing looks idle in CPU mode until you pass `--event wall`. Wall samples the thread that receives SIGALRM each tick (often the runtime park thread), not every worker.
