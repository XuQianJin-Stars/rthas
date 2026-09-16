# Environment

| Variable | Default | Purpose |
|---|---|---|
| `RTHAS_SOCK` | auto (`/tmp/rthas-<pid>.sock`) | Agent socket path |
| `RTHAS_SOCK_DIR` | `/tmp` | Socket directory |
| `RTHAS_AGENT` | `1` | `0` skips the agent; `lazy` defers it until `rthas attach` |
| `RTHAS_CAPACITY` | `16384` | Ring buffer size (events retained) |
| `RTHAS_TT_CAPACITY` | `100` | Time-tunnel fragments retained |
| `RTHAS_MAX_STR` | `256` | Max chars per arg/return value (`options max-str` can change this later) |
| `RTHAS_TZ_HOURS` | `0` (UTC) | Display timezone offset (`options tz-hours` can change this later) |
| `RTHAS_PASSWORD` | unset (off) | If set, each connection must `auth` (or CLI `--password`) |
| `RTHAS_USERNAME` | `rthas` | Username checked by `auth --username` |
| `RTHAS_MACRO_DEBUG` | off | Print macro expansion to stderr |
| `RTHAS_DEBUG` | off | Log every event the agent ingests to stderr |
| `RTHAS_OTLP_ENDPOINT` | unset | Planned: optional OTLP export. See [Roadmap](/roadmap) |

License: Apache-2.0.
