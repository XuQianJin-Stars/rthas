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
    use crate::symbols::{self, Symbol};
    use crate::tree::Forest;
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
            "list" => cmd_list(&args, symbols, out)?,
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
            "stack" | "tt" | "monitor" | "stats" | "top" | "dashboard" | "thread" | "memory"
            | "sysenv" | "jvm" | "runtime" | "sysprop" | "session" | "options" | "reset"
            | "clear" | "version" | "profiler" => {
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
}
