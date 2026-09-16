# All Commands

Patterns use shell-style globbing: `*` is a wildcard; with no `*`, the match is a substring. `get_status` finds `goosefs_sdk::client::master::MasterClient::get_status`. A pattern *with* a `*` is tried at every position, so `MasterClient::*` finds it too.

| Command | Description |
|---|---|
| `ps` | List processes exposing an rthas agent |
| `ps --all [filter]` | List every process, flagging those built with `#[rthas::trace]` |
| `attach <pid>` | Start the agent inside a running process that deferred it |
| `attach --ebpf <pid>` | eBPF uprobes on an uninstrumented process (Linux + root + symbols) |
| `list [pattern]` | Enumerate instrumented functions |
| `on <pattern>` / `off [pattern]` | Toggle probes (off by default) |
| [`trace`](/trace) | Stream call trees |
| [`watch`](/watch) | One line per call |
| [`stack`](/stack) | Call path that reached each matching call |
| [`stats`](/stats) | p50 / p95 / p99 / max |
| [`top`](/stats) | Hottest or slowest functions |
| [`monitor`](/monitor) | Periodic method stats |
| [`tt`](/tt) | Record calls into the time tunnel |
| [`dashboard`](/dashboard) | Live process overview |
| [`thread`](/thread) | Per-thread CPU + last span / native stacks |
| [`profiler`](/profiler) | CPU or wall-clock sampling |
| `memory` | OS memory: rss / virt / threads / fds |
| `jvm` (`runtime`) | Process snapshot: os / arch / rustc / features / memory |
| `sysprop [NAME]` | Read-only knobs (`os`, `rustc`, `rthas`) |
| `sysenv [NAME]` | Process environment (read-only) |
| `session` | pid, socket, probes, ring, tunnel, profiler, auth |
| `options` / `vmoption` `[name] [value]` | List or set runtime knobs (`max-str`, `tz-hours`) |
| `sm [pattern]` | Alias of `list` |
| `jobs` / `kill N` | Background jobs; `<cmd> > FILE &` |
| `history [N]` / `history -c` | Command history |
| `cls` / `keymap` | Clear screen / supported keys |
| `base64 [-d] PATH` | Encode / decode a file (2 MiB cap) |
| `-f FILE` / `-c COMMAND` | Batch script (CLI) |
| `auth [password]` | Authenticate when `RTHAS_PASSWORD` is set |
| `pwd` / `cat PATH` / `echo ...` | Working directory and files (cat capped at 2 MiB) |
| `<cmd> \| grep PATTERN` | Pipe: `-i -v -n -c -m N -A N -B N -C N` (substring; no regex) |
| `<cmd> \| tee [-a] FILE` / `<cmd> \| wc` | Copy output / count lines |
| `version` | rthas library version in the target process |
| `reset` | Disable all probes |
| `stop` | Unbind the agent; `rthas attach <pid>` restarts it |
| `clear` | Drop buffered events |
| `help [command]` | Full reference, or one command (`help watch`) |

## Session extras

Pipes, jobs, and history live in the interactive shell (`rthas shell`) and in `-c` / `-f` batch scripts:

```text
rthas> list | grep handle
rthas> sysenv | grep -i path | wc
rthas> monitor handle --interval 1 --count 0 > /tmp/mon.log &
rthas> jobs
```

`jobs` / `&` / `kill` have no fg/bg/ctrl-z. `cat` and `base64` are capped at 2 MiB.
