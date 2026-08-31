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

//! Completed-span events and the bounded buffer they land in.
//!
//! A span is recorded **once, on completion** (`SpanGuard::finish`), never on
//! entry. Rationale: on entry we do not yet know the duration or the return
//! value, and emitting two records per call would double the buffer pressure
//! for no benefit. The cost is that a call is only visible after it returns —
//! same as Arthas `trace`, which also prints on method exit.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use crate::time::now_ns;

/// Default number of completed spans retained in the ring.
///
/// Sized so that a few seconds of a busy service fit: at ~100k calls/s this
/// holds ~160ms of history, which is plenty for `stats`/`top`, while `trace`
/// streams events out as they arrive and does not depend on the buffer depth.
pub const DEFAULT_CAPACITY: usize = 16_384;

/// One completed function call.
#[derive(Clone, Debug)]
pub struct Event {
    /// Monotonic sequence number; agents use it as a read cursor.
    pub seq: u64,
    /// Unique id of this span.
    pub span: u64,
    /// Id of the lexically enclosing span, or 0 for a root.
    pub parent: u64,
    /// Nesting depth at entry, used to indent the tree.
    pub depth: u32,
    /// Index into the probe registry.
    pub probe: usize,
    /// OS thread the span completed on.
    pub tid: u64,
    /// Tokio task id when the `tokio-task` feature is on, else 0.
    ///
    /// This is the field that makes async comprehensible: a single request
    /// is one task, even though its spans complete on many different threads.
    pub task: u64,
    /// Nanoseconds since process start (monotonic).
    pub start_ns: u64,
    pub dur_ns: u64,
    /// False when the return value looked like an `Err(..)`, or when the
    /// function returned early (panic / `return` / `?`) and we could not
    /// capture the value.
    pub ok: bool,
    /// Rendered arguments, e.g. `path="/a.txt"  off=0`.
    pub args: String,
    /// Rendered return value, e.g. `Ok(120)`.
    pub ret: String,
    /// Native backtrace captured as the span opened.
    ///
    /// `None` unless `stack` asked for it: symbolising a stack costs far more
    /// than the span itself, so the common case pays only for the `Option`.
    pub stack: Option<String>,
}

impl Event {
    pub fn dur_ms(&self) -> f64 {
        self.dur_ns as f64 / 1_000_000.0
    }
}

/// Bounded FIFO of recent spans.
///
/// A plain `Mutex<VecDeque>` rather than a lock-free ring: probe output is
/// only produced while a probe is explicitly enabled, so the lock is not on
/// the steady-state path, and avoiding `unsafe` in a diagnostics crate is
/// worth more than a few nanoseconds.
pub struct Recorder {
    inner: Mutex<Inner>,
    capacity: usize,
}

struct Inner {
    buf: VecDeque<Event>,
    next_seq: u64,
    dropped: u64,
    recorded: u64,
}

impl Recorder {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                buf: VecDeque::with_capacity(capacity.min(65_536)),
                next_seq: 1,
                dropped: 0,
                recorded: 0,
            }),
            capacity: capacity.max(1),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn push(&self, mut event: Event) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        event.seq = inner.next_seq;
        inner.next_seq += 1;
        inner.recorded += 1;
        if inner.buf.len() == self.capacity {
            inner.buf.pop_front();
            inner.dropped += 1;
        }
        inner.buf.push_back(event);
    }

    /// Every event newer than `seq`.
    pub fn since(&self, seq: u64) -> Vec<Event> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner
            .buf
            .iter()
            .filter(|e| e.seq > seq)
            .cloned()
            .collect()
    }

    pub fn last_seq(&self) -> u64 {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.next_seq.saturating_sub(1)
    }

    pub fn snapshot(&self) -> Vec<Event> {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.buf.iter().cloned().collect()
    }

    pub fn stats(&self) -> (u64, u64, u64) {
        let inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        (inner.recorded, inner.dropped, inner.next_seq - 1)
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        inner.buf.clear();
    }

    /// Wall-clock timestamp of `start_ns`, formatted for humans.
    pub fn recorded_now(&self) -> u64 {
        now_ns()
    }
}

static RECORDER: OnceLock<Recorder> = OnceLock::new();

/// The process-wide recorder.
///
/// Capacity is read once from `RTHAS_CAPACITY`.
pub fn recorder() -> &'static Recorder {
    RECORDER.get_or_init(|| {
        let cap = std::env::var("RTHAS_CAPACITY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_CAPACITY);
        Recorder::new(cap)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(seq: u64) -> Event {
        Event {
            seq,
            span: seq,
            parent: 0,
            depth: 0,
            probe: 0,
            tid: 1,
            task: 0,
            start_ns: 0,
            dur_ns: 1_000_000,
            ok: true,
            args: String::new(),
            ret: String::new(),
            stack: None,
        }
    }

    #[test]
    fn assigns_seq_and_drops_oldest() {
        let r = Recorder::new(2);
        r.push(ev(0));
        r.push(ev(0));
        r.push(ev(0));
        let all = r.snapshot();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].seq, 2);
        assert_eq!(all[1].seq, 3);
        assert_eq!(r.stats().1, 1);
    }

    #[test]
    fn since_returns_only_newer() {
        let r = Recorder::new(16);
        r.push(ev(0));
        r.push(ev(0));
        assert_eq!(r.since(1).len(), 1);
        assert_eq!(r.since(2).len(), 0);
    }
}
