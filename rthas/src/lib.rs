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

//! `rthas` — an Arthas-flavoured runtime probe toolkit for Rust.
//!
//! Java gets Arthas because the JVM can rewrite bytecode and re-attach to a
//! live process. Rust is ahead-of-time compiled machine code with no runtime
//! to hook, so the same tricks are simply unavailable. `rthas` takes the part
//! that *is* achievable and makes it pleasant:
//!
//! ```text
//!   #[rthas::trace]              ->  rthas::init()          ->  rthas trace 'db::*'
//!   compile-time probe points        background agent            call trees, live
//! ```
//!
//! # What you get
//!
//! - [`trace`] — call trees with per-node duration, arguments and return value
//! - `watch`   — one line per matching call, filterable on args/return
//! - `stats` / `top` — p50/p95/p99/max over a rolling ring buffer
//! - all of it **off by default**: a disabled probe costs one relaxed atomic
//!   load, and argument formatting is behind a closure so it never runs
//!
//! # What you do not get (and cannot)
//!
//! - attaching to a process that was not compiled with the probes
//! - hot-patching a running function (there is no bytecode to swap)
//! - a *guaranteed* correct async call tree — see the [`span`] module docs
//!
//! # Wiring it up
//!
//! ```rust,ignore
//! #[rthas::trace]
//! async fn query(sql: &str) -> Result<usize, Error> { /* ... */ }
//!
//! #[tokio::main]
//! async fn main() {
//!     rthas::init();   // starts the control-plane thread
//!     // ... your service ...
//! }
//! ```
//!
//! ```text
//! $ cargo run --bin rthas -- ps
//! $ cargo run --bin rthas -- trace 'query' --count 5
//! ```

mod agent;
mod event;
mod probe;
mod sample;
mod span;
mod time;
mod tree;

use std::fmt::Write as _;

pub use crate::agent::{attach_trigger_path, init, init_lazy, socket_path, spawn, spawn_lazy};
pub use crate::event::{recorder, Event, Recorder, DEFAULT_CAPACITY};
pub use crate::probe::{glob_match, registry, Probe, ProbeKind, Registry};
pub use crate::probe::ProbeSubmission;
pub use crate::sample::{cpu_count, load_avg, rss_bytes, Meter, Sample, ThreadRow};
pub use crate::span::SpanGuard;
pub use crate::time::{format_dur, format_ts, now_ns};

/// The `#[rthas::trace]` attribute. Re-exported so users only need one dep.
pub use rthas_macros::trace;

/// Re-exported so the macro's `inventory::submit!` resolves without forcing
/// every user to add `inventory` to their own `Cargo.toml`.
#[doc(hidden)]
pub use inventory;

/// Marker compiled into every instrumented binary.
///
/// `rthas attach` greps the target binary for this string to decide whether
/// that process carries probe points at all. Deliberately a string literal in
/// read-only data rather than a symbol name: `strip` removes symbol tables
/// first, and a stripped release binary must still be recognisable.
#[doc(hidden)]
pub const MAGIC: &str = "rthas/probe/v1";

/// Upper bound on a rendered argument or return value.
///
/// Unbounded `Debug` output of a large struct would blow up the ring buffer
/// and make the trace unreadable. Override with `RTHAS_MAX_STR`.
fn max_str() -> usize {
    static MAX: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *MAX.get_or_init(|| {
        std::env::var("RTHAS_MAX_STR")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(256)
    })
}

/// Clip `s` to [`max_str`], marking truncation with an ellipsis.
fn clip(s: &str) -> &str {
    let max = max_str();
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Render named arguments the way `watch` displays them: `a=1  b="x"`.
///
/// Called through a closure from the generated probe so the formatting only
/// happens while the probe is enabled.
pub fn fmt_args(pairs: &[(&str, &dyn std::fmt::Debug)]) -> String {
    let mut out = String::new();
    for (i, (name, value)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push_str("  ");
        }
        let rendered = format!("{:?}", value);
        let _ = write!(out, "{}={}", name, clip(&rendered));
        let _ = write!(out, "{}", if rendered.len() > max_str() { "…" } else { "" });
    }
    out
}

/// Render a return value.
pub fn fmt_ret<T: std::fmt::Debug>(value: &T) -> String {
    let rendered = format!("{:?}", value);
    let clipped = clip(&rendered);
    if clipped.len() < rendered.len() {
        format!("{}…", clipped)
    } else {
        clipped.to_string()
    }
}

/// Heuristic used to flag a span red in the trace output.
///
/// Rust has no runtime representation of "this returned an Err" once the
/// value has been `Debug`-formatted, so we look at the shape of the text.
/// Good enough for a diagnostics overlay, and honest about being a heuristic.
pub fn looks_like_err(ret: &str) -> bool {
    ret.starts_with("Err(") || ret.starts_with("Err ") || ret == EARLY_RETURN
}

/// Placeholder return value for a function that exited before `finish`.
pub const EARLY_RETURN: &str = "<early-return>";

#[cfg(test)]
mod tests {
    use super::{fmt_args, fmt_ret, looks_like_err, EARLY_RETURN};

    #[test]
    fn renders_named_args() {
        let a = 1;
        let b = "x";
        assert_eq!(fmt_args(&[("a", &a), ("b", &b)]), r#"a=1  b="x""#);
    }

    #[test]
    fn detects_err_shapes() {
        assert!(looks_like_err("Err(Timeout)"));
        assert!(looks_like_err(EARLY_RETURN));
        assert!(!looks_like_err("Ok(3)"));
        assert!(!looks_like_err("3"));
    }

    #[test]
    fn clips_long_values() {
        let long = "x".repeat(500);
        let out = fmt_ret(&long);
        assert!(out.len() < 300);
        assert!(out.ends_with('…'));
    }
}
