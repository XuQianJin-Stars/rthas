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
use std::time::{Duration, Instant};

use crate::event::{recorder, Event};
use crate::probe::{glob_match, registry, Probe};
use crate::sample::{cpu_count, thread_count, thread_rows, Meter, Sample};
use crate::time::format_dur;
use crate::tree::{render, render_flat, render_stacks, Forest, RenderOpts, Tree};

/// Default number of root calls a `trace` collects before returning.
const DEFAULT_TRACE_COUNT: usize = 20;
/// Poll interval while streaming. 5ms keeps latency perceptually instant
/// without spinning a core.
const POLL_INTERVAL: Duration = Duration::from_millis(5);

/// Sentinel written after every response so clients know a reply is complete.
pub const END: &str = "<<<end>>>";

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

    let listener = UnixListener::bind(&path)?;
    std::thread::Builder::new()
        .name("rthas-agent".to_string())
        .spawn(move || {
            for stream in listener.incoming() {
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
        })?;
    Ok(path)
}

fn handle_client(stream: UnixStream) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = BufWriter::new(stream);
    let mut line = String::new();
    loop {
        line.clear();
        if reader.read_line(&mut line)? == 0 {
            break;
        }
        let cmd = line.trim();
        if cmd.is_empty() {
            continue;
        }
        let keep_open = dispatch(cmd, &mut writer)?;
        // The connection stays open for the next command, so EOF cannot mark
        // the end of a reply. The sentinel does, and it is cheap enough that
        // `nc` users can just ignore it.
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

fn dispatch<W: Write>(line: &str, out: &mut W) -> std::io::Result<bool> {
    let args = Args::parse(line);
    let verb = args.pos.first().copied().unwrap_or("");

    match verb {
        "help" | "?" => {
            out.write_all(HELP.as_bytes())?;
        }
        "ping" => {
            writeln!(out, "pong pid={} probes={}", std::process::id(), registry().len())?;
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
        "stats" => cmd_stats(&args, out)?,
        "top" => cmd_top(&args, out)?,
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
     --n N            show only the top N threads by CPU
     --by tid|cpu|name                  sort order (default tid)
  stats [pattern]                       p50/p95/p99/max over the ring buffer
  top [pattern] [--n N] [--by total|max|count]   slowest / hottest functions
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

    if registry().all().iter().all(|p| !glob_match(pattern, p.path)) {
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
    writeln!(out, "\n[{printed} call tree(s), probes restored to previous state]")?;
    Ok(())
}

fn cmd_watch<W: Write>(args: &Args, out: &mut W) -> std::io::Result<()> {
    let pattern = args.pattern();
    let max_count = args.num("count", 50usize);
    let seconds: f64 = args.num("seconds", 0.0);
    let args_filter = args.get("args").unwrap_or("");
    let ret_filter = args.get("ret").unwrap_or("");

    if registry().all().iter().all(|p| !glob_match(pattern, p.path)) {
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
    let limit = args.num("n", 0usize);
    let by = args.get("by").unwrap_or("tid");

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
    if limit > 0 {
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
    format!("{:02}:{:02}:{:02}", secs / 3600, (secs % 3600) / 60, secs % 60)
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
    let trigger = attach_trigger_path();
    if let Some(dir) = trigger.parent() {
        if !dir.as_os_str().is_empty() {
            std::fs::create_dir_all(dir).ok();
        }
    }

    let announced = path.clone();
    std::thread::Builder::new()
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
                    eprintln!("[rthas] agent started on attach: {}", p.display());
                    return;
                }
                Err(e) => eprintln!(
                    "[rthas] attach: could not bind {}: {e} — still waiting",
                    announced.display()
                ),
            }
        })?;
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
    use super::{matches_filters, Args};
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
}
