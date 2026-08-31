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

//! Process and thread sampling behind `dashboard` and `thread`.
//!
//! Arthas reads these numbers out of the JVM through JMX and JVMTI. A Rust
//! process has no runtime to ask, so we read them out of the OS instead:
//! `/proc` on Linux, `getrusage` and Mach on macOS.
//!
//! None of this is on the probe hot path — it only runs while a `dashboard` or
//! `thread` session is attached, which is exactly when paying for a few syscalls
//! is acceptable.

use std::time::{Duration, Instant};

/// How long `thread` waits between two reads to turn cumulative CPU clocks into
/// a rate. Long enough to smooth out scheduling noise, short enough that the
/// command still feels interactive. Only Linux needs it: Mach reports an
/// instantaneous usage that needs no differencing.
#[cfg(target_os = "linux")]
const THREAD_WINDOW: Duration = Duration::from_millis(150);

/// Mach's scale factor for `thread_basic_info::cpu_usage` (1000 = one core).
#[cfg(target_os = "macos")]
const TH_USAGE_SCALE: f64 = 1000.0;

/// One process-wide reading.
#[derive(Clone, Copy, Debug, Default)]
pub struct Sample {
    /// Share of one core burned since the previous reading. `1.0` means one
    /// core fully saturated; the ceiling is [`cpu_count`].
    pub cpu: f64,
    pub rss_bytes: u64,
    pub threads: u32,
    /// One-minute load average. `0.0` where the platform does not expose it.
    pub load1: f64,
    /// Time since `rthas` initialised its clock, i.e. essentially process uptime.
    pub uptime: Duration,
}

/// One row of the `thread` table.
#[derive(Clone, Debug)]
pub struct ThreadRow {
    /// OS thread id — the same value recorded as `tid` on every event, which is
    /// what lets the table be joined against the span stream.
    pub id: u64,
    pub name: String,
    /// `running` / `sleeping` / `disk` / `stopped` / `zombie` / `unknown`.
    pub state: &'static str,
    /// Share of one core used over the sampling window, `0.0`–`1.0`.
    pub cpu: f64,
}

/// Turns cumulative CPU clocks into a rate.
///
/// CPU time is only meaningful as a delta, so the first reading of a `Meter` is
/// always zero — a `dashboard` frame needs two samples to say anything.
pub struct Meter {
    cpu: Duration,
    at: Instant,
}

impl Meter {
    pub fn new() -> Self {
        Self {
            cpu: cpu_time(),
            at: Instant::now(),
        }
    }

    /// Advance one interval and report what changed during it.
    pub fn tick(&mut self) -> Sample {
        let cpu = cpu_time();
        let at = Instant::now();
        let wall = at.saturating_duration_since(self.at).as_nanos().max(1) as f64;
        let used = cpu.saturating_sub(self.cpu).as_nanos() as f64;
        self.cpu = cpu;
        self.at = at;

        Sample {
            cpu: (used / wall).clamp(0.0, cpu_count() as f64),
            rss_bytes: rss_bytes(),
            threads: thread_count(),
            load1: load_avg(),
            uptime: Duration::from_nanos(crate::time::now_ns()),
        }
    }
}

impl Default for Meter {
    fn default() -> Self {
        Self::new()
    }
}

/// Usable parallelism, used as the ceiling for `Sample::cpu`.
pub fn cpu_count() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1)
}

/// Cumulative CPU time burned by the whole process.
pub fn cpu_time() -> Duration {
    #[cfg(target_os = "linux")]
    {
        // Field 14 (utime) and 15 (stime) of /proc/self/stat, in clock ticks.
        // `comm` is parenthesised and may contain spaces, so anchor on the last
        // ')' before counting fields.
        std::fs::read_to_string("/proc/self/stat")
            .ok()
            .and_then(|stat| {
                let after = stat.rsplit_once(')')?.1;
                let fields: Vec<&str> = after.split_whitespace().collect();
                let utime: u64 = fields.get(11)?.parse().ok()?;
                let stime: u64 = fields.get(12)?.parse().ok()?;
                Some(ticks_to_duration(utime + stime))
            })
            .unwrap_or(Duration::ZERO)
    }

    #[cfg(not(target_os = "linux"))]
    {
        // macOS has no /proc; getrusage is the portable fallback.
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: `usage` is a live rusage and getrusage fills it on success.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if rc != 0 {
            return Duration::ZERO;
        }
        // SAFETY: getrusage returned 0, so the struct is initialised.
        let usage = unsafe { usage.assume_init() };
        timeval_to_duration(usage.ru_utime) + timeval_to_duration(usage.ru_stime)
    }
}

/// Resident set size in bytes.
pub fn rss_bytes() -> u64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .and_then(|status| {
                status.lines().find_map(|line| {
                    let rest = line.strip_prefix("VmRSS:")?;
                    let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                    Some(kb.saturating_mul(1024))
                })
            })
            .unwrap_or(0)
    }

    #[cfg(not(target_os = "linux"))]
    {
        // No /proc, and getrusage only reports the *peak* resident set. Close
        // enough for a dashboard, and honest about being a peak.
        let mut usage = std::mem::MaybeUninit::<libc::rusage>::uninit();
        // SAFETY: `usage` is a live rusage and getrusage fills it on success.
        let rc = unsafe { libc::getrusage(libc::RUSAGE_SELF, usage.as_mut_ptr()) };
        if rc != 0 {
            return 0;
        }
        // SAFETY: getrusage returned 0, so the struct is initialised.
        let usage = unsafe { usage.assume_init() };
        usage.ru_maxrss.max(0) as u64
    }
}

/// One-minute load average, or `0.0` when unavailable.
pub fn load_avg() -> f64 {
    #[cfg(target_os = "linux")]
    {
        std::fs::read_to_string("/proc/loadavg")
            .ok()
            .and_then(|line| line.split_whitespace().next()?.parse().ok())
            .unwrap_or(0.0)
    }

    #[cfg(target_os = "macos")]
    {
        let mut out = [0f64; 3];
        // SAFETY: `out` has room for exactly the three requested samples.
        if unsafe { libc::getloadavg(out.as_mut_ptr(), 3) } >= 1 {
            out[0]
        } else {
            0.0
        }
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        0.0
    }
}

/// Number of OS threads in this process.
pub fn thread_count() -> u32 {
    raw_threads().len() as u32
}

/// Snapshot every thread, converting CPU clocks into a rate.
///
/// Blocks for [`THREAD_WINDOW`]; there is no way to get a rate out of a
/// cumulative clock without two reads.
pub fn thread_rows() -> Vec<ThreadRow> {
    #[cfg(target_os = "linux")]
    {
        // Linux hands out a per-thread cumulative CPU clock, so difference it.
        let before = raw_threads();
        let started = Instant::now();
        std::thread::sleep(THREAD_WINDOW);
        let after = raw_threads();
        let wall = started.elapsed().as_nanos().max(1) as f64;

        let prior: std::collections::HashMap<u64, u64> =
            before.iter().map(|t| (t.id, t.cpu)).collect();
        // A thread that exited mid-window is simply not reported.
        after
            .into_iter()
            .map(|t| {
                let base = prior.get(&t.id).copied().unwrap_or(t.cpu);
                let used = t.cpu.saturating_sub(base) as f64;
                ThreadRow {
                    id: t.id,
                    name: t.name,
                    state: state_name(t.state),
                    cpu: (used / wall).clamp(0.0, 1.0),
                }
            })
            .collect()
    }

    #[cfg(target_os = "macos")]
    {
        // Mach reports a scaled *instantaneous* usage rather than a clock, so
        // there is nothing to difference.
        raw_threads()
            .into_iter()
            .map(|t| ThreadRow {
                id: t.id,
                name: t.name,
                state: state_name(t.state),
                cpu: (t.cpu as f64 / TH_USAGE_SCALE).clamp(0.0, 1.0),
            })
            .collect()
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        Vec::new()
    }
}

/// A raw platform reading for one thread.
struct RawThread {
    id: u64,
    name: String,
    state: char,
    /// Linux: cumulative CPU time in nanoseconds. macOS: scaled usage.
    cpu: u64,
}

/// Render `state` the way a `/proc` reader expects to see it.
fn state_name(state: char) -> &'static str {
    match state {
        'R' => "running",
        'S' => "sleeping",
        'D' => "disk",
        'T' => "stopped",
        'Z' => "zombie",
        _ => "unknown",
    }
}

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
fn raw_threads() -> Vec<RawThread> {
    let mut out = Vec::new();
    let Ok(dir) = std::fs::read_dir("/proc/self/task") else {
        return out;
    };
    for entry in dir.flatten() {
        let tid = match entry.file_name().to_string_lossy().parse::<u64>() {
            Ok(tid) => tid,
            Err(_) => continue,
        };
        let path = entry.path();
        let name = std::fs::read_to_string(path.join("comm"))
            .map(|s| s.trim().to_string())
            .unwrap_or_default();
        let (state, cpu) = std::fs::read_to_string(path.join("stat"))
            .map(|s| parse_thread_stat(&s))
            .unwrap_or(('-', 0));
        out.push(RawThread {
            id: tid,
            name: if name.is_empty() {
                "<unnamed>".to_string()
            } else {
                name
            },
            state,
            cpu,
        });
    }
    out.sort_by_key(|t| t.id);
    out
}

/// Pull `(state, cpu-ns)` out of one `/proc/<pid>/task/<tid>/stat` line.
#[cfg(target_os = "linux")]
fn parse_thread_stat(stat: &str) -> (char, u64) {
    // `comm` is parenthesised and may itself contain spaces and parens, so
    // anchor on the *last* ')' before counting fields.
    let Some(after) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
        return ('-', 0);
    };
    let fields: Vec<&str> = after.split_whitespace().collect();
    // What follows ')' starts at field 3 (state), so field N sits at index N-3.
    let state = fields.first().and_then(|s| s.chars().next()).unwrap_or('-');
    let utime = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0u64);
    let stime = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0u64);
    (state, ticks_to_duration(utime + stime).as_nanos() as u64)
}

/// Convert `CLK_TCK` ticks to a duration.
#[cfg(target_os = "linux")]
fn ticks_to_duration(ticks: u64) -> Duration {
    // SAFETY: sysconf has no preconditions; a non-positive answer means the
    // value is unknown, so fall back to the near-universal 100 Hz.
    let hz = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    let hz = if hz > 0 { hz as u64 } else { 100 };
    Duration::from_nanos(ticks.saturating_mul(1_000_000_000) / hz)
}

// ---------------------------------------------------------------------------
// macOS
// ---------------------------------------------------------------------------

#[cfg(target_os = "macos")]
fn raw_threads() -> Vec<RawThread> {
    use std::ptr;

    let mut out = Vec::new();
    // `mach_task_self` is soft-deprecated in favour of the mach2 crate; pulling
    // in a whole crate for one call is not worth it.
    #[allow(deprecated)]
    unsafe {
        let task = libc::mach_task_self();
        let mut list: libc::thread_act_array_t = ptr::null_mut();
        let mut count: libc::mach_msg_type_number_t = 0;
        if libc::task_threads(task, &mut list, &mut count) != libc::KERN_SUCCESS {
            return out;
        }

        for i in 0..count {
            let thread: libc::thread_act_t = *list.add(i as usize);

            let mut info = std::mem::MaybeUninit::<libc::thread_basic_info>::uninit();
            let mut info_count = (std::mem::size_of::<libc::thread_basic_info>()
                / std::mem::size_of::<libc::integer_t>())
                as libc::mach_msg_type_number_t;
            // SAFETY: `info` has room for exactly one thread_basic_info and
            // `info_count` says so.
            let rc = libc::thread_info(
                thread,
                libc::THREAD_BASIC_INFO as libc::thread_flavor_t,
                info.as_mut_ptr() as *mut libc::integer_t,
                &mut info_count,
            );
            let (state, cpu) = if rc == libc::KERN_SUCCESS {
                let info = info.assume_init();
                (
                    mach_state(info.run_state),
                    info.cpu_usage.max(0) as u64,
                )
            } else {
                ('-', 0)
            };

            // The pthread handle is the only way to reach a name, and Mach
            // hands back a port instead.
            let pthread = libc::pthread_from_mach_thread_np(thread);
            let (id, name) = if pthread == 0 {
                (u64::from(thread), String::new())
            } else {
                let mut id: u64 = 0;
                // SAFETY: `id` is a live u64; a failure just leaves it at 0.
                libc::pthread_threadid_np(pthread, &mut id);
                (id, pthread_name(pthread))
            };

            out.push(RawThread {
                id: if id != 0 { id } else { u64::from(thread) },
                name: if name.is_empty() {
                    "<unnamed>".to_string()
                } else {
                    name
                },
                state,
                cpu,
            });
        }

        // The array was allocated in our task by Mach; leaving it would leak
        // both the memory and the send rights it holds.
        let _ = libc::vm_deallocate(
            task as libc::vm_map_t,
            list as libc::vm_address_t,
            (count as libc::vm_size_t)
                * std::mem::size_of::<libc::thread_act_t>() as libc::vm_size_t,
        );
    }
    out
}

/// Ask a pthread for the name set by `pthread_setname_np`.
#[cfg(target_os = "macos")]
fn pthread_name(thread: libc::pthread_t) -> String {
    let mut buf = [0i8; 64];
    // SAFETY: `buf` is a live 64-byte buffer and the length matches it.
    // A non-zero return simply means there is no name to read.
    if unsafe { libc::pthread_getname_np(thread, buf.as_mut_ptr(), buf.len()) } != 0 {
        return String::new();
    }
    let len = buf.iter().position(|&c| c == 0).unwrap_or(buf.len());
    // SAFETY: reading `len` initialised bytes out of `buf`.
    let bytes =
        unsafe { std::slice::from_raw_parts(buf.as_ptr() as *const u8, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// Mach `run_state` constants, which `libc` does not re-export.
#[cfg(target_os = "macos")]
fn mach_state(run_state: libc::integer_t) -> char {
    match run_state {
        1 => 'R', // TH_STATE_RUNNING
        2 => 'T', // TH_STATE_STOPPED
        3 => 'S', // TH_STATE_WAITING
        4 => 'D', // TH_STATE_UNINTERRUPTIBLE
        5 => 'Z', // TH_STATE_HALTED
        _ => '-',
    }
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

/// Convert a `timeval` to a duration, tolerating the `i32`/`i64` split between
/// Linux (`suseconds_t = i64`) and macOS (`suseconds_t = i32`).
#[cfg(not(target_os = "linux"))]
fn timeval_to_duration(tv: libc::timeval) -> Duration {
    let secs = tv.tv_sec.max(0) as u64;
    let micros = tv.tv_usec.max(0) as u64;
    Duration::new(secs, (micros % 1_000_000) as u32 * 1_000)
        .saturating_add(Duration::from_secs(micros / 1_000_000))
}

#[cfg(test)]
mod tests {
    use super::{cpu_count, cpu_time, load_avg, rss_bytes, state_name, thread_rows};

    #[test]
    fn reports_plausible_process_numbers() {
        // A process that has run at all has burned some CPU and owns memory.
        assert!(cpu_count() >= 1);
        assert!(rss_bytes() > 0, "rss should be readable");
        assert!(cpu_time() > std::time::Duration::ZERO);
        // Load average can legitimately be 0 on a quiet box, so only require
        // that it parse into a finite number.
        assert!(load_avg().is_finite());
    }

    #[test]
    fn lists_at_least_the_calling_thread() {
        let rows = thread_rows();
        assert!(!rows.is_empty(), "expected at least one thread");
        for row in &rows {
            assert!(row.cpu >= 0.0 && row.cpu <= 1.0, "cpu out of range");
            assert!(!row.name.is_empty());
        }
    }

    #[test]
    fn maps_proc_states_to_words() {
        assert_eq!(state_name('R'), "running");
        assert_eq!(state_name('S'), "sleeping");
        assert_eq!(state_name('?'), "unknown");
    }
}
