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
// See the License for the specific language governing terms and
// limitations under the License.

//! Userspace side of `rthas attach --ebpf`.
//!
//! The helper binary binds the same Unix socket as an in-process agent so
//! `rthas trace --pid` keeps working. The eBPF program itself only builds on
//! Linux; this library (symbols, glob, call-stack pairing) is what we can
//! test on macOS.

pub mod args;
pub mod event;
pub mod format;
pub mod glob;
pub mod server;
pub mod stats;
pub mod symbols;
pub mod tree;

#[cfg(target_os = "linux")]
pub mod bpf;

pub const END: &str = "<<<end>>>";
pub const MAX_ATTACH: usize = 64;
pub const DEFAULT_TRACE_COUNT: usize = 20;
pub const DEFAULT_WATCH_COUNT: usize = 50;
/// `stats` / `top` on eBPF attach collect for this long when neither
/// `--seconds` nor `--count` is given (there is no in-process ring to snapshot).
pub const DEFAULT_STATS_SECONDS: f64 = 3.0;

/// Human-readable reason this helper cannot run on the current OS.
pub const LINUX_REQUIRED: &str =
    "eBPF attach requires Linux (kernel 5.5+, CAP_BPF/root, unstripped binary)";
