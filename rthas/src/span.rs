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

//! Span lifetime: where a call sits in the call tree, and when it ends.
//!
//! # The async caveat (read this before trusting a tree)
//!
//! Parent/child linking uses a **thread-local** stack. That is exactly right
//! for synchronous code. For `async fn` it is right only within a single
//! `poll`: an `async fn` body may suspend at an `.await`, the worker thread
//! goes off and polls a different task, and that task pushes and pops its own
//! spans on the *same* thread-local stack.
//!
//! In practice most trees still come out correct, because the common case is
//! nested `async fn`s that are polled to completion without the parent
//! suspending in between. When it does go wrong you get a span reparented to
//! an unrelated root — use the `task` column to spot it: spans belonging to
//! one logical request always share a task id. A future version will carry the
//! parent explicitly in a tokio task-local, which removes the problem entirely.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::event::{recorder, Event};
use crate::probe::Probe;
use crate::time::now_ns;
use crate::{looks_like_err, EARLY_RETURN};

static NEXT_SPAN: AtomicU64 = AtomicU64::new(1);

thread_local! {
    static CURRENT: Cell<u64> = const { Cell::new(0) };
    static DEPTH: Cell<u32> = const { Cell::new(0) };
    /// OS thread id rather than a number of our own: `thread` has to join a
    /// thread row against the `tid` recorded on every event, and only the OS
    /// knows the identity of the threads it scheduled.
    static TID: u64 = os_tid();
}

/// The OS identifier of the calling thread.
fn os_tid() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: `gettid` cannot fail and takes no arguments.
        unsafe { libc::gettid() as u64 }
    }
    #[cfg(target_os = "macos")]
    {
        let mut id: u64 = 0;
        // SAFETY: `pthread_self()` is always valid and `id` is a live `u64`.
        unsafe { libc::pthread_threadid_np(libc::pthread_self(), &mut id) };
        if id != 0 {
            id
        } else {
            alloc_tid()
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        alloc_tid()
    }
}

/// Fallback numbering for platforms with no cheap thread id.
fn alloc_tid() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// An open span. Emits one event, either in `finish` or in `Drop`.
pub struct SpanGuard {
    probe: &'static Probe,
    span: u64,
    parent: u64,
    depth: u32,
    start_ns: u64,
    args: String,
    stack: Option<String>,
    finished: bool,
}

impl SpanGuard {
    /// Open a span if the probe is enabled, else return `None`.
    ///
    /// `args` is a closure so argument formatting (the expensive part) only
    /// happens when the probe is actually on — a disabled probe costs one
    /// relaxed load and nothing else.
    pub fn begin<F: FnOnce() -> String>(probe: &'static Probe, args: F) -> Option<Self> {
        if !probe.enabled() {
            return None;
        }

        let span = NEXT_SPAN.fetch_add(1, Ordering::Relaxed);
        let (parent, depth) = CURRENT.with(|c| {
            let parent = c.get();
            c.set(span);
            let depth = DEPTH.with(|d| {
                let v = d.get();
                d.set(v + 1);
                v
            });
            (parent, depth)
        });

        Some(Self {
            probe,
            span,
            parent,
            depth,
            start_ns: now_ns(),
            args: args(),
            stack: capture_stack(probe),
            finished: false,
        })
    }

    /// Close the span normally, recording the return value.
    pub fn finish(mut self, ret: String) {
        let ok = !looks_like_err(&ret);
        self.emit(ret, ok);
        self.finished = true;
    }

    fn emit(&self, ret: String, ok: bool) {
        let event = Event {
            seq: 0,
            span: self.span,
            parent: self.parent,
            depth: self.depth,
            probe: self.probe.id(),
            tid: thread_id(),
            task: task_id(),
            start_ns: self.start_ns,
            dur_ns: now_ns().saturating_sub(self.start_ns),
            ok,
            args: self.args.clone(),
            ret,
            stack: self.stack.clone(),
        };
        recorder().push(event);
    }

    /// Id of the span currently open on this thread, 0 at the top level.
    pub fn current() -> u64 {
        CURRENT.with(Cell::get)
    }

    pub fn depth() -> u32 {
        DEPTH.with(Cell::get)
    }
}

impl Drop for SpanGuard {
    fn drop(&mut self) {
        if !self.finished {
            // `?`, `return`, or a panic unwinding through the body.
            self.emit(EARLY_RETURN.to_string(), false);
        }
        // Restore the enclosing span. Best effort under async (see module doc).
        CURRENT.with(|c| c.set(self.parent));
        DEPTH.with(|d| d.set(self.depth));
    }
}

fn thread_id() -> u64 {
    TID.with(|t| *t)
}

/// Symbolise the native stack as the span opens, if `stack` asked for it.
///
/// `force_capture` rather than `capture`: `RUST_BACKTRACE` is unset in most
/// deployments, and an explicit `stack` command must still produce a stack.
fn capture_stack(probe: &Probe) -> Option<String> {
    probe
        .capture_stack()
        .then(|| std::backtrace::Backtrace::force_capture().to_string())
}

#[cfg(feature = "tokio-task")]
fn task_id() -> u64 {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};

    // `tokio::task::Id` is opaque: no `as_u64`, and `to_string()` would
    // allocate on every span. Hash it instead — same id, same number, no
    // allocation. Truncated to 24 bits so the `task#N` column stays readable;
    // a collision would need ~16M live tasks.
    tokio::task::try_id()
        .map(|id| {
            let mut hasher = DefaultHasher::new();
            id.hash(&mut hasher);
            hasher.finish() & 0x00ff_ffff
        })
        .unwrap_or(0)
}

#[cfg(not(feature = "tokio-task"))]
fn task_id() -> u64 {
    0
}
