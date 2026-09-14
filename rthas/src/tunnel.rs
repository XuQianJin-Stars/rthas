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

//! Time-tunnel storage behind `tt`.
//!
//! Arthas keeps recorded calls in a `Map<Integer, TimeFragment>` so you can
//! inspect a call long after it happened. We do the same with a bounded FIFO
//! of [`Fragment`]s, indexed from 1000. Replay (`tt -p`) is deliberately
//! absent: we only store `Debug` strings, not typed values that could be
//! fed back into the function.

use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

use crate::event::Event;

/// Default number of fragments retained. Matches Arthas's map size of 100.
pub const DEFAULT_TT_CAPACITY: usize = 100;

/// First INDEX issued, matching Arthas so the numbers look familiar.
pub const FIRST_INDEX: u64 = 1000;

/// One recorded call, surviving after the probes that captured it go back off.
#[derive(Clone, Debug)]
pub struct Fragment {
    pub index: u64,
    pub start_ns: u64,
    pub dur_ns: u64,
    pub ok: bool,
    pub probe: usize,
    pub path: String,
    pub tid: u64,
    pub task: u64,
    pub args: String,
    pub ret: String,
}

impl Fragment {
    fn from_event(index: u64, event: &Event, path: &str) -> Self {
        Self {
            index,
            start_ns: event.start_ns,
            dur_ns: event.dur_ns,
            ok: event.ok,
            probe: event.probe,
            path: path.to_string(),
            tid: event.tid,
            task: event.task,
            args: event.args.clone(),
            ret: event.ret.clone(),
        }
    }
}

/// Bounded, indexed store of recorded calls.
pub struct Tunnel {
    inner: Mutex<Inner>,
    capacity: usize,
}

struct Inner {
    buf: VecDeque<Fragment>,
    next_index: u64,
}

impl Tunnel {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(Inner {
                buf: VecDeque::with_capacity(capacity.min(4_096)),
                next_index: FIRST_INDEX,
            }),
            capacity: capacity.max(1),
        }
    }

    pub fn capacity(&self) -> usize {
        self.capacity
    }

    pub fn len(&self) -> usize {
        self.lock().buf.len()
    }

    /// Append `event` and return the INDEX assigned to it.
    ///
    /// Drops the oldest fragment when full, the same way Arthas caps the map.
    pub fn record(&self, event: &Event, path: &str) -> u64 {
        let mut inner = self.lock();
        let index = inner.next_index;
        inner.next_index += 1;
        if inner.buf.len() == self.capacity {
            inner.buf.pop_front();
        }
        inner.buf.push_back(Fragment::from_event(index, event, path));
        index
    }

    pub fn list(&self) -> Vec<Fragment> {
        self.lock().buf.iter().cloned().collect()
    }

    pub fn get(&self, index: u64) -> Option<Fragment> {
        self.lock().buf.iter().find(|f| f.index == index).cloned()
    }

    pub fn delete(&self, index: u64) -> bool {
        let mut inner = self.lock();
        let before = inner.buf.len();
        inner.buf.retain(|f| f.index != index);
        inner.buf.len() < before
    }

    pub fn clear(&self) {
        self.lock().buf.clear();
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

static TUNNEL: OnceLock<Tunnel> = OnceLock::new();

/// The process-wide time tunnel.
///
/// Capacity is read once from `RTHAS_TT_CAPACITY`.
pub fn tunnel() -> &'static Tunnel {
    TUNNEL.get_or_init(|| {
        let cap = std::env::var("RTHAS_TT_CAPACITY")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(DEFAULT_TT_CAPACITY);
        Tunnel::new(cap)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(probe: usize, ok: bool) -> Event {
        Event {
            seq: 0,
            span: 1,
            parent: 0,
            depth: 0,
            probe,
            tid: 7,
            task: 3,
            start_ns: 1_000,
            dur_ns: 2_000_000,
            ok,
            args: "n=1".into(),
            ret: if ok { "Ok(1)".into() } else { "Err(x)".into() },
            stack: None,
        }
    }

    #[test]
    fn indexes_from_one_thousand_and_evicts_oldest() {
        let t = Tunnel::new(2);
        let a = t.record(&ev(0, true), "a");
        let b = t.record(&ev(0, true), "b");
        let c = t.record(&ev(0, true), "c");
        assert_eq!(a, FIRST_INDEX);
        assert_eq!(b, FIRST_INDEX + 1);
        assert_eq!(c, FIRST_INDEX + 2);
        let all = t.list();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].index, b);
        assert_eq!(all[1].index, c);
        assert!(t.get(a).is_none());
        assert_eq!(t.get(c).unwrap().path, "c");
    }

    #[test]
    fn delete_and_clear() {
        let t = Tunnel::new(8);
        let i = t.record(&ev(1, false), "foo");
        t.record(&ev(1, true), "bar");
        assert!(t.delete(i));
        assert!(!t.delete(i));
        assert_eq!(t.len(), 1);
        t.clear();
        assert_eq!(t.len(), 0);
    }
}
