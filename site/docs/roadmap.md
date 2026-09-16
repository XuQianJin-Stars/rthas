# Roadmap

rthas stays an Arthas-style diagnostic CLI for **one Rust process**. The next work borrows protocol-level observation from [OpenTelemetry eBPF Instrumentation](https://opentelemetry.io/docs/zero-code/obi/) (OBI / Beyla): hook the request on the socket, not another function symbol. It does **not** turn rthas into an APM backend (no web console, no cluster topology, no AI root-cause product). In-process `#[rthas::trace]` remains the source of truth for async call trees and `Debug` arguments.

Implement in this order. Each row is a slice that should land on its own.

| # | Slice | Why | Intended surface |
|---|---|---|---|
| 1 | **Protocol probes** on `attach --ebpf`: HTTP/1.1 then gRPC, via sock/kprobe (or sockops) filtered to the target pid. Userspace parses the request line / status. | Function uprobes cannot see “this process is slow on `/api`” or a 5xx. A request-boundary span does not fragment across tokio `poll`. | `watch http --status 5xx`, `stats http --path '/api/*'`, `top grpc --by max`, a RED row on `dashboard` |
| 2 | **Fail-rate from protocol status** on `stats` / `top` / `monitor` / `dashboard`. | eBPF `monitor` ERR is always 0 today because return values are not readable. HTTP 4xx/5xx and gRPC status fill that without `Debug`. | Same commands; ERR column becomes real on the protocol path |
| 3 | **Stripped binaries**: protocol attach must work when the function-uprobe path refuses to start. | `strip` currently makes `attach --ebpf` fail entirely. Protocol probes do not need the target's symbol table. | `attach --ebpf` succeeds; `list` shows `http` / `grpc` even with no function symbols; function `trace` still needs symbols |
| 4 | **Optional OTLP export** of in-process spans and protocol spans. Apache-2.0 in, OTLP out — the user picks Jaeger / Grafana / any collector. Do not vendor an APM UI (Databuff is AGPL-3.0). | README marks web-console / HTTP API as out of scope; a sidecar export is the license-safe substitute. | `RTHAS_OTLP_ENDPOINT=http://127.0.0.1:4318` (CLI unchanged) |
| 5 | **`net`**: peers of *this* pid (fd, proto, peer, bytes, optional RTT / retransmit). | Cluster service graphs are APM. A local connection table is `dashboard` / `thread` for sockets. | `rthas net --pid 1234` |
| 6 | **Read** W3C `traceparent` on captured HTTP (never inject / rewrite packets by default). | Lets an inbound request line up with an outbound call. Injecting headers mutates production traffic. | Extra column or filter on `watch http`; correlation id if OTLP export is on |
| 7 | **BTF / CO-RE** when the first kprobe/sock program lands. Function uprobes can stay on kernel 5.5; socket programs should follow OBI's bar (5.8+, or RHEL-family 4.18+ with backports). | Portable kprobe programs need BTF. Not a user-facing feature on its own. | Documented in the eBPF toolchain notes when that program exists |
| 8 | **Do not double-uprobe** a process that already has an in-process agent. If `/tmp/rthas-<pid>.sock` is a live rthas library, skip function symbols; protocol probes may still attach. | OBI disables duplicate OTel signals. `attach` vs `attach --ebpf` should auto-prefer the library when both are possible. | `attach --ebpf` prints that it is protocol-only because the process already has probes |
| 9 | **TLS**: OpenSSL / BoringSSL `SSL_read` / `SSL_write` uprobes first (plaintext HTTP over TLS). rustls has none of those symbols — the kernel sees ciphertext. rustls comes later, and only with symbols or a hyper/tokio hook. | Many Rust services terminate TLS in rustls, not OpenSSL. Claim plaintext HTTPS only where the probe can actually see it. | `watch https` when OpenSSL is present; rustls documented as a follow-up |

## Leave out of rthas on purpose

- A web console, Grafana app, or in-tree APM backend
- AI root-cause analysis (dump `trace` / `tt` to an external model if needed)
- Kubernetes DaemonSet / host-wide, language-agnostic collection
- Kafka / Mongo / GenAI / GPU protocol catalogues
- Restoring arbitrary Rust functions and `Debug` args from a stripped binary
