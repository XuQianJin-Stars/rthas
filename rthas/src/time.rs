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
/// once in your shell (`export RTHAS_TZ_HOURS=8`) if you want local time.
fn tz_offset_secs() -> i64 {
    static OFFSET: OnceLock<i64> = OnceLock::new();
    *OFFSET.get_or_init(|| {
        std::env::var("RTHAS_TZ_HOURS")
            .ok()
            .and_then(|v| v.parse::<f64>().ok())
            .map(|h| (h * 3600.0) as i64)
            .unwrap_or(0)
    })
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
    use super::{format_dur, now_ns};

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
}
