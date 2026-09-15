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

//! Arthas `jvm` / `sysprop` analog: a snapshot of this process, not a VM.
//!
//! There is no HotSpot to query. What we *can* show is the OS view of the
//! process plus the rustc facts stamped in at compile time. Properties are
//! read-only — Rust has no `System.setProperty`. Mutable rthas knobs live
//! under `options`.

use std::fmt::Write as _;

use crate::sample::{cpu_count, memory_info};
use crate::time::{format_datetime, format_dur, now_ns, to_system_time, tz_hours};
use crate::{max_str, MAGIC};

/// Compile-time rustc line (`rustc 1.xx ...`), or `"unknown"`.
pub fn rustc_version() -> &'static str {
    env!("RTHAS_RUSTC")
}

/// Cargo target triple this crate was built for.
pub fn rustc_target() -> &'static str {
    env!("RTHAS_TARGET")
}

pub fn rustc_profile() -> &'static str {
    env!("RTHAS_PROFILE")
}

pub fn rustc_opt_level() -> &'static str {
    env!("RTHAS_OPT_LEVEL")
}

/// Flat key/value table behind `sysprop [NAME]`.
///
/// Empty `name` lists everything. A hit on the exact key wins; otherwise keys
/// that start with `name.` are returned, so `sysprop os` shows `os.name`.
pub fn sysprop_entries(name: &str) -> Vec<(String, String)> {
    let mut rows = all_props();
    if !name.is_empty() {
        let exact: Vec<_> = rows.iter().filter(|(k, _)| k == name).cloned().collect();
        if !exact.is_empty() {
            rows = exact;
        } else {
            let prefix = format!("{name}.");
            rows.retain(|(k, _)| k.starts_with(&prefix) || k == name);
        }
    }
    rows.sort_by(|a, b| a.0.cmp(&b.0));
    rows
}

/// Arthas-style sectioned dump for `jvm` (also `runtime`).
pub fn render_jvm() -> String {
    let mut out = String::new();
    let mem = memory_info();
    let uname = uname_info();

    section(&mut out, "RUNTIME");
    kv(&mut out, "HOST", hostname());
    kv(&mut out, "PID", std::process::id());
    kv(&mut out, "START-TIME", format_datetime(to_system_time(0)));
    kv(&mut out, "UPTIME", format_dur(now_ns()));
    kv(&mut out, "SPEC-NAME", "Rust");
    kv(&mut out, "VM-NAME", "rthas");
    kv(&mut out, "VM-VERSION", env!("CARGO_PKG_VERSION"));
    kv(&mut out, "RUSTC", rustc_version());
    kv(&mut out, "TARGET", rustc_target());
    kv(&mut out, "PROFILE", rustc_profile());
    kv(&mut out, "OPT-LEVEL", rustc_opt_level());
    kv(&mut out, "OS", std::env::consts::OS);
    kv(&mut out, "ARCH", std::env::consts::ARCH);
    kv(
        &mut out,
        "UNAME",
        format!("{} {}", uname.sysname, uname.release),
    );
    kv(&mut out, "CPUS", cpu_count());
    kv(&mut out, "POINTER-WIDTH", format!("{}-bit", usize::BITS));
    kv(&mut out, "ENDIAN", endian());
    kv(&mut out, "EXE", exe_path());
    kv(&mut out, "CWD", cwd());
    kv(&mut out, "INPUT-ARGUMENTS", args_line());
    kv(&mut out, "LIBRARY-PATH", library_path());
    kv(&mut out, "PROBE-MAGIC", MAGIC);

    section(&mut out, "FEATURES");
    kv(&mut out, "TOKIO-TASK", yes_no(cfg!(feature = "tokio-task")));
    kv(&mut out, "DEBUG-ASSERTIONS", yes_no(cfg!(debug_assertions)));
    kv(&mut out, "PANIC", panic_strategy());

    section(&mut out, "THREADS");
    kv(&mut out, "COUNT", mem.threads);

    section(&mut out, "MEMORY");
    kv(
        &mut out,
        "RSS",
        format!(
            "{}{}",
            fmt_bytes(mem.rss_bytes),
            if mem.rss_is_peak { " (peak)" } else { "" }
        ),
    );
    if mem.virt_bytes > 0 {
        kv(&mut out, "VIRT", fmt_bytes(mem.virt_bytes));
    }
    if mem.peak_rss_bytes > 0 {
        kv(&mut out, "PEAK-RSS", fmt_bytes(mem.peak_rss_bytes));
    }
    if mem.swap_bytes > 0 || !mem.rss_is_peak {
        kv(&mut out, "SWAP", fmt_bytes(mem.swap_bytes));
    }

    section(&mut out, "FILE-DESC");
    if mem.fds > 0 {
        kv(
            &mut out,
            "OPEN",
            format!(
                "{} / {}",
                mem.fds,
                if mem.fd_limit == 0 {
                    "-".to_string()
                } else {
                    mem.fd_limit.to_string()
                }
            ),
        );
    } else if mem.fd_limit > 0 {
        kv(&mut out, "MAX", mem.fd_limit);
    } else {
        kv(&mut out, "OPEN", "-");
    }

    out
}

/// Table dump for `sysprop [NAME]`.
pub fn render_sysprop(name: &str) -> String {
    let rows = sysprop_entries(name);
    let mut out = String::new();
    if rows.is_empty() {
        if name.is_empty() {
            let _ = writeln!(out, "no properties");
        } else {
            let _ = writeln!(out, "no property named '{name}'");
        }
        return out;
    }
    let _ = writeln!(out, "{:<28} VALUE", "KEY");
    for (k, v) in &rows {
        let _ = writeln!(out, "{:<28} {v}", truncate(k, 28));
    }
    let _ = writeln!(out, "\n{} prop(s)", rows.len());
    out
}

fn all_props() -> Vec<(String, String)> {
    let uname = uname_info();
    vec![
        ("available.processors", cpu_count().to_string()),
        ("cwd", cwd()),
        ("endian", endian().to_string()),
        ("exe", exe_path()),
        ("file.separator", std::path::MAIN_SEPARATOR.to_string()),
        ("hostname", hostname()),
        ("line.separator", "\\n".to_string()),
        ("os.arch", std::env::consts::ARCH.to_string()),
        ("os.family", std::env::consts::FAMILY.to_string()),
        ("os.machine", uname.machine),
        ("os.name", std::env::consts::OS.to_string()),
        ("os.version", uname.release),
        ("os.uname", uname.sysname),
        (
            "path.separator",
            if cfg!(windows) { ";" } else { ":" }.to_string(),
        ),
        ("pid", std::process::id().to_string()),
        ("pointer.width", usize::BITS.to_string()),
        ("rthas.max-str", max_str().to_string()),
        (
            "rthas.tokio-task",
            yes_no(cfg!(feature = "tokio-task")).to_string(),
        ),
        ("rthas.tz-hours", format_tz(tz_hours())),
        ("rthas.version", env!("CARGO_PKG_VERSION").to_string()),
        (
            "rustc.debug-assertions",
            yes_no(cfg!(debug_assertions)).to_string(),
        ),
        ("rustc.endian", endian().to_string()),
        ("rustc.opt-level", rustc_opt_level().to_string()),
        ("rustc.panic", panic_strategy().to_string()),
        ("rustc.pointer-width", usize::BITS.to_string()),
        ("rustc.profile", rustc_profile().to_string()),
        ("rustc.target", rustc_target().to_string()),
        ("rustc.version", rustc_version().to_string()),
        ("user.dir", cwd()),
        ("user.home", env_or("HOME", "-")),
        ("user.name", env_or("USER", "-")),
    ]
    .into_iter()
    .map(|(k, v)| (k.to_string(), v))
    .collect()
}

struct Uname {
    sysname: String,
    release: String,
    machine: String,
}

fn uname_info() -> Uname {
    let mut u = std::mem::MaybeUninit::<libc::utsname>::uninit();
    if unsafe { libc::uname(u.as_mut_ptr()) } != 0 {
        return Uname {
            sysname: "-".into(),
            release: "-".into(),
            machine: "-".into(),
        };
    }
    let u = unsafe { u.assume_init() };
    Uname {
        sysname: c_str(&u.sysname),
        release: c_str(&u.release),
        machine: c_str(&u.machine),
    }
}

fn hostname() -> String {
    let mut buf = [0 as libc::c_char; 256];
    let rc = unsafe { libc::gethostname(buf.as_mut_ptr(), buf.len()) };
    if rc == 0 {
        let s = c_str(&buf);
        if !s.is_empty() {
            return s;
        }
    }
    std::env::var("HOSTNAME")
        .or_else(|_| std::env::var("HOST"))
        .unwrap_or_else(|_| "-".into())
}

fn c_str(buf: &[libc::c_char]) -> String {
    let bytes: Vec<u8> = buf
        .iter()
        .map(|c| *c as u8)
        .take_while(|b| *b != 0)
        .collect();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn exe_path() -> String {
    std::env::current_exe()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "-".into())
}

fn cwd() -> String {
    std::env::current_dir()
        .map(|p| p.display().to_string())
        .unwrap_or_else(|_| "-".into())
}

fn args_line() -> String {
    let args: Vec<String> = std::env::args().collect();
    format!("{args:?}")
}

fn library_path() -> String {
    for key in [
        "LD_LIBRARY_PATH",
        "DYLD_LIBRARY_PATH",
        "DYLD_FALLBACK_LIBRARY_PATH",
    ] {
        if let Ok(v) = std::env::var(key) {
            if !v.is_empty() {
                return v;
            }
        }
    }
    "-".into()
}

fn endian() -> &'static str {
    if cfg!(target_endian = "little") {
        "little"
    } else {
        "big"
    }
}

fn panic_strategy() -> &'static str {
    if cfg!(panic = "abort") {
        "abort"
    } else {
        "unwind"
    }
}

fn yes_no(v: bool) -> &'static str {
    if v {
        "true"
    } else {
        "false"
    }
}

fn env_or(key: &str, fallback: &str) -> String {
    std::env::var(key).unwrap_or_else(|_| fallback.to_string())
}

fn format_tz(hours: f64) -> String {
    if hours.fract() == 0.0 {
        format!("{hours:.0}")
    } else {
        format!("{hours}")
    }
}

fn section(out: &mut String, title: &str) {
    if !out.is_empty() {
        out.push('\n');
    }
    let _ = writeln!(out, "{title}");
    let _ = writeln!(out, "{}", "-".repeat(94));
}

fn kv(out: &mut String, key: &str, value: impl std::fmt::Display) {
    let _ = writeln!(out, " {:<28} {value}", key);
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
    fn sysprop_exact_os_name() {
        let rows = sysprop_entries("os.name");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].0, "os.name");
        assert_eq!(rows[0].1, std::env::consts::OS);
    }

    #[test]
    fn sysprop_prefix_lists_os_keys() {
        let rows = sysprop_entries("os");
        let keys: Vec<_> = rows.iter().map(|(k, _)| k.as_str()).collect();
        assert!(keys.contains(&"os.name"));
        assert!(keys.contains(&"os.arch"));
        assert!(keys.contains(&"os.family"));
        assert!(!keys.iter().any(|k| k.starts_with("rustc.")));
    }

    #[test]
    fn sysprop_unknown_is_empty() {
        assert!(sysprop_entries("no.such.prop.xyz").is_empty());
    }

    #[test]
    fn jvm_dump_has_runtime_and_rustc() {
        let dump = render_jvm();
        assert!(dump.contains("RUNTIME"));
        assert!(dump.contains("FEATURES"));
        assert!(dump.contains("THREADS"));
        assert!(dump.contains("MEMORY"));
        assert!(dump.contains(std::env::consts::OS));
        assert!(dump.contains(std::env::consts::ARCH));
        assert!(dump.contains(env!("CARGO_PKG_VERSION")));
        assert!(dump.contains("rthas"));
        let rustc = rustc_version();
        assert!(!rustc.is_empty());
        assert_ne!(rustc, "-");
        assert!(dump.contains(rustc));
    }

    #[test]
    fn rustc_stamp_is_plausible() {
        assert!(rustc_version().starts_with("rustc ") || rustc_version() == "unknown");
        assert!(!rustc_target().is_empty());
        assert!(!rustc_profile().is_empty());
    }
}
