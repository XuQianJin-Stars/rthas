// Copyright (C) 2026 Tencent. All rights reserved.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing terms and
// limitations under the License.

use std::path::Path;

pub fn serve_linux(pid: u32, sock_dir: &Path) -> Result<(), String> {
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (pid, sock_dir);
        Err(crate::LINUX_REQUIRED.into())
    }
    #[cfg(target_os = "linux")]
    {
        linux::run(pid, sock_dir)
    }
}

#[cfg(target_os = "linux")]
mod linux {
    use std::io::Write;
    use std::path::{Path, PathBuf};
    use std::time::{Duration, Instant};

    use crate::args::{self, Args, HELP};
    use crate::bpf::BpfEngine;
    use crate::event::{KIND_ENTER, KIND_EXIT};
    use crate::format::format_dur;
    use crate::glob::glob_match;
    use crate::proc;
    use crate::stats::{self, Sample};
    use crate::symbols::{self, Symbol};
    use crate::tree::Forest;
    use crate::{DEFAULT_TRACE_COUNT, DEFAULT_WATCH_COUNT, MAX_ATTACH};
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicBool, Ordering};

    pub fn run(pid: u32, sock_dir: &Path) -> Result<(), String> {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return Err(format!("no process with pid {pid}"));
        }
        let (mut symbols, biases) = symbols::load_process_symbols(pid)?;
        let sock = sock_dir.join(format!("rthas-{pid}.sock"));
        let listener =
            args::bind_socket(&sock).map_err(|e| format!("bind {}: {e}", sock.display()))?;
        eprintln!(
            "[rthas-ebpf] pid {pid}: {} symbols, listening on {}",
            symbols.len(),
            sock.display()
        );

        let mut bpf = BpfEngine::new(pid);
        let stop = std::sync::atomic::AtomicBool::new(false);
        for stream in listener.incoming() {
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
            let stream = match stream {
                Ok(s) => s,
                Err(e) => {
                    eprintln!("[rthas-ebpf] accept: {e}");
                    continue;
                }
            };
            let result = args::handle_client(stream, |line, out| {
                dispatch(line, out, pid, &mut symbols, &biases, &mut bpf, &stop)
            });
            if let Err(e) = result {
                if e.kind() != std::io::ErrorKind::BrokenPipe {
                    eprintln!("[rthas-ebpf] client: {e}");
                }
            }
            if !sock.exists() || stop.load(std::sync::atomic::Ordering::SeqCst) {
                break;
            }
        }
        bpf.detach();
        let _ = std::fs::remove_file(&sock);
        Ok(())
    }

    fn dispatch(
        line: &str,
        out: &mut dyn Write,
        pid: u32,
        symbols: &mut [Symbol],
        biases: &[(PathBuf, u64)],
        bpf: &mut BpfEngine,
        stop: &AtomicBool,
    ) -> std::io::Result<bool> {
        let args = Args::parse(line);
        let verb = args.pos.first().copied().unwrap_or("");
        match verb {
            "help" | "?" => out.write_all(HELP.as_bytes())?,
            "ping" => writeln!(out, "pong pid={pid} probes={} backend=ebpf", symbols.len())?,
            "list" | "sm" => cmd_list(&args, symbols, out)?,
            "on" => {
                let n = set_enabled(symbols, args.pattern(), true);
                writeln!(out, "enabled {n} symbol(s) matching '{}'", args.pattern())?;
            }
            "off" => {
                if args.pattern().is_empty() {
                    for s in symbols.iter_mut() {
                        s.enabled = false;
                    }
                    writeln!(out, "disabled all symbols")?;
                } else {
                    let n = set_enabled(symbols, args.pattern(), false);
                    writeln!(out, "disabled {n} symbol(s) matching '{}'", args.pattern())?;
                }
            }
            "trace" => cmd_stream(&args, symbols, biases, bpf, true, out)?,
            "watch" => cmd_stream(&args, symbols, biases, bpf, false, out)?,
            "stats" => cmd_stats(&args, symbols, biases, bpf, out)?,
            "top" => cmd_top(&args, symbols, biases, bpf, out)?,
            "monitor" => cmd_monitor(&args, symbols, biases, bpf, out)?,
            "pwd" => cmd_pwd(pid, out)?,
            "sysenv" => cmd_sysenv(pid, &args, out)?,
            "memory" => cmd_memory(pid, out)?,
            "session" => cmd_session(pid, symbols.len(), out)?,
            "version" => writeln!(out, "rthas-ebpf {}", env!("CARGO_PKG_VERSION"))?,
            "jvm" | "runtime" => cmd_jvm(pid, out)?,
            "stop" => {
                bpf.detach();
                stop.store(true, Ordering::SeqCst);
                writeln!(out, "stopped eBPF helper for pid {pid}")?;
                return Ok(false);
            }
            "quit" | "exit" | "q" => {
                writeln!(out, "bye")?;
                return Ok(false);
            }
            "stack" | "tt" | "dashboard" | "thread" | "sysprop" | "options" | "reset" | "clear"
            | "profiler" => {
                writeln!(out, "{verb} is not available on eBPF attach")?;
            }
            other => writeln!(out, "unknown command '{other}'. try 'help'")?,
        }
        Ok(true)
    }

    fn set_enabled(symbols: &mut [Symbol], pattern: &str, on: bool) -> usize {
        let mut n = 0;
        for s in symbols.iter_mut() {
            if crate::symbols::keep_symbol(pattern, &s.demangled, &s.mangled) {
                if s.enabled != on {
                    n += 1;
                }
                s.enabled = on;
            }
        }
        n
    }

    fn cmd_list<W: Write>(args: &Args, symbols: &[Symbol], out: &mut W) -> std::io::Result<()> {
        let pattern = args.pattern();
        let hits: Vec<(usize, &Symbol)> = symbols
            .iter()
            .enumerate()
            .filter(|(_, s)| glob_match(pattern, &s.demangled) || glob_match(pattern, &s.mangled))
            .collect();
        if hits.is_empty() {
            writeln!(out, "no symbols matching '{pattern}'")?;
            return Ok(());
        }
        writeln!(out, "{:<5} {:<6} {:<8} PATH", "ID", "STATE", "KIND")?;
        for (id, s) in &hits {
            writeln!(
                out,
                "{:<5} {:<6} {:<8} {}",
                id,
                if s.enabled { "on" } else { "off" },
                "uprobe",
                s.demangled
            )?;
        }
        writeln!(out, "\n{} of {} symbol(s) shown", hits.len(), symbols.len())?;
        Ok(())
    }

    fn cmd_stream<W: Write>(
        args: &Args,
        symbols: &[Symbol],
        biases: &[(PathBuf, u64)],
        bpf: &mut BpfEngine,
        trees: bool,
        out: &mut W,
    ) -> std::io::Result<()> {
        if args.get("args").is_some() || args.get("ret").is_some() {
            writeln!(
                out,
                "warning: --args/--ret are ignored on eBPF attach (no Debug values)"
            )?;
        }
        let pattern = args.pattern();
        let ids = match symbols::select_ids(symbols, pattern) {
            Ok(ids) => ids,
            Err(e) => {
                writeln!(out, "{e}")?;
                return Ok(());
            }
        };
        if ids.len() == MAX_ATTACH {
            writeln!(
                out,
                "attaching first {MAX_ATTACH} matches (cap); tighten the pattern to see the rest"
            )?;
        }
        if let Err(e) = bpf.attach(&ids, symbols, biases) {
            writeln!(out, "attach failed: {e}")?;
            return Ok(());
        }

        let names: Vec<String> = symbols.iter().map(|s| s.demangled.clone()).collect();
        let default_count = if trees {
            DEFAULT_TRACE_COUNT
        } else {
            DEFAULT_WATCH_COUNT
        };
        let max_count = args.num("count", default_count);
        let seconds: f64 = args.num("seconds", 0.0);
        let depth = args.num("depth", 0usize);
        let min_ns = (args.num("min-ms", 0.0f64) * 1_000_000.0) as u64;
        let deadline = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));
        let mut forest = Forest::new();
        let mut printed = 0usize;

        loop {
            for ev in bpf.poll() {
                let Some(id) = bpf.lookup(ev.ip) else {
                    continue;
                };
                if ev.kind == KIND_ENTER {
                    forest.enter(ev.tid, id, ev.ts_ns);
                    continue;
                }
                if ev.kind != KIND_EXIT {
                    continue;
                }
                let Some(done) = forest.exit(ev.tid, id, ev.ts_ns) else {
                    continue;
                };
                if trees {
                    if !done.is_root || done.node.dur_ns < min_ns {
                        continue;
                    }
                    let text = crate::tree::render_tree(&done.node, &names, 0, depth);
                    if out.write_all(text.as_bytes()).is_err() {
                        bpf.detach();
                        return Ok(());
                    }
                    printed += 1;
                } else {
                    let line = format!(
                        "{:<40} {:>10}  tid={}\n",
                        names[done.node.id],
                        format_dur(done.node.dur_ns),
                        done.tid
                    );
                    if out.write_all(line.as_bytes()).is_err() {
                        bpf.detach();
                        return Ok(());
                    }
                    printed += 1;
                }
                if max_count > 0 && printed >= max_count {
                    break;
                }
            }
            if max_count > 0 && printed >= max_count {
                break;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            if out.flush().is_err() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        bpf.detach();
        let kind = if trees { "call tree(s)" } else { "call(s)" };
        writeln!(out, "\n[{printed} {kind}, uprobes detached]")?;
        Ok(())
    }

    fn cmd_stats<W: Write>(
        args: &Args,
        symbols: &[Symbol],
        biases: &[(PathBuf, u64)],
        bpf: &mut BpfEngine,
        out: &mut W,
    ) -> std::io::Result<()> {
        let Some(names) = prepare_attach(args, symbols, biases, bpf, out)? else {
            return Ok(());
        };
        let (max_count, deadline) = snapshot_window(args);
        writeln!(
            out,
            "collecting stats for '{}' (ERR always 0 on eBPF attach)",
            args.pattern()
        )?;
        let _ = out.flush();
        let samples = collect_samples(bpf, max_count, deadline);
        bpf.detach();
        write!(out, "{}", stats::render_stats(&samples, &names))?;
        Ok(())
    }

    fn cmd_top<W: Write>(
        args: &Args,
        symbols: &[Symbol],
        biases: &[(PathBuf, u64)],
        bpf: &mut BpfEngine,
        out: &mut W,
    ) -> std::io::Result<()> {
        let Some(names) = prepare_attach(args, symbols, biases, bpf, out)? else {
            return Ok(());
        };
        let n = args.num("n", 10usize);
        let by = args.get("by").unwrap_or("total");
        let (max_count, deadline) = snapshot_window(args);
        writeln!(
            out,
            "collecting top for '{}' (ERR always 0 on eBPF attach)",
            args.pattern()
        )?;
        let _ = out.flush();
        let samples = collect_samples(bpf, max_count, deadline);
        bpf.detach();
        write!(out, "{}", stats::render_top(&samples, &names, n, by))?;
        Ok(())
    }

    fn cmd_monitor<W: Write>(
        args: &Args,
        symbols: &[Symbol],
        biases: &[(PathBuf, u64)],
        bpf: &mut BpfEngine,
        out: &mut W,
    ) -> std::io::Result<()> {
        let Some(names) = prepare_attach(args, symbols, biases, bpf, out)? else {
            return Ok(());
        };
        let interval = Duration::from_secs_f64(args.num("interval", 5.0f64).max(0.05));
        let max_frames = args.num("count", 0usize);
        let seconds: f64 = args.num("seconds", 0.0);
        let deadline = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));
        let mut forest = Forest::new();
        let mut frames = 0usize;

        loop {
            let until = Instant::now() + interval;
            let mut batch = Vec::new();
            while Instant::now() < until {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    break;
                }
                batch.extend(ingest(bpf, &mut forest));
                std::thread::sleep(Duration::from_millis(5));
            }
            let frame = stats::render_monitor(&batch, &names, &stats::utc_stamp());
            if out
                .write_all(frame.as_bytes())
                .and_then(|_| out.flush())
                .is_err()
            {
                break;
            }
            frames += 1;
            if max_frames > 0 && frames >= max_frames {
                break;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
        }
        bpf.detach();
        Ok(())
    }

    fn prepare_attach(
        args: &Args,
        symbols: &[Symbol],
        biases: &[(PathBuf, u64)],
        bpf: &mut BpfEngine,
        out: &mut dyn Write,
    ) -> std::io::Result<Option<Vec<String>>> {
        let pattern = args.pattern();
        let ids = match symbols::select_ids(symbols, pattern) {
            Ok(ids) => ids,
            Err(e) => {
                writeln!(out, "{e}")?;
                return Ok(None);
            }
        };
        if ids.len() == MAX_ATTACH {
            writeln!(
                out,
                "attaching first {MAX_ATTACH} matches (cap); tighten the pattern to see the rest"
            )?;
        }
        if let Err(e) = bpf.attach(&ids, symbols, biases) {
            writeln!(out, "attach failed: {e}")?;
            return Ok(None);
        }
        Ok(Some(symbols.iter().map(|s| s.demangled.clone()).collect()))
    }

    fn snapshot_window(args: &Args) -> (usize, Option<Instant>) {
        let (seconds, max_count) =
            stats::collect_limit(args.num("seconds", 0.0), args.num("count", 0usize));
        let deadline = seconds.map(|s| Instant::now() + Duration::from_secs_f64(s));
        (max_count, deadline)
    }

    fn collect_samples(
        bpf: &mut BpfEngine,
        max_count: usize,
        deadline: Option<Instant>,
    ) -> Vec<Sample> {
        let mut forest = Forest::new();
        let mut buf = VecDeque::new();
        loop {
            for s in ingest(bpf, &mut forest) {
                if buf.len() == stats::SAMPLE_CAP {
                    buf.pop_front();
                }
                buf.push_back(s);
            }
            if max_count > 0 && buf.len() >= max_count {
                break;
            }
            if deadline.is_some_and(|d| Instant::now() >= d) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        buf.into_iter().collect()
    }

    fn ingest(bpf: &mut BpfEngine, forest: &mut Forest) -> Vec<Sample> {
        let mut out = Vec::new();
        for ev in bpf.poll() {
            let Some(id) = bpf.lookup(ev.ip) else {
                continue;
            };
            if ev.kind == KIND_ENTER {
                forest.enter(ev.tid, id, ev.ts_ns);
                continue;
            }
            if ev.kind != KIND_EXIT {
                continue;
            }
            if let Some(done) = forest.exit(ev.tid, id, ev.ts_ns) {
                out.push(Sample {
                    id: done.node.id,
                    dur_ns: done.node.dur_ns,
                });
            }
        }
        out
    }

    fn cmd_pwd<W: Write>(pid: u32, out: &mut W) -> std::io::Result<()> {
        match std::fs::read_link(format!("/proc/{pid}/cwd")) {
            Ok(p) => writeln!(out, "{}", p.display()),
            Err(e) => writeln!(out, "pwd: {e}"),
        }
    }

    fn cmd_sysenv<W: Write>(pid: u32, args: &Args, out: &mut W) -> std::io::Result<()> {
        match std::fs::read(format!("/proc/{pid}/environ")) {
            Ok(bytes) => {
                let rows = proc::parse_environ(&bytes);
                write!(out, "{}", proc::render_sysenv(&rows, args.pattern()))
            }
            Err(e) => writeln!(out, "sysenv: {e}"),
        }
    }

    fn cmd_memory<W: Write>(pid: u32, out: &mut W) -> std::io::Result<()> {
        match std::fs::read_to_string(format!("/proc/{pid}/status")) {
            Ok(status) => write!(
                out,
                "{}",
                proc::render_memory(&proc::parse_status_memory(&status))
            ),
            Err(e) => writeln!(out, "memory: {e}"),
        }
    }

    fn cmd_session<W: Write>(pid: u32, symbols: usize, out: &mut W) -> std::io::Result<()> {
        let dir = std::env::var("RTHAS_SOCK_DIR").unwrap_or_else(|_| "/tmp".into());
        writeln!(out, " {:<12} {}", "Name", "Value")?;
        writeln!(out, "{}", "-".repeat(50))?;
        writeln!(out, " {:<12} {pid}", "PID")?;
        writeln!(out, " {:<12} {dir}/rthas-{pid}.sock", "SOCKET")?;
        writeln!(out, " {:<12} {symbols}", "SYMBOLS")?;
        writeln!(out, " {:<12} ebpf", "BACKEND")?;
        Ok(())
    }

    fn cmd_jvm<W: Write>(pid: u32, out: &mut W) -> std::io::Result<()> {
        let exe = std::fs::read_link(format!("/proc/{pid}/exe"))
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "-".into());
        let cwd = std::fs::read_link(format!("/proc/{pid}/cwd"))
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| "-".into());
        let args = std::fs::read(format!("/proc/{pid}/cmdline"))
            .map(|b| proc::parse_cmdline(&b))
            .unwrap_or_else(|_| "-".into());
        writeln!(out, "RUNTIME")?;
        writeln!(out, "{}", "-".repeat(94))?;
        writeln!(out, " {:<28} {pid}", "PID")?;
        writeln!(out, " {:<28} {exe}", "EXE")?;
        writeln!(out, " {:<28} {cwd}", "CWD")?;
        writeln!(out, " {:<28} {args}", "INPUT-ARGUMENTS")?;
        writeln!(out, " {:<28} linux", "OS")?;
        writeln!(out, " {:<28} ebpf-uprobe", "VM-NAME")?;
        Ok(())
    }
}
