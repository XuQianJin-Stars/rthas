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

//! Parse `/proc/<pid>` snapshots so eBPF attach can answer sysenv / memory / jvm
//! about the *target* process. Parsers take strings so they unit-test on macOS.

use std::fmt::Write as _;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MemorySnap {
    pub rss_bytes: u64,
    pub virt_bytes: u64,
    pub peak_rss_bytes: u64,
    pub swap_bytes: u64,
    pub threads: u32,
}

pub fn parse_environ(bytes: &[u8]) -> Vec<(String, String)> {
    let mut rows = Vec::new();
    for chunk in bytes.split(|b| *b == 0) {
        if chunk.is_empty() {
            continue;
        }
        let s = String::from_utf8_lossy(chunk);
        if let Some((k, v)) = s.split_once('=') {
            rows.push((k.to_string(), v.to_string()));
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

pub fn parse_cmdline(bytes: &[u8]) -> String {
    let parts: Vec<String> = bytes
        .split(|b| *b == 0)
        .filter(|c| !c.is_empty())
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect();
    format!("{parts:?}")
}

pub fn parse_status_memory(status: &str) -> MemorySnap {
    MemorySnap {
        rss_bytes: status_kb(status, "VmRSS:"),
        virt_bytes: status_kb(status, "VmSize:"),
        peak_rss_bytes: status_kb(status, "VmHWM:"),
        swap_bytes: status_kb(status, "VmSwap:"),
        threads: status_u32(status, "Threads:").unwrap_or(0),
    }
}

fn status_kb(status: &str, key: &str) -> u64 {
    status
        .lines()
        .find_map(|line| {
            let rest = line.strip_prefix(key)?;
            let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
            Some(kb.saturating_mul(1024))
        })
        .unwrap_or(0)
}

fn status_u32(status: &str, key: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        let rest = line.strip_prefix(key)?;
        rest.split_whitespace().next()?.parse().ok()
    })
}

pub fn render_memory(m: &MemorySnap) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{:<16} {:<12}", "METRIC", "VALUE");
    let _ = writeln!(out, "{:<16} {:<12}", "rss", fmt_bytes(m.rss_bytes));
    if m.virt_bytes > 0 {
        let _ = writeln!(out, "{:<16} {:<12}", "virt", fmt_bytes(m.virt_bytes));
    }
    if m.peak_rss_bytes > 0 {
        let _ = writeln!(
            out,
            "{:<16} {:<12}",
            "peak_rss",
            fmt_bytes(m.peak_rss_bytes)
        );
    }
    let _ = writeln!(out, "{:<16} {:<12}", "swap", fmt_bytes(m.swap_bytes));
    let _ = writeln!(out, "{:<16} {:<12}", "threads", m.threads);
    out
}

pub fn render_sysenv(rows: &[(String, String)], name: &str) -> String {
    let rows: Vec<_> = if name.is_empty() {
        rows.to_vec()
    } else {
        rows.iter().filter(|(k, _)| k == name).cloned().collect()
    };
    let mut out = String::new();
    if rows.is_empty() {
        if name.is_empty() {
            let _ = writeln!(out, "no environment variables");
        } else {
            let _ = writeln!(out, "no env var named '{name}'");
        }
        return out;
    }
    let _ = writeln!(out, "{:<28} VALUE", "KEY");
    for (k, v) in &rows {
        let _ = writeln!(out, "{:<28} {v}", truncate(k, 28));
    }
    let _ = writeln!(out, "\n{} var(s)", rows.len());
    out
}

fn truncate(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// One `/proc/<pid>/task/<tid>/stat` reading (cumulative CPU in ticks).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TaskSnap {
    pub id: u64,
    pub name: String,
    pub state: char,
    pub cpu_ticks: u64,
}

/// `(state, utime+stime ticks)` from a `/proc/.../stat` line.
pub fn parse_stat_cpu(stat: &str) -> (char, u64) {
    let Some(after) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
        return ('-', 0);
    };
    let fields: Vec<&str> = after.split_whitespace().collect();
    let state = fields.first().and_then(|s| s.chars().next()).unwrap_or('-');
    let utime = fields.get(11).and_then(|s| s.parse().ok()).unwrap_or(0u64);
    let stime = fields.get(12).and_then(|s| s.parse().ok()).unwrap_or(0u64);
    (state, utime.saturating_add(stime))
}

pub fn state_name(state: char) -> &'static str {
    match state {
        'R' => "running",
        'S' => "sleeping",
        'D' => "disk",
        'T' => "stopped",
        'Z' => "zombie",
        _ => "unknown",
    }
}

pub fn parse_loadavg(line: &str) -> f64 {
    line.split_whitespace()
        .next()
        .and_then(|s| s.parse().ok())
        .unwrap_or(0.0)
}

pub fn parse_uptime_secs(line: &str) -> f64 {
    parse_loadavg(line)
}

/// Field 22 (starttime) of `/proc/<pid>/stat`, in clock ticks since boot.
pub fn parse_start_ticks(stat: &str) -> u64 {
    let Some(after) = stat.rsplit_once(')').map(|(_, rest)| rest) else {
        return 0;
    };
    after
        .split_whitespace()
        .nth(19)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0)
}

pub fn fmt_clock(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

pub fn state_matches(want: &str, have: &str) -> bool {
    if want.is_empty() {
        return true;
    }
    let n = match want.trim().to_ascii_lowercase().as_str() {
        "r" | "running" | "runnable" => "running",
        "s" | "sleeping" | "waiting" | "timed_waiting" | "timed-waiting" => "sleeping",
        "d" | "disk" | "blocked" | "uninterruptible" => "disk",
        "t" | "stopped" => "stopped",
        "z" | "zombie" | "terminated" => "zombie",
        _ => "",
    };
    if n.is_empty() {
        have.eq_ignore_ascii_case(want)
    } else {
        have == n
    }
}

pub fn ticks_to_ns(ticks: u64, hz: u64) -> u64 {
    let hz = hz.max(1);
    ticks.saturating_mul(1_000_000_000) / hz
}

/// Turn two cumulative snapshots into a CPU rate over `wall_ns`.
pub fn task_rows(before: &[TaskSnap], after: &[TaskSnap], wall_ns: u64, hz: u64) -> Vec<ThreadRow> {
    let wall_ns = wall_ns.max(1);
    let prior: std::collections::HashMap<u64, u64> =
        before.iter().map(|t| (t.id, t.cpu_ticks)).collect();
    after
        .iter()
        .map(|t| {
            let base = prior.get(&t.id).copied().unwrap_or(t.cpu_ticks);
            let used_ns = ticks_to_ns(t.cpu_ticks.saturating_sub(base), hz) as f64;
            ThreadRow {
                id: t.id,
                name: t.name.clone(),
                state: state_name(t.state),
                cpu: (used_ns / wall_ns as f64).clamp(0.0, 1.0),
            }
        })
        .collect()
}

#[derive(Clone, Debug)]
pub struct ThreadRow {
    pub id: u64,
    pub name: String,
    pub state: &'static str,
    pub cpu: f64,
}

pub fn render_thread_table(rows: &[ThreadRow], total: usize) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "{:>8}  {:<24} {:<10} {:>7}",
        "TID", "NAME", "STATE", "CPU"
    );
    for t in rows {
        let name = if t.name.len() <= 24 {
            t.name.as_str()
        } else {
            &t.name[..24]
        };
        let _ = writeln!(
            out,
            "{:>8}  {:<24} {:<10} {:>6.1}%",
            t.id,
            name,
            t.state,
            t.cpu * 100.0
        );
    }
    let _ = writeln!(out, "\n{} of {} thread(s) shown", rows.len(), total);
    out
}

pub struct DashSample {
    pub pid: u32,
    pub cpu: f64,
    pub rss_bytes: u64,
    pub threads: u32,
    pub load1: f64,
    pub uptime: String,
    pub symbols: usize,
    pub enabled: usize,
}

pub fn render_dashboard(sample: &DashSample, activity: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        "── rthas dashboard ── pid {} ── up {} ── ebpf ──────────────",
        sample.pid, sample.uptime
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
    let _ = writeln!(
        out,
        "  {:<8}{} registered · {} enabled · backend ebpf",
        "SYMBOLS", sample.symbols, sample.enabled
    );
    out.push('\n');
    out.push_str(activity);
    out
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn environ_splits_on_nul() {
        let rows = parse_environ(b"PATH=/bin\0HOME=/tmp\0");
        assert_eq!(rows[0].0, "HOME");
        assert_eq!(rows[1].1, "/bin");
    }

    #[test]
    fn cmdline_joins() {
        assert_eq!(parse_cmdline(b"app\0--foo\0"), r#"["app", "--foo"]"#);
    }

    #[test]
    fn status_rss() {
        let status = "VmRSS:\t  1234 kB\nThreads:\t7\n";
        let m = parse_status_memory(status);
        assert_eq!(m.rss_bytes, 1234 * 1024);
        assert_eq!(m.threads, 7);
    }

    #[test]
    fn stat_cpu_and_task_rows() {
        // comm in parens, then state R, then 11 skipped fields, then utime 10 stime 5
        let stat = "1234 (worker) R 1 1 1 1 1 1 1 1 1 1 10 5 0";
        let (state, ticks) = parse_stat_cpu(stat);
        assert_eq!(state, 'R');
        assert_eq!(ticks, 15);
        let before = [TaskSnap {
            id: 7,
            name: "worker".into(),
            state: 'R',
            cpu_ticks: 10,
        }];
        let after = [TaskSnap {
            id: 7,
            name: "worker".into(),
            state: 'S',
            cpu_ticks: 20,
        }];
        let rows = task_rows(&before, &after, 1_000_000_000, 100);
        assert_eq!(rows[0].state, "sleeping");
        assert!((rows[0].cpu - 0.1).abs() < 1e-9);
        assert_eq!(parse_loadavg("1.25 0.80 0.50 2/100 1"), 1.25);
        assert!(state_matches("RUNNABLE", "running"));
        assert!(!state_matches("sleeping", "running"));
        assert_eq!(
            parse_start_ticks("1 (x) S 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 0 12345 0"),
            12345
        );
        assert_eq!(fmt_clock(3661), "01:01:01");
    }
}
