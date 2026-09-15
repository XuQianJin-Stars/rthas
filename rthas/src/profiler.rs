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

//! In-process profiler (Arthas `profiler`).
//!
//! `--event cpu` (default) samples with SIGPROF via `pprof` — only while the
//! thread is on CPU. `--event wall` uses ITIMER_REAL / SIGALRM so sleeping
//! `.await`s still produce stacks. The handler is installed only while a
//! session is running; a disabled probe is still one atomic load, and this
//! module is not on that path.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use pprof::ProfilerGuard;

const DEFAULT_HZ: i32 = 99;
const BLOCKLIST: &[&str] = &[
    "libc",
    "libgcc",
    "pthread",
    "vdso",
    "libsystem",
    "dyld",
    "ld-linux",
];

/// Sampling event. `cpu` is SIGPROF (on-CPU only); `wall` is ITIMER_REAL.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EventKind {
    Cpu,
    Wall,
}

impl EventKind {
    pub fn parse(name: &str) -> Result<Self, String> {
        match name {
            "" | "cpu" => Ok(Self::Cpu),
            "wall" | "wall-clock" | "wallclock" => Ok(Self::Wall),
            other => Err(format!(
                "unknown --event '{other}'. try cpu or wall (alloc/lock need a JVM)"
            )),
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Cpu => "cpu",
            Self::Wall => "wall",
        }
    }
}

struct Running {
    kind: EventKind,
    guard: Option<ProfilerGuard<'static>>,
    wall_prev: Option<libc::sigaction>,
    started: Instant,
    hz: i32,
}

static SESSION: Mutex<Option<Running>> = Mutex::new(None);

fn session() -> std::sync::MutexGuard<'static, Option<Running>> {
    SESSION.lock().unwrap_or_else(|e| e.into_inner())
}

/// Clamp to what the kernel and `pprof` will both tolerate.
pub fn clamp_hz(hz: i32) -> i32 {
    hz.clamp(1, 1000)
}

pub fn default_hz() -> i32 {
    DEFAULT_HZ
}

pub fn is_running() -> bool {
    session().is_some()
}

pub fn start_kind(hz: i32, kind: EventKind) -> Result<(), String> {
    let hz = clamp_hz(hz);
    let mut slot = session();
    if slot.is_some() {
        return Err("profiler is already running; `profiler stop` first".into());
    }
    let running = match kind {
        EventKind::Cpu => {
            let guard = pprof::ProfilerGuardBuilder::default()
                .frequency(hz)
                .blocklist(BLOCKLIST)
                .build()
                .map_err(|e| format!("failed to start profiler: {e}"))?;
            Running {
                kind,
                guard: Some(guard),
                wall_prev: None,
                started: Instant::now(),
                hz,
            }
        }
        EventKind::Wall => {
            let prev = wall_start(hz)?;
            Running {
                kind,
                guard: None,
                wall_prev: Some(prev),
                started: Instant::now(),
                hz,
            }
        }
    };
    *slot = Some(running);
    Ok(())
}

pub fn status_line() -> String {
    match session().as_ref() {
        Some(s) => format!(
            "[{}] profiling is running for {:.1} seconds at {} Hz",
            s.kind.as_str(),
            s.started.elapsed().as_secs_f64(),
            s.hz
        ),
        None => "profiler is not running".into(),
    }
}

/// Sample count of the current session, without stopping it.
pub fn sample_count() -> Result<i64, String> {
    let slot = session();
    let running = slot
        .as_ref()
        .ok_or_else(|| "profiler is not running".to_string())?;
    match running.kind {
        EventKind::Cpu => {
            let report = running
                .guard
                .as_ref()
                .ok_or("profiler is not running")?
                .report()
                .build()
                .map_err(|e| format!("profiler report: {e}"))?;
            Ok(report.data.values().map(|c| *c as i64).sum())
        }
        EventKind::Wall => Ok(wall_sample_count()),
    }
}

pub fn stop() -> Result<Snapshot, String> {
    let running = session()
        .take()
        .ok_or_else(|| "profiler is not running".to_string())?;
    let elapsed = running.started.elapsed();
    let hz = running.hz;
    match running.kind {
        EventKind::Cpu => {
            let guard = running.guard.ok_or("profiler is not running")?;
            let report = guard
                .report()
                .build()
                .map_err(|e| format!("profiler report: {e}"))?;
            Ok(Snapshot::from_report(report, elapsed, hz))
        }
        EventKind::Wall => {
            if let Some(prev) = running.wall_prev {
                wall_stop(prev);
            }
            Ok(wall_snapshot(elapsed, hz))
        }
    }
}

/// Folded stacks plus, for CPU sessions, the original `pprof` report (SVG).
pub struct Snapshot {
    pub event: EventKind,
    pub elapsed: Duration,
    pub hz: i32,
    /// Samples that survived frame filtering.
    pub stacks: Vec<(Vec<String>, i64)>,
    /// Samples recorded, including stacks we later dropped.
    pub raw_samples: i64,
    report: Option<pprof::Report>,
}

impl Snapshot {
    fn from_report(report: pprof::Report, elapsed: Duration, hz: i32) -> Self {
        let raw_samples: i64 = report.data.values().map(|c| *c as i64).sum();
        let mut stacks = Vec::with_capacity(report.data.len());
        for (frames, count) in &report.data {
            let stack = frames_to_stack(frames);
            if stack.is_empty() {
                continue;
            }
            stacks.push((stack, *count as i64));
        }
        stacks.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Self {
            event: EventKind::Cpu,
            elapsed,
            hz,
            stacks,
            raw_samples,
            report: Some(report),
        }
    }

    fn from_wall(stacks: Vec<(Vec<String>, i64)>, raw_samples: i64, elapsed: Duration, hz: i32) -> Self {
        let mut stacks = stacks;
        stacks.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
        Self {
            event: EventKind::Wall,
            elapsed,
            hz,
            stacks,
            raw_samples,
            report: None,
        }
    }

    pub fn total(&self) -> i64 {
        self.stacks.iter().map(|(_, n)| *n).sum()
    }

    pub fn render_text(&self, depth: usize, top_n: usize, compact: bool) -> String {
        let stacks = if compact {
            compact_stacks(&self.stacks)
        } else {
            self.stacks.clone()
        };
        render_text(
            &stacks,
            self.total(),
            self.elapsed,
            self.hz,
            depth,
            top_n,
            self.event.as_str(),
        )
    }

    pub fn render_collapsed(&self) -> String {
        render_collapsed(&self.stacks)
    }

    pub fn write_flamegraph<W: Write>(&self, writer: W) -> Result<(), String> {
        if let Some(report) = &self.report {
            return report
                .flamegraph(writer)
                .map_err(|e| format!("flamegraph: {e}"));
        }
        flamegraph_from_stacks(&self.stacks, writer)
    }
}

fn flamegraph_from_stacks<W: Write>(
    stacks: &[(Vec<String>, i64)],
    writer: W,
) -> Result<(), String> {
    let lines: Vec<String> = stacks
        .iter()
        .filter(|(s, _)| !s.is_empty())
        .map(|(s, n)| format!("{} {n}", s.join(";")))
        .collect();
    if lines.is_empty() {
        return Err("no samples to render as a flamegraph".into());
    }
    let mut opts = inferno::flamegraph::Options::default();
    opts.title = "rthas profiler [wall]".into();
    inferno::flamegraph::from_lines(&mut opts, lines.iter().map(String::as_str), writer)
        .map_err(|e| format!("flamegraph: {e}"))
}

fn frames_to_stack(frames: &pprof::Frames) -> Vec<String> {
    let mut names = Vec::new();
    // pprof stores leaf-first; collapsed / flame graphs want root-first.
    for frame in frames.frames.iter().rev() {
        for symbol in frame.iter().rev() {
            let raw = symbol.to_string();
            let name = trim_rustc_hash(&raw);
            if skip_frame(name) {
                continue;
            }
            names.push(name.to_string());
        }
    }
    names
}

/// Strip the `::h` + 16 hex digits rustc appends to every symbol.
pub fn trim_rustc_hash(name: &str) -> &str {
    const SUFFIX: usize = 19; // "::h" + 16 hex
    if name.len() <= SUFFIX {
        return name;
    }
    let tail = &name[name.len() - SUFFIX..];
    if tail.starts_with("::h") && tail[3..].bytes().all(|b| b.is_ascii_hexdigit()) {
        return &name[..name.len() - SUFFIX];
    }
    name
}

fn skip_frame(name: &str) -> bool {
    if name.is_empty() || name == "<unknown>" {
        return true;
    }
    name.contains("pprof::")
        || name.contains("backtrace::")
        || name.contains("rthas::profiler")
        || name.contains("on_sigalrm")
        || name.contains("sigtramp")
        || name.contains("_sigtramp")
}

/// Tokio / std / pthread frames bury user functions in a text tree.
pub fn is_runtime_frame(name: &str) -> bool {
    const MARKERS: &[&str] = &[
        "tokio::",
        "std::thread",
        "std::panic",
        "std::panicking",
        "std::sys::",
        "std::rt::",
        "std::time::",
        "std::hint::",
        "core::ops::function",
        "core::panic",
        "core::ptr::",
        "core::cmp::",
        "core::hint::",
        "alloc::boxed",
        "__pthread",
        "_pthread",
        "pthread_",
        "___rust_try",
        "start_wqthread",
    ];
    MARKERS.iter().any(|m| name.contains(m))
}

fn compact_stacks(stacks: &[(Vec<String>, i64)]) -> Vec<(Vec<String>, i64)> {
    let mut out = Vec::with_capacity(stacks.len());
    for (stack, n) in stacks {
        let kept: Vec<String> = stack
            .iter()
            .filter(|name| !is_runtime_frame(name))
            .cloned()
            .collect();
        if !kept.is_empty() {
            out.push((kept, *n));
        }
    }
    out
}

struct Node {
    count: i64,
    children: HashMap<String, Node>,
}

impl Node {
    fn add(&mut self, stack: &[String], n: i64) {
        self.count += n;
        if let Some((head, rest)) = stack.split_first() {
            self.children.entry(head.clone()).or_default().add(rest, n);
        }
    }

    fn self_count(&self) -> i64 {
        let kids: i64 = self.children.values().map(|c| c.count).sum();
        self.count - kids
    }
}

impl Default for Node {
    fn default() -> Self {
        Self {
            count: 0,
            children: HashMap::new(),
        }
    }
}

pub fn render_text(
    stacks: &[(Vec<String>, i64)],
    total: i64,
    elapsed: Duration,
    hz: i32,
    depth: usize,
    top_n: usize,
    event: &str,
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "── rthas profiler [{event}] ── {:.1}s ── {hz} Hz ── {total} samples ──",
        elapsed.as_secs_f64()
    );
    if total <= 0 {
        out.push_str("no samples collected — was the process idle?\n");
        return out;
    }

    let mut root = Node::default();
    for (stack, n) in stacks {
        root.add(stack, *n);
    }

    let _ = writeln!(out, "  {:>6}  {:>7}  STACK", "SHARE", "SAMPLES");
    print_tree(&mut out, &root, total, 0, depth);

    let mut hot: Vec<(i64, String)> = Vec::new();
    collect_self(&root, &mut Vec::new(), &mut hot);
    hot.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    hot.truncate(top_n.max(1));

    out.push('\n');
    let _ = writeln!(out, "top {n} by self:", n = hot.len());
    for (count, name) in hot {
        let share = 100.0 * count as f64 / total as f64;
        let _ = writeln!(out, "  {share:>5.1}%  {count:>7}  {name}");
    }
    out
}

fn print_tree(out: &mut String, node: &Node, total: i64, indent: usize, depth: usize) {
    if indent > depth {
        return;
    }
    let mut kids: Vec<(&String, &Node)> = node.children.iter().collect();
    kids.sort_by(|a, b| b.1.count.cmp(&a.1.count).then_with(|| a.0.cmp(b.0)));
    for (name, child) in kids {
        let share = 100.0 * child.count as f64 / total as f64;
        let pad = indent.saturating_mul(2);
        let _ = writeln!(
            out,
            "  {share:>5.1}%  {:>7}  {:pad$}{name}",
            child.count,
            "",
            pad = pad
        );
        print_tree(out, child, total, indent + 1, depth);
    }
}

fn collect_self(node: &Node, path: &mut Vec<String>, out: &mut Vec<(i64, String)>) {
    for (name, child) in &node.children {
        path.push(name.clone());
        let self_n = child.self_count();
        if self_n > 0 {
            out.push((self_n, path.last().cloned().unwrap_or_default()));
        }
        collect_self(child, path, out);
        path.pop();
    }
}

pub fn render_collapsed(stacks: &[(Vec<String>, i64)]) -> String {
    let mut out = String::new();
    for (stack, n) in stacks {
        if stack.is_empty() {
            continue;
        }
        let _ = writeln!(out, "{} {n}", stack.join(";"));
    }
    out
}

// ---------------------------------------------------------------------------
// Wall clock: ITIMER_REAL / SIGALRM (samples even when the process is asleep)
// ---------------------------------------------------------------------------

const WALL_CAP: usize = 4096;
const WALL_DEPTH: usize = 64;

#[derive(Copy, Clone)]
struct WallSlot {
    len: usize,
    ips: [*mut libc::c_void; WALL_DEPTH],
}

static WALL_ON: AtomicBool = AtomicBool::new(false);
static WALL_NEXT: AtomicUsize = AtomicUsize::new(0);
static mut WALL_SLOTS: [WallSlot; WALL_CAP] = [WallSlot {
    len: 0,
    ips: [ptr::null_mut(); WALL_DEPTH],
}; WALL_CAP];

extern "C" fn on_sigalrm(_sig: libc::c_int) {
    if !WALL_ON.load(Ordering::Relaxed) {
        return;
    }
    let seq = WALL_NEXT.fetch_add(1, Ordering::Relaxed);
    let i = seq % WALL_CAP;
    let mut ips = [ptr::null_mut(); WALL_DEPTH];
    let mut len = 0usize;
    // SAFETY: signal handler; only stores instruction pointers.
    unsafe {
        backtrace::trace_unsynchronized(|frame| {
            if len >= WALL_DEPTH {
                return false;
            }
            ips[len] = frame.ip();
            len += 1;
            true
        });
        WALL_SLOTS[i].len = len;
        WALL_SLOTS[i].ips = ips;
    }
}

fn wall_start(hz: i32) -> Result<libc::sigaction, String> {
    WALL_NEXT.store(0, Ordering::SeqCst);
    WALL_ON.store(true, Ordering::SeqCst);
    let prev = unsafe { install_sigalrm() };
    if set_itimer_real(hz) != 0 {
        WALL_ON.store(false, Ordering::SeqCst);
        unsafe { restore_sigalrm(prev) };
        return Err("setitimer(ITIMER_REAL) failed".into());
    }
    Ok(prev)
}

fn wall_stop(prev: libc::sigaction) {
    let _ = set_itimer_real(0);
    WALL_ON.store(false, Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(5));
    unsafe { restore_sigalrm(prev) };
}

fn wall_sample_count() -> i64 {
    WALL_NEXT.load(Ordering::Relaxed) as i64
}

fn wall_snapshot(elapsed: Duration, hz: i32) -> Snapshot {
    let ticks = WALL_NEXT.load(Ordering::SeqCst);
    let n = ticks.min(WALL_CAP);
    let mut folded: HashMap<Vec<String>, i64> = HashMap::new();
    let mut raw_samples = 0i64;
    // SAFETY: timer is off; handlers have had a few ms to finish.
    unsafe {
        for i in 0..n {
            let slot = ptr::addr_of!(WALL_SLOTS[i]).read();
            if slot.len == 0 {
                continue;
            }
            raw_samples += 1;
            let stack = symbolise_wall(&slot.ips[..slot.len]);
            if stack.is_empty() {
                continue;
            }
            *folded.entry(stack).or_insert(0) += 1;
        }
    }
    Snapshot::from_wall(folded.into_iter().collect(), raw_samples, elapsed, hz)
}

fn symbolise_wall(ips: &[*mut libc::c_void]) -> Vec<String> {
    let mut names = Vec::new();
    for &ip in ips {
        let mut hit = false;
        backtrace::resolve(ip, |sym| {
            if hit {
                return;
            }
            let raw = match (sym.name(), sym.filename()) {
                (Some(name), _) => name.to_string(),
                _ => return,
            };
            let name = trim_rustc_hash(&raw);
            if skip_frame(name) {
                hit = true;
                return;
            }
            names.push(name.to_string());
            hit = true;
        });
    }
    names.reverse();
    names
}

fn set_itimer_real(hz: i32) -> libc::c_int {
    let (sec, usec) = if hz <= 0 {
        (0, 0)
    } else {
        let us = 1_000_000 / i64::from(hz.max(1));
        ((us / 1_000_000) as libc::time_t, (us % 1_000_000) as libc::suseconds_t)
    };
    let mut it = libc::itimerval {
        it_interval: libc::timeval {
            tv_sec: sec,
            tv_usec: usec,
        },
        it_value: libc::timeval {
            tv_sec: sec,
            tv_usec: usec,
        },
    };
    unsafe { libc::setitimer(libc::ITIMER_REAL, &mut it, ptr::null_mut()) }
}

unsafe fn install_sigalrm() -> libc::sigaction {
    let mut new: libc::sigaction = std::mem::zeroed();
    new.sa_sigaction = on_sigalrm as libc::sighandler_t;
    new.sa_flags = libc::SA_RESTART;
    libc::sigemptyset(&mut new.sa_mask);
    let mut old: libc::sigaction = std::mem::zeroed();
    libc::sigaction(libc::SIGALRM, &new, &mut old);
    old
}

unsafe fn restore_sigalrm(old: libc::sigaction) {
    libc::sigaction(libc::SIGALRM, &old, ptr::null_mut());
}

/// Exercise the sampler. Isolated from other tests because SIGPROF / SIGALRM
/// are process-global and cargo test runs cases in parallel.
#[cfg(test)]
fn burn_cpu(ms: u64) -> u64 {
    let until = Instant::now() + Duration::from_millis(ms);
    let mut x = 1u64;
    while Instant::now() < until {
        x = x
            .wrapping_mul(0x5bd1e995)
            .wrapping_add(x >> 3)
            .wrapping_add(0x27d4eb2f);
        std::hint::black_box(x);
    }
    x
}

#[cfg(test)]
mod tests {
    use super::{render_collapsed, render_text, trim_rustc_hash, EventKind};
    use std::sync::Mutex;
    use std::time::Duration;

    static TEST_LOCK: Mutex<()> = Mutex::new(());

    fn sample_stacks() -> Vec<(Vec<String>, i64)> {
        vec![
            (
                vec!["main".into(), "handle_request".into(), "lookup".into()],
                7,
            ),
            (
                vec!["main".into(), "handle_request".into(), "read_block".into()],
                3,
            ),
        ]
    }

    #[test]
    fn strips_rustc_symbol_hash() {
        assert_eq!(
            trim_rustc_hash("example_app::checksum::h0123456789abcdef"),
            "example_app::checksum"
        );
        assert_eq!(trim_rustc_hash("plain"), "plain");
        assert_eq!(
            trim_rustc_hash("::hnothexxxxxxxxxxxx"),
            "::hnothexxxxxxxxxxxx"
        );
    }

    #[test]
    fn event_kind_parses_cpu_and_wall() {
        assert_eq!(EventKind::parse("").unwrap(), EventKind::Cpu);
        assert_eq!(EventKind::parse("cpu").unwrap(), EventKind::Cpu);
        assert_eq!(EventKind::parse("wall").unwrap(), EventKind::Wall);
        assert!(EventKind::parse("alloc").is_err());
    }

    #[test]
    fn text_tree_sums_to_one_hundred() {
        let stacks = sample_stacks();
        let text = render_text(&stacks, 10, Duration::from_secs(1), 99, 8, 5, "cpu");
        assert!(text.contains("100.0%"));
        assert!(text.contains("handle_request"));
        assert!(text.contains("lookup"));
        assert!(text.contains("top 2 by self:"));
        assert!(text.contains("10 samples"));
    }

    #[test]
    fn collapsed_is_root_to_leaf() {
        let folded = render_collapsed(&sample_stacks());
        assert!(folded.contains("main;handle_request;lookup 7"));
        assert!(folded.contains("main;handle_request;read_block 3"));
    }

    #[test]
    fn empty_report_explains_itself() {
        let text = render_text(&[], 0, Duration::from_millis(200), 99, 8, 5, "cpu");
        assert!(text.contains("no samples collected"));
    }

    #[test]
    fn start_stop_collects_samples() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = super::stop();
        super::start_kind(200, EventKind::Cpu).expect("start profiler");
        let _ = super::burn_cpu(400);
        let snap = super::stop().expect("stop profiler");
        assert_eq!(snap.event, EventKind::Cpu);
        assert!(
            snap.total() > 0,
            "expected CPU samples after a busy loop, got 0 (elapsed {:?})",
            snap.elapsed
        );
        let text = snap.render_text(8, 5, true);
        assert!(text.contains("samples"));
        assert!(text.contains("[cpu]"));
    }

    #[test]
    fn wall_samples_while_sleeping() {
        let _lock = TEST_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let _ = super::stop();
        super::start_kind(80, EventKind::Wall).expect("start wall profiler");
        std::thread::sleep(Duration::from_millis(350));
        let snap = super::stop().expect("stop wall profiler");
        assert_eq!(snap.event, EventKind::Wall);
        assert!(
            snap.raw_samples > 0,
            "expected wall samples during sleep, got raw=0 elapsed={:?}",
            snap.elapsed
        );
        let text = snap.render_text(8, 5, false);
        assert!(text.contains("[wall]"));
    }

    #[test]
    fn compact_drops_runtime_keeps_user() {
        let stacks = vec![(
            vec![
                "__pthread_cond_wait".into(),
                "tokio::runtime::task::raw::poll".into(),
                "example_app::handle_request".into(),
                "example_app::crunch".into(),
            ],
            4,
        )];
        let compact = super::compact_stacks(&stacks);
        assert_eq!(
            compact,
            vec![(
                vec![
                    "example_app::handle_request".into(),
                    "example_app::crunch".into()
                ],
                4
            )]
        );
    }
}
