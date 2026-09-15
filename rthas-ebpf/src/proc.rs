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
}
