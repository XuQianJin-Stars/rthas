# tt

Time tunnel: record `Debug` snapshots of matching calls, then list and inspect them later. There is **no** `tt -p` replay — fragments are strings, not live objects.

```bash
rthas tt handle_request --count 5
rthas tt --list
rthas tt --index 1000
rthas tt --delete 1000
rthas tt --clear
```

| Flag | Meaning |
|---|---|
| `--count N` | Record N fragments |
| `--args S` / `--ret S` | Same filters as [watch](/watch) |
| `--list [pattern]` | Show recorded fragments |
| `--index N` | Print one fragment |
| `--delete N` / `--clear` | Drop one / all |

Capacity is `RTHAS_TT_CAPACITY` (default 100). See [Environment](/env).
