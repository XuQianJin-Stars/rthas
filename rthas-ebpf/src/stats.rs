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

//! `stats` / `top` / `monitor` over eBPF enter/exit pairs.
//!
//! There is no in-process ring and no `Err` bit: every sample is a latency.
//! The ERR / fail-rate columns stay in the table so the layout matches the
//! instrumented agent, but they are always zero.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::format::format_dur;
use crate::DEFAULT_STATS_SECONDS;

/// One completed uprobe/uretprobe pair.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sample {
    pub id: usize,
    pub dur_ns: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Agg {
    pub count: u64,
    pub total_ns: u64,
    pub max_ns: u64,
    pub durs: Vec<u64>,
}

/// How long `stats` / `top` collect. Neither flag → default window.
pub fn collect_limit(seconds: f64, count: usize) -> (Option<f64>, usize) {
    if seconds > 0.0 {
        (Some(seconds), count)
    } else if count == 0 {
        (Some(DEFAULT_STATS_SECONDS), 0)
    } else {
        (None, count)
    }
}

pub fn aggregate(samples: &[Sample]) -> Vec<(usize, Agg)> {
    let mut by_id: HashMap<usize, Agg> = HashMap::new();
    for s in samples {
        let a = by_id.entry(s.id).or_default();
        a.count += 1;
        a.total_ns += s.dur_ns;
        a.max_ns = a.max_ns.max(s.dur_ns);
        a.durs.push(s.dur_ns);
    }
    let mut v: Vec<(usize, Agg)> = by_id.into_iter().collect();
    v.sort_by_key(|(id, _)| *id);
    v
}

pub fn quantile(sorted: &[u64], q: f64) -> u64 {
    if sorted.is_empty() {
        return 0;
    }
    let idx = ((sorted.len() - 1) as f64 * q).round() as usize;
    sorted[idx.min(sorted.len() - 1)]
}

pub fn render_stats(samples: &[Sample], names: &[String]) -> String {
    let mut rows = aggregate(samples);
    let mut out = String::new();
    if rows.is_empty() {
        let _ = writeln!(
            out,
            "no calls recorded. the pattern may not be on a hot path, or the window was too short"
        );
        return out;
    }
    let _ = writeln!(
        out,
        "{:<44} {:>7} {:>6} {:>9} {:>9} {:>9} {:>9} {:>10}",
        "PATH", "COUNT", "ERR", "P50", "P95", "P99", "MAX", "TOTAL"
    );
    for (id, mut a) in rows.drain(..) {
        a.durs.sort_unstable();
        let _ = writeln!(
            out,
            "{:<44} {:>7} {:>6} {:>9} {:>9} {:>9} {:>9} {:>10}",
            truncate(name_of(names, id), 44),
            a.count,
            0,
            format_dur(quantile(&a.durs, 0.50)),
            format_dur(quantile(&a.durs, 0.95)),
            format_dur(quantile(&a.durs, 0.99)),
            format_dur(a.max_ns),
            format_dur(a.total_ns),
        );
    }
    let _ = writeln!(
        out,
        "\n{} sample(s); ERR is always 0 on eBPF attach (no Debug values)",
        samples.len()
    );
    out
}

pub fn render_top(samples: &[Sample], names: &[String], n: usize, by: &str) -> String {
    let mut rows = aggregate(samples);
    match by {
        "max" => rows.sort_by_key(|(_, a)| std::cmp::Reverse(a.max_ns)),
        "count" => rows.sort_by_key(|(_, a)| std::cmp::Reverse(a.count)),
        _ => rows.sort_by_key(|(_, a)| std::cmp::Reverse(a.total_ns)),
    }
    rows.truncate(n.max(1));

    let mut out = String::new();
    if rows.is_empty() {
        let _ = writeln!(out, "no calls recorded");
        return out;
    }
    let _ = writeln!(
        out,
        "{:<44} {:>7} {:>6} {:>10} {:>10} {:>9}",
        "PATH", "COUNT", "ERR", "TOTAL", "MAX", "AVG"
    );
    for (id, a) in rows {
        let avg = a.total_ns / a.count.max(1);
        let _ = writeln!(
            out,
            "{:<44} {:>7} {:>6} {:>10} {:>10} {:>9}",
            truncate(name_of(names, id), 44),
            a.count,
            0,
            format_dur(a.total_ns),
            format_dur(a.max_ns),
            format_dur(avg),
        );
    }
    out
}

pub fn render_monitor(samples: &[Sample], names: &[String], ts: &str) -> String {
    let mut out = String::new();
    let _ = writeln!(
        out,
        " timestamp            PATH                                       total  success  fail  avg-rt     fail-rate"
    );
    let _ = writeln!(
        out,
        "-----------------------------------------------------------------------------------------------------------"
    );
    let mut rows = aggregate(samples);
    if rows.is_empty() {
        let _ = writeln!(out, " {ts:<20} (no calls this interval)");
        let _ = writeln!(out);
        return out;
    }
    rows.sort_by_key(|(_, a)| std::cmp::Reverse(a.total_ns));
    for (id, a) in rows {
        let avg = a.total_ns / a.count.max(1);
        let _ = writeln!(
            out,
            " {ts:<20} {:<42} {:>5} {:>8} {:>5} {:>9} {:>9.2}%",
            truncate(name_of(names, id), 42),
            a.count,
            a.count,
            0,
            format_dur(avg),
            0.0,
        );
    }
    let _ = writeln!(out);
    out
}

/// UTC `YYYY-MM-DD HH:MM:SS` for monitor frames (no timezone crate).
pub fn utc_stamp() -> String {
    format_utc(SystemTime::now())
}

fn format_utc(st: SystemTime) -> String {
    let dur = st.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64;
    let days = secs.div_euclid(86_400);
    let day_secs = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

fn civil_from_days(z: i64) -> (i32, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y as i32, m as u32, d)
}

fn name_of(names: &[String], id: usize) -> &str {
    names.get(id).map(|s| s.as_str()).unwrap_or("<unknown>")
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

#[cfg(test)]
mod tests {
    use super::*;

    fn names() -> Vec<String> {
        vec!["fast".into(), "slow".into(), "rare".into()]
    }

    fn samples() -> Vec<Sample> {
        vec![
            Sample { id: 0, dur_ns: 10 },
            Sample { id: 0, dur_ns: 20 },
            Sample { id: 0, dur_ns: 30 },
            Sample {
                id: 1,
                dur_ns: 1_000,
            },
            Sample {
                id: 1,
                dur_ns: 3_000,
            },
            Sample { id: 2, dur_ns: 5 },
        ]
    }

    #[test]
    fn stats_table_has_percentiles_and_zero_err() {
        let text = render_stats(&samples(), &names());
        assert!(text.contains("PATH"));
        assert!(text.contains("fast"));
        assert!(text.contains("slow"));
        assert!(text.contains("P50"));
        assert!(text.contains("ERR is always 0"));
        let fast = text.lines().find(|l| l.contains("fast")).unwrap();
        assert!(fast.contains("      0"));
    }

    #[test]
    fn empty_stats_explains_itself() {
        let text = render_stats(&[], &names());
        assert!(text.contains("no calls recorded"));
    }

    #[test]
    fn top_by_max_puts_slow_first() {
        let text = render_top(&samples(), &names(), 10, "max");
        let lines: Vec<_> = text
            .lines()
            .filter(|l| l.contains("fast") || l.contains("slow") || l.contains("rare"))
            .collect();
        assert!(lines[0].contains("slow"), "{text}");
    }

    #[test]
    fn top_by_count_puts_fast_first() {
        let text = render_top(&samples(), &names(), 1, "count");
        assert!(text.contains("fast"));
        assert!(!text.contains("slow"));
    }

    #[test]
    fn monitor_empty_interval() {
        let text = render_monitor(&[], &names(), "2020-01-01 00:00:00");
        assert!(text.contains("(no calls this interval)"));
        assert!(text.contains("2020-01-01 00:00:00"));
    }

    #[test]
    fn monitor_success_equals_total() {
        let text = render_monitor(&samples(), &names(), "t");
        let slow = text.lines().find(|l| l.contains("slow")).unwrap();
        // total 2, success 2, fail 0
        assert!(slow.contains("    2"));
        assert!(slow.contains("0.00%"));
    }

    #[test]
    fn quantile_p50_of_three() {
        let mut d = vec![10u64, 20, 30];
        d.sort_unstable();
        assert_eq!(quantile(&d, 0.50), 20);
    }

    #[test]
    fn utc_epoch() {
        assert_eq!(format_utc(UNIX_EPOCH), "1970-01-01 00:00:00");
    }

    #[test]
    fn default_window_is_three_seconds() {
        assert_eq!(collect_limit(0.0, 0), (Some(DEFAULT_STATS_SECONDS), 0));
        assert_eq!(collect_limit(5.0, 0), (Some(5.0), 0));
        assert_eq!(collect_limit(0.0, 100), (None, 100));
    }

    #[test]
    fn stats_from_paired_exits() {
        use crate::tree::Forest;
        let mut f = Forest::new();
        f.enter(1, 0, 0);
        f.enter(1, 1, 10);
        let inner = f.exit(1, 1, 30).unwrap();
        let root = f.exit(1, 0, 50).unwrap();
        let samples = vec![
            Sample {
                id: inner.node.id,
                dur_ns: inner.node.dur_ns,
            },
            Sample {
                id: root.node.id,
                dur_ns: root.node.dur_ns,
            },
        ];
        let names = vec!["outer".into(), "inner".into()];
        let text = render_stats(&samples, &names);
        assert!(text.contains("outer"));
        assert!(text.contains("inner"));
        assert!(text.contains("2 sample(s)"));
    }
}
