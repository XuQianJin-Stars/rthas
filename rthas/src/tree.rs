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

//! Rebuilding call trees from the flat event stream.
//!
//! The recorder emits spans on *completion*, so a child always arrives before
//! its parent. That is the awkward part: when a span shows up we cannot tell
//! whether its parent is "not instrumented" (it will never arrive) or
//! "instrumented but still running" (it will arrive in a moment). Taking the
//! obvious shortcut — treat every unknown parent as a root — flattens *every*
//! tree into a list of one-node roots, because a child always beats its parent.
//!
//! So the forest buffers instead. A span whose parent has not arrived yet is
//! parked until either
//!
//! * the parent arrives — the span is adopted, and its subtree is complete, or
//! * [`DEFAULT_GRACE`] elapses — the span is released as a root of its own.
//!
//! The grace window is what keeps memory bounded: a parent may block forever
//! (a hung request, a span that never finishes, an event evicted from the ring
//! buffer) and the subtree still has to be shown.
//!
//! One shortcut *is* safe. Span ids come from a single monotonic counter and a
//! parent is always entered before its children, so `parent < span` is a
//! necessary condition for a genuine ancestor. A span reporting a parent id
//! greater than or equal to its own is reading a stale thread-local — residue
//! of an async task polled on this thread after the real parent popped — and is
//! released as a root immediately, since that parent can never produce an event.

use std::collections::hash_map::Entry;
use std::collections::{HashMap, VecDeque};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use crate::event::Event;
use crate::probe::{glob_match, registry};
use crate::time::{format_dur, format_ts, to_system_time};

/// Guard against pathological recursion when a span chain is corrupted
/// (cycle) or simply very deep.
const MAX_DEPTH: usize = 256;

/// How long a parentless span waits for its parent before being released.
///
/// Long enough to cover the gap between a child completing and its parent
/// completing (usually microseconds), short enough that a `trace` still feels
/// live. Override per command with `--grace-ms`.
pub const DEFAULT_GRACE: Duration = Duration::from_millis(250);

pub struct Node {
    pub event: Option<Event>,
    pub children: Vec<u64>,
}

/// A span that arrived before its parent and is waiting to be adopted.
struct Waiting {
    span: u64,
    since: Instant,
}

/// Accumulates spans and releases complete subtrees.
pub struct Forest {
    nodes: HashMap<u64, Node>,
    /// Spans with an unseen parent, keyed by the parent they are waiting for.
    waiting: HashMap<u64, Vec<Waiting>>,
    /// Roots released but not yet taken, in release order.
    ready: VecDeque<u64>,
    grace: Duration,
}

impl Default for Forest {
    fn default() -> Self {
        Self::with_grace(DEFAULT_GRACE)
    }
}

impl Forest {
    /// A forest with the default grace window. Only the tests use it directly;
    /// callers go through [`Self::with_grace`].
    #[allow(dead_code)]
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_grace(grace: Duration) -> Self {
        Self {
            nodes: HashMap::new(),
            waiting: HashMap::new(),
            ready: VecDeque::new(),
            grace,
        }
    }

    /// Ingest one span, then report whether a complete subtree is waiting.
    ///
    /// The return value is a **peek**: it does not remove anything. One event
    /// can release several roots (or release one while another is still in its
    /// grace window), so callers must drain the queue with [`Self::take_ready`]
    /// rather than relying on this hint — and must not treat a `None` here as
    /// "nothing to collect".
    pub fn add(&mut self, event: Event) -> Option<u64> {
        self.promote_expired(Instant::now());
        self.ingest(event);
        self.ready.front().copied()
    }

    fn ingest(&mut self, event: Event) {
        let span = event.span;
        let parent = event.parent;

        // Classified before inserting, so an absent parent cannot be confused
        // with a placeholder node created by this very call.
        let is_root = parent == 0 || parent >= span;

        match self.nodes.entry(span) {
            Entry::Occupied(mut e) => e.get_mut().event = Some(event),
            Entry::Vacant(e) => {
                e.insert(Node {
                    event: Some(event),
                    children: Vec::new(),
                });
            }
        }

        if is_root {
            self.ready.push_back(span);
        } else if let Some(p) = self.nodes.get_mut(&parent) {
            p.children.push(span);
        } else {
            self.waiting.entry(parent).or_default().push(Waiting {
                span,
                since: Instant::now(),
            });
        }

        // Anything that was waiting for *this* span can now be adopted.
        self.adopt(span);
    }

    /// Attach spans that were waiting for `span` to arrive.
    fn adopt(&mut self, span: u64) {
        let Some(waiting) = self.waiting.remove(&span) else {
            return;
        };
        // A span that already expired out of the grace window has been taken
        // by the caller, so it is no longer here to adopt.
        let Some(node) = self.nodes.get_mut(&span) else {
            return;
        };
        node.children.extend(waiting.iter().map(|w| w.span));
    }

    /// Release spans that waited longer than the grace window.
    fn promote_expired(&mut self, now: Instant) {
        if self.waiting.is_empty() {
            return;
        }
        let grace = self.grace;
        let mut expired: Vec<u64> = Vec::new();
        self.waiting.retain(|_, waiting| {
            waiting.retain(|w| {
                if now.saturating_duration_since(w.since) >= grace {
                    expired.push(w.span);
                    false
                } else {
                    true
                }
            });
            !waiting.is_empty()
        });
        for span in expired {
            if self.nodes.contains_key(&span) {
                self.ready.push_back(span);
            }
        }
    }

    /// Next released root, if one is waiting.
    pub fn take_ready(&mut self) -> Option<u64> {
        self.ready.pop_front()
    }

    /// Detach a subtree, removing it from the forest.
    ///
    /// Also clears the root out of the ready queue: `add` only peeks, so a
    /// caller that reaches for `take` directly must not leave a stale entry
    /// behind for the next drain to pick up.
    pub fn take(&mut self, root: u64) -> Option<Tree> {
        self.ready.retain(|r| *r != root);
        let node = self.nodes.remove(&root)?;
        let event = node.event?;
        let mut children = Vec::with_capacity(node.children.len());
        for child in node.children {
            if let Some(t) = self.take(child) {
                children.push(t);
            }
        }
        Some(Tree { event, children })
    }

    /// Everything left when tracing stops; kept so a partial tail is not lost.
    pub fn drain_roots(&mut self) -> Vec<Tree> {
        // Spans still waiting for a parent that is never coming become roots.
        let orphans: Vec<u64> = self
            .waiting
            .values()
            .flat_map(|w| w.iter().map(|w| w.span))
            .collect();
        self.waiting.clear();
        for span in orphans {
            if self.nodes.contains_key(&span) {
                self.ready.push_back(span);
            }
        }

        let mut out = Vec::new();
        while let Some(root) = self.take_ready() {
            if let Some(tree) = self.take(root) {
                out.push(tree);
            }
        }
        out
    }

    /// True when nothing is buffered, waiting, or pending collection.
    #[allow(dead_code)]
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty() && self.waiting.is_empty() && self.ready.is_empty()
    }
}

pub struct Tree {
    pub event: Event,
    pub children: Vec<Tree>,
}

impl Tree {
    /// Total wall time covered by the subtree — the root's own duration, since
    /// children are always contained within it.
    pub fn root_dur_ns(&self) -> u64 {
        self.event.dur_ns
    }
}

#[derive(Clone, Debug)]
pub struct RenderOpts {
    /// Print children deeper than this many levels. `0` = unlimited.
    pub depth: usize,
    /// Suppress trees whose root is faster than this.
    pub min_ns: u64,
    pub show_task: bool,
    pub show_ts: bool,
    /// Pattern marking the nodes the user asked about. Empty marks nothing.
    pub mark: String,
    /// Print the native backtrace captured at each marked node.
    pub show_stack: bool,
}

impl Default for RenderOpts {
    fn default() -> Self {
        Self {
            depth: 0,
            min_ns: 0,
            show_task: true,
            show_ts: true,
            mark: String::new(),
            show_stack: false,
        }
    }
}

/// Whether a node's probe path is the one the user asked for.
fn is_marked(opts: &RenderOpts, name: &str) -> bool {
    !opts.mark.is_empty() && glob_match(&opts.mark, name)
}

/// The native stacks captured at the marked nodes, if any were captured.
///
/// Printed separately from the tree so the tree stays readable: a symbolised
/// backtrace is tens of lines and belongs after the call path, not inside it.
pub fn render_stacks(tree: &Tree, opts: &RenderOpts) -> String {
    if !opts.show_stack {
        return String::new();
    }
    let mut out = String::new();
    render_stacks_inner(tree, opts, &mut out);
    out
}

fn render_stacks_inner(tree: &Tree, opts: &RenderOpts, out: &mut String) {
    let name = registry().path_of(tree.event.probe);
    if is_marked(opts, name) {
        match &tree.event.stack {
            Some(stack) => {
                let _ = write!(out, "\n  native stack captured entering {name}:\n");
                let frames = parse_frames(stack);
                if frames.is_empty() {
                    out.push_str("      (empty — the binary was likely stripped)\n");
                }
                for (i, frame) in frames.iter().enumerate() {
                    let _ = write!(out, "      {i:>3}: {}\n", frame.symbol);
                    if !frame.location.is_empty() {
                        let _ = write!(out, "           at {}\n", frame.location);
                    }
                }
            }
            None => {
                let _ = write!(
                    out,
                    "\n  no native stack for {name} — pass --native to capture one\n"
                );
            }
        }
    }
    for child in &tree.children {
        render_stacks_inner(child, opts, out);
    }
}

/// One frame of a captured backtrace.
struct Frame {
    symbol: String,
    location: String,
}

/// Split a `std::backtrace` rendering into frames, dropping capture noise.
///
/// A backtrace taken inside `SpanGuard::begin` runs the unwinder first
/// (`std::backtrace_rs`, `std::backtrace`) and then rthas's own span
/// bookkeeping. None of that is part of the caller's path, so every frame up to
/// and including the *last* rthas one is dropped.
fn parse_frames(stack: &str) -> Vec<Frame> {
    let frames = split_frames(stack);
    // `rposition` rather than `position`: the unwinder frames come first, but
    // rthas frames are the last ones before the caller's own code.
    let start = frames
        .iter()
        .rposition(|f| f.symbol.starts_with("rthas::"))
        .map_or(0, |i| i + 1);
    frames.into_iter().skip(start).collect()
}

/// Turn the raw rendering into `(symbol, location)` pairs.
fn split_frames(stack: &str) -> Vec<Frame> {
    let mut out: Vec<Frame> = Vec::new();
    for line in stack.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with("note:") {
            continue;
        }
        // Locations are printed on their own line, right under their frame.
        if let Some(loc) = line.strip_prefix("at ") {
            if let Some(last) = out.last_mut() {
                if last.location.is_empty() {
                    last.location = loc.to_string();
                }
            }
            continue;
        }
        // Frame lines are "N: symbol"; anything else is taken verbatim.
        let symbol = match line.split_once(':') {
            Some((index, rest)) if index.trim().chars().all(|c| c.is_ascii_digit()) => rest.trim(),
            _ => line,
        };
        out.push(Frame {
            symbol: symbol.to_string(),
            location: String::new(),
        });
    }
    out
}

/// Render a complete tree as indented text.
pub fn render(tree: &Tree, opts: &RenderOpts) -> String {
    let mut out = String::new();
    render_node(tree, opts, 0, &mut out);
    out
}

fn render_node(
    tree: &Tree,
    opts: &RenderOpts,
    level: usize,
    out: &mut String,
) {
    if level > MAX_DEPTH {
        return;
    }
    if opts.depth > 0 && level >= opts.depth {
        return;
    }

    let e = &tree.event;
    let name = registry().path_of(e.probe);

    if opts.show_ts {
        let _ = write!(out, "{} ", format_ts(to_system_time(e.start_ns)));
    }
    let _ = write!(out, "{:>10}", format_dur(e.dur_ns));
    if opts.show_task && e.task != 0 {
        let _ = write!(out, "  task#{:<4}", e.task);
    } else {
        let _ = write!(out, "  {:<4}", "");
    }
    let _ = write!(out, " {}", if e.ok { " " } else { "!" });
    if is_marked(opts, name) {
        out.push_str("==> ");
    }
    let _ = write!(out, "{}", name);

    if !e.args.is_empty() {
        let _ = write!(out, "  ({})", e.args);
    }
    if !e.ret.is_empty() {
        let _ = write!(out, "  => {}", e.ret);
    }
    out.push('\n');

    // `render_node` only ever draws a root, so children start flush-left.
    let child_prefix = "  ".to_string();
    let n = tree.children.len();
    for (i, child) in tree.children.iter().enumerate() {
        render_child(child, opts, &child_prefix, i + 1 == n, level + 1, out);
    }
}

fn render_child(
    tree: &Tree,
    opts: &RenderOpts,
    parent_prefix: &str,
    is_last: bool,
    level: usize,
    out: &mut String,
) {
    if level > MAX_DEPTH {
        return;
    }
    if opts.depth > 0 && level >= opts.depth {
        return;
    }

    let e = &tree.event;
    let name = registry().path_of(e.probe);

    if opts.show_ts {
        out.push_str(&format!("{} ", format_ts(to_system_time(e.start_ns))));
    }
    let _ = write!(out, "{:>10}", format_dur(e.dur_ns));
    if opts.show_task && e.task != 0 {
        let _ = write!(out, "  task#{:<4}", e.task);
    } else {
        let _ = write!(out, "  {:<4}", "");
    }

    let branch = if is_last { "└─ " } else { "├─ " };
    let _ = write!(out, " {}{}{}", parent_prefix, branch, if e.ok { " " } else { "!" });
    if is_marked(opts, name) {
        out.push_str("==> ");
    }
    let _ = write!(out, "{}", name);
    if !e.args.is_empty() {
        let _ = write!(out, "  ({})", e.args);
    }
    if !e.ret.is_empty() {
        let _ = write!(out, "  => {}", e.ret);
    }
    out.push('\n');

    let child_prefix = format!("{}{}   ", parent_prefix, if is_last { " " } else { "│" });
    let n = tree.children.len();
    for (i, child) in tree.children.iter().enumerate() {
        render_child(child, opts, &child_prefix, i + 1 == n, level + 1, out);
    }
}

/// Flattened rendering for `watch`: one line per call, no tree.
pub fn render_flat(e: &Event) -> String {
    let mut out = String::new();
    let name = registry().path_of(e.probe);
    let _ = write!(out, "{} ", format_ts(to_system_time(e.start_ns)));
    let _ = write!(out, "{:>10}", format_dur(e.dur_ns));
    if e.task != 0 {
        let _ = write!(out, "  task#{:<4}", e.task);
    } else {
        let _ = write!(out, "  {:<4}", "");
    }
    let _ = write!(out, " {}{}", if e.ok { " " } else { "!" }, name);
    if !e.args.is_empty() {
        let _ = write!(out, "  ({})", e.args);
    }
    if !e.ret.is_empty() {
        let _ = write!(out, "  => {}", e.ret);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    fn ev(span: u64, parent: u64, dur: u64) -> Event {
        Event {
            seq: span,
            span,
            parent,
            depth: 0,
            probe: usize::MAX,
            tid: 1,
            task: 7,
            start_ns: 0,
            dur_ns: dur,
            ok: true,
            args: String::new(),
            ret: String::new(),
            stack: None,
        }
    }

    #[test]
    fn child_before_parent_yields_root() {
        let mut f = Forest::new();
        assert_eq!(f.add(ev(2, 1, 100)), None);
        assert_eq!(f.add(ev(1, 0, 500)), Some(1));
        let tree = f.take(1).expect("root tree");
        assert_eq!(tree.children.len(), 1);
        assert!(f.is_empty());
    }

    #[test]
    fn grandchildren_link_through_a_buffered_child() {
        // Leaf-first, which is the order completion actually produces: 3 -> 2 -> 1.
        let mut f = Forest::new();
        assert_eq!(f.add(ev(3, 2, 10)), None);
        assert_eq!(f.add(ev(2, 1, 20)), None);
        assert_eq!(f.add(ev(1, 0, 30)), Some(1));
        let tree = f.take(1).expect("root tree");
        assert_eq!(tree.children.len(), 1);
        assert_eq!(tree.children[0].children.len(), 1);
        assert!(f.is_empty());
    }

    #[test]
    fn orphan_is_released_once_the_grace_window_passes() {
        let mut f = Forest::with_grace(Duration::from_millis(10));
        assert_eq!(f.add(ev(2, 1, 100)), None);
        // The parent never completes; the child must not be held forever.
        std::thread::sleep(Duration::from_millis(25));
        assert_eq!(f.add(ev(3, 0, 100)), Some(2));
        assert!(f.take(2).is_some());
    }

    #[test]
    fn untraced_parent_becomes_root() {
        let mut f = Forest::new();
        // Parent 99 was never instrumented, so it never arrives.
        assert_eq!(f.add(ev(2, 99, 100)), Some(2));
        assert!(f.take(2).is_some());
    }

    #[test]
    fn forest_reclaims_memory_after_take() {
        let mut f = Forest::new();
        f.add(ev(2, 1, 100));
        f.add(ev(1, 0, 500));
        f.take(1);
        assert!(f.is_empty());
    }
}
