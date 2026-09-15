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
// See the License for the specific language governing permissions and
// limitations under the License.

//! The in-process control plane.
//!
//! A background thread listens on a Unix socket and speaks a line-oriented
//! text protocol. Text (rather than protobuf or bincode) is a deliberate
//! choice: `printf 'list\n' | nc -U /tmp/rthas-1234.sock` works as a
//! zero-dependency fallback client, which matters when you are debugging a
//! box that does not have your CLI deployed.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::event::{recorder, Event};
use crate::probe::{glob_match, registry, Probe};
use crate::sample::{cpu_count, memory_info, thread_count, thread_rows, Meter, Sample};
use crate::time::{format_datetime, format_dur, now_ns, set_tz_hours, to_system_time, tz_hours};
use crate::tree::{render, render_flat, render_stacks, Forest, RenderOpts, Tree};
use crate::tunnel::tunnel;
use crate::{max_str, set_max_str};

/// Default number of root calls a `trace` collects before returning.
const DEFAULT_TRACE_COUNT: usize = 20;
/// Poll interval while streaming. 5ms keeps latency perceptually instant
/// without spinning a core.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Sentinel written after every response so clients know a reply is complete.
pub const END: &str = "<<<end>>>";

/// Set by `stop`; the accept loop checks it after each connection.
static STOP: AtomicBool = AtomicBool::new(false);

/// True while a deferred-attach watcher thread is sitting on the trigger file.
static LAZY_WATCHING: AtomicBool = AtomicBool::new(false);

/// Flags that never consume the next token (`--native --count 5`).
const VALUELESS_FLAGS: &[&str] = &["native", "list", "clear", "full", "all", "stack"];

/// Where this process's control socket lives.
///
/// `RTHAS_SOCK` wins outright; otherwise `$RTHAS_SOCK_DIR/rthas-<pid>.sock`
/// (`/tmp` by default).
pub fn socket_path() -> PathBuf {
    if let Ok(p) = std::env::var("RTHAS_SOCK") {
        return PathBuf::from(p);
    }
    let dir = std::env::var("RTHAS_SOCK_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join(format!("rthas-{}.sock", std::process::id()))
}

/// Start the control-plane thread. Returns the socket it is listening on.
///
/// Safe to call twice: the second call rebinds the same path.
pub fn spawn() -> std::io::Result<PathBuf> {
    let path = socket_path();
    if let Some(dir) = path.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }
    // A crashed process can leave a stale socket at a recycled pid.
    let _ = std::fs::remove_file(&path);

    STOP.store(false, Ordering::SeqCst);
    let listener = UnixListener::bind(&path)?;
    let announced = path.clone();
    std::thread::Builder::new()
        .name("rthas-agent".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
                if STOP.load(Ordering::SeqCst) {
                    break;
                }
                match stream {
                    Ok(s) => {
                        std::thread::spawn(|| {
                            if let Err(e) = handle_client(s) {
                                eprintln!("[rthas] client: {e}");
                            }
                        });
                    }
                    Err(e) => eprintln!("[rthas] accept: {e}"),
                }
            }
            drop(listener);
            let _ = std::fs::remove_file(&announced);
        })?;
    Ok(path)
}

fn handle_client(stream: UnixStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let mut line = String::new();
    let need_auth = password_configured();
    let mut authed = !need_auth;
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        let keep_open = dispatch(cmd, &mut writer, &mut authed)?;
        writeln!(writer, "{END}")?;
        writer.flush()?;
        if !keep_open {
            break;
        }
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Argument parsing
// ---------------------------------------------------------------------------

struct Args<'a> {
    pos: Vec<&'a str>,
    flags: HashMap<&'a str, &'a str>,
}

impl<'a> Args<'a> {
    fn parse(line: &'a str) -> Self {
        let mut pos = Vec::new();
        let mut flags = HashMap::new();
        let mut it = line.split_whitespace().peekable();
        while let Some(tok) = it.next() {
            if let Some(rest) = tok.strip_prefix("--") {
                if let Some((k, v)) = rest.split_once('=') {
                    flags.insert(k, v);
                } else if VALUELESS_FLAGS.contains(&rest) {
                    flags.insert(rest, "");
                } else {
                    // A valueless flag must not swallow the next flag, or
                    // `--native --count 5` would read `--count` as native's value.
                    let value = match it.peek() {
                        Some(next) if !next.starts_with("--") => it.next().unwrap_or(""),
                        _ => "",
                    };
                    flags.insert(rest, value);
                }
            } else {
                pos.push(tok);
            }
        }
        Self { pos, flags }
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.flags.get(key).copied()
    }

    /// Whether a valueless flag was present at all.
    fn flag(&self, key: &str) -> bool {
        self.flags.contains_key(key)
    }

    fn num<T: std::str::FromStr>(&self, key: &str, default: T) -> T {
        self.get(key)
            .and_then(|v| v.parse().ok())
            .unwrap_or(default)
    }

    /// Positional pattern; empty means "everything".
    fn pattern(&self) -> &str {
        self.pos.get(1).copied().unwrap_or("")
    }
}

// ---------------------------------------------------------------------------
// Dispatch
// ---------------------------------------------------------------------------

fn dispatch<W: Write>(line: &str, out: &mut W, authed: &mut bool) -> std::io::Result<bool> {
    let (cmd, pipes) = crate::pipe::split_pipeline(line);
    let verb = cmd.split_whitespace().next().unwrap_or("");
    if password_configured() && !*authed && !auth_free(verb) {
        writeln!(
            out,
            "command not permitted, try to use 'auth' command to authenticates."
        )?;
        return Ok(true);
    }
    if pipes.is_empty() {
        return dispatch_verb(&cmd, out, authed);
    }
    let mut pipe = match crate::pipe::Pipeline::new(&mut *out, &pipes) {
        Ok(p) => p,
        Err(e) => {
            writeln!(out, "{e}")?;
            return Ok(true);
        }
    };
    let keep = dispatch_verb(&cmd, &mut pipe, authed)?;
    pipe.finish()?;
    Ok(keep)
}

fn auth_free(verb: &str) -> bool {
    matches!(verb, "auth" | "help" | "?" | "ping" | "quit" | "exit" | "q")
}

fn password_configured() -> bool {
    std::env::var("RTHAS_PASSWORD")
        .map(|s| !s.is_empty())
        .unwrap_or(false)
}

fn expected_username() -> String {
    std::env::var("RTHAS_USERNAME").unwrap_or_else(|_| "rthas".to_string())
}

fn expected_password() -> Option<String> {
    std::env::var("RTHAS_PASSWORD")
        .ok()
        .filter(|s| !s.is_empty())
}

fn secrets_match(a: &str, b: &str) -> bool {
    let ab = a.as_bytes();
    let bb = b.as_bytes();
    let max = ab.len().max(bb.len());
    let mut diff = ab.len() ^ bb.len();
    for i in 0..max {
        let x = *ab.get(i).unwrap_or(&0);
        let y = *bb.get(i).unwrap_or(&0);
        diff |= (x ^ y) as usize;
    }
    diff == 0
}

fn dispatch_verb<W: Write>(line: &str, out: &mut W, authed: &mut bool) -> std::io::Result<bool> {
    let args = Args::parse(line);
    let verb = args.pos.first().copied().unwrap_or("");

    match verb {
        "help" | "?" => {
            out.write_all(HELP.as_bytes())?;
        }
        "ping" => {
            writeln!(
                out,
                "pong pid={} probes={}",
                std::process::id(),
                registry().len()
            )?;
        }
        "auth" => cmd_auth(&args, out, authed)?,
        "pwd" => cmd_pwd(out)?,
        "cat" => cmd_cat(&args, out)?,
        "echo" => cmd_echo(&args, out)?,
        "grep" | "tee" | "wc" => {
            writeln!(
                out,
                "{verb} is a pipe. try: <command> | {verb} ..."
            )?;
        }
        "list" => cmd_list(&args, out)?,
        "on" => {
            let n = registry().set_enabled_matching(args.pattern(), true);
            writeln!(out, "enabled {n} probe(s) matching '{}'", args.pattern())?;
        }
        "off" => {
            if args.pattern().is_empty() {
                registry().disable_all();
                writeln!(out, "disabled all probes")?;
            } else {
                let n = registry().set_enabled_matching(args.pattern(), false);
                writeln!(out, "disabled {n} probe(s) matching '{}'", args.pattern())?;
            }
        }
        "trace" => cmd_trace(&args, out)?,
        "watch" => cmd_watch(&args, out)?,
        "stack" => cmd_stack(&args, out)?,
        "dashboard" => cmd_dashboard(&args, out)?,
        "thread" => cmd_thread(&args, out)?,
        "profiler" => cmd_profiler(&args, out)?,
        "stats" => cmd_stats(&args, out)?,
        "top" => cmd_top(&args, out)?,
        "monitor" => cmd_monitor(&args, out)?,
        "tt" => cmd_tt(&args, out)?,
        "sysenv" => cmd_sysenv(&args, out)?,
        "memory" => cmd_memory(out)?,
        "version" => writeln!(out, "rthas {}", env!("CARGO_PKG_VERSION"))?,
        "session" => cmd_session(out, *authed)?,
        "options" => cmd_options(&args, out)?,
        "reset" => {
            registry().disable_all();
            writeln!(out, "disabled all probes")?;
        }
        "stop" => {
            cmd_stop(out)?;
            return Ok(false);
        }
        "clear" => {
            recorder().clear();
            writeln!(out, "cleared event buffer")?;
        }
        "quit" | "exit" | "q" => {
            writeln!(out, "bye")?;
            return Ok(false);
        }
        other => {
            writeln!(out, "unknown command '{other}'. try 'help'")?;
        }
    }
    Ok(true)
}

const HELP: &str = "\
rthas control commands
  list [pattern]                        enumerate instrumented functions
  on <pattern>                          enable probes (they are off by default)
  off [pattern]                         disable probes (no pattern = all)
  trace <pattern> [opts]                stream call trees for matching functions
     --count N        stop after N root calls (default 20, 0 = until Ctrl-C)
     --seconds F      stop after F seconds
     --depth N        only print N levels (0 = unlimited)
     --min-ms F       ignore roots faster than F milliseconds
  watch <pattern> [opts]                stream one line per matching call
     --count N        stop after N calls (default 50, 0 = until Ctrl-C)
     --args S         only calls whose arguments contain S
     --ret S          only calls whose return value contains S
  stack <pattern> [opts]                call path that reached each matching call
     --count N        stop after N trees (default 5, 0 = until Ctrl-C)
     --native         also symbolise the native stack captured on entry
     --depth N        only print N levels (0 = unlimited)
  dashboard [opts]                      live process overview (until Ctrl-C)
     --interval F     seconds between frames (default 1.0)
     --count N        stop after N frames (0 = until Ctrl-C)
     --n N            hottest probes per frame (default 5)
  thread [opts]                         per-thread CPU and last recorded span
     --n N            top N busiest threads, with native stacks (Arthas -n)
     --all            native stack of every thread
     <tid>            native stack of one thread
     --stack          dump stacks for the listed rows
     --by tid|cpu|name                  sort order (default tid; cpu when dumping)
     --full           keep tokio/std/pthread frames
     --interval F     wait for dump signals (default 0.15s)
     --depth N        stack frames shown (default 16)
  profiler [action] [opts]              CPU sampling (Arthas profiler)
     start            install SIGPROF sampler (default 99 Hz)
     stop             dump the report and uninstall
     status           whether a session is running
     getSamples       sample count of the current session
     --seconds F      one-shot: start, wait F seconds, stop
     --hz N           sample rate (1–1000, default 99)
     --format text|collapsed|flamegraph   stop output (default text)
     --file PATH      also write the report to PATH
     --n N / --depth N   hottest frames / tree depth
     --full           keep tokio/std/pthread frames in the text tree
  stats [pattern]                       p50/p95/p99/max over the ring buffer
  top [pattern] [--n N] [--by total|max|count]   slowest / hottest functions
  monitor [pattern] [opts]              periodic method stats (Arthas monitor)
     --interval F     seconds per frame (default 5.0)
     --count N        stop after N frames (0 = until Ctrl-C)
     --seconds F      stop after F seconds
  tt <pattern> [opts]                   record calls into the time tunnel
     --count N        stop after N calls (default 0 = until Ctrl-C)
     --seconds F      stop after F seconds
     --args S / --ret S                 substring filters, same as watch
  tt --list [pattern]                   list recorded fragments
  tt --index N                          inspect one fragment
  tt --delete N / tt --clear            drop one fragment / drop all
  sysenv [NAME]                         process environment (read-only)
  memory                                OS memory: rss / virt / threads / fds
  version                               rthas library version in this process
  session                               pid, socket, probes, ring, tunnel
  options [name] [value]                list or set runtime knobs
  auth [password]                       authenticate this connection (RTHAS_PASSWORD)
  pwd / cat PATH / echo ...             process cwd and files
  <cmd> | grep PATTERN | tee FILE | wc  pipes (-i -v -n -c -m -A -B -C; tee -a)
  reset                                 disable all probes (Arthas reset)
  stop                                  unbind the agent; `rthas attach` restarts it
  clear                                 drop buffered events
  quit                                  close this session

patterns are glob-ish: `*` is a wildcard, a pattern without `*` matches by
substring, so `get_status` finds `goosefs_sdk::client::master::...::get_status`.
A pattern containing `*` is tried at every position, so `MasterClient::*` finds
`goosefs_sdk::client::master::MasterClient::get_status`.
";

// ---------------------------------------------------------------------------
// Commands
// ---------------------------------------------------------------------------

fn cmd_list<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let pattern = args.pattern();
    let all = registry().all();
    let hits: Vec<&&Probe> = all.iter().filter(|p| glob_match(pattern, p.path)).collect();

    if hits.is_empty() {
        writeln!(
            out,
            "no probes matching '{pattern}' ({} probe(s) known; is the crate compiled with #[rthas::trace]?)",
            all.len()
        )?;
        return Ok(());
    }

    writeln!(
        out,
        "{:<5} {:<6} {:<6} {:<34} PATH",
        "ID", "STATE", "KIND", "LOCATION"
    )?;
    for p in &hits {
        writeln!(
            out,
            "{:<5} {:<6} {:<6} {:<34} {}",
            p.id(),
            if p.enabled() { "on" } else { "off" },
            p.kind.as_str(),
            truncate(&format!("{}:{}", p.file, p.line), 34),
            p.path,
        )?;
    }
    writeln!(out, "\n{} of {} probe(s) shown", hits.len(), all.len())?;
    Ok(())
}

/// Enable probes for the duration of a streaming command, restoring the
/// previous state afterwards so a `trace` never leaves probes hot by accident.
struct ProbeScope {
    /// `(probe, was it enabled, was it capturing stacks)`.
    saved: Vec<(&'static Probe, bool, bool)>,
}

impl ProbeScope {
    fn enable(pattern: &str) -> Self {
        Self::enter(pattern, false)
    }

    /// `capture_stack` makes the matching probes symbolise a native backtrace
    /// as each span opens. Costs far more than a span, so it is opt-in.
    fn enter(pattern: &str, capture_stack: bool) -> Self {
        let matching: Vec<&'static Probe> = registry()
            .all()
            .into_iter()
            .filter(|p| glob_match(pattern, p.path))
            .collect();
        // Snapshot before mutating: a probe the user had already turned on must
        // still be on when the command exits.
        let saved: Vec<_> = matching
            .iter()
            .map(|p| (*p, p.enabled(), p.capture_stack()))
            .collect();
        for p in &matching {
            p.set_enabled(true);
            p.set_capture_stack(capture_stack);
        }
        Self { saved }
    }

    fn is_empty(&self) -> bool {
        self.saved.is_empty()
    }
}

impl Drop for ProbeScope {
    fn drop(&mut self) {
        for (probe, enabled, stack) in &self.saved {
            probe.set_enabled(*enabled);
            probe.set_capture_stack(*stack);
        }
    }
}

fn cmd_trace<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let pattern = args.pattern();
    let max_count = args.num("count", DEFAULT_TRACE_COUNT);
    let seconds: f64 = args.num("seconds", 0.0);
    let depth = args.num("depth", 0usize);
    let min_ns = (args.num("min-ms", 0.0f64) * 1_000_000.0) as u64;

    if registry()
        .all()
        .iter()
        .all(|p| !glob_match(pattern, p.path))
    {
        writeln!(out, "no probes matching '{pattern}'. try 'list'")?;
        return Ok(());
    }

    let _scope = ProbeScope::enable(pattern);
    let opts = RenderOpts {
        depth,
        min_ns,
        show_task: true,
        show_ts: true,
        ..Default::default()
    };

    let grace = Duration::from_millis(args.num("grace-ms", 250u64).max(1));
    let deadline = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));
    let mut forest = Forest::with_grace(grace);
    let mut cursor = recorder().last_seq();
    let mut printed = 0usize;

    let debug = std::env::var("RTHAS_DEBUG").is_ok();
    loop {
        let mut seen = 0usize;
        for event in recorder().since(cursor) {
            cursor = cursor.max(event.seq);
            if debug {
                eprintln!(
                    "[rthas] ev seq={} span={} parent={} probe={}",
                    event.seq,
                    event.span,
                    event.parent,
                    registry().path_of(event.probe)
                );
            }
            forest.add(event);
            seen += 1;
        }
        // One event can release several roots (or none at all, when the only
        // root is still inside its grace window), so always drain the queue.
        while let Some(root) = forest.take_ready() {
            if let Some(tree) = forest.take(root) {
                if tree.root_dur_ns() >= opts.min_ns {
                    out.write_all(render(&tree, &opts).as_bytes())?;
                    printed += 1;
                }
            }
        }

        if debug {
            eprintln!("[rthas] tick seen={seen} printed={printed} cursor={cursor}");
        }
        if max_count > 0 && printed >= max_count {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        out.flush()?;
        std::thread::sleep(POLL_INTERVAL);
    }

    // Partial tails are still useful: show them rather than dropping them.
    for tree in forest.drain_roots() {
        if tree.root_dur_ns() >= opts.min_ns {
            out.write_all(render(&tree, &opts).as_bytes())?;
        }
    }
    writeln!(
        out,
        "\n[{printed} call tree(s), probes restored to previous state]"
    )?;
    Ok(())
}

fn cmd_watch<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let pattern = args.pattern();
    let max_count = args.num("count", 50usize);
    let seconds: f64 = args.num("seconds", 0.0);
    let args_filter = args.get("args").unwrap_or("");
    let ret_filter = args.get("ret").unwrap_or("");

    if registry()
        .all()
        .iter()
        .all(|p| !glob_match(pattern, p.path))
    {
        writeln!(out, "no probes matching '{pattern}'. try 'list'")?;
        return Ok(());
    }

    let _scope = ProbeScope::enable(pattern);
    let deadline = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));
    let mut cursor = recorder().last_seq();
    let mut printed = 0usize;

    loop {
        for event in recorder().since(cursor) {
            cursor = cursor.max(event.seq);
            // Filter on the probe path, not just on which probes are enabled:
            // with `on '*'` every probe emits, and the user asked for one.
            if !glob_match(pattern, registry().path_of(event.probe)) {
                continue;
            }
            if !matches_filters(&event, args_filter, ret_filter) {
                continue;
            }
            out.write_all(render_flat(&event).as_bytes())?;
            out.write_all(b"\n")?;
            printed += 1;
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
        out.flush()?;
        std::thread::sleep(POLL_INTERVAL);
    }
    writeln!(out, "\n[{printed} call(s)]")?;
    Ok(())
}

/// An empty filter passes everything, so no filters means "show all calls".
fn matches_filters(e: &Event, args_filter: &str, ret_filter: &str) -> bool {
    (args_filter.is_empty() || e.args.contains(args_filter))
        && (ret_filter.is_empty() || e.ret.contains(ret_filter))
}

struct Agg {
    count: u64,
    errs: u64,
    total_ns: u64,
    max_ns: u64,
    durs: Vec<u64>,
}

/// Aggregate the whole ring buffer, for `stats` and `top`.
fn aggregate(pattern: &str) -> Vec<(usize, Agg)> {
    aggregate_events(&recorder().snapshot(), pattern)
}

/// Aggregate an arbitrary slice of events per probe.
///
/// `dashboard` passes only what arrived during one interval, which is what
/// turns a set of totals into a rate.
fn aggregate_events(events: &[Event], pattern: &str) -> Vec<(usize, Agg)> {
    let mut by_probe: HashMap<usize, Agg> = HashMap::new();
    for e in events {
        let path = registry().path_of(e.probe);
        if !glob_match(pattern, path) {
            continue;
        }
        let a = by_probe.entry(e.probe).or_insert(Agg {
            count: 0,
            errs: 0,
            total_ns: 0,
            max_ns: 0,
            durs: Vec::new(),
        });
        a.count += 1;
        a.errs += u64::from(!e.ok);
        a.total_ns += e.dur_ns;
        a.max_ns = a.max_ns.max(e.dur_ns);
        a.durs.push(e.dur_ns);
    }
    let mut v: Vec<(usize, Agg)> = by_probe.into_iter().collect();
    v.sort_by_key(|(id, _)| registry().path_of(*id));
    v
}

fn quantile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

fn cmd_stats<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let rows = aggregate(args.pattern());
    if rows.is_empty() {
        writeln!(
            out,
            "no buffered events match. Note: stats reads the ring buffer only — \
             nothing is recorded until a probe is enabled."
        )?;
        return Ok(());
    }

    writeln!(
        out,
        "{:<44} {:>7} {:>6} {:>9} {:>9} {:>9} {:>9} {:>10}",
        "PATH", "COUNT", "ERR", "P50", "P95", "P99", "MAX", "TOTAL"
    )?;
    for (id, mut a) in rows {
        a.durs.sort_unstable();
        writeln!(
            out,
            "{:<44} {:>7} {:>6} {:>9} {:>9} {:>9} {:>9} {:>10}",
            truncate(registry().path_of(id), 44),
            a.count,
            a.errs,
            format_dur(quantile(&a.durs, 0.50)),
            format_dur(quantile(&a.durs, 0.95)),
            format_dur(quantile(&a.durs, 0.99)),
            format_dur(a.max_ns),
            format_dur(a.total_ns),
        )?;
    }
    let (recorded, dropped, _) = recorder().stats();
    writeln!(
        out,
        "\nring buffer: {recorded} recorded, {dropped} evicted, {} retained",
        recorder().snapshot().len()
    )?;
    Ok(())
}

fn cmd_top<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let n = args.num("n", 10usize);
    let by = args.get("by").unwrap_or("total");
    let mut rows = aggregate(args.pattern());

    match by {
        "max" => rows.sort_by_key(|(_, a)| std::cmp::Reverse(a.max_ns)),
        "count" => rows.sort_by_key(|(_, a)| std::cmp::Reverse(a.count)),
        _ => rows.sort_by_key(|(_, a)| std::cmp::Reverse(a.total_ns)),
    }
    rows.truncate(n);

    if rows.is_empty() {
        writeln!(out, "no buffered events match")?;
        return Ok(());
    }

    writeln!(
        out,
        "{:<44} {:>7} {:>6} {:>10} {:>10} {:>9}",
        "PATH", "COUNT", "ERR", "TOTAL", "MAX", "AVG"
    )?;
    for (id, a) in rows {
        let avg = a.total_ns / a.count.max(1);
        writeln!(
            out,
            "{:<44} {:>7} {:>6} {:>10} {:>10} {:>9}",
            truncate(registry().path_of(id), 44),
            a.count,
            a.errs,
            format_dur(a.total_ns),
            format_dur(a.max_ns),
            format_dur(avg),
        )?;
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// stack: the call path that reached a matching function
// ---------------------------------------------------------------------------

/// Default number of call paths `stack` collects before returning.
const DEFAULT_STACK_COUNT: usize = 5;

/// Whether any node in `tree` belongs to a probe matching `pattern`.
fn tree_contains(tree: &Tree, pattern: &str) -> bool {
    glob_match(pattern, registry().path_of(tree.event.probe))
        || tree.children.iter().any(|c| tree_contains(c, pattern))
}

fn cmd_stack<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let pattern = args.pattern();
    let max_count = args.num("count", DEFAULT_STACK_COUNT);
    let seconds: f64 = args.num("seconds", 0.0);
    let depth = args.num("depth", 0usize);
    let native = args.flag("native");

    let scope = ProbeScope::enter(pattern, native);
    if scope.is_empty() {
        writeln!(out, "no probes matching '{pattern}'. try 'list'")?;
        return Ok(());
    }

    let opts = RenderOpts {
        depth,
        min_ns: 0,
        show_task: true,
        show_ts: true,
        mark: pattern.to_string(),
        show_stack: native,
    };

    let grace = Duration::from_millis(args.num("grace-ms", 250u64).max(1));
    let deadline = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));
    let mut forest = Forest::with_grace(grace);
    let mut cursor = recorder().last_seq();
    let mut printed = 0usize;

    loop {
        for event in recorder().since(cursor) {
            cursor = cursor.max(event.seq);
            forest.add(event);
        }
        while let Some(root) = forest.take_ready() {
            let Some(tree) = forest.take(root) else {
                continue;
            };
            // A tree with no matching node is not a call path to anything the
            // user asked about, so it is noise.
            if !tree_contains(&tree, pattern) {
                continue;
            }
            out.write_all(render(&tree, &opts).as_bytes())?;
            out.write_all(render_stacks(&tree, &opts).as_bytes())?;
            printed += 1;
        }

        if max_count > 0 && printed >= max_count {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        out.flush()?;
        std::thread::sleep(POLL_INTERVAL);
    }

    for tree in forest.drain_roots() {
        if !tree_contains(&tree, pattern) {
            continue;
        }
        out.write_all(render(&tree, &opts).as_bytes())?;
        out.write_all(render_stacks(&tree, &opts).as_bytes())?;
    }
    writeln!(
        out,
        "\n[{printed} call path(s), probes restored to previous state]"
    )?;
    Ok(())
}

// ---------------------------------------------------------------------------
// dashboard: live process overview
// ---------------------------------------------------------------------------

fn cmd_dashboard<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let interval = Duration::from_secs_f64(args.num("interval", 1.0f64).max(0.05));
    let max_frames = args.num("count", 0usize);
    let seconds: f64 = args.num("seconds", 0.0);
    let hottest = args.num("n", 5usize);

    let mut meter = Meter::new();
    let mut cursor = recorder().last_seq();
    let deadline = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));
    let mut frames = 0usize;

    loop {
        std::thread::sleep(interval);

        let events = recorder().since(cursor);
        for e in &events {
            cursor = cursor.max(e.seq);
        }
        let sample = meter.tick();

        let mut frame = String::new();
        render_dashboard(&sample, &events, hottest, interval, &mut frame);

        // Buffered writes only reach the socket on flush, so the hang-up can
        // surface from either call.
        let written = out.write_all(frame.as_bytes()).and_then(|_| out.flush());
        if let Err(e) = written {
            // The client hung up (Ctrl-C on the CLI side). Not worth a stack
            // trace in the target process's stderr.
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                break;
            }
            return Err(e);
        }

        frames += 1;
        if max_frames > 0 && frames >= max_frames {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }
    Ok(())
}

fn render_dashboard(
    sample: &Sample,
    events: &[Event],
    hottest: usize,
    interval: Duration,
    out: &mut String,
) {
    let _ = writeln!(
        out,
        "── rthas dashboard ── pid {} ── up {} ── {} cores ──────────────",
        std::process::id(),
        fmt_clock(sample.uptime),
        cpu_count(),
    );
    let _ = writeln!(
        out,
        "  {:<8}{:>10}                {:<10}{:>9}",
        "CPU",
        format!("{:.1}%", sample.cpu * 100.0),
        "load1",
        format!("{:.2}", sample.load1),
    );
    let _ = writeln!(
        out,
        "  {:<8}{:>10}                {:<10}{:>9}",
        "MEM",
        fmt_bytes(sample.rss_bytes),
        "threads",
        sample.threads,
    );

    let probes = registry();
    let enabled = probes.all().iter().filter(|p| p.enabled()).count();
    let (recorded, dropped, _) = recorder().stats();
    let _ = writeln!(
        out,
        "  {:<8}{} registered · {} enabled · {} events buffered · {} evicted",
        "PROBES",
        probes.len(),
        enabled,
        recorded,
        dropped,
    );

    let _ = writeln!(
        out,
        "\n  probe activity over the last {:.2}s",
        interval.as_secs_f64()
    );
    let mut rows = aggregate_events(events, "");
    if rows.is_empty() {
        let _ = writeln!(
            out,
            "    (nothing recorded — enable probes with 'on <pattern>', \
             or run 'trace'/'watch' in another session)"
        );
        return;
    }

    rows.sort_by_key(|(_, a)| std::cmp::Reverse(a.total_ns));
    rows.truncate(hottest);
    let _ = writeln!(
        out,
        "  {:<40} {:>7} {:>6} {:>9} {:>9} {:>10}",
        "PATH", "COUNT", "ERR", "P50", "MAX", "TOTAL"
    );
    for (id, mut a) in rows {
        a.durs.sort_unstable();
        let _ = writeln!(
            out,
            "  {:<40} {:>7} {:>6} {:>9} {:>9} {:>10}",
            truncate(registry().path_of(id), 40),
            a.count,
            a.errs,
            format_dur(quantile(&a.durs, 0.50)),
            format_dur(a.max_ns),
            format_dur(a.total_ns),
        );
    }
}

// ---------------------------------------------------------------------------
// thread: per-thread CPU plus the last span each thread recorded
// ---------------------------------------------------------------------------

fn cmd_thread<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let tid_arg = args.pos.get(1).and_then(|s| s.parse::<u64>().ok());
    let all = args.flag("all");
    let want_stack = args.flag("stack") || all || tid_arg.is_some() || args.get("n").is_some();
    let limit = args.num("n", 0usize);
    let dump = want_stack;
    let by = args.get("by").unwrap_or(if dump { "cpu" } else { "tid" });
    let compact = !args.flag("full");
    let wait = Duration::from_secs_f64(args.num("interval", 0.15f64).max(0.02));
    let depth = args.num("depth", 16usize);

    let mut rows = thread_rows();

    // What each thread was last seen doing. Events are oldest first, so later
    // inserts win and the map ends up holding the newest span per thread.
    let mut last: HashMap<u64, &'static str> = HashMap::new();
    for e in recorder().snapshot() {
        last.insert(e.tid, registry().path_of(e.probe));
    }

    match by {
        "cpu" => rows.sort_by(|a, b| {
            b.cpu
                .partial_cmp(&a.cpu)
                .unwrap_or(std::cmp::Ordering::Equal)
        }),
        "name" => rows.sort_by(|a, b| a.name.cmp(&b.name)),
        _ => rows.sort_by_key(|t| t.id),
    }

    if let Some(tid) = tid_arg {
        rows.retain(|t| t.id == tid);
        if rows.is_empty() {
            writeln!(out, "no thread with tid {tid}")?;
            return Ok(());
        }
    } else if limit > 0 {
        rows.truncate(limit);
    }

    let total = thread_count();
    if rows.is_empty() {
        writeln!(
            out,
            "no threads reported — per-thread sampling needs /proc (Linux) or Mach (macOS)"
        )?;
        return Ok(());
    }

    if !dump {
        writeln!(
            out,
            "{:>8}  {:<24} {:<10} {:>7}  LAST SPAN",
            "TID", "NAME", "STATE", "CPU"
        )?;
        for t in &rows {
            let span = last.get(&t.id).copied().unwrap_or("-");
            writeln!(
                out,
                "{:>8}  {:<24} {:<10} {:>6.1}%  {}",
                t.id,
                truncate(&t.name, 24),
                t.state,
                t.cpu * 100.0,
                span,
            )?;
        }
        writeln!(out, "\n{} of {} thread(s) shown", rows.len(), total)?;
        return Ok(());
    }

    let tids: Vec<u64> = rows.iter().map(|t| t.id).collect();
    let stacks = crate::thread_dump::capture(&tids, wait, compact);

    for t in &rows {
        let span = last.get(&t.id).copied().unwrap_or("-");
        writeln!(
            out,
            "\"{}\" Id={} cpu={:.1}% {}  last={span}",
            t.name,
            t.id,
            t.cpu * 100.0,
            t.state
        )?;
        match stacks.get(&t.id) {
            Some(frames) if !frames.is_empty() => {
                let shown = if depth == 0 {
                    frames.len()
                } else {
                    frames.len().min(depth)
                };
                for frame in frames.iter().take(shown) {
                    writeln!(out, "    at {frame}")?;
                }
                if shown < frames.len() {
                    writeln!(out, "    ... {} more", frames.len() - shown)?;
                }
            }
            Some(_) if compact => {
                writeln!(
                    out,
                    "    (runtime frames only — pass --full for tokio/std/pthread)"
                )?;
            }
            _ => {
                writeln!(
                    out,
                    "    (no native stack — thread did not respond to SIGURG)"
                )?;
            }
        }
        writeln!(out)?;
    }
    writeln!(out, "{} of {} thread(s) shown", rows.len(), total)?;
    Ok(())
}

// ---------------------------------------------------------------------------
// profiler: SIGPROF sampling (Arthas profiler)
// ---------------------------------------------------------------------------

fn cmd_profiler<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let action = args.pos.get(1).copied().unwrap_or("");
    let seconds: f64 = args.num("seconds", 0.0);
    let hz = args.num("hz", crate::profiler::default_hz());
    let format = args.get("format").unwrap_or("text");
    let file = args.get("file").unwrap_or("");
    let top_n = args.num("n", 15usize);
    let depth = args.num("depth", 24usize);
    let full = args.flag("full");

    if let Some(event) = args.get("event") {
        if !event.is_empty() && event != "cpu" {
            writeln!(out, "only --event cpu is supported; ignoring '{event}'")?;
        }
    }

    if seconds > 0.0 && matches!(action, "" | "start") {
        return profiler_oneshot(seconds, hz, format, file, top_n, depth, full, out);
    }

    match action {
        "" => {
            writeln!(out, "{}", crate::profiler::status_line())?;
            if !crate::profiler::is_running() {
                writeln!(
                    out,
                    "try: profiler start | profiler --seconds 5 | profiler stop"
                )?;
            }
            Ok(())
        }
        "start" => match crate::profiler::start(hz) {
            Ok(()) => writeln!(
                out,
                "Started [cpu] profiling at {} Hz",
                crate::profiler::clamp_hz(hz)
            ),
            Err(e) => writeln!(out, "{e}"),
        },
        "stop" => profiler_dump(format, file, top_n, depth, full, out),
        "status" => writeln!(out, "{}", crate::profiler::status_line()),
        "getSamples" | "getsamples" | "get-samples" | "samples" => {
            match crate::profiler::sample_count() {
                Ok(n) => writeln!(out, "{n}"),
                Err(e) => writeln!(out, "{e}"),
            }
        }
        other => writeln!(
            out,
            "unknown profiler action '{other}'. try start|stop|status|getSamples"
        ),
    }
}

fn profiler_oneshot<W: Write>(
    seconds: f64,
    hz: i32,
    format: &str,
    file: &str,
    top_n: usize,
    depth: usize,
    full: bool,
    out: &mut W,
) -> std::io::Result<()> {
    if crate::profiler::is_running() {
        writeln!(out, "profiler is already running; `profiler stop` first")?;
        return Ok(());
    }
    if let Err(e) = crate::profiler::start(hz) {
        writeln!(out, "{e}")?;
        return Ok(());
    }
    writeln!(
        out,
        "Started [cpu] profiling at {} Hz for {seconds:.1}s",
        crate::profiler::clamp_hz(hz)
    )?;
    let _ = out.flush();
    std::thread::sleep(Duration::from_secs_f64(seconds.max(0.05)));
    profiler_dump(format, file, top_n, depth, full, out)
}

fn profiler_dump<W: Write>(
    format: &str,
    file: &str,
    top_n: usize,
    depth: usize,
    full: bool,
    out: &mut W,
) -> std::io::Result<()> {
    let snap = match crate::profiler::stop() {
        Ok(s) => s,
        Err(e) => {
            writeln!(out, "{e}")?;
            return Ok(());
        }
    };
    writeln!(
        out,
        "Stopped [cpu] profiling. samples={} elapsed={:.1}s hz={}",
        snap.total(),
        snap.elapsed.as_secs_f64(),
        snap.hz
    )?;
    if snap.raw_samples > 0 && snap.total() == 0 {
        writeln!(
            out,
            "({} raw sample(s) dropped — frames were in filtered libc/pprof code)",
            snap.raw_samples
        )?;
    }

    let kind = match format {
        "collapsed" | "folded" => "collapsed",
        "flamegraph" | "svg" | "html" => "flamegraph",
        _ => "text",
    };

    if kind == "flamegraph" {
        let path = if file.is_empty() {
            std::env::temp_dir().join(format!("rthas-{}.svg", std::process::id()))
        } else {
            std::path::PathBuf::from(file)
        };
        match std::fs::File::create(&path) {
            Ok(f) => match snap.write_flamegraph(f) {
                Ok(()) => writeln!(out, "wrote {} (flamegraph svg)", path.display())?,
                Err(e) => writeln!(out, "{e}")?,
            },
            Err(e) => writeln!(out, "could not create {}: {e}", path.display())?,
        }
        // A few hot frames so the terminal is not only a path.
        let text = snap.render_text(4, 8, !full);
        write_profiler_body(out, &text)?;
        return Ok(());
    }

    let body = if kind == "collapsed" {
        snap.render_collapsed()
    } else {
        snap.render_text(depth, top_n, !full)
    };
    write_profiler_body(out, &body)?;

    if !file.is_empty() {
        match std::fs::write(file, &body) {
            Ok(()) => writeln!(out, "wrote {file}")?,
            Err(e) => writeln!(out, "could not write {file}: {e}")?,
        }
    }
    Ok(())
}

fn write_profiler_body<W: Write>(out: &mut W, body: &str) -> std::io::Result<()> {
    match out.write_all(body.as_bytes()) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => Ok(()),
        Err(e) => Err(e),
    }
}

// ---------------------------------------------------------------------------
// monitor: Arthas-style periodic method stats
// ---------------------------------------------------------------------------

fn cmd_monitor<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let pattern = args.pattern();
    let interval = Duration::from_secs_f64(args.num("interval", 5.0f64).max(0.05));
    let max_frames = args.num("count", 0usize);
    let seconds: f64 = args.num("seconds", 0.0);

    if registry()
        .all()
        .iter()
        .all(|p| !glob_match(pattern, p.path))
    {
        writeln!(out, "no probes matching '{pattern}'. try 'list'")?;
        return Ok(());
    }

    let _scope = ProbeScope::enable(pattern);
    let deadline = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));
    let mut cursor = recorder().last_seq();
    let mut frames = 0usize;

    loop {
        std::thread::sleep(interval);

        let events = recorder().since(cursor);
        for e in &events {
            cursor = cursor.max(e.seq);
        }
        let matching: Vec<Event> = events
            .into_iter()
            .filter(|e| glob_match(pattern, registry().path_of(e.probe)))
            .collect();

        let mut frame = String::new();
        render_monitor(&matching, &mut frame);
        let written = out.write_all(frame.as_bytes()).and_then(|_| out.flush());
        if let Err(e) = written {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                break;
            }
            return Err(e);
        }

        frames += 1;
        if max_frames > 0 && frames >= max_frames {
            break;
        }
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
    }
    Ok(())
}

fn render_monitor(events: &[Event], out: &mut String) {
    let ts = format_datetime(to_system_time(now_ns()));
    let _ = writeln!(
        out,
        " timestamp            PATH                                       total  success  fail  avg-rt     fail-rate"
    );
    let _ = writeln!(
        out,
        "-----------------------------------------------------------------------------------------------------------"
    );
    let mut rows = aggregate_events(events, "");
    if rows.is_empty() {
        let _ = writeln!(out, " {ts:<20} (no calls this interval)");
        let _ = writeln!(out);
        return;
    }
    rows.sort_by_key(|(_, a)| std::cmp::Reverse(a.total_ns));
    for (id, a) in rows {
        let success = a.count.saturating_sub(a.errs);
        let avg = a.total_ns / a.count.max(1);
        let fail_rate = if a.count == 0 {
            0.0
        } else {
            a.errs as f64 / a.count as f64 * 100.0
        };
        let _ = writeln!(
            out,
            " {ts:<20} {:<42} {:>5} {:>8} {:>5} {:>9} {:>9.2}%",
            truncate(registry().path_of(id), 42),
            a.count,
            success,
            a.errs,
            format_dur(avg),
            fail_rate,
        );
    }
    let _ = writeln!(out);
}

// ---------------------------------------------------------------------------
// tt: time tunnel
// ---------------------------------------------------------------------------

fn cmd_tt<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    if args.flag("clear") {
        let n = tunnel().len();
        tunnel().clear();
        writeln!(out, "cleared {n} fragment(s)")?;
        return Ok(());
    }
    if args.flag("list") {
        return cmd_tt_list(args.pattern(), out);
    }
    if let Some(raw) = args.get("index") {
        return cmd_tt_index(raw, out);
    }
    if let Some(raw) = args.get("delete") {
        return cmd_tt_delete(raw, out);
    }
    if args.pattern().is_empty() {
        writeln!(
            out,
            "usage: tt <pattern> [--count N] [--seconds F] [--args S] [--ret S]\n\
             or     tt --list [pattern] | tt --index N | tt --delete N | tt --clear\n\
             replay (Arthas tt -p) is not supported: rthas stores Debug strings, not typed values"
        )?;
        return Ok(());
    }
    cmd_tt_record(args, out)
}

fn cmd_tt_record<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let pattern = args.pattern();
    let max_count = args.num("count", 0usize);
    let seconds: f64 = args.num("seconds", 0.0);
    let args_filter = args.get("args").unwrap_or("");
    let ret_filter = args.get("ret").unwrap_or("");

    if registry()
        .all()
        .iter()
        .all(|p| !glob_match(pattern, p.path))
    {
        writeln!(out, "no probes matching '{pattern}'. try 'list'")?;
        return Ok(());
    }

    let _scope = ProbeScope::enable(pattern);
    let deadline = (seconds > 0.0).then(|| Instant::now() + Duration::from_secs_f64(seconds));
    let mut cursor = recorder().last_seq();
    let mut printed = 0usize;

    writeln!(
        out,
        "{:<7} {:<19} {:>10} {:<6} {:>10} {:>10}  PATH",
        "INDEX", "TIMESTAMP", "COST", "OK", "TID", "TASK"
    )?;

    loop {
        for event in recorder().since(cursor) {
            cursor = cursor.max(event.seq);
            let path = registry().path_of(event.probe);
            if !glob_match(pattern, path) {
                continue;
            }
            if !matches_filters(&event, args_filter, ret_filter) {
                continue;
            }
            let index = tunnel().record(&event, path);
            writeln!(out, "{}", format_tt_row(index, &event, path))?;
            printed += 1;
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
        out.flush()?;
        std::thread::sleep(POLL_INTERVAL);
    }
    writeln!(
        out,
        "\n[{printed} fragment(s) recorded, {} retained, probes restored]\n\
         next: tt --list   or   tt --index {}",
        tunnel().len(),
        if printed > 0 { "N" } else { "(none yet)" },
    )?;
    Ok(())
}

fn format_tt_row(index: u64, event: &Event, path: &str) -> String {
    format!(
        "{:<7} {:<19} {:>10} {:<6} {:>10} {:>10}  {}",
        index,
        format_datetime(to_system_time(event.start_ns)),
        format_dur(event.dur_ns),
        if event.ok { "true" } else { "false" },
        event.tid,
        event.task,
        path,
    )
}

fn cmd_tt_list<W: Write>(pattern: &str, out: &mut W) -> std::io::Result<()> {
    let rows: Vec<_> = tunnel()
        .list()
        .into_iter()
        .filter(|f| glob_match(pattern, &f.path))
        .collect();
    if rows.is_empty() {
        writeln!(
            out,
            "no fragments{}. record with `tt <pattern>`, then `tt --list`",
            if pattern.is_empty() {
                String::new()
            } else {
                format!(" matching '{pattern}'")
            }
        )?;
        return Ok(());
    }
    writeln!(
        out,
        "{:<7} {:<19} {:>10} {:<6} {:>10} {:>10}  PATH",
        "INDEX", "TIMESTAMP", "COST", "OK", "TID", "TASK"
    )?;
    for f in &rows {
        writeln!(
            out,
            "{:<7} {:<19} {:>10} {:<6} {:>10} {:>10}  {}",
            f.index,
            format_datetime(to_system_time(f.start_ns)),
            format_dur(f.dur_ns),
            if f.ok { "true" } else { "false" },
            f.tid,
            f.task,
            f.path,
        )?;
    }
    writeln!(out, "\n{} fragment(s)", rows.len())?;
    Ok(())
}

fn cmd_tt_index<W: Write>(raw: &str, out: &mut W) -> std::io::Result<()> {
    let Ok(index) = raw.parse::<u64>() else {
        writeln!(out, "tt --index needs a number, got '{raw}'")?;
        return Ok(());
    };
    let Some(f) = tunnel().get(index) else {
        writeln!(out, "no fragment {index}. try 'tt --list'")?;
        return Ok(());
    };
    writeln!(out, " INDEX       {index}")?;
    writeln!(
        out,
        " TIMESTAMP   {}",
        format_datetime(to_system_time(f.start_ns))
    )?;
    writeln!(out, " COST        {}", format_dur(f.dur_ns))?;
    writeln!(out, " PATH        {}", f.path)?;
    writeln!(out, " TID         {}", f.tid)?;
    writeln!(out, " TASK        {}", f.task)?;
    writeln!(out, " OK          {}", f.ok)?;
    writeln!(out, " ARGS        {}", empty_dash(&f.args))?;
    writeln!(out, " RETURN      {}", empty_dash(&f.ret))?;
    Ok(())
}

fn cmd_tt_delete<W: Write>(raw: &str, out: &mut W) -> std::io::Result<()> {
    let Ok(index) = raw.parse::<u64>() else {
        writeln!(out, "tt --delete needs a number, got '{raw}'")?;
        return Ok(());
    };
    if tunnel().delete(index) {
        writeln!(out, "deleted fragment {index}")?;
    } else {
        writeln!(out, "no fragment {index}")?;
    }
    Ok(())
}

fn empty_dash(s: &str) -> &str {
    if s.is_empty() {
        "-"
    } else {
        s
    }
}

// ---------------------------------------------------------------------------
// sysenv / memory / session / options / stop
// ---------------------------------------------------------------------------

fn sysenv_entries(name: &str) -> Vec<(String, String)> {
    let mut v: Vec<_> = std::env::vars()
        .filter(|(k, _)| name.is_empty() || k == name)
        .collect();
    v.sort_by(|a, b| a.0.cmp(&b.0));
    v
}

fn cmd_sysenv<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let name = args.pattern();
    let rows = sysenv_entries(name);
    if rows.is_empty() {
        if name.is_empty() {
            writeln!(out, "no environment variables")?;
        } else {
            writeln!(out, "no env var named '{name}'")?;
        }
        return Ok(());
    }
    writeln!(out, "{:<28} VALUE", "KEY")?;
    for (k, v) in &rows {
        writeln!(out, "{:<28} {}", truncate(k, 28), v)?;
    }
    writeln!(out, "\n{} var(s)", rows.len())?;
    Ok(())
}

fn cmd_memory<W: Write>(out: &mut W) -> std::io::Result<()> {
    let m = memory_info();
    let rss_note = if m.rss_is_peak {
        "peak (getrusage)"
    } else {
        "current"
    };
    writeln!(out, "{:<16} {:<12}  NOTE", "METRIC", "VALUE")?;
    writeln!(
        out,
        "{:<16} {:<12}  {rss_note}",
        "rss",
        fmt_bytes(m.rss_bytes)
    )?;
    if m.virt_bytes > 0 {
        writeln!(out, "{:<16} {:<12}", "virt", fmt_bytes(m.virt_bytes))?;
    }
    if m.peak_rss_bytes > 0 {
        writeln!(
            out,
            "{:<16} {:<12}",
            "peak_rss",
            fmt_bytes(m.peak_rss_bytes)
        )?;
    }
    if m.swap_bytes > 0 || !m.rss_is_peak {
        writeln!(out, "{:<16} {:<12}", "swap", fmt_bytes(m.swap_bytes))?;
    }
    writeln!(out, "{:<16} {:<12}", "threads", m.threads)?;
    if m.fds > 0 {
        writeln!(
            out,
            "{:<16} {} / {}",
            "fds",
            m.fds,
            if m.fd_limit == 0 {
                "-".to_string()
            } else {
                m.fd_limit.to_string()
            }
        )?;
    } else if m.fd_limit > 0 {
        writeln!(out, "{:<16} (limit {})", "fds", m.fd_limit)?;
    }
    Ok(())
}

fn cmd_auth<W: Write>(args: &Args, out: &mut W, authed: &mut bool) -> std::io::Result<()> {
    let Some(want_pass) = expected_password() else {
        *authed = true;
        writeln!(out, "Authentication result: true (auth is not configured)")?;
        return Ok(());
    };
    let user = args
        .get("username")
        .or_else(|| args.get("user"))
        .unwrap_or("");
    let user = if user.is_empty() { "rthas" } else { user };
    let pass = args
        .get("password")
        .filter(|s| !s.is_empty())
        .or_else(|| args.pos.get(1).copied())
        .unwrap_or("");
    let ok = secrets_match(user, &expected_username()) && secrets_match(pass, &want_pass);
    *authed = ok;
    writeln!(out, "Authentication result: {ok}")?;
    Ok(())
}

fn cmd_pwd<W: Write>(out: &mut W) -> std::io::Result<()> {
    match std::env::current_dir() {
        Ok(p) => writeln!(out, "{}", p.display()),
        Err(e) => writeln!(out, "pwd: {e}"),
    }
}

const CAT_MAX: u64 = 2 * 1024 * 1024;

fn cmd_cat<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let path = args.pos.get(1).copied().unwrap_or("");
    if path.is_empty() {
        writeln!(out, "cat needs a path")?;
        return Ok(());
    }
    let meta = match std::fs::metadata(path) {
        Ok(m) => m,
        Err(e) => {
            writeln!(out, "cat {path}: {e}")?;
            return Ok(());
        }
    };
    if !meta.is_file() {
        writeln!(out, "cat {path}: not a regular file")?;
        return Ok(());
    }
    if meta.len() > CAT_MAX {
        writeln!(
            out,
            "cat {path}: {} bytes exceeds {} byte limit",
            meta.len(),
            CAT_MAX
        )?;
        return Ok(());
    }
    match std::fs::read_to_string(path) {
        Ok(s) => write!(out, "{s}"),
        Err(e) => writeln!(out, "cat {path}: {e}"),
    }
}

fn cmd_echo<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let rest: Vec<&str> = args.pos.iter().copied().skip(1).collect();
    writeln!(out, "{}", rest.join(" "))
}

fn cmd_session<W: Write>(out: &mut W, authed: bool) -> std::io::Result<()> {
    let probes = registry();
    let enabled = probes.all().iter().filter(|p| p.enabled()).count();
    let (recorded, dropped, _) = recorder().stats();
    writeln!(out, " {:<12} {}", "Name", "Value")?;
    writeln!(out, "{}", "-".repeat(50))?;
    writeln!(out, " {:<12} {}", "PID", std::process::id())?;
    writeln!(out, " {:<12} {}", "SOCKET", socket_path().display())?;
    writeln!(
        out,
        " {:<12} {} registered, {} enabled",
        "PROBES",
        probes.len(),
        enabled
    )?;
    writeln!(
        out,
        " {:<12} {} recorded, {} evicted, {} capacity",
        "RING",
        recorded,
        dropped,
        recorder().capacity()
    )?;
    writeln!(
        out,
        " {:<12} {} fragment(s), {} capacity",
        "TUNNEL",
        tunnel().len(),
        tunnel().capacity()
    )?;
    writeln!(
        out,
        " {:<12} {}",
        "UPTIME",
        fmt_clock(Duration::from_nanos(now_ns()))
    )?;
    writeln!(
        out,
        " {:<12} {}",
        "PROFILER",
        crate::profiler::status_line()
    )?;
    let auth = if !password_configured() {
        "off"
    } else if authed {
        "ok"
    } else {
        "required"
    };
    writeln!(out, " {:<12} {auth}", "AUTH")?;
    Ok(())
}

fn cmd_options<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let name = args.pos.get(1).copied().unwrap_or("");
    let value = args.pos.get(2).copied().unwrap_or("");
    if name.is_empty() {
        writeln!(
            out,
            "{:<12} {:<12} {:<8} SUMMARY",
            "NAME", "VALUE", "MUTABLE"
        )?;
        writeln!(
            out,
            "{:<12} {:<12} {:<8} max chars per arg/return value",
            "max-str",
            max_str(),
            "yes"
        )?;
        writeln!(
            out,
            "{:<12} {:<12} {:<8} display timezone offset in hours",
            "tz-hours",
            format_tz_hours(tz_hours()),
            "yes"
        )?;
        writeln!(
            out,
            "{:<12} {:<12} {:<8} ring buffer size (set RTHAS_CAPACITY before init)",
            "capacity",
            recorder().capacity(),
            "no"
        )?;
        return Ok(());
    }
    if value.is_empty() {
        match name {
            "max-str" => writeln!(out, "max-str = {}", max_str())?,
            "tz-hours" => writeln!(out, "tz-hours = {}", format_tz_hours(tz_hours()))?,
            "capacity" => writeln!(
                out,
                "capacity = {} (immutable at runtime)",
                recorder().capacity()
            )?,
            other => writeln!(out, "unknown option '{other}'. try 'options'")?,
        }
        return Ok(());
    }
    match name {
        "max-str" => match value.parse::<usize>() {
            Ok(n) if n >= 1 => {
                set_max_str(n);
                writeln!(out, "max-str = {}", max_str())?;
            }
            _ => writeln!(out, "max-str needs a positive integer, got '{value}'")?,
        },
        "tz-hours" => match value.parse::<f64>() {
            Ok(h) if h.is_finite() => {
                set_tz_hours(h);
                writeln!(out, "tz-hours = {}", format_tz_hours(tz_hours()))?;
            }
            _ => writeln!(out, "tz-hours needs a number, got '{value}'")?,
        },
        "capacity" => writeln!(
            out,
            "capacity is fixed after init; restart with RTHAS_CAPACITY={value}"
        )?,
        other => writeln!(out, "unknown option '{other}'. try 'options'")?,
    }
    Ok(())
}

fn format_tz_hours(h: f64) -> String {
    if (h - h.round()).abs() < 1e-9 {
        format!("{}", h as i64)
    } else {
        format!("{h:.2}")
    }
}

fn cmd_stop<W: Write>(out: &mut W) -> std::io::Result<()> {
    registry().disable_all();
    let pid = std::process::id();
    writeln!(out, "stopped agent for pid {pid}; all probes disabled")?;
    writeln!(out, "next: rthas attach {pid}")?;
    STOP.store(true, Ordering::SeqCst);
    // Wake the blocking accept() so the listener thread can notice STOP.
    let _ = UnixStream::connect(socket_path());
    match spawn_lazy() {
        Ok(_) => {}
        Err(e) => writeln!(out, "could not arm deferred attach: {e}")?,
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Formatting helpers
// ---------------------------------------------------------------------------

/// Human-readable size in binary units.
fn fmt_bytes(bytes: u64) -> String {
    const KIB: f64 = 1024.0;
    let b = bytes as f64;
    if b < KIB {
        return format!("{bytes} B");
    }
    let kib = b / KIB;
    if kib < KIB {
        return format!("{kib:.1} KiB");
    }
    let mib = kib / KIB;
    if mib < KIB {
        return format!("{mib:.1} MiB");
    }
    format!("{:.2} GiB", mib / KIB)
}

/// `HH:MM:SS` from a duration, for the uptime column.
fn fmt_clock(d: Duration) -> String {
    let secs = d.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    // Cut on a char boundary to avoid panicking on multi-byte paths.
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

// ---------------------------------------------------------------------------
// Deferred start, for `rthas attach`
// ---------------------------------------------------------------------------

/// How often a deferred agent checks whether somebody asked for it.
const ATTACH_POLL: Duration = Duration::from_millis(200);

/// File whose creation asks a deferred agent to bind its control socket.
///
/// A file rather than a signal: signals have to be handled, and a library has
/// no business owning `SIGUSR2` in somebody else's process. A file needs no
/// handler, no privileges beyond write access to the socket directory, and
/// works identically on every platform.
pub fn attach_trigger_path() -> PathBuf {
    let dir = std::env::var("RTHAS_SOCK_DIR").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(dir).join(format!(".rthas-attach-{}", std::process::id()))
}

/// Don't bind the socket yet — wait for `rthas attach <pid>` to ask for it.
///
/// This is what makes `attach` possible without restarting the process: the
/// probe points are already compiled in, so the only thing missing is the
/// control plane, and that can be created on demand. A deferred process pays
/// for one idle thread that stats a file five times a second and nothing else.
///
/// Returns the socket path that *will* be used, so callers can log it up front.
pub fn spawn_lazy() -> std::io::Result<PathBuf> {
    let path = socket_path();
    if LAZY_WATCHING
        .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return Ok(path);
    }
    let trigger = attach_trigger_path();
    if let Some(dir) = trigger.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }

    let announced = path.clone();
    let spawn_result = std::thread::Builder::new()
        .name("rthas-attach".to_string())
        .spawn(move || loop {
            if !trigger.exists() {
                std::thread::sleep(ATTACH_POLL);
                continue;
            }
            // Consume the trigger; a later attach starts from a clean slate.
            let _ = std::fs::remove_file(&trigger);
            match spawn() {
                Ok(p) => {
                    LAZY_WATCHING.store(false, Ordering::SeqCst);
                    eprintln!("[rthas] agent started on attach: {}", p.display());
                    return;
                }
                Err(e) => eprintln!(
                    "[rthas] attach: could not bind {}: {e} — still waiting",
                    announced.display()
                ),
            }
        });
    if let Err(e) = spawn_result {
        LAZY_WATCHING.store(false, Ordering::SeqCst);
        return Err(e);
    }
    Ok(path)
}

/// Convenience: start the agent according to `RTHAS_AGENT`.
///
/// * `0`     — never start.
/// * `lazy`  — wait for `rthas attach <pid>` ([`spawn_lazy`]).
/// * anything else, or unset — start immediately.
///
/// Call once from `main`. Silently ignores bind failures — a missing control
/// socket is never a reason to take down the service being debugged.
pub fn init() {
    // Anchor the display clock so `session` uptime is time since init, not
    // since the first command that happened to call `now_ns`.
    let _ = now_ns();
    match std::env::var("RTHAS_AGENT").as_deref() {
        Ok("0") => {}
        Ok("lazy") => match spawn_lazy() {
            Ok(_) => eprintln!(
                "[rthas] agent deferred for pid {} — `rthas attach {}` to start it",
                std::process::id(),
                std::process::id()
            ),
            Err(e) => eprintln!("[rthas] deferred agent unavailable: {e}"),
        },
        _ => match spawn() {
            Ok(path) => eprintln!("[rthas] agent listening on {}", path.display()),
            Err(e) => eprintln!("[rthas] agent not started: {e}"),
        },
    }
}

/// Defer the agent regardless of `RTHAS_AGENT`, for callers that want the
/// decision made in code rather than in the environment.
pub fn init_lazy() {
    let _ = now_ns();
    match spawn_lazy() {
        Ok(path) => eprintln!(
            "[rthas] agent deferred for pid {} — will bind {} on attach",
            std::process::id(),
            path.display()
        ),
        Err(e) => eprintln!("[rthas] deferred agent unavailable: {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::{matches_filters, secrets_match, sysenv_entries, Args};
    use crate::event::Event;

    #[test]
    fn parses_kv_and_separate_flag_values() {
        let a = Args::parse("trace db::* --count 5 --depth=3");
        assert_eq!(a.pos, vec!["trace", "db::*"]);
        assert_eq!(a.get("count"), Some("5"));
        assert_eq!(a.get("depth"), Some("3"));
        assert_eq!(a.num("count", 1usize), 5);
        assert_eq!(a.num("missing", 7usize), 7);
    }

    #[test]
    fn valueless_flag_does_not_swallow_the_next_flag() {
        let a = Args::parse("stack read_block --native --count 2");
        assert!(a.flag("native"));
        assert_eq!(a.get("count"), Some("2"));
        assert_eq!(a.pos, vec!["stack", "read_block"]);
    }

    #[test]
    fn valueless_list_does_not_swallow_the_pattern() {
        let a = Args::parse("tt --list handle_request");
        assert!(a.flag("list"));
        assert_eq!(a.pattern(), "handle_request");
        assert_eq!(a.get("list"), Some(""));
    }

    #[test]
    fn valueless_all_does_not_swallow_n() {
        let a = Args::parse("thread --all --n 3");
        assert!(a.flag("all"));
        assert_eq!(a.get("n"), Some("3"));
    }

    #[test]
    fn index_flag_still_takes_a_value() {
        let a = Args::parse("tt --index 1003");
        assert_eq!(a.get("index"), Some("1003"));
        assert!(a.pattern().is_empty());
    }

    #[test]
    fn sysenv_filters_by_exact_name() {
        let rows = sysenv_entries("PATH");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "PATH");
        assert!(sysenv_entries("RTHAS_NO_SUCH_VAR_XYZ").is_empty());
    }

    #[test]
    fn no_filters_matches_everything() {
        let e = Event {
            seq: 1,
            span: 1,
            parent: 0,
            depth: 0,
            probe: 0,
            tid: 1,
            task: 0,
            start_ns: 0,
            dur_ns: 1,
            ok: true,
            args: "a=1".into(),
            ret: "Ok(2)".into(),
            stack: None,
        };
        assert!(matches_filters(&e, "", ""));
        assert!(matches_filters(&e, "a=1", ""));
        assert!(!matches_filters(&e, "a=2", ""));
        assert!(!matches_filters(&e, "", "Ok(3)"));
    }

    #[test]
    fn password_compare_is_length_aware() {
        assert!(secrets_match("secret", "secret"));
        assert!(!secrets_match("secret", "secre"));
        assert!(!secrets_match("secret", "secret!"));
        assert!(!secrets_match("", "x"));
    }
}
