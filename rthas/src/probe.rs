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

//! Probe points: the static (`#[trace]`) instrumentation sites.
//!
//! Every instrumented function declares one `static Probe` inside its body
//! and submits it to a linker-collected registry (`inventory`). Collection
//! happens at link time, so `rthas-cli list` sees probe points that have
//! never been executed — the same discovery experience Arthas gives you by
//! walking loaded classes.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{OnceLock, RwLock};

/// Whether an instrumented function is a plain `fn` or an `async fn`.
///
/// Recorded only for display: it warns the user that a span may be
/// fragmented (see the async caveat on [`crate::span`]).
#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum ProbeKind {
    Sync,
    Async,
}

impl ProbeKind {
    pub const fn as_str(self) -> &'static str {
        match self {
            ProbeKind::Sync => "sync",
            ProbeKind::Async => "async",
        }
    }
}

const UNASSIGNED: usize = usize::MAX;

/// `flag` bit: the probe records spans.
const FLAG_ENABLED: u8 = 1;
/// `flag` bit: the probe captures a native backtrace when a span opens.
const FLAG_STACK: u8 = 2;

/// A single instrumentation site.
///
/// `const`-constructible so it can live in a function-local `static`. The
/// hot-path check is a single relaxed atomic load.
pub struct Probe {
    pub path: &'static str,
    pub file: &'static str,
    pub line: u32,
    pub kind: ProbeKind,
    id: AtomicUsize,
    flag: AtomicU8,
}

// Placed in a `static`, so it must be shareable across threads.
unsafe impl Sync for Probe {}

impl Probe {
    pub const fn new(path: &'static str, file: &'static str, line: u32, kind: ProbeKind) -> Self {
        Self {
            path,
            file,
            line,
            kind,
            id: AtomicUsize::new(UNASSIGNED),
            flag: AtomicU8::new(0),
        }
    }

    /// Stable index of this probe in the global registry.
    #[inline]
    pub fn id(&self) -> usize {
        // Building the registry is what back-fills ids, so force it first.
        registry();
        let id = self.id.load(Ordering::Relaxed);
        debug_assert_ne!(id, UNASSIGNED, "probe was not collected by the linker");
        id
    }

    /// Hot path: one relaxed load, ~1ns, perfectly predicted when disabled.
    #[inline(always)]
    pub fn enabled(&self) -> bool {
        self.flag.load(Ordering::Relaxed) & FLAG_ENABLED != 0
    }

    pub fn set_enabled(&self, on: bool) {
        if on {
            self.flag.fetch_or(FLAG_ENABLED, Ordering::Relaxed);
        } else {
            self.flag.fetch_and(!FLAG_ENABLED, Ordering::Relaxed);
        }
    }

    /// Whether an opening span should also capture the native call stack.
    ///
    /// Off unless `stack` asks for it: symbolising a backtrace costs far more
    /// than the span itself, so it must never be on by accident.
    #[inline(always)]
    pub fn capture_stack(&self) -> bool {
        self.flag.load(Ordering::Relaxed) & FLAG_STACK != 0
    }

    pub fn set_capture_stack(&self, on: bool) {
        if on {
            self.flag.fetch_or(FLAG_STACK, Ordering::Relaxed);
        } else {
            self.flag.fetch_and(!FLAG_STACK, Ordering::Relaxed);
        }
    }
}

/// Linker-collected registration emitted by `#[rthas::trace]`.
///
/// Not part of the public API — it only exists so the macro has something to
/// submit. Kept `pub` because `inventory::submit!` requires it.
#[doc(hidden)]
pub struct ProbeSubmission(pub &'static Probe);

inventory::collect!(ProbeSubmission);

/// The global list of every instrumented function in the process.
pub struct Registry {
    probes: RwLock<Vec<&'static Probe>>,
}

impl Registry {
    /// Collect every submitted probe and back-fill its id.
    ///
    /// Sorting matters: `inventory` gives linker order, which is not stable
    /// across builds. Without the sort, probe ids (and therefore anything a
    /// user references by id) would shuffle between compilations.
    fn collect() -> Self {
        let mut probes: Vec<&'static Probe> =
            inventory::iter::<ProbeSubmission>().map(|s| s.0).collect();
        probes.sort_by_key(|p| (p.file, p.line, p.path));
        for (i, probe) in probes.iter().enumerate() {
            probe.id.store(i, Ordering::Relaxed);
        }
        Self {
            probes: RwLock::new(probes),
        }
    }

    pub fn len(&self) -> usize {
        self.probes.read().map(|p| p.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub fn get(&self, id: usize) -> Option<&'static Probe> {
        self.probes.read().ok().and_then(|p| p.get(id).copied())
    }

    pub fn all(&self) -> Vec<&'static Probe> {
        self.probes.read().map(|p| p.clone()).unwrap_or_default()
    }

    pub fn path_of(&self, id: usize) -> &'static str {
        self.get(id).map(|p| p.path).unwrap_or("<unknown>")
    }

    /// Enable/disable every probe whose path matches `pattern`.
    ///
    /// Returns how many probes changed state.
    pub fn set_enabled_matching(&self, pattern: &str, on: bool) -> usize {
        self.all()
            .iter()
            .filter(|p| glob_match(pattern, p.path))
            .map(|p| {
                let changed = p.enabled() != on;
                p.set_enabled(on);
                usize::from(changed)
            })
            .sum()
    }

    pub fn disable_all(&self) {
        for p in self.all() {
            p.set_enabled(false);
        }
    }
}

static REGISTRY: OnceLock<Registry> = OnceLock::new();

pub fn registry() -> &'static Registry {
    REGISTRY.get_or_init(Registry::collect)
}

/// Shell-style glob matching used by every `pattern` argument.
///
/// Semantics chosen to match how people actually search for Rust paths:
///   - `*` is the only metacharacter (`foo*` / `*::bar` / `a*b`)
///   - a pattern with no `*` matches by **substring**, so `get_status`
///     finds `goosefs_sdk::client::master::MasterClient::get_status`
///   - a pattern *with* a `*` is tried at every position, so `MasterClient::*`
///     also finds `goosefs_sdk::...::MasterClient::get_status`
///   - empty pattern matches everything
///
/// Trying every position is deliberate: Rust paths carry a crate and module
/// prefix that nobody wants to type, so anchoring at the start would force
/// `*` onto the front of every pattern.
pub fn glob_match(pattern: &str, s: &str) -> bool {
    if pattern.is_empty() || pattern == "*" {
        return true;
    }
    if !pattern.contains('*') {
        return s.contains(pattern);
    }

    let parts: Vec<&str> = pattern.split('*').collect();
    let (first, last) = match (parts.first(), parts.last()) {
        (Some(f), Some(l)) => (*f, *l),
        _ => return true,
    };
    let middles = &parts[1..parts.len() - 1];

    // Every offset where the leading literal occurs is a candidate start.
    // Consuming each middle at its leftmost occurrence is safe: finishing
    // earlier always leaves the most room for what follows.
    let starts: Vec<usize> = if first.is_empty() {
        vec![0]
    } else {
        s.match_indices(first).map(|(i, _)| i).collect()
    };

    for start in starts {
        let mut rest = &s[start + first.len()..];
        let mut matched = true;
        for middle in middles {
            if middle.is_empty() {
                continue;
            }
            match rest.find(middle) {
                Some(i) => rest = &rest[i + middle.len()..],
                None => {
                    matched = false;
                    break;
                }
            }
        }
        if matched && (last.is_empty() || rest.ends_with(last)) {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::glob_match;

    #[test]
    fn substring_without_star() {
        assert!(glob_match("get_status", "a::b::get_status"));
        assert!(!glob_match("get_status", "a::b::set_status"));
    }

    #[test]
    fn prefix_and_suffix_stars() {
        assert!(glob_match("MasterClient::*", "m::MasterClient::get_status"));
        assert!(!glob_match("MasterClient::*", "m::WorkerClient::get_status"));
        assert!(glob_match("*::query", "db::query"));
        assert!(glob_match("*db*", "a::db::query"));
    }

    #[test]
    fn empty_matches_all() {
        assert!(glob_match("", "anything"));
        assert!(glob_match("*", "anything"));
    }
}
