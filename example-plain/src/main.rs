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

//! A tiny service with **no** `rthas` probes. Used to exercise
//! `rthas attach --ebpf` on Linux:
//!
//! ```text
//! cargo build -p example-plain -p rthas-ebpf -p rthas-cli
//! ./target/debug/example-plain
//! sudo ./target/debug/rthas attach --ebpf <pid>
//! ./target/debug/rthas trace handle_request --count 3
//! ```

use std::thread;
use std::time::Duration;

#[inline(never)]
fn handle_request(id: u64, path: &str) -> Result<u64, &'static str> {
    let _ = lookup_metadata(path);
    Ok(read_block(id))
}

#[inline(never)]
fn lookup_metadata(path: &str) -> usize {
    path.len()
}

#[inline(never)]
fn read_block(id: u64) -> u64 {
    thread::sleep(Duration::from_millis(5));
    id.wrapping_mul(3)
}

fn main() {
    eprintln!("example-plain pid={} (no rthas probes)", std::process::id());
    let mut id = 0u64;
    loop {
        let _ = handle_request(id, "/data/a.txt");
        id += 1;
        thread::sleep(Duration::from_millis(80));
    }
}
