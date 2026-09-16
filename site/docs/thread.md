# thread

Per-thread CPU and what each thread last did. Native stacks are in-process only.

```bash
rthas thread --by cpu --n 3
rthas thread 12345
rthas thread --all
rthas thread --state running
```

| Flag | Meaning |
|---|---|
| `--n N` | Dump native stacks for the top N threads |
| `<tid>` | Dump one thread |
| `--all` | Dump every thread |
| `--by tid\|cpu\|name` | Sort |
| `--state S` | Keep one OS state (`running` / `sleeping` / `disk` / `stopped` / `zombie`, plus JVM names like `RUNNABLE`) |
| `--full` | Keep tokio / std / pthread frames |

Without flags, `thread` is the CPU table. `--n`, `<tid>`, and `--all` interrupt those threads with SIGURG and print a native stack (most recent frame first, like Arthas). A thread that is asleep may not respond.

On eBPF attach the table comes from `/proc/<pid>/task` and there are no native stacks.

`--by cpu` is an instantaneous reading on macOS; every other field is the same as Linux.
