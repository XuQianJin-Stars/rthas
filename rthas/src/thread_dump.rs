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

//! One-shot native stacks for `thread <tid>` / `--n` / `--all`.
//!
//! There is no JVMTI `GetAllStackTraces` in Rust. The portable trick is the
//! same one profilers use: interrupt each thread with a signal, record
//! instruction pointers in the handler, symbolise afterwards. SIGURG is
//! used so this does not collide with the SIGPROF sampler in `profiler`.

use std::collections::HashMap;
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use crate::profiler::{is_runtime_frame, trim_rustc_hash};
use crate::sample::signal_threads;

const MAX_THREADS: usize = 256;
const MAX_DEPTH: usize = 64;
const DUMP_SIG: libc::c_int = libc::SIGURG;

#[derive(Copy, Clone)]
struct Slot {
    tid: u64,
    len: usize,
    ips: [*mut libc::c_void; MAX_DEPTH],
}

static ACTIVE: AtomicBool = AtomicBool::new(false);
static NEXT: AtomicUsize = AtomicUsize::new(0);
static mut SLOTS: [Slot; MAX_THREADS] = [Slot {
    tid: 0,
    len: 0,
    ips: [ptr::null_mut(); MAX_DEPTH],
}; MAX_THREADS];

extern "C" fn on_sigurg(_sig: libc::c_int) {
    if !ACTIVE.load(Ordering::SeqCst) {
        return;
    }
    capture_slot();
}

fn capture_slot() {
    let i = NEXT.fetch_add(1, Ordering::SeqCst);
    if i >= MAX_THREADS {
        return;
    }
    let mut ips = [ptr::null_mut(); MAX_DEPTH];
    let mut len = 0usize;
    // SAFETY: `trace_unsynchronized` is the backtrace crate's signal-handler
    // entry. We only store instruction pointers; symbolisation happens later.
    unsafe {
        backtrace::trace_unsynchronized(|frame| {
            if len >= MAX_DEPTH {
                return false;
            }
            ips[len] = frame.ip();
            len += 1;
            true
        });
    }
    let tid = os_tid();
    // SAFETY: `i` is unique because of fetch_add, and ACTIVE stays true until
    // the dumper copies these slots out.
    unsafe {
        SLOTS[i].tid = tid;
        SLOTS[i].len = len;
        SLOTS[i].ips = ips;
    }
}

fn os_tid() -> u64 {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: gettid cannot fail.
        unsafe { libc::gettid() as u64 }
    }
    #[cfg(target_os = "macos")]
    {
        let mut id: u64 = 0;
        // SAFETY: pthread_self is always valid.
        unsafe { libc::pthread_threadid_np(libc::pthread_self(), &mut id) };
        id
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0
    }
}

/// Capture native stacks for `tids`. Waits `wait` for signals to land.
pub fn capture(tids: &[u64], wait: Duration, compact: bool) -> HashMap<u64, Vec<String>> {
    if tids.is_empty() {
        return HashMap::new();
    }
    let me = os_tid();
    NEXT.store(0, Ordering::SeqCst);
    ACTIVE.store(true, Ordering::SeqCst);

    let prev = install_handler();
    capture_slot();
    let _sent = signal_threads(DUMP_SIG, tids, me);
    std::thread::sleep(wait.max(Duration::from_millis(20)));
    let n = NEXT.load(Ordering::SeqCst).min(MAX_THREADS);
    let mut raw: Vec<(u64, Vec<*mut libc::c_void>)> = Vec::with_capacity(n);
    // SAFETY: handlers have had `wait` to finish; we copy before clearing ACTIVE.
    unsafe {
        for i in 0..n {
            let slot = ptr::addr_of!(SLOTS[i]).read();
            if slot.tid == 0 || slot.len == 0 {
                continue;
            }
            raw.push((slot.tid, slot.ips[..slot.len].to_vec()));
        }
    }
    ACTIVE.store(false, Ordering::SeqCst);
    restore_handler(prev);

    let mut best: HashMap<u64, Vec<*mut libc::c_void>> = HashMap::new();
    for (tid, ips) in raw {
        if !tids.contains(&tid) && tid != me {
            continue;
        }
        best.entry(tid)
            .and_modify(|old| {
                if ips.len() > old.len() {
                    *old = ips.clone();
                }
            })
            .or_insert(ips);
    }

    let want: std::collections::HashSet<u64> = tids.iter().copied().collect();
    best.into_iter()
        .filter(|(tid, _)| want.contains(tid))
        .map(|(tid, ips)| (tid, symbolise(&ips, compact)))
        .collect()
}

fn symbolise(ips: &[*mut libc::c_void], compact: bool) -> Vec<String> {
    let mut names = Vec::new();
    for &ip in ips {
        let mut hit = false;
        backtrace::resolve(ip, |sym| {
            if hit {
                return;
            }
            let raw = match sym.name() {
                Some(n) => n.to_string(),
                None => return,
            };
            let name = trim_rustc_hash(&raw);
            if skip_frame(name) {
                return;
            }
            if compact && is_runtime_frame(name) {
                return;
            }
            names.push(name.to_string());
            hit = true;
        });
    }
    names
}

fn skip_frame(name: &str) -> bool {
    name.is_empty()
        || name.contains("rthas::thread_dump")
        || name.contains("backtrace::")
        || name.contains("sigtramp")
        || name.contains("_sigtramp")
}

fn install_handler() -> libc::sigaction {
    unsafe {
        let mut new: libc::sigaction = std::mem::zeroed();
        new.sa_sigaction = on_sigurg as libc::sighandler_t;
        new.sa_flags = libc::SA_RESTART;
        libc::sigemptyset(&mut new.sa_mask);
        let mut old: libc::sigaction = std::mem::zeroed();
        libc::sigaction(DUMP_SIG, &new, &mut old);
        old
    }
}

fn restore_handler(old: libc::sigaction) {
    unsafe {
        libc::sigaction(DUMP_SIG, &old, ptr::null_mut());
    }
}

#[cfg(test)]
mod tests {
    use super::{capture, os_tid, skip_frame};
    use std::time::Duration;

    #[test]
    fn dump_includes_this_thread() {
        let me = os_tid();
        let stacks = capture(&[me], Duration::from_millis(30), true);
        let frames = stacks.get(&me).expect("self stack");
        assert!(
            !frames.is_empty(),
            "expected at least one symbolised frame, got {frames:?}"
        );
    }

    #[test]
    fn skips_handler_frames() {
        assert!(skip_frame("rthas::thread_dump::capture_slot"));
        assert!(!skip_frame("example_app::crunch"));
    }
}
