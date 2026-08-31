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

//! A small async service wired up with `rthas` probes.
//!
//! Run it, then poke at it from another terminal:
//!
//! ```text
//! cargo run --bin rthas -- ps
//! cargo run --bin rthas -- trace handle_request --count 3
//! cargo run --bin rthas -- watch read_block --ret Err --count 5
//! cargo run --bin rthas -- top --n 5 --by max
//! ```
//!
//! The `send` option on the request path is not decoration: instrumenting an
//! `async fn` rewrites its return type to `impl Future`, which is not
//! automatically `Send`, and `tokio::spawn` requires `Send`. Add it to any
//! instrumented `async fn` whose future you spawn.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Domain types
// ---------------------------------------------------------------------------

/// Number of bytes `read_block` pretends to have fetched.
const BLOCK_LEN: usize = 32;

#[derive(Debug, Clone)]
struct Meta {
    block_id: u64,
    #[allow(dead_code)]
    len: usize,
}

#[derive(Debug)]
enum AppError {
    NotFound(String),
    Timeout(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::NotFound(p) => write!(f, "not found: {p}"),
            AppError::Timeout(p) => write!(f, "timeout reading: {p}"),
        }
    }
}

// ---------------------------------------------------------------------------
// A stateful component: proves instrumentation works on `impl` methods
// ---------------------------------------------------------------------------

struct Cache {
    hits: AtomicU64,
    misses: AtomicU64,
}

impl Cache {
    fn new() -> Self {
        Self {
            hits: AtomicU64::new(0),
            misses: AtomicU64::new(0),
        }
    }

    /// `async fn` method taking `&self`: the macro must add `+ '_` to the
    /// generated `impl Future`, otherwise this will not compile.
    #[rthas::trace(send)]
    async fn get(&self, key: &str) -> Option<u64> {
        // Actually yields, so the async span path (not just the sync one)
        // gets exercised.
        tokio::time::sleep(Duration::from_millis(2)).await;
        if hash(key) % 3 == 0 {
            self.hits.fetch_add(1, Ordering::Relaxed);
            Some(key.len() as u64 * 8)
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
            None
        }
    }

    /// Synchronous method holding a secret: `skip` keeps it out of the trace.
    #[rthas::trace(skip(secret))]
    fn authorize(&self, user: &str, secret: &str) -> bool {
        !secret.is_empty() && user.len() > 2
    }
}

static AUTH: LazyLock<Cache> = LazyLock::new(Cache::new);
static CACHE: LazyLock<Cache> = LazyLock::new(Cache::new);

// ---------------------------------------------------------------------------
// Request path
// ---------------------------------------------------------------------------

/// Entry point. `?` here exits early on error — the span is still reported
/// with `<early-return>` as its value, so failures are never silently lost.
#[rthas::trace(send)]
async fn handle_request(id: u64, path: &str) -> Result<u64, AppError> {
    if !AUTH.authorize("alice", "s3cret") {
        return Err(AppError::NotFound(path.to_string()));
    }

    if let Some(cached) = CACHE.get(path).await {
        return Ok(cached);
    }

    let meta = lookup_metadata(path).await?;
    let block = read_block(id, meta.block_id).await?;
    Ok(checksum(&block))
}

#[rthas::trace(send)]
async fn lookup_metadata(path: &str) -> Result<Meta, AppError> {
    let jitter = path.len() as u64 % 25;
    tokio::time::sleep(Duration::from_millis(15 + jitter)).await;
    if path.contains("missing") {
        return Err(AppError::NotFound(path.to_string()));
    }
    Ok(Meta {
        block_id: hash(path),
        len: BLOCK_LEN,
    })
}

#[rthas::trace(send)]
async fn read_block(id: u64, block_id: u64) -> Result<Vec<u8>, AppError> {
    let jitter = (id.wrapping_mul(2_654_435_761) % 30) as u64;
    tokio::time::sleep(Duration::from_millis(5 + jitter)).await;

    // Every 7th request fails, so `watch --ret Err` has something to show.
    if id % 7 == 0 {
        return Err(AppError::Timeout(format!("block {block_id}")));
    }
    Ok(vec![b'x'; BLOCK_LEN])
}

/// Synchronous leaf: a plain `fn` probe nested under an async parent.
#[rthas::trace]
fn checksum(bytes: &[u8]) -> u64 {
    bytes.iter().map(|b| u64::from(*b)).sum()
}

fn hash(s: &str) -> u64 {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in s.as_bytes() {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x100_0000_01b3);
    }
    h
}

// ---------------------------------------------------------------------------
// Driver
// ---------------------------------------------------------------------------

#[tokio::main]
async fn main() {
    rthas::init();

    let paths = [
        "/data/a.txt",
        "/data/missing.txt",
        "/data/big.parquet",
        "/data/report.csv",
    ];

    eprintln!("example-app pid={} — now try:", std::process::id());
    eprintln!("  cargo run --bin rthas -- trace handle_request --count 3");
    eprintln!("  cargo run --bin rthas -- watch read_block --ret Err --count 5");
    eprintln!("  cargo run --bin rthas -- top --n 5 --by max");

    let mut id: u64 = 0;
    loop {
        let path = paths[(id as usize) % paths.len()];
        // Concurrent tasks: the `task#N` column is what lets you tell one
        // logical request from another as it hops across worker threads.
        tokio::spawn(async move {
            match handle_request(id, path).await {
                Ok(n) => eprintln!("[{id}] ok: checksum {n} for {path}"),
                Err(e) => eprintln!("[{id}] err: {e}"),
            }
        });
        id += 1;
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}
