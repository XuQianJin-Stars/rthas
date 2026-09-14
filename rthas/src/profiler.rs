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

//! In-process CPU profiler (Arthas `profiler`).
//!
//! Sampling uses SIGPROF via `pprof`. The handler is installed only while a
//! session is running; a disabled probe is still one atomic load, and this
//! module is not on that path.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::io::Write;
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

struct Running {
    guard: ProfilerGuard<'static>,
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

pub fn start(hz: i32) -> Result<(), String> {
    let hz = clamp_hz(hz);
    let mut slot = session();
    if slot.is_some() {
        return Err("profiler is already running; `profiler stop` first".into());
    }
    let guard = pprof::ProfilerGuardBuilder::default()
        .frequency(hz)
        .blocklist(BLOCKLIST)
        .build()
        .map_err(|e| format!("failed to start profiler: {e}"))?;
    *slot = Some(Running {
        guard,
        started: Instant::now(),
        hz,
    });
    Ok(())
}

pub fn status_line() -> String {
    match session().as_ref() {
        Some(s) => format!(
            "[cpu] profiling is running for {:.1} seconds at {} Hz",
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
    let report = running
        .guard
        .report()
        .build()
        .map_err(|e| format!("profiler report: {e}"))?;
    Ok(report.data.values().map(|c| *c as i64).sum())
}

pub fn stop() -> Result<Snapshot, String> {
    let running = session()
        .take()
        .ok_or_else(|| "profiler is not running".to_string())?;
    let elapsed = running.started.elapsed();
    let hz = running.hz;
    let report = running
        .guard
        .report()
        .build()
        .map_err(|e| format!("profiler report: {e}"))?;
    Ok(Snapshot::from_report(report, elapsed, hz))
}

/// Folded stacks plus the original `pprof` report (needed for SVG).
pub struct Snapshot {
    pub elapsed: Duration,
    pub hz: i32,
    /// Samples that survived frame filtering.
    pub stacks: Vec<(Vec<String>, i64)>,
    /// Samples `pprof` recorded, including stacks we later dropped.
    pub raw_samples: i64,
    report: pprof::Report,
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
            elapsed,
            hz,
            stacks,
            raw_samples,
            report,
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
        render_text(&stacks, self.total(), self.elapsed, self.hz, depth, top_n)
    }

    pub fn render_collapsed(&self) -> String {
        render_collapsed(&self.stacks)
    }

    pub fn write_flamegraph<W: Write>(&self, mut writer: W) -> Result<(), String> {
        self.report
            .flamegraph(&mut writer)
            .map_err(|e| format!("flamegraph: {e}"))
    }
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
) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "── rthas profiler ── {:.1}s ── {hz} Hz ── {total} samples ──",
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

/// Exercise the sampler. Isolated from other tests because SIGPROF is
/// process-global and cargo test runs cases in parallel.
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
    use super::{render_collapsed, render_text, trim_rustc_hash};
    use std::time::Duration;

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
    fn text_tree_sums_to_one_hundred() {
        let stacks = sample_stacks();
        let text = render_text(&stacks, 10, Duration::from_secs(1), 99, 8, 5);
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
        let text = render_text(&[], 0, Duration::from_millis(200), 99, 8, 5);
        assert!(text.contains("no samples collected"));
    }

    #[test]
    fn start_stop_collects_samples() {
        let _ = super::stop();
        super::start(200).expect("start profiler");
        let _ = super::burn_cpu(400);
        let snap = super::stop().expect("stop profiler");
        assert!(
            snap.total() > 0,
            "expected CPU samples after a busy loop, got 0 (elapsed {:?})",
            snap.elapsed
        );
        let text = snap.render_text(8, 5, true);
        assert!(text.contains("samples"));
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
