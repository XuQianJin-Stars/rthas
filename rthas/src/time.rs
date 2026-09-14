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

//! Monotonic timing plus wall-clock rendering.
//!
//! Durations come from `Instant` (immune to NTP steps); display timestamps
//! come from a `SystemTime` sampled once next to it, so the two stay aligned
//! even though they are different clocks.

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

struct Clock {
    mono: Instant,
    wall: SystemTime,
}

fn clock() -> &'static Clock {
    static CLOCK: OnceLock<Clock> = OnceLock::new();
    CLOCK.get_or_init(|| Clock {
        mono: Instant::now(),
        wall: SystemTime::now(),
    })
}

/// Nanoseconds since the first call in this process.
#[inline]
pub fn now_ns() -> u64 {
    let c = clock();
    Instant::now().saturating_duration_since(c.mono).as_nanos() as u64
}

/// Convert a `now_ns()` reading into a wall-clock `SystemTime`.
pub fn to_system_time(ns_since_start: u64) -> SystemTime {
    clock().wall + Duration::from_nanos(ns_since_start)
}

/// Local time-zone offset in hours, from `RTHAS_TZ_HOURS` (default `0` = UTC).
///
/// Deliberately not trying to read the system zone database: that would drag
/// in `libc`/`iana-time-zone` for a purely cosmetic concern. Set the env var
/// once in your shell (`export RTHAS_TZ_HOURS=8`) if you want local time, or
/// change it later with `options tz-hours`.
fn tz_cell() -> &'static AtomicI64 {
    static OFFSET: OnceLock<AtomicI64> = OnceLock::new();
    OFFSET.get_or_init(|| {
        let secs = std::env::var("RTHAS_TZ_HOURS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|h| (h * 3600.0) as i64)
            .unwrap_or(0);
        AtomicI64::new(secs)
    })
}

fn tz_offset_secs() -> i64 {
    tz_cell().load(Ordering::Relaxed)
}

/// Display timezone offset in hours (may be fractional).
pub fn tz_hours() -> f64 {
    tz_offset_secs() as f64 / 3600.0
}

/// Update the display timezone. Takes effect on the next formatted timestamp.
pub fn set_tz_hours(hours: f64) {
    tz_cell().store((hours * 3600.0) as i64, Ordering::Relaxed);
}

/// `YYYY-MM-DD HH:MM:SS` in the configured zone, for `monitor` / `tt`.
pub fn format_datetime(st: SystemTime) -> String {
    let dur = st.duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = dur.as_secs() as i64 + tz_offset_secs();
    let days = secs.div_euclid(86_400);
    let day_secs = secs.rem_euclid(86_400);
    let (y, mo, d) = civil_from_days(days);
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    format!("{y:04}-{mo:02}-{d:02} {h:02}:{m:02}:{s:02}")
}

/// Days since Unix epoch → `(year, month, day)`. Howard Hinnant's algorithm.
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

/// `HH:MM:SS.mmm` in the configured zone.
pub fn format_ts(st: SystemTime) -> String {
    let secs = st
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
        + tz_offset_secs();
    let millis = st
        .duration_since(UNIX_EPOCH)
        .map(|d| d.subsec_millis())
        .unwrap_or(0);

    let day_secs = secs.rem_euclid(86_400);
    let h = day_secs / 3600;
    let m = (day_secs % 3600) / 60;
    let s = day_secs % 60;
    format!("{:02}:{:02}:{:02}.{:03}", h, m, s, millis)
}

/// Render a duration the way a human reads a profile: unit-scaled, 3 sig figs.
pub fn format_dur(ns: u64) -> String {
    if ns < 1_000 {
        return format!("{}ns", ns);
    }
    let us = ns as f64 / 1_000.0;
    if us < 1_000.0 {
        return format!("{:.3}us", us);
    }
    let ms = us / 1_000.0;
    if ms < 1_000.0 {
        return format!("{:.3}ms", ms);
    }
    format!("{:.3}s", ms / 1_000.0)
}

#[cfg(test)]
mod tests {
    use super::{format_datetime, format_dur, now_ns, set_tz_hours, tz_hours, UNIX_EPOCH};

    #[test]
    fn duration_units_scale() {
        assert_eq!(format_dur(999), "999ns");
        assert_eq!(format_dur(1_500), "1.500us");
        assert_eq!(format_dur(1_500_000), "1.500ms");
        assert_eq!(format_dur(1_500_000_000), "1.500s");
    }

    #[test]
    fn monotonic_clock_advances() {
        let a = now_ns();
        let b = now_ns();
        assert!(b >= a);
    }

    #[test]
    fn datetime_at_epoch_is_utc() {
        let old = tz_hours();
        set_tz_hours(0.0);
        assert_eq!(format_datetime(UNIX_EPOCH), "1970-01-01 00:00:00");
        set_tz_hours(old);
    }
}
