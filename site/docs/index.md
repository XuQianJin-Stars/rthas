---
layout: home
title: Home
hero:
  name: rthas
  text: Runtime probes for Rust
  tagline: Arthas-flavoured trace / watch / stack / dashboard / profiler / attach --ebpf — without a debugger.
  actions:
    - theme: brand
      text: Quick Start
      link: /quick-start
    - theme: alt
      text: Install
      link: /install
features:
  - icon: 🖥
    title: Dashboard
    details: Live CPU, memory, threads, and probe activity from /proc or Mach — no JVM layer.
  - icon: 🔬
    title: Args / return values
    details: watch reads the real Rust value via Debug, with glob filters for errors and successes.
  - icon: 🌳
    title: Call trees
    details: trace keeps a logical span tree across .await points. Native stacks are optional.
  - icon: ⚡️
    title: Flame graphs
    details: profiler samples CPU (SIGPROF) or wall clock (ITIMER_REAL) and writes SVG.
  - icon: 📎
    title: Attach
    details: Wake a deferred agent with no restart, or load eBPF uprobes on an uninstrumented Linux process.
  - icon: 🩺
    title: Observer
    details: Probes are off by default (~1 ns). The agent never stops your threads.
---
